//! Git-related CLI commands.

mod amend;
mod check;
mod create_pr;
mod info;
mod twiddle;
mod view;

pub use amend::AmendCommand;
pub use check::CheckCommand;
pub use create_pr::{CreatePrCommand, PrContent};
pub use info::InfoCommand;
pub use twiddle::TwiddleCommand;
pub use view::ViewCommand;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

/// Parses a `--beta-header key:value` string into a `(key, value)` tuple.
pub(crate) fn parse_beta_header(s: &str) -> Result<(String, String)> {
    let (k, v) = s.split_once(':').ok_or_else(|| {
        anyhow::anyhow!("Invalid --beta-header format '{}'. Expected key:value", s)
    })?;
    Ok((k.to_string(), v.to_string()))
}

/// Git operations.
#[derive(Parser)]
pub struct GitCommand {
    /// Git subcommand to execute.
    #[command(subcommand)]
    pub command: GitSubcommands,
}

/// Git subcommands.
#[derive(Subcommand)]
pub enum GitSubcommands {
    /// Commit-related operations.
    Commit(CommitCommand),
    /// Branch-related operations.
    Branch(BranchCommand),
}

/// Commit operations.
#[derive(Parser)]
pub struct CommitCommand {
    /// Commit subcommand to execute.
    #[command(subcommand)]
    pub command: CommitSubcommands,
}

/// Commit subcommands.
#[derive(Subcommand)]
pub enum CommitSubcommands {
    /// Commit message operations.
    Message(MessageCommand),
}

/// Message operations.
#[derive(Parser)]
pub struct MessageCommand {
    /// Message subcommand to execute.
    #[command(subcommand)]
    pub command: MessageSubcommands,
}

/// Message subcommands.
#[derive(Subcommand)]
pub enum MessageSubcommands {
    /// Analyzes commits and outputs repository information in YAML format.
    View(ViewCommand),
    /// Amends commit messages based on a YAML configuration file.
    Amend(AmendCommand),
    /// AI-powered commit message improvement using Claude.
    Twiddle(TwiddleCommand),
    /// Checks commit messages against guidelines without modifying them.
    Check(CheckCommand),
}

/// Branch operations.
#[derive(Parser)]
pub struct BranchCommand {
    /// Branch subcommand to execute.
    #[command(subcommand)]
    pub command: BranchSubcommands,
}

/// Branch subcommands.
#[derive(Subcommand)]
pub enum BranchSubcommands {
    /// Analyzes branch commits and outputs repository information in YAML format.
    Info(InfoCommand),
    /// Create operations.
    Create(CreateCommand),
}

/// Create operations.
#[derive(Parser)]
pub struct CreateCommand {
    /// Create subcommand to execute.
    #[command(subcommand)]
    pub command: CreateSubcommands,
}

/// Create subcommands.
#[derive(Subcommand)]
pub enum CreateSubcommands {
    /// Creates a pull request with AI-generated description.
    Pr(CreatePrCommand),
}

impl GitCommand {
    /// Executes the git command.
    pub fn execute(self) -> Result<()> {
        match self.command {
            GitSubcommands::Commit(commit_cmd) => commit_cmd.execute(),
            GitSubcommands::Branch(branch_cmd) => branch_cmd.execute(),
        }
    }
}

impl CommitCommand {
    /// Executes the commit command.
    pub fn execute(self) -> Result<()> {
        match self.command {
            CommitSubcommands::Message(message_cmd) => message_cmd.execute(),
        }
    }
}

impl MessageCommand {
    /// Executes the message command.
    pub fn execute(self) -> Result<()> {
        match self.command {
            MessageSubcommands::View(view_cmd) => view_cmd.execute(),
            MessageSubcommands::Amend(amend_cmd) => amend_cmd.execute(),
            MessageSubcommands::Twiddle(twiddle_cmd) => {
                // Use tokio runtime for async execution
                let rt =
                    tokio::runtime::Runtime::new().context("Failed to create tokio runtime")?;
                rt.block_on(twiddle_cmd.execute())
            }
            MessageSubcommands::Check(check_cmd) => {
                // Use tokio runtime for async execution
                let rt =
                    tokio::runtime::Runtime::new().context("Failed to create tokio runtime")?;
                rt.block_on(check_cmd.execute())
            }
        }
    }
}

impl BranchCommand {
    /// Executes the branch command.
    pub fn execute(self) -> Result<()> {
        match self.command {
            BranchSubcommands::Info(info_cmd) => info_cmd.execute(),
            BranchSubcommands::Create(create_cmd) => {
                // Use tokio runtime for async execution
                let rt = tokio::runtime::Runtime::new()
                    .context("Failed to create tokio runtime for PR creation")?;
                rt.block_on(create_cmd.execute())
            }
        }
    }
}

