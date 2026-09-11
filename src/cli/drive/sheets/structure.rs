//! CLI commands for `omni-dev drive sheets add-sheet`/`rename-sheet`/
//! `insert-rows`/`insert-columns` (issue #1613), `duplicate-sheet`/
//! `reorder-sheet`/`hide-sheet`/`show-sheet` (issue #1643), and
//! `delete-sheet`/`delete-rows`/`delete-columns`/`delete-range` (issue
//! #1623).
//!
//! Twelve clap structs over one engine call. They share `run_structure`, so
//! the gate wiring, `--dry-run` handling, output rendering and request
//! logging cannot drift between them — the same arrangement `write.rs` uses
//! for its three verbs. The additive verbs are gated on
//! `DriveOperation::SheetsStructure`; the destructive ones on
//! `DriveOperation::SheetsDelete` — `StructureVerb::gate_operation` decides
//! which, not this layer.
//!
//! There is deliberately **no** command that takes a raw
//! `spreadsheets.batchUpdate` request array, additive or destructive. Each
//! verb names its effect in typed arguments, which is what lets `--dry-run`
//! describe the change; see `crate::drive::sheets::structure`.

use anyhow::Result;
use clap::Parser;

use crate::cli::drive::format::{output_as, OutputFormat};
use crate::cli::drive::helpers;
use crate::cli::format::sanitize_for_terminal;
use crate::drive::client::DriveClient;
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::structure::{describe_lines, structure, StructureOptions, StructureVerb};

/// Adds a new sheet to a spreadsheet.
#[derive(Parser)]
pub struct AddSheetCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Title for the new sheet. Must not already exist in the workbook.
    #[arg(long, value_name = "TITLE")]
    pub title: String,

    /// Zero-based position in the workbook. Omitted appends to the end.
    #[arg(long, value_name = "N")]
    pub index: Option<i64>,

    /// Initial row count. Omitted takes Sheets' own default (1000).
    #[arg(long, value_name = "N")]
    pub rows: Option<i64>,

    /// Initial column count. Omitted takes Sheets' own default (26).
    #[arg(long, value_name = "N")]
    pub columns: Option<i64>,

    /// Reports the gate verdict and the change that would be made, without
    /// calling `spreadsheets.batchUpdate`.
    #[arg(long)]
    pub dry_run: bool,

    /// The lease token from `drive lease acquire`, required unless the
    /// deciding write-permission rule sets `require_lease: false`
    /// ([ADR-0080](../../../../docs/adrs/adr-0080.md) §1/§9/§13). Never
    /// needed with `--dry-run`.
    #[arg(long, value_name = "TOKEN")]
    pub lease: Option<String>,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

/// Renames an existing sheet.
#[derive(Parser)]
pub struct RenameSheetCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Current title of the sheet to rename.
    #[arg(long, value_name = "NAME")]
    pub sheet: String,

    /// The new title.
    #[arg(long, value_name = "TITLE")]
    pub title: String,

    /// Reports the gate verdict and the change that would be made, without
    /// calling `spreadsheets.batchUpdate`.
    #[arg(long)]
    pub dry_run: bool,

    /// The lease token from `drive lease acquire`, required unless the
    /// deciding write-permission rule sets `require_lease: false`
    /// ([ADR-0080](../../../../docs/adrs/adr-0080.md) §1/§9/§13). Never
    /// needed with `--dry-run`.
    #[arg(long, value_name = "TOKEN")]
    pub lease: Option<String>,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

/// Inserts empty rows, shifting existing rows down.
#[derive(Parser)]
pub struct InsertRowsCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Title of the sheet to modify.
    #[arg(long, value_name = "NAME")]
    pub sheet: String,

    /// Insert before this row, 1-based — the row number the spreadsheet
    /// itself shows. `--at 5` puts the new rows above the current row 5.
    #[arg(long, value_name = "ROW")]
    pub at: i64,

    /// How many rows to insert.
    #[arg(long, value_name = "N", default_value_t = 1)]
    pub count: i64,

    /// Reports the gate verdict and the change that would be made, without
    /// calling `spreadsheets.batchUpdate`.
    #[arg(long)]
    pub dry_run: bool,

    /// The lease token from `drive lease acquire`, required unless the
    /// deciding write-permission rule sets `require_lease: false`
    /// ([ADR-0080](../../../../docs/adrs/adr-0080.md) §1/§9/§13). Never
    /// needed with `--dry-run`.
    #[arg(long, value_name = "TOKEN")]
    pub lease: Option<String>,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

