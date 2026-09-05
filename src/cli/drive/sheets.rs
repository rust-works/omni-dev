//! CLI commands for `omni-dev drive sheets` — reading the *cells* of a
//! Google Sheet via the Sheets v4 API (issue #1589).
//!
//! Nested under `drive` rather than given its own top-level tree so it
//! inherits `--account` resolution, the `auth` commands and the write-
//! permission diagnostics: a Sheet is a Drive file, and the permission gate
//! is a Drive concept.

pub(crate) mod info;
pub(crate) mod read;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::drive::client::DriveClient;

/// Reads the cells of a Google Sheet.
#[derive(Parser)]
pub struct SheetsCommand {
    /// The sheets subcommand to execute.
    #[command(subcommand)]
    pub command: SheetsSubcommands,
}

/// Sheets subcommands.
#[derive(Subcommand)]
pub enum SheetsSubcommands {
    /// Shows a spreadsheet's title and the sheets (tabs) it contains.
    Info(info::InfoCommand),
    /// Reads cell values from one range, or from every sheet.
    Read(read::ReadCommand),
}

impl SheetsCommand {
    /// Runs the command against the shared Drive client resolved by the
    /// parent `DriveCommand::execute`.
    ///
    /// Each leaf derives its own `SheetsClient` from that Drive client so the
    /// two hosts share one OAuth session — see
    /// [`crate::drive::sheets::client::SheetsClient::from_drive_client`].
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        match self.command {
            SheetsSubcommands::Info(cmd) => cmd.execute(client).await,
            SheetsSubcommands::Read(cmd) => cmd.execute(client).await,
        }
    }
}
