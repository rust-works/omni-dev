//! CLI commands for `omni-dev drive sheets add-sheet`/`rename-sheet`/
//! `insert-rows`/`insert-columns` (issue #1613).
//!
//! Four clap structs over one engine call. They share `run_structure`, so
//! the gate wiring, `--dry-run` handling, output rendering and request
//! logging cannot drift between them — the same arrangement `write.rs` uses
//! for its three verbs.
//!
//! There is deliberately **no** command that takes a raw
//! `spreadsheets.batchUpdate` request array. Each verb names its effect in
//! typed arguments, which is what lets `--dry-run` describe the change and
//! what keeps the destructive requests unreachable; see
//! `crate::drive::sheets::structure`.

use anyhow::Result;
use clap::Parser;

use crate::cli::drive::format::{output_as, OutputFormat};
use crate::cli::drive::helpers;
use crate::drive::client::DriveClient;
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::structure::{describe, structure, StructureOptions, StructureVerb};

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
        };
        run_structure(client, &opts, &self.output).await
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
    println!("{}", describe(&outcome, &opts.verb));
    Ok(())
}