/// Inserts empty columns, shifting existing columns right.
#[derive(Parser)]
pub struct InsertColumnsCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Title of the sheet to modify.
    #[arg(long, value_name = "NAME")]
    pub sheet: String,

    /// Insert before this column, 1-based (column A is 1).
    #[arg(long, value_name = "COLUMN")]
    pub at: i64,

    /// How many columns to insert.
    #[arg(long, value_name = "N", default_value_t = 1)]
    pub count: i64,

    /// Reports the gate verdict and the change that would be made, without
    /// calling `spreadsheets.batchUpdate`.
    #[arg(long)]
    pub dry_run: bool,

    /// The lease token from `drive lease acquire`, required unless the
    /// deciding write-permission rule sets `require_lease: false`
    /// ([ADR-0080](../../../../docs/adrs/adr-0080.md) §1/§9/§13). Never
    /// needed with `--dry-run`.
    #[arg(long, value_name = "TOKEN")]
    pub lease: Option<String>,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

/// Deletes an entire sheet from a spreadsheet.
///
/// Gated by the folder write-permission rules' `sheets-delete` operation
/// (issue #1623) — distinct from `sheets-structure`, so an existing
/// `allow: ["sheets-structure"]` rule does not also grant this. This cannot
/// be undone through omni-dev; the `--lease` this delete requires (ADR-0080
/// §9) already backed the file up, and that backup — not Google Drive's own
/// version history — is the primary recovery path.
#[derive(Parser)]
pub struct DeleteSheetCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Title of the sheet to delete.
    #[arg(long, value_name = "NAME")]
    pub sheet: String,

    /// Reports the gate verdict and the change that would be made, without
    /// calling `spreadsheets.batchUpdate`.
    #[arg(long)]
    pub dry_run: bool,

    /// The lease token from `drive lease acquire`, required unless the
    /// deciding write-permission rule sets `require_lease: false`
    /// ([ADR-0080](../../../../docs/adrs/adr-0080.md) §1/§9/§13). Never
    /// needed with `--dry-run`.
    #[arg(long, value_name = "TOKEN")]
    pub lease: Option<String>,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

/// Deletes whole rows, shifting the remainder up to close the gap.
///
/// Gated by `sheets-delete` (issue #1623); see [`DeleteSheetCommand`]'s doc
/// comment.
#[derive(Parser)]
pub struct DeleteRowsCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Title of the sheet to modify.
    #[arg(long, value_name = "NAME")]
    pub sheet: String,

    /// First row to delete, 1-based inclusive — the row number the
    /// spreadsheet itself shows.
    #[arg(long, value_name = "ROW")]
    pub at: i64,

    /// How many rows to delete.
    #[arg(long, value_name = "N", default_value_t = 1)]
    pub count: i64,

    /// Reports the gate verdict and the change that would be made, without
    /// calling `spreadsheets.batchUpdate`.
    #[arg(long)]
    pub dry_run: bool,

    /// The lease token from `drive lease acquire`, required unless the
    /// deciding write-permission rule sets `require_lease: false`
    /// ([ADR-0080](../../../../docs/adrs/adr-0080.md) §1/§9/§13). Never
    /// needed with `--dry-run`.
    #[arg(long, value_name = "TOKEN")]
    pub lease: Option<String>,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

