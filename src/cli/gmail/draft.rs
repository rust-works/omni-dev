//! CLI commands for Gmail drafts (#1920).
//!
//! `draft` only ever *stages* mail: there is deliberately no `send` or
//! `delete` leaf (see [`crate::gmail::drafts_api`]).

pub(crate) mod list;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::gmail::client::GmailClient;

/// Manages Gmail drafts.
#[derive(Parser)]
pub struct DraftCommand {
    /// The draft subcommand to execute.
    #[command(subcommand)]
    pub command: DraftSubcommands,
}

/// Draft subcommands.
#[derive(Subcommand)]
pub enum DraftSubcommands {
    /// Lists drafts with their draft ids (`gmail.readonly` is enough).
    List(list::ListCommand),
}

impl DraftCommand {
    /// Executes the draft command.
    pub async fn execute(self, client: &GmailClient) -> Result<()> {
        match self.command {
            DraftSubcommands::List(cmd) => cmd.execute(client).await,
        }
    }
}
