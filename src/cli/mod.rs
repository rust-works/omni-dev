//! CLI interface for omni-dev

use anyhow::Result;
use clap::{Parser, Subcommand};

pub mod git;

/// omni-dev: A comprehensive development toolkit
#[derive(Parser)]
#[command(name = "omni-dev")]
#[command(about = "A comprehensive development toolkit", long_about = None)]
#[command(version)]
pub struct Cli {
    /// The main command to execute
    #[command(subcommand)]
    pub command: Commands,
}

/// Main command categories
#[derive(Subcommand)]
pub enum Commands {
    /// Git-related operations
    Git(git::GitCommand),
}

impl Cli {
    /// Execute the CLI command
    pub fn execute(self) -> Result<()> {
        match self.command {
            Commands::Git(git_cmd) => git_cmd.execute(),
        }
    }
}
