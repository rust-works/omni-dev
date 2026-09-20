//! CLI commands for `omni-dev drive sheets add-chart`/`update-chart`/
//! `delete-chart`/`list-charts`/`add-slicer`/`update-slicer`/
//! `delete-slicer`/`list-slicers` (issue #1797).
//!
//! Every mutating verb here is gated by
//! [`DriveOperation::SheetsStructure`](crate::drive::write_gate::DriveOperation::SheetsStructure)
//! (ADR-0081 §3), including the two deletes. `list-charts`/`list-slicers`
//! are plain reads, ungated like `list-protections`/`list-filter-views`.
//!
//! **Chart subset (v1):** `column`, `bar`, `line`, `area`, `scatter`, and
//! `pie`. See `embedded_object.rs`'s module docs for the full list of
//! documented cuts (COMBO/STEPPED_AREA charts, moving/resizing an existing
//! object, borders, condition-based slicer criteria).

use anyhow::Result;
use clap::Parser;

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::cli::drive::helpers;
use crate::drive::client::DriveClient;
use crate::drive::sheets::api::SheetsApi;
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::embedded_object::{
    charts_from_workbook, describe_lines, embedded_object, slicers_from_workbook,
    EmbeddedObjectOptions, EmbeddedObjectSummary, EmbeddedObjectVerb,
};

/// Adds a chart. See the module docs for the supported chart-type subset.
#[derive(Parser)]
pub struct AddChartCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// `column`, `bar`, `line`, `area`, `scatter`, or `pie`.
    #[arg(long = "type", value_name = "TYPE")]
    pub chart_type: String,

    /// The domain (category/x-axis) range, e.g. `A2:A10`. Must carry its
    /// own `Sheet!` prefix unless `--sheet` is given.
    #[arg(long, value_name = "A1")]
    pub domain: String,

    /// A data series range. Repeatable for a basic chart; a pie chart
    /// takes exactly one.
    #[arg(long, value_name = "A1")]
    pub series: Vec<String>,

    /// Sheet title, supplying the prefix for `--domain`/`--series`/
    /// `--anchor` when they don't carry their own.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// The chart's title.
    #[arg(long)]
    pub title: Option<String>,

    /// The chart's subtitle.
    #[arg(long)]
    pub subtitle: Option<String>,

    /// `bottom`, `top`, `left`, `right`, or `none`.
    #[arg(long, value_name = "POSITION")]
    pub legend: Option<String>,

    /// `none`, `stacked`, or `percent` — column/bar/area charts only.
    #[arg(long, value_name = "MODE")]
    pub stacked: Option<String>,

    /// How many leading rows/columns of the source range are headers —
    /// basic charts only.
    #[arg(long, value_name = "N")]
    pub header_count: Option<i64>,

    /// The horizontal axis title — basic charts only.
    #[arg(long)]
    pub horizontal_axis_title: Option<String>,

    /// The vertical axis title — basic charts only.
    #[arg(long)]
    pub vertical_axis_title: Option<String>,

    /// `0.0`-`1.0` center-hole radius — pie charts only. `0.0` (or absent)
    /// is a solid pie.
    #[arg(long, value_name = "0.0-1.0")]
    pub pie_hole: Option<f64>,

    /// The anchor cell, e.g. `E2`. Required unless `--new-sheet`.
    #[arg(long, value_name = "A1", conflicts_with = "new_sheet")]
    pub anchor: Option<String>,

    /// Additional horizontal offset from the anchor cell, in pixels.
    #[arg(long, value_name = "PX", conflicts_with = "new_sheet")]
    pub offset_x: Option<i64>,

    /// Additional vertical offset from the anchor cell, in pixels.
    #[arg(long, value_name = "PX", conflicts_with = "new_sheet")]
    pub offset_y: Option<i64>,

    /// The chart's width in pixels.
    #[arg(long, value_name = "PX", conflicts_with = "new_sheet")]
    pub width: Option<i64>,

    /// The chart's height in pixels.
    #[arg(long, value_name = "PX", conflicts_with = "new_sheet")]
    pub height: Option<i64>,

    /// Put the chart on a brand-new sheet of its own, instead of anchoring
    /// it to an existing one. Cannot move or resize an existing chart —
    /// see the module docs' documented cuts.
    #[arg(long)]
    pub new_sheet: bool,

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

impl AddChartCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = EmbeddedObjectOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: EmbeddedObjectVerb::AddChart {
                chart_type: self.chart_type,
                domain: self.domain,
                series: self.series,
                sheet: self.sheet,
                title: self.title,
                subtitle: self.subtitle,
                legend: self.legend,
                stacked: self.stacked,
                header_count: self.header_count,
                horizontal_axis_title: self.horizontal_axis_title,
                vertical_axis_title: self.vertical_axis_title,
                pie_hole: self.pie_hole,
                anchor: self.anchor,
                offset_x: self.offset_x,
                offset_y: self.offset_y,
                width: self.width,
                height: self.height,
                new_sheet: self.new_sheet,
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_embedded_object(client, &opts, &self.output).await
    }
}

