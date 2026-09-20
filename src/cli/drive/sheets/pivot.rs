//! CLI commands for `omni-dev drive sheets add-pivot-table`/
//! `delete-pivot-table`/`list-pivot-tables` (issue #1798,
//! [ADR-0081](../../../../docs/adrs/adr-0081.md) §5).
//!
//! `add-pivot-table` is gated by **both**
//! [`DriveOperation::SheetsWrite`](crate::drive::write_gate::DriveOperation::SheetsWrite)
//! and
//! [`DriveOperation::SheetsStructure`](crate::drive::write_gate::DriveOperation::SheetsStructure);
//! `delete-pivot-table` needs `SheetsWrite` alone. `list-pivot-tables` is a
//! plain read, ungated like `list-conditional-formats` — see
//! `crate::drive::sheets::pivot`'s module doc for why it exists although
//! the issue named only the first two verbs.

use anyhow::Result;
use clap::Parser;

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::cli::drive::helpers;
use crate::drive::client::DriveClient;
use crate::drive::sheets::api::SheetsApi;
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::pivot::{
    describe_lines, list_entries, pivot, PivotOptions, PivotVerb, ValueLayout,
};

/// `--value-layout`'s value set — mirrors [`ValueLayout`] with a
/// `clap::ValueEnum` derive, since the engine type deliberately carries no
/// `clap` dependency (the same split `GradientMidTypeArg`/
/// `GradientPointType` use).
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum ValueLayoutArg {
    /// Values laid out as columns.
    Horizontal,
    /// Values laid out as rows.
    Vertical,
}

impl From<ValueLayoutArg> for ValueLayout {
    fn from(value: ValueLayoutArg) -> Self {
        match value {
            ValueLayoutArg::Horizontal => Self::Horizontal,
            ValueLayoutArg::Vertical => Self::Vertical,
        }
    }
}

/// Writes a new pivot table at `--anchor`, refused if one is already
/// there — use `list-pivot-tables` first to check, or `delete-pivot-table`
/// to clear it. Gated by **both** the `sheets-write` and `sheets-structure`
/// write-permission operations (ADR-0081 §5): an operator must hold both
/// for this to succeed.
#[derive(Parser)]
pub struct AddPivotTableCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Sheet (tab) title `--anchor` lives on.
    #[arg(long, value_name = "NAME")]
    pub sheet: String,

    /// A single-cell A1 reference (no `Sheet!` prefix — `--sheet` supplies
    /// it) the pivot table is anchored at.
    #[arg(long, value_name = "A1")]
    pub anchor: String,

    /// The pivot's source range. May carry its own `Sheet!` prefix (a
    /// pivot commonly sources from a different tab than it's anchored on);
    /// falls back to `--sheet` when it doesn't. Must be a bounded
    /// rectangle.
    #[arg(long, value_name = "[SHEET!]A1_RANGE")]
    pub source: String,

    /// A row grouping, by 0-based column offset into `--source`, optionally
    /// followed by a sort order. Repeatable, outermost first.
    #[arg(long = "row", value_name = "COLUMN[:asc|desc]")]
    pub rows: Vec<String>,

    /// A column grouping, by 0-based column offset into `--source`,
    /// optionally followed by a sort order. Repeatable, outermost first.
    #[arg(long = "column", value_name = "COLUMN[:asc|desc]")]
    pub columns: Vec<String>,

    /// An aggregated value column, by 0-based column offset into
    /// `--source` and a summarize function (`sum`, `counta`, `count`,
    /// `countunique`, `average`, `max`, `min`, `median`, `product`,
    /// `stdev`, `stdevp`, `var`, `varp`). Repeatable; at least one is
    /// required.
    #[arg(long = "value", value_name = "COLUMN:FUNC", required = true)]
    pub values: Vec<String>,

    /// A source-row filter, by 0-based column offset into `--source` and a
    /// comma-separated allow-list of raw values. Repeatable.
    #[arg(long = "filter", value_name = "COLUMN:VALUE[,VALUE...]")]
    pub filters: Vec<String>,

    /// Lays values out as columns (the default) or rows.
    #[arg(long, value_enum)]
    pub value_layout: Option<ValueLayoutArg>,

    /// Omits the totals row/column that every group shows by default.
    #[arg(long)]
    pub no_totals: bool,

    /// Reports the gate verdict, the pivot configuration, and the anchor's
    /// current content, without calling `spreadsheets.batchUpdate`. Cannot
    /// report the region the pivot table will actually overwrite — see
    /// `docs/drive.md`.
    #[arg(long)]
    pub dry_run: bool,

    #[command(flatten)]
    pub lease: crate::cli::drive::helpers::LeaseTokenArg,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl AddPivotTableCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = PivotOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: PivotVerb::AddPivotTable {
                sheet: self.sheet,
                anchor: self.anchor,
                source: self.source,
                rows: self.rows,
                columns: self.columns,
                values: self.values,
                filters: self.filters,
                value_layout: self.value_layout.map(ValueLayout::from),
                show_totals: !self.no_totals,
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_pivot(client, &opts, &self.output).await
    }
}

/// Clears the pivot table at `--anchor`, refused if there isn't one — use
/// `list-pivot-tables` to find one. Gated by the `sheets-write`
/// write-permission operation alone (ADR-0081 §5): it only ever clears the
/// anchor's own value, no structural effect.
#[derive(Parser)]
pub struct DeletePivotTableCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Sheet (tab) title `--anchor` lives on.
    #[arg(long, value_name = "NAME")]
    pub sheet: String,

    /// A single-cell A1 reference (no `Sheet!` prefix — `--sheet` supplies
    /// it) the pivot table is anchored at. See `list-pivot-tables`.
    #[arg(long, value_name = "A1")]
    pub anchor: String,

    /// Reports the gate verdict and the pivot table currently at
    /// `--anchor`, without calling `spreadsheets.batchUpdate`.
    #[arg(long)]
    pub dry_run: bool,

    #[command(flatten)]
    pub lease: crate::cli::drive::helpers::LeaseTokenArg,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl DeletePivotTableCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = PivotOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: PivotVerb::DeletePivotTable {
                sheet: self.sheet,
                anchor: self.anchor,
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_pivot(client, &opts, &self.output).await
    }
}

/// Lists every pivot table in a spreadsheet, by anchor cell — the one way
/// to discover the `--anchor` `delete-pivot-table` needs. Read-only and
/// ungated, like `list-conditional-formats`.
#[derive(Parser)]
pub struct ListPivotTablesCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ListPivotTablesCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let sheets = SheetsClient::from_drive_client(client)?;
        let api = SheetsApi::new(&sheets);
        let workbook = api
            .get_spreadsheet_with_pivot_tables(&self.spreadsheet_id)
            .await?;
        if output_as(&workbook, &self.output)? {
            return Ok(());
        }
        for entry in list_entries(&workbook) {
            println!(
                "{}",
                sanitize_for_terminal(&format!(
                    "sheet={} anchor={} {}",
                    entry.sheet, entry.anchor, entry.summary
                ))
            );
        }
        Ok(())
    }
}

async fn run_pivot(client: &DriveClient, opts: &PivotOptions, output: &OutputFormat) -> Result<()> {
    let sheets = SheetsClient::from_drive_client(client)?;
    let rules = helpers::active_account_rules()?;
    let outcome = pivot(client, &sheets, opts, &rules).await;
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
