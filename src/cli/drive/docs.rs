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
pub(crate) mod style;
pub(crate) mod table;
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
    /// Shows a document's title, revision id and structural outline
    /// (mirrors the `drive_docs_info` MCP tool).
    Info(info::InfoCommand),
    /// Reads a document's structural elements with their index ranges
    /// (mirrors the `drive_docs_read` MCP tool).
    Read(read::ReadCommand),
    /// Replaces every occurrence of some text, gated by the
    /// write-permission rules (issue #1615). Requires the `drive.file` or
    /// `drive` scope (`drive auth login --write-file`/`--write-full`).
    /// (mirrors the `drive_docs_replace` MCP tool).
    Replace(write::ReplaceCommand),
    /// Appends text to the end of a document, gated by the write-permission
    /// rules (issue #1615).
    /// (mirrors the `drive_docs_append` MCP tool).
    Append(write::AppendCommand),
    /// Inserts text before or after a unique body or segment anchor, gated by docs-write.
    Insert(write::InsertCommand),
    /// Deletes a unique match or inclusive anchor range, gated by docs-delete.
    Delete(write::DeleteCommand),
    /// Creates bullets or numbering on anchor-selected paragraphs, under docs-write.
    CreateBullets(write::CreateBulletsCommand),
    /// Removes bullets while preserving prose and indentation, under docs-write.
    DeleteBullets(write::DeleteBulletsCommand),
    /// Sets explicit character formatting on an anchored range under docs-format.
    TextStyle(style::TextStyleCommand),
    /// Styles whole paragraphs overlapping an anchored range under docs-format.
    ParagraphStyle(style::ParagraphStyleCommand),
    /// Inserts an empty table next to a body anchor, under docs-structure.
    InsertTable(table::InsertTableCommand),
    /// Inserts one empty table row above/below a reference cell, under docs-structure.
    InsertTableRow(table::InsertDimensionCommand),
    /// Inserts one empty table column left/right of a reference cell, under docs-structure.
    InsertTableColumn(table::InsertDimensionCommand),
    /// Removes one table row, under docs-table-delete; refuses the last row.
    DeleteTableRow(table::DeleteDimensionCommand),
    /// Removes one table column, under docs-table-delete; refuses the last column.
    DeleteTableColumn(table::DeleteDimensionCommand),
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
            DocsSubcommands::Insert(cmd) => cmd.execute(client).await,
            DocsSubcommands::Delete(cmd) => cmd.execute(client).await,
            DocsSubcommands::CreateBullets(cmd) => cmd.execute(client).await,
            DocsSubcommands::DeleteBullets(cmd) => cmd.execute(client).await,
            DocsSubcommands::TextStyle(cmd) => cmd.execute(client).await,
            DocsSubcommands::ParagraphStyle(cmd) => cmd.execute(client).await,
            DocsSubcommands::InsertTable(cmd) => cmd.execute(client).await,
            DocsSubcommands::InsertTableRow(cmd) => {
                cmd.execute(client, crate::drive::docs::table::TableVerb::InsertRow)
                    .await
            }
            DocsSubcommands::InsertTableColumn(cmd) => {
                cmd.execute(client, crate::drive::docs::table::TableVerb::InsertColumn)
                    .await
            }
            DocsSubcommands::DeleteTableRow(cmd) => {
                cmd.execute(client, crate::drive::docs::table::TableVerb::DeleteRow)
                    .await
            }
            DocsSubcommands::DeleteTableColumn(cmd) => {
                cmd.execute(client, crate::drive::docs::table::TableVerb::DeleteColumn)
                    .await
            }
            DocsSubcommands::Create(cmd) => cmd.execute(client).await,
        }
    }
}