/// Replaces an existing chart's spec. `updateChartSpec` carries no field
/// mask, so this reads the existing spec, refuses it if it isn't a
/// supported kind, and otherwise applies only the flags actually set —
/// see `embedded_object.rs`'s module docs.
#[derive(Parser)]
pub struct UpdateChartCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Which chart to update, discovered via `list-charts`.
    #[arg(long, value_name = "ID")]
    pub chart_id: i64,

    /// Change the chart's type. Refused if it would switch between a basic
    /// chart and a pie chart — delete and re-add instead.
    #[arg(long = "type", value_name = "TYPE")]
    pub chart_type: Option<String>,

    /// Replace the domain range (requires `--series` too).
    #[arg(long, value_name = "A1")]
    pub domain: Option<String>,

    /// Replace every series wholesale. Repeatable.
    #[arg(long, value_name = "A1")]
    pub series: Vec<String>,

    /// Sheet title, supplying the prefix for `--domain`/`--series` when
    /// they don't carry their own.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// Change the title.
    #[arg(long)]
    pub title: Option<String>,

    /// Change the subtitle.
    #[arg(long)]
    pub subtitle: Option<String>,

    /// Change the legend position.
    #[arg(long, value_name = "POSITION")]
    pub legend: Option<String>,

    /// Change the stacking mode — basic charts only.
    #[arg(long, value_name = "MODE")]
    pub stacked: Option<String>,

    /// Change the header row/column count — basic charts only.
    #[arg(long, value_name = "N")]
    pub header_count: Option<i64>,

    /// Change the horizontal axis title — basic charts only.
    #[arg(long)]
    pub horizontal_axis_title: Option<String>,

    /// Change the vertical axis title — basic charts only.
    #[arg(long)]
    pub vertical_axis_title: Option<String>,

    /// Change the pie-hole radius — pie charts only.
    #[arg(long, value_name = "0.0-1.0")]
    pub pie_hole: Option<f64>,

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

impl UpdateChartCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = EmbeddedObjectOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: EmbeddedObjectVerb::UpdateChart {
                chart_id: self.chart_id,
                chart_type: self.chart_type,
                domain: self.domain,
                series: self.series,
                sheet: self.sheet,
                title: self.title,
                subtitle: self.subtitle,
                legend: self.legend,
                stacked: self.stacked,
                header_count: self.header_count,
                horizontal_axis_title: self.horizontal_axis_title,
                vertical_axis_title: self.vertical_axis_title,
                pie_hole: self.pie_hole,
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_embedded_object(client, &opts, &self.output).await
    }
}

/// Removes a chart. Reads back and reports its spec before deleting —
/// ADR-0081 §3's mandatory preview for an unrecoverable object removal.
#[derive(Parser)]
pub struct DeleteChartCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Which chart to remove, discovered via `list-charts`.
    #[arg(long, value_name = "ID")]
    pub chart_id: i64,

    /// Reports the gate verdict and what would be removed, without calling
    /// `spreadsheets.batchUpdate`.
    #[arg(long)]
    pub dry_run: bool,

    #[command(flatten)]
    pub lease: crate::cli::drive::helpers::LeaseTokenArg,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl DeleteChartCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = EmbeddedObjectOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: EmbeddedObjectVerb::DeleteChart {
                chart_id: self.chart_id,
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_embedded_object(client, &opts, &self.output).await
    }
}

/// Lists the charts in a spreadsheet.
///
/// Read-only and ungated, like `list-protections`/`list-filter-views` —
/// needed so `update-chart`/`delete-chart` are usable at all, since a
/// chart's numeric id is otherwise invisible from the CLI.
#[derive(Parser)]
pub struct ListChartsCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ListChartsCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let sheets = SheetsClient::from_drive_client(client)?;
        let api = SheetsApi::new(&sheets);
        let workbook = api
            .get_spreadsheet_with_embedded_objects(&self.spreadsheet_id)
            .await?;
        if output_as(&workbook, &self.output)? {
            return Ok(());
        }
        for object in charts_from_workbook(&workbook) {
            println!("{}", sanitize_for_terminal(&describe_summary_line(&object)));
        }
        Ok(())
    }
}

/// Adds a slicer.
#[derive(Parser)]
pub struct AddSlicerCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Sheet title, supplying the prefix for `--range`/`--anchor` when they
    /// don't carry their own.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// The range the slicer filters.
    #[arg(long, value_name = "A1")]
    pub range: String,

    /// The 0-based column within `--range` the filter criteria apply to —
    /// not an A1 letter, the same indexing `--hide-values`/`--sort-by` use
    /// on `set-basic-filter`.
    #[arg(long, value_name = "N")]
    pub column: i64,

    /// A value to hide in `--column`. Repeatable.
    #[arg(long = "hide-values", value_name = "VALUES", value_delimiter = ',')]
    pub hide_values: Vec<String>,

    /// A human-readable name for the slicer.
    #[arg(long)]
    pub title: Option<String>,

    /// Whether this slicer also filters pivot tables built from `--range`.
    #[arg(long, value_name = "BOOL")]
    pub apply_to_pivot_tables: Option<bool>,

    /// The anchor cell, e.g. `F2`.
    #[arg(long, value_name = "A1")]
    pub anchor: String,

    /// Additional horizontal offset from the anchor cell, in pixels.
    #[arg(long, value_name = "PX")]
    pub offset_x: Option<i64>,

    /// Additional vertical offset from the anchor cell, in pixels.
    #[arg(long, value_name = "PX")]
    pub offset_y: Option<i64>,

    /// The slicer's width in pixels.
    #[arg(long, value_name = "PX")]
    pub width: Option<i64>,

    /// The slicer's height in pixels.
    #[arg(long, value_name = "PX")]
    pub height: Option<i64>,

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

