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
    /// Branch-related operations
    Branch(BranchCommand),
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

/// Branch operations
#[derive(Parser)]
pub struct BranchCommand {
    /// Branch subcommand to execute
    #[command(subcommand)]
    pub command: BranchSubcommands,
}

/// Branch subcommands
#[derive(Subcommand)]
pub enum BranchSubcommands {
    /// Analyze branch commits and output repository information in YAML format
    Info(InfoCommand),
}

/// Info command options
#[derive(Parser)]
pub struct InfoCommand {
    /// Base branch to compare against (defaults to main/master)
    #[arg(value_name = "BASE_BRANCH")]
    pub base_branch: Option<String>,
}

impl GitCommand {
    /// Execute git command
    pub fn execute(self) -> Result<()> {
        match self.command {
            GitSubcommands::Commit(commit_cmd) => commit_cmd.execute(),
            GitSubcommands::Branch(branch_cmd) => branch_cmd.execute(),
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
        use crate::data::{FieldExplanation, FileStatusInfo, RepositoryView, WorkingDirectoryInfo};
        use crate::git::{GitRepository, RemoteInfo};

        let commit_range = self.commit_range.as_deref().unwrap_or("HEAD");

        // Open git repository
        let repo = GitRepository::open()
            .context("Failed to open git repository. Make sure you're in a git repository.")?;

        // Get working directory status
        let wd_status = repo.get_working_directory_status()?;
        let working_directory = WorkingDirectoryInfo {
            clean: wd_status.clean,
            untracked_changes: wd_status
                .untracked_changes
                .into_iter()
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
        let mut repo_view = RepositoryView {
            explanation: FieldExplanation::default(),
            working_directory,
            remotes,
            commits,
            branch_info: None,
            pr_template: None,
            branch_prs: None,
        };

        // Update field presence based on actual data
        repo_view.update_field_presence();

        // Output as YAML
        let yaml_output = crate::data::to_yaml(&repo_view)?;
        println!("{}", yaml_output);

        Ok(())
    }
}

impl AmendCommand {
    /// Execute amend command
    pub fn execute(self) -> Result<()> {
        use crate::git::AmendmentHandler;

        println!("🔄 Starting commit amendment process...");
        println!("📄 Loading amendments from: {}", self.yaml_file);

        // Create amendment handler and apply amendments
        let handler = AmendmentHandler::new().context("Failed to initialize amendment handler")?;

        handler
            .apply_amendments(&self.yaml_file)
            .context("Failed to apply amendments")?;

        Ok(())
    }
}

impl BranchCommand {
    /// Execute branch command
    pub fn execute(self) -> Result<()> {
        match self.command {
            BranchSubcommands::Info(info_cmd) => info_cmd.execute(),
        }
    }
}

impl InfoCommand {
    /// Execute info command
    pub fn execute(self) -> Result<()> {
        use crate::data::{
            BranchInfo, FieldExplanation, FileStatusInfo, RepositoryView, WorkingDirectoryInfo,
        };
        use crate::git::{GitRepository, RemoteInfo};

        // Open git repository
        let repo = GitRepository::open()
            .context("Failed to open git repository. Make sure you're in a git repository.")?;

        // Get current branch name
        let current_branch = repo.get_current_branch().context(
            "Failed to get current branch. Make sure you're not in detached HEAD state.",
        )?;

        // Determine base branch
        let base_branch = match self.base_branch {
            Some(branch) => {
                // Validate that the specified base branch exists
                if !repo.branch_exists(&branch)? {
                    anyhow::bail!("Base branch '{}' does not exist", branch);
                }
                branch
            }
            None => {
                // Default to main or master
                if repo.branch_exists("main")? {
                    "main".to_string()
                } else if repo.branch_exists("master")? {
                    "master".to_string()
                } else {
                    anyhow::bail!("No default base branch found (main or master)");
                }
            }
        };

        // Calculate commit range: [base_branch]..HEAD
        let commit_range = format!("{}..HEAD", base_branch);

        // Get working directory status
        let wd_status = repo.get_working_directory_status()?;
        let working_directory = WorkingDirectoryInfo {
            clean: wd_status.clean,
            untracked_changes: wd_status
                .untracked_changes
                .into_iter()
                .map(|fs| FileStatusInfo {
                    status: fs.status,
                    file: fs.file,
                })
                .collect(),
        };

        // Get remote information
        let remotes = RemoteInfo::get_all_remotes(repo.repository())?;

        // Parse commit range and get commits
        let commits = repo.get_commits_in_range(&commit_range)?;

        // Check for PR template
        let pr_template = Self::read_pr_template().ok();

        // Get PRs for current branch
        let branch_prs = Self::get_branch_prs(&current_branch)
            .ok()
            .filter(|prs| !prs.is_empty());

        // Build repository view with branch info
        let mut repo_view = RepositoryView {
            explanation: FieldExplanation::default(),
            working_directory,
            remotes,
            commits,
            branch_info: Some(BranchInfo {
                branch: current_branch,
            }),
            pr_template,
            branch_prs,
        };

        // Update field presence based on actual data
        repo_view.update_field_presence();

        // Output as YAML
        let yaml_output = crate::data::to_yaml(&repo_view)?;
        println!("{}", yaml_output);

        Ok(())
    }

    /// Read PR template file if it exists
    fn read_pr_template() -> Result<String> {
        use std::fs;
        use std::path::Path;

        let template_path = Path::new(".github/pull_request_template.md");
        if template_path.exists() {
            fs::read_to_string(template_path)
                .context("Failed to read .github/pull_request_template.md")
        } else {
            anyhow::bail!("PR template file does not exist")
        }
    }

    /// Get pull requests for the current branch using gh CLI
    fn get_branch_prs(branch_name: &str) -> Result<Vec<crate::data::PullRequest>> {
        use serde_json::Value;
        use std::process::Command;

        // Use gh CLI to get PRs for the branch
        let output = Command::new("gh")
            .args([
                "pr",
                "list",
                "--head",
                branch_name,
                "--json",
                "number,title,state,url,body",
                "--limit",
                "50",
            ])
            .output()
            .context("Failed to execute gh command")?;

        if !output.status.success() {
            anyhow::bail!(
                "gh command failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let json_str = String::from_utf8_lossy(&output.stdout);
        let prs_json: Value =
            serde_json::from_str(&json_str).context("Failed to parse PR JSON from gh")?;

        let mut prs = Vec::new();
        if let Some(prs_array) = prs_json.as_array() {
            for pr_json in prs_array {
                if let (Some(number), Some(title), Some(state), Some(url), Some(body)) = (
                    pr_json.get("number").and_then(|n| n.as_u64()),
                    pr_json.get("title").and_then(|t| t.as_str()),
                    pr_json.get("state").and_then(|s| s.as_str()),
                    pr_json.get("url").and_then(|u| u.as_str()),
                    pr_json.get("body").and_then(|b| b.as_str()),
                ) {
                    prs.push(crate::data::PullRequest {
                        number,
                        title: title.to_string(),
                        state: state.to_string(),
                        url: url.to_string(),
                        body: body.to_string(),
                    });
                }
            }
        }

        Ok(prs)
    }
}
