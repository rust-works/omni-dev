//! Git operations and repository management.

use anyhow::{Context, Result};
use git2::Repository;

pub mod amendment;
pub mod commit;
pub mod remote;
pub mod repository;

pub use amendment::AmendmentHandler;
pub use commit::{CommitAnalysis, CommitAnalysisForAI, CommitInfo, CommitInfoForAI};
pub use remote::RemoteInfo;
pub use repository::GitRepository;

/// Number of hex characters to show in abbreviated commit hashes.
pub const SHORT_HASH_LEN: usize = 8;

/// Checks if the current directory is a git repository.
pub fn check_git_repo() -> Result<()> {
    Repository::open(".").context("Not in a git repository")?;
    Ok(())
}

/// Checks if the working directory is clean.
pub fn check_working_directory_clean() -> Result<()> {
    let repo = Repository::open(".").context("Failed to open git repository")?;

    let statuses = repo
        .statuses(None)
        .context("Failed to get repository status")?;

    if !statuses.is_empty() {
        anyhow::bail!(
            "Working directory is not clean. Please commit or stash changes before amending commit messages."
        );
    }

    Ok(())
}
