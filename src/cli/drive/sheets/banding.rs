//! CLI commands for `omni-dev drive sheets add-banding`/`update-banding`/
//! `delete-banding`/`list-bandings` (issue #1832).
//!
//! The first three are gated by
//! [`DriveOperation::SheetsStructure`](crate::drive::write_gate::DriveOperation::SheetsStructure)
//! (ADR-0082, following ADR-0078's original reasoning). `list-bandings` is
//! a plain read, ungated like `list-protections`.

use anyhow::Result;
use clap::Parser;

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::cli::drive::helpers;
use crate::drive::client::DriveClient;
use crate::drive::sheets::api::SheetsApi;
use crate::drive::sheets::banding::{
    banding, describe_lines, BandingAxis, BandingOptions, BandingVerb,
};
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::render_grid_range;

/// The `--axis` values for `add-banding`/`update-banding` — mirrors
/// [`BandingAxis`] with a `clap::ValueEnum` derive, since the engine type
/// deliberately carries no `clap` dependency (the same split
/// `conditional_format.rs`'s `GradientMidTypeArg`/`GradientPointType` uses).
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum BandingAxisArg {
    /// Alternate by row (the common case — the Sheets UI's default).
    Rows,
    /// Alternate by column.
    Columns,
}

impl From<BandingAxisArg> for BandingAxis {
    fn from(value: BandingAxisArg) -> Self {
        match value {
            BandingAxisArg::Rows => Self::Rows,
            BandingAxisArg::Columns => Self::Columns,
        }
    }
}

/// Adds a new banded range — alternating row or column colors.
///
/// v1 sets at most one of row/column banding per call, selected by `--axis`
/// (default `rows`); the Sheets API allows both simultaneously on one
/// range, which this crate does not expose. Colors are `#RRGGBB`, written
/// via the modern `*ColorStyle` fields — never the deprecated plain
/// `Color` fields, never a theme color.
#[derive(Parser)]
pub struct AddBandingCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Sheet (tab) title. Supplies the prefix for a bare `--range`.
    #[arg(long, value_name = "NAME")]
    pub sheet: String,

    /// A1 range to band, optionally carrying its own `Sheet!` prefix.
    #[arg(long, value_name = "A1")]
    pub range: String,

    /// Which axis to alternate along.
    #[arg(long, value_enum, default_value_t = BandingAxisArg::Rows)]
    pub axis: BandingAxisArg,

    /// The header row/column's color, `#RRGGBB`, if distinct from the
    /// alternating bands.
    #[arg(long, value_name = "HEX")]
    pub header_color: Option<String>,

    /// The first band's color, `#RRGGBB`.
    #[arg(long, value_name = "HEX")]
    pub first_band_color: String,

    /// The second (alternating) band's color, `#RRGGBB`.
    #[arg(long, value_name = "HEX")]
    pub second_band_color: String,

    /// The footer row/column's color, `#RRGGBB`, if distinct from the
    /// alternating bands.
    #[arg(long, value_name = "HEX")]
    pub footer_color: Option<String>,

    /// Reports the gate verdict and the change that would be made, without
    /// calling `spreadsheets.batchUpdate`.
    #[arg(long)]
    pub dry_run: bool,

    #[command(flatten)]
    pub lease: crate::cli::drive::helpers::LeaseTokenArg,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl AddBandingCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = BandingOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: BandingVerb::AddBanding {
                sheet: self.sheet,
                range: self.range,
                axis: self.axis.into(),
                header_color: self.header_color,
                first_band_color: self.first_band_color,
                second_band_color: self.second_band_color,
                footer_color: self.footer_color,
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_banding(client, &opts, &self.output).await
    }
}

/// Changes an existing banded range's range and/or colors. The target is
/// addressed directly by `--banded-range-id`, discovered via
/// `drive sheets list-bandings`.
#[derive(Parser)]
#[command(group(clap::ArgGroup::new("change")
    .args(["sheet", "range", "header_color", "first_band_color", "second_band_color", "footer_color"])
    .multiple(true)
    .required(true)))]