/// Deletes whole columns, shifting the remainder left to close the gap.
///
/// Gated by `sheets-delete` (issue #1623); see [`DeleteSheetCommand`]'s doc
/// comment.
#[derive(Parser)]
pub struct DeleteColumnsCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Title of the sheet to modify.
    #[arg(long, value_name = "NAME")]
    pub sheet: String,

    /// First column to delete, 1-based inclusive (column A is 1).
    #[arg(long, value_name = "COLUMN")]
    pub at: i64,

    /// How many columns to delete.
    #[arg(long, value_name = "N", default_value_t = 1)]
    pub count: i64,

    /// Reports the gate verdict and the change that would be made, without
    /// calling `spreadsheets.batchUpdate`.
    #[arg(long)]
    pub dry_run: bool,

    /// The lease token from `drive lease acquire`, required unless the
    /// deciding write-permission rule sets `require_lease: false`
    /// ([ADR-0080](../../../../docs/adrs/adr-0080.md) §1/§9/§13). Never
    /// needed with `--dry-run`.
    #[arg(long, value_name = "TOKEN")]
    pub lease: Option<String>,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

/// Which way [`DeleteRangeCommand`] shifts the cells remaining after a
/// delete, mirroring [`crate::drive::sheets::types::ShiftDimension`].
///
/// A CLI-facing copy for the same reason
/// [`crate::cli::drive::permissions::check::OperationArg`] mirrors
/// `DriveOperation`: the pure engine module stays free of `clap`.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum ShiftArg {
    /// Cells below the deleted range shift up.
    Rows,
    /// Cells to the right of the deleted range shift left.
    Columns,
}

impl From<ShiftArg> for crate::drive::sheets::types::ShiftDimension {
    fn from(arg: ShiftArg) -> Self {
        match arg {
            ShiftArg::Rows => Self::Rows,
            ShiftArg::Columns => Self::Columns,
        }
    }
}

/// Deletes a rectangular cell range, shifting the remainder along one axis
/// to close the gap.
///
/// Gated by `sheets-delete` (issue #1623); see [`DeleteSheetCommand`]'s doc
/// comment. All four bounds are required together — a rectangle, not an
/// open-ended span; `delete-rows`/`delete-columns` cover the whole-dimension
/// case.
#[derive(Parser)]
pub struct DeleteRangeCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Title of the sheet to modify.
    #[arg(long, value_name = "NAME")]
    pub sheet: String,

    /// First row of the range, 1-based inclusive.
    #[arg(long, value_name = "ROW")]
    pub start_row: i64,

    /// Last row of the range, 1-based inclusive.
    #[arg(long, value_name = "ROW")]
    pub end_row: i64,

    /// First column of the range, 1-based inclusive.
    #[arg(long, value_name = "COLUMN")]
    pub start_column: i64,

    /// Last column of the range, 1-based inclusive.
    #[arg(long, value_name = "COLUMN")]
    pub end_column: i64,

    /// Which way to shift the remaining cells afterward.
    #[arg(long, value_enum)]
    pub shift: ShiftArg,

    /// Reports the gate verdict and the change that would be made, without
    /// calling `spreadsheets.batchUpdate`.
    #[arg(long)]
    pub dry_run: bool,

    /// The lease token from `drive lease acquire`, required unless the
    /// deciding write-permission rule sets `require_lease: false`
    /// ([ADR-0080](../../../../docs/adrs/adr-0080.md) §1/§9/§13). Never
    /// needed with `--dry-run`.
    #[arg(long, value_name = "TOKEN")]
    pub lease: Option<String>,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

/// Copies an existing sheet within the same workbook.
#[derive(Parser)]
pub struct DuplicateSheetCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Title of the sheet to copy.
    #[arg(long, value_name = "NAME")]
    pub sheet: String,

    /// Title for the copy. Omitted takes Sheets' own "Copy of X" default.
    /// Must not already exist in the workbook (checked against the source's
    /// own title too — the source keeps its name).
    #[arg(long, value_name = "TITLE")]
    pub title: Option<String>,

    /// Zero-based position for the copy. Omitted takes Sheets' own default
    /// — confirmed against the live API to be the *front* of the workbook
    /// (index 0), not the end: unlike `add-sheet`, `duplicateSheetRequest`
    /// does not default to appending.
    #[arg(long, value_name = "N")]
    pub index: Option<i64>,

    /// Reports the gate verdict and the change that would be made, without
    /// calling `spreadsheets.batchUpdate`.
    #[arg(long)]
    pub dry_run: bool,

    /// The lease token from `drive lease acquire`, required unless the
    /// deciding write-permission rule sets `require_lease: false`
    /// ([ADR-0080](../../../../docs/adrs/adr-0080.md) §1/§9/§13). Never
    /// needed with `--dry-run`.
    #[arg(long, value_name = "TOKEN")]
    pub lease: Option<String>,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

