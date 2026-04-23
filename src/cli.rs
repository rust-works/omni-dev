//! CLI interface for omni-dev.

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

pub mod ai;
pub mod atlassian;
pub mod commands;
pub mod config;
pub mod git;
pub mod help;

/// AI backend selector.
#[derive(Clone, Copy, Debug, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum AiBackend {
    /// Default backend dispatch (HTTP to Anthropic/Bedrock/OpenAI/Ollama via
    /// the existing `USE_*` env vars).
    Default,
    /// Shell out to the `claude -p` CLI (reuses an existing Claude Code auth
    /// session). Equivalent to setting `OMNI_DEV_AI_BACKEND=claude-cli`.
    ClaudeCli,
}

/// omni-dev: A comprehensive development toolkit.
#[derive(Parser)]
#[command(name = "omni-dev")]
#[command(about = "A comprehensive development toolkit", long_about = None)]
#[command(version)]
pub struct Cli {
    /// Selects the AI backend used by commands that invoke an AI model.
    ///
    /// Overrides the `OMNI_DEV_AI_BACKEND` environment variable.
    #[arg(long, global = true, value_enum)]
    pub ai_backend: Option<AiBackend>,

    /// Weakens the `claude-cli` sandbox by allowing the nested `claude -p`
    /// session to use its default tool set (Read/Edit/Write/Bash/Glob/Grep
    /// and any user-level MCP servers).
    ///
    /// **Only use for deliberately tool-capable use cases.** By default the
    /// nested session runs with `--tools ""` and cannot touch the
    /// file system. This flag removes that guard. Equivalent to setting
    /// `OMNI_DEV_CLAUDE_CLI_ALLOW_TOOLS=true`.
    ///
    /// Ignored when `--ai-backend` is not `claude-cli`.
    #[arg(long, global = true)]
    pub claude_cli_allow_tools: bool,

    /// Per-invocation spending cap in USD for the `claude-cli` backend.
    ///
    /// Forwarded to `claude -p --max-budget-usd`. When the nested session
    /// exceeds this budget it aborts rather than running away with cost.
    /// Equivalent to setting `OMNI_DEV_CLAUDE_CLI_MAX_BUDGET_USD`.
    ///
    /// Ignored when `--ai-backend` is not `claude-cli`.
    #[arg(long, global = true, value_name = "AMOUNT")]
    pub claude_cli_max_budget_usd: Option<f64>,

    /// The main command to execute.
    #[command(subcommand)]
    pub command: Commands,
}

/// Main command categories.
#[derive(Subcommand)]
pub enum Commands {
    /// AI operations.
    Ai(ai::AiCommand),
    /// Git-related operations.
    Git(git::GitCommand),
    /// Command template management.
    Commands(commands::CommandsCommand),
    /// Configuration and model information.
    Config(config::ConfigCommand),
    /// Atlassian: JIRA and Confluence operations.
    Atlassian(atlassian::AtlassianCommand),
    /// Displays comprehensive help for all commands.
    #[command(name = "help-all")]
    HelpAll(help::HelpCommand),
}

impl Cli {
    /// Executes the CLI command.
    pub async fn execute(self) -> Result<()> {
        // Propagate --ai-backend to the env var the factory reads. Setting
        // the env var here (rather than threading an extra argument through
        // every command) keeps the factory signature stable.
        if let Some(backend) = self.ai_backend {
            match backend {
                AiBackend::Default => std::env::remove_var("OMNI_DEV_AI_BACKEND"),
                AiBackend::ClaudeCli => std::env::set_var("OMNI_DEV_AI_BACKEND", "claude-cli"),
            }
        }

        if self.claude_cli_allow_tools {
            std::env::set_var("OMNI_DEV_CLAUDE_CLI_ALLOW_TOOLS", "true");
        }

        if let Some(budget) = self.claude_cli_max_budget_usd {
            std::env::set_var("OMNI_DEV_CLAUDE_CLI_MAX_BUDGET_USD", format!("{budget}"));
        }

        match self.command {
            Commands::Ai(ai_cmd) => ai_cmd.execute().await,
            Commands::Git(git_cmd) => git_cmd.execute().await,
            Commands::Commands(commands_cmd) => commands_cmd.execute(),
            Commands::Atlassian(cmd) => cmd.execute().await,
            Commands::Config(config_cmd) => config_cmd.execute(),
            Commands::HelpAll(help_cmd) => help_cmd.execute(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_ai_backend_claude_cli() {
        let cli =
            Cli::try_parse_from(["omni-dev", "--ai-backend", "claude-cli", "help-all"]).unwrap();
        assert!(matches!(cli.ai_backend, Some(AiBackend::ClaudeCli)));
        assert!(!cli.claude_cli_allow_tools);
    }

    #[test]
    fn parses_ai_backend_default() {
        let cli = Cli::try_parse_from(["omni-dev", "--ai-backend", "default", "help-all"]).unwrap();
        assert!(matches!(cli.ai_backend, Some(AiBackend::Default)));
    }

    #[test]
    fn parses_ai_backend_absent() {
        let cli = Cli::try_parse_from(["omni-dev", "help-all"]).unwrap();
        assert!(cli.ai_backend.is_none());
        assert!(!cli.claude_cli_allow_tools);
    }

    #[test]
    fn parses_claude_cli_allow_tools_flag() {
        let cli =
            Cli::try_parse_from(["omni-dev", "--claude-cli-allow-tools", "help-all"]).unwrap();
        assert!(cli.claude_cli_allow_tools);
    }

    #[test]
    fn global_flags_accepted_after_subcommand() {
        // clap global = true allows the flag before or after the subcommand.
        let cli = Cli::try_parse_from([
            "omni-dev",
            "help-all",
            "--ai-backend",
            "claude-cli",
            "--claude-cli-allow-tools",
        ])
        .unwrap();
        assert!(matches!(cli.ai_backend, Some(AiBackend::ClaudeCli)));
        assert!(cli.claude_cli_allow_tools);
    }

    #[test]
    fn parses_max_budget_usd_flag() {
        let cli = Cli::try_parse_from([
            "omni-dev",
            "--claude-cli-max-budget-usd",
            "0.50",
            "help-all",
        ])
        .unwrap();
        assert_eq!(cli.claude_cli_max_budget_usd, Some(0.50));
    }

    #[test]
    fn max_budget_usd_absent_is_none() {
        let cli = Cli::try_parse_from(["omni-dev", "help-all"]).unwrap();
        assert!(cli.claude_cli_max_budget_usd.is_none());
    }

    #[test]
    fn max_budget_usd_rejects_non_numeric() {
        let result = Cli::try_parse_from([
            "omni-dev",
            "--claude-cli-max-budget-usd",
            "cheap",
            "help-all",
        ]);
        let Err(err) = result else {
            panic!("expected parse error for non-numeric budget");
        };
        assert!(err.to_string().contains("invalid"));
    }
}
