//! Coverage analysis CLI commands.

pub(crate) mod diff;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Coverage analysis: diff/patch coverage for PR comments.
#[derive(Parser)]
pub struct CoverageCommand {
    /// The coverage subcommand to execute.
    #[command(subcommand)]
    pub command: CoverageSubcommands,
}

/// Coverage subcommands.
#[derive(Subcommand)]
pub enum CoverageSubcommands {
    /// Analyses diff/patch coverage from a per-line report and a git diff.
    Diff(diff::DiffCommand),
}

impl CoverageCommand {
    /// Executes the coverage command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            CoverageSubcommands::Diff(cmd) => cmd.execute().await,
        }
    }
}