/// Moves an existing sheet to a new position among its siblings.
#[derive(Parser)]
pub struct ReorderSheetCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Title of the sheet to move.
    #[arg(long, value_name = "NAME")]
    pub sheet: String,

    /// The new zero-based position among the workbook's existing sheets.
    #[arg(long, value_name = "N")]
    pub index: i64,

    /// Reports the gate verdict and the change that would be made, without
    /// calling `spreadsheets.batchUpdate`.
    #[arg(long)]
    pub dry_run: bool,

    /// The lease token from `drive lease acquire`, required unless the
    /// deciding write-permission rule sets `require_lease: false`
    /// ([ADR-0080](../../../../docs/adrs/adr-0080.md) §1/§9/§13). Never
    /// needed with `--dry-run`.
    #[arg(long, value_name = "TOKEN")]
    pub lease: Option<String>,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

/// Hides an existing sheet.
#[derive(Parser)]
pub struct HideSheetCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Title of the sheet to hide.
    #[arg(long, value_name = "NAME")]
    pub sheet: String,

    /// Reports the gate verdict and the change that would be made, without
    /// calling `spreadsheets.batchUpdate`.
    #[arg(long)]
    pub dry_run: bool,

    /// The lease token from `drive lease acquire`, required unless the
    /// deciding write-permission rule sets `require_lease: false`
    /// ([ADR-0080](../../../../docs/adrs/adr-0080.md) §1/§9/§13). Never
    /// needed with `--dry-run`.
    #[arg(long, value_name = "TOKEN")]
    pub lease: Option<String>,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

/// Shows an existing hidden sheet.
#[derive(Parser)]
pub struct ShowSheetCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Title of the sheet to show.
    #[arg(long, value_name = "NAME")]
    pub sheet: String,

    /// Reports the gate verdict and the change that would be made, without
    /// calling `spreadsheets.batchUpdate`.
    #[arg(long)]
    pub dry_run: bool,

    /// The lease token from `drive lease acquire`, required unless the
    /// deciding write-permission rule sets `require_lease: false`
    /// ([ADR-0080](../../../../docs/adrs/adr-0080.md) §1/§9/§13). Never
    /// needed with `--dry-run`.
    #[arg(long, value_name = "TOKEN")]
    pub lease: Option<String>,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl AddSheetCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = StructureOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: StructureVerb::AddSheet {
                title: self.title,
                index: self.index,
                rows: self.rows,
                columns: self.columns,
            },
            dry_run: self.dry_run,
            lease_token: self.lease,
            ledger_path: resolve_ledger_path(self.dry_run)?,
        };
        run_structure(client, &opts, &self.output).await
    }
}

impl RenameSheetCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = StructureOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: StructureVerb::RenameSheet {
                sheet: self.sheet,
                new_title: self.title,
            },
            dry_run: self.dry_run,
            lease_token: self.lease,
            ledger_path: resolve_ledger_path(self.dry_run)?,
        };
        run_structure(client, &opts, &self.output).await
    }
}

impl InsertRowsCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = StructureOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: StructureVerb::InsertRows {
                sheet: self.sheet,
                at: self.at,
                count: self.count,
            },
            dry_run: self.dry_run,
            lease_token: self.lease,
            ledger_path: resolve_ledger_path(self.dry_run)?,
        };
        run_structure(client, &opts, &self.output).await
    }
}

impl InsertColumnsCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = StructureOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: StructureVerb::InsertColumns {
                sheet: self.sheet,
                at: self.at,
                count: self.count,
            },
            dry_run: self.dry_run,
            lease_token: self.lease,
            ledger_path: resolve_ledger_path(self.dry_run)?,
        };
        run_structure(client, &opts, &self.output).await
    }
}

