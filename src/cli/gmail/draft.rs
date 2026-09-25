//! CLI commands for Gmail drafts (#1920).
//!
//! `draft` only ever *stages* mail: there is deliberately no `send` or
//! `delete` leaf (see [`crate::gmail::drafts_api`]).

pub(crate) mod create;
pub(crate) mod list;
pub(crate) mod show;
pub(crate) mod update;

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
    /// Lists drafts with their draft ids (`gmail.readonly` is enough; mirrors
    /// the `gmail_draft_list` MCP tool).
    List(list::ListCommand),
    /// Shows one draft by its draft id (`gmail.readonly` is enough; mirrors
    /// the `gmail_draft_show` MCP tool).
    Show(show::ShowCommand),
    /// Creates a draft to review and send from Gmail (needs `gmail.modify`;
    /// CLI-only).
    Create(create::CreateCommand),
    /// Updates a draft in place, keeping its id and thread (needs
    /// `gmail.modify`; CLI-only).
    Update(update::UpdateCommand),
}

impl DraftCommand {
    /// Executes the draft command.
    pub async fn execute(self, client: &GmailClient) -> Result<()> {
        match self.command {
            DraftSubcommands::List(cmd) => cmd.execute(client).await,
            DraftSubcommands::Show(cmd) => cmd.execute(client).await,
            DraftSubcommands::Create(cmd) => cmd.execute(client).await,
            DraftSubcommands::Update(cmd) => cmd.execute(client).await,
        }
    }
}
