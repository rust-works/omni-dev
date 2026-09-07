//! CLI commands for `omni-dev drive docs` — reading the *structural model*
//! of a Google Doc via the Docs v1 API (issue #1615).
//!
//! Nested under `drive` rather than given its own top-level tree, for the
//! same reasons `drive sheets` is: it inherits `--account` resolution, the
//! `auth` commands and the write-permission diagnostics, because a Doc is a
//! Drive file and the permission gate is a Drive concept.

pub(crate) mod create;
pub(crate) mod info;
pub(crate) mod read;
pub(crate) mod write;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::drive::client::DriveClient;

/// Reads the structure and text of a Google Doc.
#[derive(Parser)]
pub struct DocsCommand {
    /// The docs subcommand to execute.
    #[command(subcommand)]
    pub command: DocsSubcommands,
}

/// Docs subcommands.
#[derive(Subcommand)]
pub enum DocsSubcommands {
    /// Shows a document's title, revision id and structural outline.
    Info(info::InfoCommand),
    /// Reads a document's structural elements with their index ranges.
    Read(read::ReadCommand),
    /// Replaces every occurrence of some text, gated by the
    /// write-permission rules (issue #1615). Requires the `drive.file` or
    /// `drive` scope (`drive auth login --write-file`/`--write-full`).
    Replace(write::ReplaceCommand),
    /// Appends text to the end of a document, gated by the write-permission
    /// rules (issue #1615).
    Append(write::AppendCommand),
    /// Creates a new Google Doc, optionally seeded with text. Gated by the
    /// write-permission rules' `create` operation (issue #1615).
    Create(create::CreateCommand),
}

impl DocsCommand {
    /// Runs the command against the shared Drive client resolved by the
    /// parent `DriveCommand::execute`.
    ///
    /// Each leaf derives its own `DocsClient` from that Drive client so the
    /// two hosts share one OAuth session — see
    /// [`crate::drive::docs::client::DocsClient::from_drive_client`].
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        match self.command {
            DocsSubcommands::Info(cmd) => cmd.execute(client).await,
            DocsSubcommands::Read(cmd) => cmd.execute(client).await,
            DocsSubcommands::Replace(cmd) => cmd.execute(client).await,
            DocsSubcommands::Append(cmd) => cmd.execute(client).await,
            DocsSubcommands::Create(cmd) => cmd.execute(client).await,
        }
    }
}