impl DeleteSheetCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = StructureOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: StructureVerb::DeleteSheet { sheet: self.sheet },
            dry_run: self.dry_run,
            lease_token: self.lease,
            ledger_path: resolve_ledger_path(self.dry_run)?,
        };
        run_structure(client, &opts, &self.output).await
    }
}

impl DeleteRowsCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = StructureOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: StructureVerb::DeleteRows {
                sheet: self.sheet,
                at: self.at,
                count: self.count,
            },
            dry_run: self.dry_run,
            lease_token: self.lease,
            ledger_path: resolve_ledger_path(self.dry_run)?,
        };
        run_structure(client, &opts, &self.output).await
    }
}

impl DeleteColumnsCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = StructureOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: StructureVerb::DeleteColumns {
                sheet: self.sheet,
                at: self.at,
                count: self.count,
            },
            dry_run: self.dry_run,
            lease_token: self.lease,
            ledger_path: resolve_ledger_path(self.dry_run)?,
        };
        run_structure(client, &opts, &self.output).await
    }
}

impl DeleteRangeCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = StructureOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: StructureVerb::DeleteRange {
                sheet: self.sheet,
                start_row: self.start_row,
                end_row: self.end_row,
                start_column: self.start_column,
                end_column: self.end_column,
                shift: self.shift.into(),
            },
            dry_run: self.dry_run,
            lease_token: self.lease,
            ledger_path: resolve_ledger_path(self.dry_run)?,
        };
        run_structure(client, &opts, &self.output).await
    }
}

impl DuplicateSheetCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = StructureOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: StructureVerb::DuplicateSheet {
                sheet: self.sheet,
                title: self.title,
                index: self.index,
            },
            dry_run: self.dry_run,
            lease_token: self.lease,
            ledger_path: resolve_ledger_path(self.dry_run)?,
        };
        run_structure(client, &opts, &self.output).await
    }
}

impl ReorderSheetCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = StructureOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: StructureVerb::ReorderSheet {
                sheet: self.sheet,
                index: self.index,
            },
            dry_run: self.dry_run,
            lease_token: self.lease,
            ledger_path: resolve_ledger_path(self.dry_run)?,
        };
        run_structure(client, &opts, &self.output).await
    }
}

impl HideSheetCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = StructureOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: StructureVerb::SetSheetVisibility {
                sheet: self.sheet,
                hidden: true,
            },
            dry_run: self.dry_run,
            lease_token: self.lease,
            ledger_path: resolve_ledger_path(self.dry_run)?,
        };
        run_structure(client, &opts, &self.output).await
    }
}

impl ShowSheetCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = StructureOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: StructureVerb::SetSheetVisibility {
                sheet: self.sheet,
                hidden: false,
            },
            dry_run: self.dry_run,
            lease_token: self.lease,
            ledger_path: resolve_ledger_path(self.dry_run)?,
        };
        run_structure(client, &opts, &self.output).await
    }
}

/// Resolves the lease ledger path for one of this module's commands.
///
/// A dry run never checks a lease (`structure_inner` returns `WouldChange`
/// before the ledger is ever touched, mirroring `drive edit`'s own
/// `--dry-run` reasoning) — resolving a real path here would make a
/// purely read-only preview depend on the state directory existing at all.
fn resolve_ledger_path(dry_run: bool) -> Result<std::path::PathBuf> {
    if dry_run {
        Ok(std::path::PathBuf::new())
    } else {
        crate::drive::lease::ledger::ledger_path()
    }
}

/// Shared tail for every structural verb: derive the Sheets client, load the
/// account's rules, run the engine, render.
///
/// The CLI layer deliberately does no gating and no logging — both live in
/// the engine, so a future MCP caller gets them by construction.
async fn run_structure(
    client: &DriveClient,
    opts: &StructureOptions,
    output: &OutputFormat,
) -> Result<()> {
    let sheets = SheetsClient::from_drive_client(client)?;
    let rules = helpers::active_account_rules()?;
    let outcome = structure(client, &sheets, opts, &rules).await;
    if output_as(&outcome, output)? {
        return Ok(());
    }
    println!("{}", sanitize_rendered(&describe_lines(&outcome)));
    Ok(())
}

