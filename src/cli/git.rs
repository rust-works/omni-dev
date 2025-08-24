//! Git-related CLI commands

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

/// Git operations
#[derive(Parser)]
pub struct GitCommand {
    /// Git subcommand to execute
    #[command(subcommand)]
    pub command: GitSubcommands,
}

/// Git subcommands
#[derive(Subcommand)]
pub enum GitSubcommands {
    /// Commit-related operations
    Commit(CommitCommand),
}

/// Commit operations
#[derive(Parser)]
pub struct CommitCommand {
    /// Commit subcommand to execute
    #[command(subcommand)]
    pub command: CommitSubcommands,
}

/// Commit subcommands
#[derive(Subcommand)]
pub enum CommitSubcommands {
    /// Commit message operations
    Message(MessageCommand),
}

/// Message operations
#[derive(Parser)]
pub struct MessageCommand {
    /// Message subcommand to execute
    #[command(subcommand)]
    pub command: MessageSubcommands,
}

/// Message subcommands
#[derive(Subcommand)]
pub enum MessageSubcommands {
    /// Analyze commits and output repository information in YAML format
    View(ViewCommand),
    /// Amend commit messages based on a YAML configuration file
    Amend(AmendCommand),
}

/// View command options
#[derive(Parser)]
pub struct ViewCommand {
    /// Commit range to analyze (e.g., HEAD~3..HEAD, abc123..def456)
    #[arg(value_name = "COMMIT_RANGE")]
    pub commit_range: Option<String>,
}

/// Amend command options  
#[derive(Parser)]
pub struct AmendCommand {
    /// YAML file containing commit amendments
    #[arg(value_name = "YAML_FILE")]
    pub yaml_file: String,
}

impl GitCommand {
    /// Execute git command
    pub fn execute(self) -> Result<()> {
        match self.command {
            GitSubcommands::Commit(commit_cmd) => commit_cmd.execute(),
        }
    }
}

impl CommitCommand {
    /// Execute commit command
    pub fn execute(self) -> Result<()> {
        match self.command {
            CommitSubcommands::Message(message_cmd) => message_cmd.execute(),
        }
    }
}

impl MessageCommand {
    /// Execute message command
    pub fn execute(self) -> Result<()> {
        match self.command {
            MessageSubcommands::View(view_cmd) => view_cmd.execute(),
            MessageSubcommands::Amend(amend_cmd) => amend_cmd.execute(),
        }
    }
}

impl ViewCommand {
    /// Execute view command
    pub fn execute(self) -> Result<()> {
        use crate::git::{GitRepository, RemoteInfo};
        use crate::data::{RepositoryView, FieldExplanation, WorkingDirectoryInfo, FileStatusInfo};
        
        let commit_range = self.commit_range.as_deref().unwrap_or("HEAD");
        
        // Open git repository
        let repo = GitRepository::open()
            .context("Failed to open git repository. Make sure you're in a git repository.")?;
        
        // Get working directory status
        let wd_status = repo.get_working_directory_status()?;
        let working_directory = WorkingDirectoryInfo {
            clean: wd_status.clean,
            untracked_changes: wd_status.untracked_changes.into_iter()
                .map(|fs| FileStatusInfo {
                    status: fs.status,
                    file: fs.file,
                })
                .collect(),
        };
        
        // Get remote information
        let remotes = RemoteInfo::get_all_remotes(repo.repository())?;
        
        // Parse commit range and get commits
        let commits = repo.get_commits_in_range(commit_range)?;
        
        // Build repository view
        let repo_view = RepositoryView {
            explanation: FieldExplanation::default(),
            working_directory,
            remotes,
            commits,
        };
        
        // Output as YAML
        let yaml_output = crate::data::to_yaml(&repo_view)?;
        println!("{}", yaml_output);
        
        Ok(())
    }
}

impl AmendCommand {
    /// Execute amend command
    pub fn execute(self) -> Result<()> {
        println!("Executing amend command with file: {}", self.yaml_file);

        // TODO: Implement commit amendment
        // This will be implemented in Phase 4

        Ok(())
    }
}