pub struct UpdateBandingCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Which banded range to change.
    #[arg(long, value_name = "ID")]
    pub banded_range_id: i64,

    /// Sheet (tab) title, when changing the banded range. Supplies the
    /// prefix for a bare `--range`.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// The new A1 range, when changing it, optionally carrying its own
    /// `Sheet!` prefix.
    #[arg(long, value_name = "A1")]
    pub range: Option<String>,

    /// Which axis's colors `--header-color`/`--first-band-color`/`--second-band-color`/`--footer-color`
    /// change.
    #[arg(long, value_enum, default_value_t = BandingAxisArg::Rows)]
    pub axis: BandingAxisArg,

    /// The new header color, `#RRGGBB`, when changing it.
    #[arg(long, value_name = "HEX")]
    pub header_color: Option<String>,

    /// The new first-band color, `#RRGGBB`, when changing it.
    #[arg(long, value_name = "HEX")]
    pub first_band_color: Option<String>,

    /// The new second-band color, `#RRGGBB`, when changing it.
    #[arg(long, value_name = "HEX")]
    pub second_band_color: Option<String>,

    /// The new footer color, `#RRGGBB`, when changing it.
    #[arg(long, value_name = "HEX")]
    pub footer_color: Option<String>,

    /// Reports the gate verdict and the change that would be made, without
    /// calling `spreadsheets.batchUpdate`.
    #[arg(long)]
    pub dry_run: bool,

    #[command(flatten)]
    pub lease: crate::cli::drive::helpers::LeaseTokenArg,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl UpdateBandingCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = BandingOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: BandingVerb::UpdateBanding {
                banded_range_id: self.banded_range_id,
                sheet: self.sheet,
                range: self.range,
                axis: self.axis.into(),
                header_color: self.header_color,
                first_band_color: self.first_band_color,
                second_band_color: self.second_band_color,
                footer_color: self.footer_color,
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_banding(client, &opts, &self.output).await
    }
}

/// Removes a banded range. The target is addressed directly by
/// `--banded-range-id`, discovered via `drive sheets list-bandings`.
#[derive(Parser)]
pub struct DeleteBandingCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Which banded range to remove.
    #[arg(long, value_name = "ID")]
    pub banded_range_id: i64,

    /// Reports the gate verdict and the change that would be made, without
    /// calling `spreadsheets.batchUpdate`.
    #[arg(long)]
    pub dry_run: bool,

    #[command(flatten)]
    pub lease: crate::cli::drive::helpers::LeaseTokenArg,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl DeleteBandingCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = BandingOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: BandingVerb::DeleteBanding {
                banded_range_id: self.banded_range_id,
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_banding(client, &opts, &self.output).await
    }
}

/// Lists the banded ranges in a spreadsheet.
///
/// Read-only and ungated, like `sheets info`/`list-protections` — needed so
/// `update-banding`/`delete-banding` are usable at all, since a banded
/// range's numeric id is otherwise invisible from the CLI.
#[derive(Parser)]
pub struct ListBandingsCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ListBandingsCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let sheets = SheetsClient::from_drive_client(client)?;
        let api = SheetsApi::new(&sheets);
        let workbook = api
            .get_spreadsheet_with_banding(&self.spreadsheet_id)
            .await?;
        if output_as(&workbook, &self.output)? {
            return Ok(());
        }
        for sheet in &workbook.sheets {
            for banded in &sheet.banded_ranges {
                let id = banded
                    .banded_range_id
                    .map_or_else(|| "?".to_string(), |id| id.to_string());
                let range = banded
                    .range
                    .as_ref()
                    .map_or_else(|| "(unresolvable)".to_string(), render_grid_range);
                let axes = match (
                    banded.row_properties.is_some(),
                    banded.column_properties.is_some(),
                ) {
                    (true, true) => "rows+columns",
                    (true, false) => "rows",
                    (false, true) => "columns",
                    (false, false) => "none",
                };
                println!(
                    "{}",
                    sanitize_for_terminal(&format!(
                        "id {id}: {range}  axis={axes}  sheet={}",
                        sheet.title()
                    ))
                );
            }
        }
        Ok(())
    }
}

async fn run_banding(
    client: &DriveClient,
    opts: &BandingOptions,
    output: &OutputFormat,
) -> Result<()> {
    let sheets = SheetsClient::from_drive_client(client)?;
    let rules = helpers::active_account_rules()?;
    let outcome = banding(client, &sheets, opts, &rules).await;
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
