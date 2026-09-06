//! CLI commands for `omni-dev drive sheets` — reading and writing the
//! *cells* of a Google Sheet via the Sheets v4 API (issue #1589), and
//! editing its *structure* via `spreadsheets.batchUpdate` (issue #1613).
//!
//! Nested under `drive` rather than given its own top-level tree so it
//! inherits `--account` resolution, the `auth` commands and the write-
//! permission diagnostics: a Sheet is a Drive file, and the permission gate
//! is a Drive concept.

pub(crate) mod create;
pub(crate) mod info;
pub(crate) mod read;
pub(crate) mod structure;
pub(crate) mod values;
pub(crate) mod write;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::drive::client::DriveClient;

/// Reads and writes the cells and structure of a Google Sheet.
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
    /// Overwrites the cells of a range, gated by the folder
    /// write-permission rules (issue #1589). Requires the `drive.file` or
    /// `drive` scope (`drive auth login --write-file`/`--write-full`).
    Write(write::WriteCommand),
    /// Appends rows after the last row of a range's table, gated by the
    /// folder write-permission rules (issue #1589).
    Append(write::AppendCommand),
    /// Clears a range's values, leaving formatting intact. Gated by the
    /// folder write-permission rules (issue #1589).
    Clear(write::ClearCommand),
    /// Creates a new Google Sheet, optionally seeded with values. Gated by
    /// the folder write-permission rules' `create` operation (issue #1589).
    Create(create::CreateCommand),
    /// Adds a new sheet (tab) to a spreadsheet. Gated by the folder
    /// write-permission rules' `sheets-structure` operation (issue #1613).
    AddSheet(structure::AddSheetCommand),
    /// Renames an existing sheet. Gated by the folder write-permission
    /// rules' `sheets-structure` operation (issue #1613).
    RenameSheet(structure::RenameSheetCommand),
    /// Inserts empty rows, shifting existing rows down. Gated by the folder
    /// write-permission rules' `sheets-structure` operation (issue #1613).
    InsertRows(structure::InsertRowsCommand),
    /// Inserts empty columns, shifting existing columns right. Gated by the
    /// folder write-permission rules' `sheets-structure` operation
    /// (issue #1613).
    InsertColumns(structure::InsertColumnsCommand),
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
            SheetsSubcommands::Write(cmd) => cmd.execute(client).await,
            SheetsSubcommands::Append(cmd) => cmd.execute(client).await,
            SheetsSubcommands::Clear(cmd) => cmd.execute(client).await,
            SheetsSubcommands::Create(cmd) => cmd.execute(client).await,
            SheetsSubcommands::AddSheet(cmd) => cmd.execute(client).await,
            SheetsSubcommands::RenameSheet(cmd) => cmd.execute(client).await,
            SheetsSubcommands::InsertRows(cmd) => cmd.execute(client).await,
            SheetsSubcommands::InsertColumns(cmd) => cmd.execute(client).await,
        }
    }
}