/// Strips terminal control sequences from a rendered outcome, then joins it,
/// as `sheets write` and `sheets create` do to theirs.
///
/// Every line interpolates a Drive-supplied file name, sheet titles read out
/// of the workbook (including the whole `available` list on a not-found
/// refusal) and raw API error details, so it is exactly as untrusted as
/// theirs.
///
/// The one difference from the siblings is that this takes the **lines**
/// rather than a finished string. Their rendered form is a single line
/// containing no control character of its own, so filtering it whole is
/// equivalent to filtering each interpolation. A structural insert preview
/// is deliberately two lines — the second is the shift, and it is the
/// substance of the dry run (ADR-0075 §6) — so filtering the joined string
/// would eat the separator along with the escapes it is there to remove,
/// while filtering per line after joining could not tell that separator from
/// a newline that arrived inside a sheet title. Taking the parts and
/// supplying the separators here removes the ambiguity rather than
/// documenting it: every newline in the output is one this function wrote,
/// and every newline in the input is stripped as the control character it is.
fn sanitize_rendered(lines: &[String]) -> String {
    lines
        .iter()
        .map(|line| sanitize_for_terminal(line))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn line(s: &str) -> Vec<String> {
        vec![s.to_string()]
    }

    #[test]
    fn resolve_ledger_path_for_a_real_run_resolves_the_ledger_path() {
        // A dry run short-circuits to an empty path (tested via `execute`
        // with `--dry-run`); a real run delegates to the shared resolver.
        let path = resolve_ledger_path(false).unwrap();
        assert!(path.ends_with("lease-ledger.jsonl"), "{}", path.display());
        assert_eq!(
            resolve_ledger_path(true).unwrap(),
            std::path::PathBuf::new()
        );
    }

    #[test]
    fn shift_arg_maps_onto_the_engine_dimension() {
        assert_eq!(
            crate::drive::sheets::types::ShiftDimension::from(ShiftArg::Rows),
            crate::drive::sheets::types::ShiftDimension::Rows
        );
        assert_eq!(
            crate::drive::sheets::types::ShiftDimension::from(ShiftArg::Columns),
            crate::drive::sheets::types::ShiftDimension::Columns
        );
    }

    #[test]
    fn sanitize_rendered_strips_escapes_from_an_injected_sheet_title() {
        let clean = sanitize_rendered(&line(
            "Refused: 'Budget' has no sheet titled 'Q9'. Available: '\u{1b}[31mQ1\u{7}'",
        ));
        assert!(!clean.contains('\u{1b}'), "{clean}");
        assert!(!clean.contains('\u{7}'), "{clean}");
        assert!(clean.contains("Available: '[31mQ1'"), "{clean}");
    }

    #[test]
    fn sanitize_rendered_keeps_the_inserts_own_second_line() {
        // The separator between these two is supplied here rather than
        // carried in the text: the shift is the substance of a structural dry
        // run, and a whole-string filter would eat the newline joining them.
        let clean = sanitize_rendered(&[
            "Would insert 3 row(s) before row 5 of 'Q2' in 'Budget'".to_string(),
            "  (500 rows -> 503; existing rows 5-500 shift down)".to_string(),
        ]);
        assert_eq!(clean.lines().count(), 2, "{clean}");
        assert!(clean.contains("500 rows -> 503"), "{clean}");
    }

    #[test]
    fn a_newline_inside_a_line_cannot_forge_a_second_line() {
        // The residual the line-list signature exists to remove: a sheet
        // title (or Drive file name) carrying a newline plus a plausible
        // shift line must not be able to add one to a single-line preview.
        let clean = sanitize_rendered(&line(
            "Would rename sheet 'Q2\n  (500 rows -> 500; nothing shifts)' to 'Q3' in 'Budget'",
        ));
        assert_eq!(clean.lines().count(), 1, "{clean}");
        assert!(!clean.contains('\n'), "{clean}");
    }

    #[test]
    fn sanitize_rendered_strips_bidi_overrides() {
        let clean = sanitize_rendered(&line("Renamed sheet 'a\u{202E}b' to 'c' in 'Budget'"));
        assert!(!clean.contains('\u{202E}'), "{clean}");
    }
}