impl AddSlicerCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = EmbeddedObjectOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: EmbeddedObjectVerb::AddSlicer {
                sheet: self.sheet,
                range: self.range,
                column: self.column,
                hide_values: self.hide_values,
                title: self.title,
                apply_to_pivot_tables: self.apply_to_pivot_tables,
                anchor: self.anchor,
                offset_x: self.offset_x,
                offset_y: self.offset_y,
                width: self.width,
                height: self.height,
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_embedded_object(client, &opts, &self.output).await
    }
}

/// Changes an existing slicer's range, filter column/criteria, title, or
/// pivot-table linkage. Unlike `update-chart`, `updateSlicerSpec` carries a
/// field mask, so only the flags actually set are written.
#[derive(Parser)]
pub struct UpdateSlicerCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Which slicer to change, discovered via `list-slicers`.
    #[arg(long, value_name = "ID")]
    pub slicer_id: i64,

    /// Sheet title, supplying the prefix for `--range` when it doesn't
    /// carry its own.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// Replace the filtered range.
    #[arg(long, value_name = "A1")]
    pub range: Option<String>,

    /// Replace the filtered column.
    #[arg(long, value_name = "N")]
    pub column: Option<i64>,

    /// Replace the hidden-value criteria. Repeatable.
    #[arg(long = "hide-values", value_name = "VALUES", value_delimiter = ',')]
    pub hide_values: Vec<String>,

    /// Reset the filter criteria to empty.
    #[arg(long)]
    pub clear_criteria: bool,

    /// Change the title.
    #[arg(long)]
    pub title: Option<String>,

    /// Change the pivot-table linkage.
    #[arg(long, value_name = "BOOL")]
    pub apply_to_pivot_tables: Option<bool>,

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

impl UpdateSlicerCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = EmbeddedObjectOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: EmbeddedObjectVerb::UpdateSlicer {
                slicer_id: self.slicer_id,
                sheet: self.sheet,
                range: self.range,
                column: self.column,
                hide_values: self.hide_values,
                clear_criteria: self.clear_criteria,
                title: self.title,
                apply_to_pivot_tables: self.apply_to_pivot_tables,
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_embedded_object(client, &opts, &self.output).await
    }
}

/// Removes a slicer. Reads back and reports its spec before deleting —
/// ADR-0081 §3's mandatory preview for an unrecoverable object removal.
#[derive(Parser)]
pub struct DeleteSlicerCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Which slicer to remove, discovered via `list-slicers`.
    #[arg(long, value_name = "ID")]
    pub slicer_id: i64,

    /// Reports the gate verdict and what would be removed, without calling
    /// `spreadsheets.batchUpdate`.
    #[arg(long)]
    pub dry_run: bool,

    #[command(flatten)]
    pub lease: crate::cli::drive::helpers::LeaseTokenArg,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl DeleteSlicerCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = EmbeddedObjectOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: EmbeddedObjectVerb::DeleteSlicer {
                slicer_id: self.slicer_id,
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_embedded_object(client, &opts, &self.output).await
    }
}

/// Lists the slicers in a spreadsheet. Read-only and ungated, like
/// [`ListChartsCommand`].
#[derive(Parser)]
pub struct ListSlicersCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ListSlicersCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let sheets = SheetsClient::from_drive_client(client)?;
        let api = SheetsApi::new(&sheets);
        let workbook = api
            .get_spreadsheet_with_embedded_objects(&self.spreadsheet_id)
            .await?;
        if output_as(&workbook, &self.output)? {
            return Ok(());
        }
        for object in slicers_from_workbook(&workbook) {
            println!("{}", sanitize_for_terminal(&describe_summary_line(&object)));
        }
        Ok(())
    }
}

fn describe_summary_line(object: &EmbeddedObjectSummary) -> String {
    let kind = object
        .chart_type
        .as_deref()
        .map_or_else(|| object.kind.clone(), |t| format!("{} ({t})", object.kind));
    let title = object
        .title
        .as_deref()
        .map_or_else(String::new, |t| format!(" '{t}'"));
    format!(
        "id {}: {kind}{title}, anchored {}",
        object.object_id, object.position
    )
}

async fn run_embedded_object(
    client: &DriveClient,
    opts: &EmbeddedObjectOptions,
    output: &OutputFormat,
) -> Result<()> {
    let sheets = SheetsClient::from_drive_client(client)?;
    let rules = helpers::active_account_rules()?;
    let outcome = embedded_object(client, &sheets, opts, &rules).await;
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
