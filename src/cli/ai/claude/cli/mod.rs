//! Claude Code CLI inspection commands.

mod model;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Claude Code CLI inspection.
#[derive(Parser)]
pub struct CliCommand {
    /// CLI subcommand to execute.
    #[command(subcommand)]
    pub command: CliSubcommands,
}

/// CLI subcommands.
#[derive(Subcommand)]
pub enum CliSubcommands {
    /// Model selection diagnostics.
    Model(model::ModelCommand),
}

impl CliCommand {
    /// Executes the CLI command.
    pub fn execute(self) -> Result<()> {
        match self.command {
            CliSubcommands::Model(cmd) => cmd.execute(),
        }
    }
}
