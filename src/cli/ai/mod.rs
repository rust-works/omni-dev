//! AI commands.

mod chat;
mod claude;

pub use chat::ChatCommand;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// AI operations.
#[derive(Parser)]
pub struct AiCommand {
    /// The AI subcommand to execute.
    #[command(subcommand)]
    pub command: AiSubcommands,
}

/// AI subcommands.
#[derive(Subcommand)]
pub enum AiSubcommands {
    /// Interactive AI chat session.
    Chat(ChatCommand),
    /// Claude Code diagnostics and inspection.
    Claude(claude::ClaudeCommand),
}

impl AiCommand {
    /// Executes the AI command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            AiSubcommands::Chat(cmd) => cmd.execute().await,
            AiSubcommands::Claude(cmd) => cmd.execute(),
        }
    }
}
