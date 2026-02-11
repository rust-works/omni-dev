//! CLI interface for omni-dev.

use anyhow::Result;
use clap::{Parser, Subcommand};

pub mod ai;
pub mod commands;
pub mod config;
pub mod git;
pub mod help;

/// omni-dev: A comprehensive development toolkit.
#[derive(Parser)]
#[command(name = "omni-dev")]
#[command(about = "A comprehensive development toolkit", long_about = None)]
#[command(version)]
pub struct Cli {
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
    /// Displays comprehensive help for all commands.
    #[command(name = "help-all")]
    HelpAll(help::HelpCommand),
}

impl Cli {
    /// Executes the CLI command.
    pub fn execute(self) -> Result<()> {
        match self.command {
            Commands::Ai(ai_cmd) => ai_cmd.execute(),
            Commands::Git(git_cmd) => git_cmd.execute(),
            Commands::Commands(commands_cmd) => commands_cmd.execute(),
            Commands::Config(config_cmd) => config_cmd.execute(),
            Commands::HelpAll(help_cmd) => help_cmd.execute(),
        }
    }
}
