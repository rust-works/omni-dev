//! Model selection diagnostics commands.

mod resolve;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Model selection diagnostics.
#[derive(Parser)]
pub struct ModelCommand {
    /// Model subcommand to execute.
    #[command(subcommand)]
    pub command: ModelSubcommands,
}

/// Model subcommands.
#[derive(Subcommand)]
pub enum ModelSubcommands {
    /// Show how Claude Code resolves the active model in the current directory.
    Resolve(resolve::ResolveCommand),
}

impl ModelCommand {
    /// Executes the model command.
    pub fn execute(self) -> Result<()> {
        match self.command {
            ModelSubcommands::Resolve(cmd) => cmd.execute(),
        }
    }
}
