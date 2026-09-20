//! CLI commands for `omni-dev drive sheets` — reading and writing the
//! *cells* of a Google Sheet via the Sheets v4 API (issue #1589), editing
//! its *structure* via `spreadsheets.batchUpdate` (issue #1613),
//! *destructively* editing it the same way (issue #1623), and applying
//! formatting, data validation and protected ranges (issue #1643), the
//! basic filter and filter views (issue #1794), and conditional formatting
//! rules (issue #1793).
//!
//! Nested under `drive` rather than given its own top-level tree so it
//! inherits `--account` resolution, the `auth` commands and the write-
//! permission diagnostics: a Sheet is a Drive file, and the permission gate
//! is a Drive concept.

pub(crate) mod conditional_format;
pub(crate) mod create;
pub(crate) mod developer_metadata;
pub(crate) mod filter;
pub(crate) mod format;
pub(crate) mod info;
pub(crate) mod protection;
pub(crate) mod read;
pub(crate) mod structure;
pub(crate) mod validation;
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
    /// Overwrites the cells of a range, gated by the
    /// write-permission rules (issues #1589, #1612). Requires the `drive.file` or
    /// `drive` scope (`drive auth login --write-file`/`--write-full`).
    Write(write::WriteCommand),
    /// Appends rows after the last row of a range's table, gated by the
    /// write-permission rules (issues #1589, #1612).
    Append(write::AppendCommand),
    /// Clears a range's values, leaving formatting intact. Gated by the
    /// write-permission rules (issues #1589, #1612).
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
    /// Deletes an entire sheet (tab) from a spreadsheet. Gated by the folder
    /// write-permission rules' `sheets-delete` operation (issue #1623).
    /// Cannot be undone through omni-dev.
    DeleteSheet(structure::DeleteSheetCommand),
    /// Deletes whole rows, shifting existing rows up. Gated by the folder
    /// write-permission rules' `sheets-delete` operation (issue #1623).
    /// Cannot be undone through omni-dev.
    DeleteRows(structure::DeleteRowsCommand),
    /// Deletes whole columns, shifting existing columns left. Gated by the
    /// folder write-permission rules' `sheets-delete` operation (issue
    /// #1623). Cannot be undone through omni-dev.
    DeleteColumns(structure::DeleteColumnsCommand),
    /// Deletes a rectangular cell range, shifting the remainder along one
    /// axis. Gated by the folder write-permission rules' `sheets-delete`
    /// operation (issue #1623). Cannot be undone through omni-dev.
    DeleteRange(structure::DeleteRangeCommand),
    /// Copies an existing sheet within the same workbook. Gated by the
    /// folder write-permission rules' `sheets-structure` operation
    /// (issue #1643).
    DuplicateSheet(structure::DuplicateSheetCommand),
    /// Moves an existing sheet to a new position among its siblings. Gated
    /// by the folder write-permission rules' `sheets-structure` operation
    /// (issue #1643).
    ReorderSheet(structure::ReorderSheetCommand),
    /// Hides an existing sheet. Gated by the folder write-permission rules'
    /// `sheets-structure` operation (issue #1643).
    HideSheet(structure::HideSheetCommand),
    /// Shows an existing hidden sheet. Gated by the folder write-permission
    /// rules' `sheets-structure` operation (issue #1643).
    ShowSheet(structure::ShowSheetCommand),
    /// Applies a cell format across a range. Gated by the folder
    /// write-permission rules' `sheets-structure` operation (issue #1643).
    FormatCells(format::FormatCellsCommand),
    /// Sets border lines on a range's edges. Gated by the folder
    /// write-permission rules' `sheets-structure` operation (issue #1643).
    UpdateBorders(format::UpdateBordersCommand),
    /// Merges a range into one cell, discarding every value but the
    /// top-left's. Gated by the folder write-permission rules'
    /// `sheets-structure` operation (issue #1643).
    MergeCells(format::MergeCellsCommand),
    /// Splits a previously merged range back apart. Gated by the folder
    /// write-permission rules' `sheets-structure` operation (issue #1643).
    UnmergeCells(format::UnmergeCellsCommand),
    /// Resizes rows or columns to fit their content. Gated by the folder
    /// write-permission rules' `sheets-structure` operation (issue #1643).
    AutoResizeDimension(format::AutoResizeDimensionCommand),
    /// Sets an explicit pixel width (columns) or height (rows). Gated by
    /// the folder write-permission rules' `sheets-structure` operation
    /// (issue #1643).
    UpdateDimensionProperties(format::UpdateDimensionPropertiesCommand),
    /// Sets a data validation rule on a range. Gated by the folder
    /// write-permission rules' `sheets-structure` operation (issue #1643).
    ///
    /// Boxed (#1792): tranche 2's ~19 extra condition flags pushed this
    /// variant far past every sibling's size, which `clippy::large_enum_variant`
    /// flags transitively up through `SheetsSubcommands`/`DriveSubcommands`/
    /// `Commands`.
    SetDataValidation(Box<validation::SetDataValidationCommand>),
    /// Removes a range's data validation rule. Gated by the folder
    /// write-permission rules' `sheets-structure` operation (issue #1643).
    ClearDataValidation(validation::ClearDataValidationCommand),
    /// Creates or updates a developer-metadata key/value pair on a
    /// spreadsheet, sheet, row or column. Restricted to `DOCUMENT`
    /// visibility. Gated by the folder write-permission rules'
    /// `sheets-structure` operation (issue #1795).
    SetDeveloperMetadata(developer_metadata::SetDeveloperMetadataCommand),
    /// Removes developer metadata matching a key and location, after
    /// reporting what would be removed. Restricted to `DOCUMENT`
    /// visibility. Gated by the folder write-permission rules'
    /// `sheets-structure` operation (issue #1795).
    DeleteDeveloperMetadata(developer_metadata::DeleteDeveloperMetadataCommand),
    /// Searches for developer metadata by key and/or location, restricted
    /// to `DOCUMENT` visibility. Read-only and ungated, like `sheets info`
    /// (issue #1795).
    SearchDeveloperMetadata(developer_metadata::SearchDeveloperMetadataCommand),
    /// Protects a range or an entire sheet. Gated by the folder
    /// write-permission rules' `sheets-protection` operation — distinct
    /// from `sheets-structure` (issue #1643).
    ProtectRange(protection::ProtectRangeCommand),
    /// Changes an existing protected range's description, warning-only
    /// flag, or editor list. Gated by the folder write-permission rules'
    /// `sheets-protection` operation (issue #1643).
    UpdateProtection(protection::UpdateProtectionCommand),
    /// Removes a protected range. Gated by the folder write-permission
    /// rules' `sheets-protection` operation (issue #1643).
    UnprotectRange(protection::UnprotectRangeCommand),
    /// Lists the protected ranges in a spreadsheet. Read-only and ungated,
    /// like `sheets info` (issue #1643).
    ListProtections(protection::ListProtectionsCommand),
    /// Sets (upserting any existing one) the basic filter on a sheet. Gated
    /// by the folder write-permission rules' `sheets-structure` operation
    /// (issue #1794).
    SetBasicFilter(filter::SetBasicFilterCommand),
    /// Removes a sheet's basic filter. Gated by the folder write-permission
    /// rules' `sheets-structure` operation (issue #1794).
    ClearBasicFilter(filter::ClearBasicFilterCommand),
    /// Adds a named filter view. Gated by the folder write-permission
    /// rules' `sheets-structure` operation (issue #1794).
    AddFilterView(filter::AddFilterViewCommand),
    /// Changes an existing filter view's title, range, sort order, or
    /// hidden values. Gated by the folder write-permission rules'
    /// `sheets-structure` operation (issue #1794).
    UpdateFilterView(filter::UpdateFilterViewCommand),
    /// Removes a filter view. Gated by the folder write-permission rules'
    /// `sheets-structure` operation (issue #1794).
    DeleteFilterView(filter::DeleteFilterViewCommand),
    /// Lists the filter views in a spreadsheet. Read-only and ungated, like
    /// `list-protections` (issue #1794).
    ListFilterViews(filter::ListFilterViewsCommand),
    /// Adds a conditional format rule to one or more ranges. Gated by the
    /// folder write-permission rules' `sheets-structure` operation
    /// (issue #1793, ADR-0081 §1).
    ///
    /// Boxed for the same `clippy::large_enum_variant` reason as
    /// `SetDataValidation` (#1792): the condition/gradient flag set is wide.
    AddConditionalFormat(Box<conditional_format::AddConditionalFormatCommand>),
    /// Replaces the conditional format rule at an index. Gated by the
    /// folder write-permission rules' `sheets-structure` operation
    /// (issue #1793, ADR-0081 §1).
    UpdateConditionalFormat(Box<conditional_format::UpdateConditionalFormatCommand>),
    /// Removes the conditional format rule at an index. Gated by the
    /// folder write-permission rules' `sheets-structure` operation
    /// (issue #1793, ADR-0081 §1).
    DeleteConditionalFormat(conditional_format::DeleteConditionalFormatCommand),
    /// Lists the conditional format rules in a spreadsheet. Read-only and
    /// ungated, like `list-protections` (issue #1793).
    ListConditionalFormats(conditional_format::ListConditionalFormatsCommand),
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
            SheetsSubcommands::DeleteSheet(cmd) => cmd.execute(client).await,
            SheetsSubcommands::DeleteRows(cmd) => cmd.execute(client).await,
            SheetsSubcommands::DeleteColumns(cmd) => cmd.execute(client).await,
            SheetsSubcommands::DeleteRange(cmd) => cmd.execute(client).await,
            SheetsSubcommands::DuplicateSheet(cmd) => cmd.execute(client).await,
            SheetsSubcommands::ReorderSheet(cmd) => cmd.execute(client).await,
            SheetsSubcommands::HideSheet(cmd) => cmd.execute(client).await,
            SheetsSubcommands::ShowSheet(cmd) => cmd.execute(client).await,
            SheetsSubcommands::FormatCells(cmd) => cmd.execute(client).await,
            SheetsSubcommands::UpdateBorders(cmd) => cmd.execute(client).await,
            SheetsSubcommands::MergeCells(cmd) => cmd.execute(client).await,
            SheetsSubcommands::UnmergeCells(cmd) => cmd.execute(client).await,
            SheetsSubcommands::AutoResizeDimension(cmd) => cmd.execute(client).await,
            SheetsSubcommands::UpdateDimensionProperties(cmd) => cmd.execute(client).await,
            SheetsSubcommands::SetDataValidation(cmd) => cmd.execute(client).await,
            SheetsSubcommands::ClearDataValidation(cmd) => cmd.execute(client).await,
            SheetsSubcommands::SetDeveloperMetadata(cmd) => cmd.execute(client).await,
            SheetsSubcommands::DeleteDeveloperMetadata(cmd) => cmd.execute(client).await,
            SheetsSubcommands::SearchDeveloperMetadata(cmd) => cmd.execute(client).await,
            SheetsSubcommands::ProtectRange(cmd) => cmd.execute(client).await,
            SheetsSubcommands::UpdateProtection(cmd) => cmd.execute(client).await,
            SheetsSubcommands::UnprotectRange(cmd) => cmd.execute(client).await,
            SheetsSubcommands::ListProtections(cmd) => cmd.execute(client).await,
            SheetsSubcommands::SetBasicFilter(cmd) => cmd.execute(client).await,
            SheetsSubcommands::ClearBasicFilter(cmd) => cmd.execute(client).await,
            SheetsSubcommands::AddFilterView(cmd) => cmd.execute(client).await,
            SheetsSubcommands::UpdateFilterView(cmd) => cmd.execute(client).await,
            SheetsSubcommands::DeleteFilterView(cmd) => cmd.execute(client).await,
            SheetsSubcommands::ListFilterViews(cmd) => cmd.execute(client).await,
            SheetsSubcommands::AddConditionalFormat(cmd) => cmd.execute(client).await,
            SheetsSubcommands::UpdateConditionalFormat(cmd) => cmd.execute(client).await,
            SheetsSubcommands::DeleteConditionalFormat(cmd) => cmd.execute(client).await,
            SheetsSubcommands::ListConditionalFormats(cmd) => cmd.execute(client).await,
        }
    }
}
