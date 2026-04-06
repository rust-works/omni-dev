//! Claude Code diagnostics and inspection commands.

mod cli;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Claude Code diagnostics and inspection.
#[derive(Parser)]
pub struct ClaudeCommand {
    /// Claude subcommand to execute.
    #[command(subcommand)]
    pub command: ClaudeSubcommands,
}

/// Claude subcommands.
#[derive(Subcommand)]
pub enum ClaudeSubcommands {
    /// Claude Code CLI inspection.
    Cli(cli::CliCommand),
}

impl ClaudeCommand {
    /// Executes the claude command.
    pub fn execute(self) -> Result<()> {
        match self.command {
            ClaudeSubcommands::Cli(cmd) => cmd.execute(),
        }
    }
}
