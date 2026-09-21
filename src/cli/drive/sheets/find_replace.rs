//! CLI surface for `omni-dev drive sheets find-replace` (issue #1841).

use anyhow::Result;
use clap::Parser;

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::cli::drive::helpers;
use crate::drive::client::DriveClient;
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::find_replace::{describe_lines, find_replace, FindReplaceOptions};

/// Finds text and replaces it in a range, a sheet, or every sheet.
#[derive(Parser)]
pub struct FindReplaceCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Text to find. Cannot be empty.
    #[arg(long, value_name = "TEXT")]
    pub find: String,

    /// Replacement text. Pass an empty string to remove matches.
    #[arg(long, value_name = "TEXT", allow_hyphen_values = true)]
    pub replacement: String,

    /// A1 range to search, optionally carrying its own `Sheet!` prefix.
    #[arg(long, value_name = "A1")]
    pub range: Option<String>,

    /// Sheet (tab) title. Supplies the prefix for a bare `--range`, and is
    /// required with `--whole-sheet`.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// Search the whole sheet named by `--sheet`.
    #[arg(long)]
    pub whole_sheet: bool,

    /// Search every sheet in the workbook.
    #[arg(long)]
    pub all_sheets: bool,

    /// Match case exactly.
    #[arg(long)]
    pub match_case: bool,

    /// Require the search text to match the whole cell.
    #[arg(long)]
    pub match_entire_cell: bool,

    /// Interpret the search and replacement as Java regular-expression text.
    #[arg(long)]
    pub search_by_regex: bool,

    /// Include cells containing formulas. Without this flag, formula cells
    /// are skipped; Sheets has no formulas-only mode.
    #[arg(long)]
    pub include_formulas: bool,

    /// Reports the gate verdict and request scope without calling
    /// `spreadsheets.batchUpdate`. Sheets computes matching counts only when
    /// the request executes.
    #[arg(long)]
    pub dry_run: bool,

    #[command(flatten)]
    pub lease: crate::cli::drive::helpers::LeaseTokenArg,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl FindReplaceCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = FindReplaceOptions {
            spreadsheet_id: self.spreadsheet_id,
            find: self.find,
            replacement: self.replacement,
            sheet: self.sheet,
            range: self.range,
            whole_sheet: self.whole_sheet,
            all_sheets: self.all_sheets,
            match_case: self.match_case,
            match_entire_cell: self.match_entire_cell,
            search_by_regex: self.search_by_regex,
            include_formulas: self.include_formulas,
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_find_replace(client, &opts, &self.output).await
    }
}

async fn run_find_replace(
    client: &DriveClient,
    opts: &FindReplaceOptions,
    output: &OutputFormat,
) -> Result<()> {
    let sheets = SheetsClient::from_drive_client(client)?;
    let rules = helpers::active_account_rules()?;
    let outcome = find_replace(client, &sheets, opts, &rules).await;
    if output_as(&outcome, output)? {
        return Ok(());
    }
    let lines: Vec<String> = describe_lines(&outcome)
        .into_iter()
        .map(|line| sanitize_for_terminal(&line))
        .collect();
    println!("{}", lines.join("\n"));
    Ok(())
}