impl CreateCommand {
    /// Executes the create command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            CreateSubcommands::Pr(pr_cmd) => pr_cmd.execute().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    // Parser trait must be in scope for try_parse_from
    use clap::Parser as _ClapParser;

    #[test]
    fn parse_beta_header_valid() {
        let (key, value) = parse_beta_header("anthropic-beta:output-128k-2025-02-19").unwrap();
        assert_eq!(key, "anthropic-beta");
        assert_eq!(value, "output-128k-2025-02-19");
    }

    #[test]
    fn parse_beta_header_multiple_colons() {
        // Only splits on the first colon
        let (key, value) = parse_beta_header("key:value:with:colons").unwrap();
        assert_eq!(key, "key");
        assert_eq!(value, "value:with:colons");
    }

    #[test]
    fn parse_beta_header_missing_colon() {
        let result = parse_beta_header("no-colon-here");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("no-colon-here"));
    }

    #[test]
    fn parse_beta_header_empty_value() {
        let (key, value) = parse_beta_header("key:").unwrap();
        assert_eq!(key, "key");
        assert_eq!(value, "");
    }

    #[test]
    fn parse_beta_header_empty_key() {
        let (key, value) = parse_beta_header(":value").unwrap();
        assert_eq!(key, "");
        assert_eq!(value, "value");
    }

    #[test]
    fn cli_parses_git_commit_message_view() {
        let cli = Cli::try_parse_from([
            "omni-dev",
            "git",
            "commit",
            "message",
            "view",
            "HEAD~3..HEAD",
        ]);
        assert!(cli.is_ok(), "Failed to parse: {:?}", cli.err());
    }

    #[test]
    fn cli_parses_git_commit_message_amend() {
        let cli = Cli::try_parse_from([
            "omni-dev",
            "git",
            "commit",
            "message",
            "amend",
            "amendments.yaml",
        ]);
        assert!(cli.is_ok(), "Failed to parse: {:?}", cli.err());
    }

    #[test]
    fn cli_parses_git_branch_info() {
        let cli = Cli::try_parse_from(["omni-dev", "git", "branch", "info"]);
        assert!(cli.is_ok(), "Failed to parse: {:?}", cli.err());
    }

    #[test]
    fn cli_parses_git_branch_info_with_base() {
        let cli = Cli::try_parse_from(["omni-dev", "git", "branch", "info", "develop"]);
        assert!(cli.is_ok(), "Failed to parse: {:?}", cli.err());
    }

    #[test]
    fn cli_parses_config_models_show() {
        let cli = Cli::try_parse_from(["omni-dev", "config", "models", "show"]);
        assert!(cli.is_ok(), "Failed to parse: {:?}", cli.err());
    }

    #[test]
    fn cli_parses_help_all() {
        let cli = Cli::try_parse_from(["omni-dev", "help-all"]);
        assert!(cli.is_ok(), "Failed to parse: {:?}", cli.err());
    }

    #[test]
    fn cli_rejects_unknown_command() {
        let cli = Cli::try_parse_from(["omni-dev", "nonexistent"]);
        assert!(cli.is_err());
    }

    #[test]
    fn cli_parses_twiddle_with_options() {
        let cli = Cli::try_parse_from([
            "omni-dev",
            "git",
            "commit",
            "message",
            "twiddle",
            "--auto-apply",
            "--no-context",
            "--concurrency",
            "8",
        ]);
        assert!(cli.is_ok(), "Failed to parse: {:?}", cli.err());
    }

    #[test]
    fn cli_parses_check_with_options() {
        let cli = Cli::try_parse_from([
            "omni-dev", "git", "commit", "message", "check", "--strict", "--quiet", "--format",
            "json",
        ]);
        assert!(cli.is_ok(), "Failed to parse: {:?}", cli.err());
    }

    #[test]
    fn cli_parses_commands_generate_all() {
        let cli = Cli::try_parse_from(["omni-dev", "commands", "generate", "all"]);
        assert!(cli.is_ok(), "Failed to parse: {:?}", cli.err());
    }

    #[test]
    fn cli_parses_ai_chat() {
        let cli = Cli::try_parse_from(["omni-dev", "ai", "chat"]);
        assert!(cli.is_ok(), "Failed to parse: {:?}", cli.err());
    }

    #[test]
    fn cli_parses_ai_chat_with_model() {
        let cli = Cli::try_parse_from(["omni-dev", "ai", "chat", "--model", "claude-sonnet-4"]);
        assert!(cli.is_ok(), "Failed to parse: {:?}", cli.err());
    }
}
