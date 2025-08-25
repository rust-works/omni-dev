//! Git repository operations

use crate::git::CommitInfo;
use anyhow::{Context, Result};
use git2::{Repository, Status};

/// Git repository wrapper
pub struct GitRepository {
    repo: Repository,
}

/// Working directory status
#[derive(Debug)]
pub struct WorkingDirectoryStatus {
    /// Whether the working directory has no changes
    pub clean: bool,
    /// List of files with uncommitted changes
    pub untracked_changes: Vec<FileStatus>,
}

/// File status information
#[derive(Debug)]
pub struct FileStatus {
    /// Git status flags (e.g., "AM", "??", "M ")
    pub status: String,
    /// Path to the file relative to repository root
    pub file: String,
}

impl GitRepository {
    /// Open repository at current directory
    pub fn open() -> Result<Self> {
        let repo = Repository::open(".").context("Not in a git repository")?;

        Ok(Self { repo })
    }

    /// Open repository at specified path
    pub fn open_at<P: AsRef<std::path::Path>>(path: P) -> Result<Self> {
        let repo = Repository::open(path).context("Failed to open git repository")?;

        Ok(Self { repo })
    }

    /// Get working directory status
    pub fn get_working_directory_status(&self) -> Result<WorkingDirectoryStatus> {
        let statuses = self
            .repo
            .statuses(None)
            .context("Failed to get repository status")?;

        let mut untracked_changes = Vec::new();

        for entry in statuses.iter() {
            if let Some(path) = entry.path() {
                let status_flags = entry.status();
                let status_str = format_status_flags(status_flags);

                untracked_changes.push(FileStatus {
                    status: status_str,
                    file: path.to_string(),
                });
            }
        }

        let clean = untracked_changes.is_empty();

        Ok(WorkingDirectoryStatus {
            clean,
            untracked_changes,
        })
    }

    /// Check if working directory is clean
    pub fn is_working_directory_clean(&self) -> Result<bool> {
        let status = self.get_working_directory_status()?;
        Ok(status.clean)
    }

    /// Get repository path
    pub fn path(&self) -> &std::path::Path {
        self.repo.path()
    }

    /// Get workdir path
    pub fn workdir(&self) -> Option<&std::path::Path> {
        self.repo.workdir()
    }

    /// Get access to the underlying git2::Repository
    pub fn repository(&self) -> &Repository {
        &self.repo
    }

    /// Get current branch name
    pub fn get_current_branch(&self) -> Result<String> {
        let head = self.repo.head().context("Failed to get HEAD reference")?;

        if let Some(name) = head.shorthand() {
            if name != "HEAD" {
                return Ok(name.to_string());
            }
        }

        anyhow::bail!("Repository is in detached HEAD state")
    }

    /// Check if a branch exists
    pub fn branch_exists(&self, branch_name: &str) -> Result<bool> {
        // Check if it exists as a local branch
        if self
            .repo
            .find_branch(branch_name, git2::BranchType::Local)
            .is_ok()
        {
            return Ok(true);
        }

        // Check if it exists as a remote branch
        if self
            .repo
            .find_branch(branch_name, git2::BranchType::Remote)
            .is_ok()
        {
            return Ok(true);
        }

        // Check if we can resolve it as a reference
        if self.repo.revparse_single(branch_name).is_ok() {
            return Ok(true);
        }

        Ok(false)
    }

    /// Parse commit range and get commits
    pub fn get_commits_in_range(&self, range: &str) -> Result<Vec<CommitInfo>> {
        let mut commits = Vec::new();

        if range == "HEAD" {
            // Single HEAD commit
            let head = self.repo.head().context("Failed to get HEAD")?;
            let commit = head
                .peel_to_commit()
                .context("Failed to peel HEAD to commit")?;
            commits.push(CommitInfo::from_git_commit(&self.repo, &commit)?);
        } else if range.contains("..") {
            // Range format like HEAD~3..HEAD
            let parts: Vec<&str> = range.split("..").collect();
            if parts.len() != 2 {
                anyhow::bail!("Invalid range format: {}", range);
            }

            let start_spec = parts[0];
            let end_spec = parts[1];

            // Parse start and end commits
            let start_obj = self
                .repo
                .revparse_single(start_spec)
                .with_context(|| format!("Failed to parse start commit: {}", start_spec))?;
            let end_obj = self
                .repo
                .revparse_single(end_spec)
                .with_context(|| format!("Failed to parse end commit: {}", end_spec))?;

            let start_commit = start_obj
                .peel_to_commit()
                .context("Failed to peel start object to commit")?;
            let end_commit = end_obj
                .peel_to_commit()
                .context("Failed to peel end object to commit")?;

            // Walk from end_commit back to start_commit (exclusive)
            let mut walker = self.repo.revwalk().context("Failed to create revwalk")?;
            walker
                .push(end_commit.id())
                .context("Failed to push end commit")?;
            walker
                .hide(start_commit.id())
                .context("Failed to hide start commit")?;

            for oid in walker {
                let oid = oid.context("Failed to get commit OID from walker")?;
                let commit = self
                    .repo
                    .find_commit(oid)
                    .context("Failed to find commit")?;

                // Skip merge commits
                if commit.parent_count() > 1 {
                    continue;
                }

                commits.push(CommitInfo::from_git_commit(&self.repo, &commit)?);
            }

            // Reverse to get chronological order (oldest first)
            commits.reverse();
        } else {
            // Single commit by hash or reference
            let obj = self
                .repo
                .revparse_single(range)
                .with_context(|| format!("Failed to parse commit: {}", range))?;
            let commit = obj
                .peel_to_commit()
                .context("Failed to peel object to commit")?;
            commits.push(CommitInfo::from_git_commit(&self.repo, &commit)?);
        }

        Ok(commits)
    }
}

/// Format git status flags into string representation
fn format_status_flags(flags: Status) -> String {
    let mut status = String::new();

    if flags.contains(Status::INDEX_NEW) {
        status.push('A');
    } else if flags.contains(Status::INDEX_MODIFIED) {
        status.push('M');
    } else if flags.contains(Status::INDEX_DELETED) {
        status.push('D');
    } else if flags.contains(Status::INDEX_RENAMED) {
        status.push('R');
    } else if flags.contains(Status::INDEX_TYPECHANGE) {
        status.push('T');
    } else {
        status.push(' ');
    }

    if flags.contains(Status::WT_NEW) {
        status.push('?');
    } else if flags.contains(Status::WT_MODIFIED) {
        status.push('M');
    } else if flags.contains(Status::WT_DELETED) {
        status.push('D');
    } else if flags.contains(Status::WT_TYPECHANGE) {
        status.push('T');
    } else if flags.contains(Status::WT_RENAMED) {
        status.push('R');
    } else {
        status.push(' ');
    }

    status
}
