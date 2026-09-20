//! CLI commands for `omni-dev drive sheets format-cells`/`update-borders`/
//! `merge-cells`/`unmerge-cells`/`auto-resize-dimension`/
//! `update-dimension-properties` (issue #1643).
//!
//! Each `clap::ValueEnum` here is the CLI-layer mirror of a small,
//! wire-shaped string the engine takes ready-made — the same split
//! `crate::cli::drive::permissions::check::OperationArg` uses for
//! `DriveOperation`, applied six times over so `format.rs` stays free of a
//! `clap` dependency. Validation therefore happens once, at parse time, via
//! `value_enum` — never as a client-side grammar check the engine would
//! have to repeat.

use anyhow::Result;
use clap::Parser;

use crate::cli::drive::format::{output_as, OutputFormat};
use crate::cli::drive::helpers;
use crate::cli::format::sanitize_for_terminal;
use crate::drive::client::DriveClient;
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::format::{
    describe_lines, format, BorderSides, CellFormatFlags, FormatOptions, FormatVerb,
    TextRotationFlag,
};
use crate::drive::sheets::types::Dimension;

/// `--horizontal-align`'s value set.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum HorizontalAlign {
    Left,
    Center,
    Right,
}

impl HorizontalAlign {
    const fn wire(self) -> &'static str {
        match self {
            Self::Left => "LEFT",
            Self::Center => "CENTER",
            Self::Right => "RIGHT",
        }
    }
}

/// `--vertical-align`'s value set.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum VerticalAlign {
    Top,
    Middle,
    Bottom,
}

impl VerticalAlign {
    const fn wire(self) -> &'static str {
        match self {
            Self::Top => "TOP",
            Self::Middle => "MIDDLE",
            Self::Bottom => "BOTTOM",
        }
    }
}

/// `--wrap`'s value set.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum WrapStrategy {
    Overflow,
    Clip,
    Wrap,
}

impl WrapStrategy {
    const fn wire(self) -> &'static str {
        match self {
            Self::Overflow => "OVERFLOW_CELL",
            Self::Clip => "CLIP",
            Self::Wrap => "WRAP",
        }
    }
}

/// `--number-format-type`'s value set.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum NumberFormatType {
    Text,
    Number,
    Percent,
    Currency,
    Date,
    Time,
    DateTime,
    Scientific,
}

impl NumberFormatType {
    const fn wire(self) -> &'static str {
        match self {
            Self::Text => "TEXT",
            Self::Number => "NUMBER",
            Self::Percent => "PERCENT",
            Self::Currency => "CURRENCY",
            Self::Date => "DATE",
            Self::Time => "TIME",
            Self::DateTime => "DATE_TIME",
            Self::Scientific => "SCIENTIFIC",
        }
    }
}

/// `--hyperlink-display-type`'s value set.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum HyperlinkDisplayType {
    Linked,
    PlainText,
}

impl HyperlinkDisplayType {
    const fn wire(self) -> &'static str {
        match self {
            Self::Linked => "LINKED",
            Self::PlainText => "PLAIN_TEXT",
        }
    }
}

/// `--text-direction`'s value set.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum TextDirection {
    LeftToRight,
    RightToLeft,
}

impl TextDirection {
    const fn wire(self) -> &'static str {
        match self {
            Self::LeftToRight => "LEFT_TO_RIGHT",
            Self::RightToLeft => "RIGHT_TO_LEFT",
        }
    }
}

/// `--style`'s value set for `update-borders`.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum BorderStyle {
    Solid,
    SolidMedium,
    SolidThick,
    Dashed,
    Dotted,
    Double,
}

impl BorderStyle {
    const fn wire(self) -> &'static str {
        match self {
            Self::Solid => "SOLID",
            Self::SolidMedium => "SOLID_MEDIUM",
            Self::SolidThick => "SOLID_THICK",
            Self::Dashed => "DASHED",
            Self::Dotted => "DOTTED",
            Self::Double => "DOUBLE",
        }
    }
}

/// `--type`'s value set for `merge-cells`.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum MergeType {
    All,
    Rows,
    Columns,
}

impl MergeType {
    const fn wire(self) -> &'static str {
        match self {
            Self::All => "MERGE_ALL",
            Self::Rows => "MERGE_ROWS",
            Self::Columns => "MERGE_COLUMNS",
        }
    }
}

/// `#[arg(default_value_t = ...)]` requires `Display`, which `ValueEnum`
/// does not provide for free.
impl std::fmt::Display for MergeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.wire().to_ascii_lowercase().replace('_', "-"))
    }
}

/// `--dimension`'s value set, shared by `auto-resize-dimension` and
/// `update-dimension-properties`.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum DimensionArg {
    Rows,
    Columns,
}

impl DimensionArg {
    const fn engine(self) -> Dimension {
        match self {
            Self::Rows => Dimension::Rows,
            Self::Columns => Dimension::Columns,
        }
    }
}

/// Applies a cell format across a range.
#[derive(Parser)]
pub struct FormatCellsCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// A1 range to format, optionally carrying its own `Sheet!` prefix.
    #[arg(long, value_name = "A1")]
    pub range: Option<String>,

    /// Sheet (tab) title. Supplies the prefix for a bare `--range`.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// Bold.
    #[arg(long, value_name = "BOOL")]
    pub bold: Option<bool>,
    /// Italic.
    #[arg(long, value_name = "BOOL")]
    pub italic: Option<bool>,
    /// Strikethrough.
    #[arg(long, value_name = "BOOL")]
    pub strikethrough: Option<bool>,
    /// Underline.
    #[arg(long, value_name = "BOOL")]
    pub underline: Option<bool>,
    /// Point size.
    #[arg(long, value_name = "N")]
    pub font_size: Option<i64>,
    /// Text color, `#RRGGBB`.
    #[arg(long, value_name = "HEX")]
    pub text_color: Option<String>,
    /// Cell background color, `#RRGGBB`.
    #[arg(long, value_name = "HEX")]
    pub background: Option<String>,
    /// Horizontal alignment.
    #[arg(long, value_enum, value_name = "ALIGN")]
    pub horizontal_align: Option<HorizontalAlign>,
    /// Vertical alignment.
    #[arg(long, value_enum, value_name = "ALIGN")]
    pub vertical_align: Option<VerticalAlign>,
    /// Display number format pattern, e.g. `"#,##0.00"`. Requires
    /// `--number-format-type`.
    #[arg(long, value_name = "PATTERN")]
    pub number_format: Option<String>,
    /// The number format's type. Requires `--number-format`.
    #[arg(long, value_enum, value_name = "TYPE")]
    pub number_format_type: Option<NumberFormatType>,
    /// Text wrap behavior.
    #[arg(long, value_enum, value_name = "WRAP")]
    pub wrap: Option<WrapStrategy>,
    /// Font family name, e.g. `"Arial"`.
    #[arg(long, value_name = "NAME")]
    pub font_family: Option<String>,
    /// Rotation angle in degrees, -90 to 90. Mutually exclusive with
    /// `--text-rotation-vertical`.
    #[arg(
        long,
        value_name = "DEGREES",
        allow_hyphen_values = true,
        conflicts_with = "text_rotation_vertical"
    )]
    pub text_rotation_angle: Option<i64>,
    /// Stack text vertically instead of rotating it. Mutually exclusive
    /// with `--text-rotation-angle`.
    #[arg(long, conflicts_with = "text_rotation_angle")]
    pub text_rotation_vertical: bool,
    /// How a cell containing a hyperlink is displayed.
    #[arg(long, value_enum, value_name = "TYPE")]
    pub hyperlink_display_type: Option<HyperlinkDisplayType>,
    /// Top padding, in pixels. Must be non-negative.
    #[arg(long, value_name = "N")]
    pub padding_top: Option<i64>,
    /// Right padding, in pixels. Must be non-negative.
    #[arg(long, value_name = "N")]
    pub padding_right: Option<i64>,
    /// Bottom padding, in pixels. Must be non-negative.
    #[arg(long, value_name = "N")]
    pub padding_bottom: Option<i64>,
    /// Left padding, in pixels. Must be non-negative.
    #[arg(long, value_name = "N")]
    pub padding_left: Option<i64>,
    /// Text direction.
    #[arg(long, value_enum, value_name = "DIR")]
    pub text_direction: Option<TextDirection>,

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

impl FormatCellsCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let format = CellFormatFlags {
            bold: self.bold,
            italic: self.italic,
            strikethrough: self.strikethrough,
            underline: self.underline,
            font_size: self.font_size,
            text_color: self.text_color,
            background: self.background,
            horizontal_align: self
                .horizontal_align
                .map(HorizontalAlign::wire)
                .map(str::to_string),
            vertical_align: self
                .vertical_align
                .map(VerticalAlign::wire)
                .map(str::to_string),
            number_format_pattern: self.number_format,
            number_format_type: self
                .number_format_type
                .map(NumberFormatType::wire)
                .map(str::to_string),
            wrap: self.wrap.map(WrapStrategy::wire).map(str::to_string),
            font_family: self.font_family,
            text_rotation: match (self.text_rotation_angle, self.text_rotation_vertical) {
                (Some(angle), _) => Some(TextRotationFlag::Angle(angle)),
                (None, true) => Some(TextRotationFlag::Vertical),
                (None, false) => None,
            },
            hyperlink_display_type: self
                .hyperlink_display_type
                .map(HyperlinkDisplayType::wire)
                .map(str::to_string),
            padding_top: self.padding_top,
            padding_right: self.padding_right,
            padding_bottom: self.padding_bottom,
            padding_left: self.padding_left,
            text_direction: self
                .text_direction
                .map(TextDirection::wire)
                .map(str::to_string),
        };
        let opts = FormatOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: FormatVerb::FormatCells {
                sheet: self.sheet,
                range: self.range,
                format: Box::new(format),
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_format(client, &opts, &self.output).await
    }
}

/// Sets border lines on a range's edges.
#[derive(Parser)]
pub struct UpdateBordersCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// A1 range to border, optionally carrying its own `Sheet!` prefix.
    #[arg(long, value_name = "A1")]
    pub range: Option<String>,

    /// Sheet (tab) title. Supplies the prefix for a bare `--range`.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// Set the top edge.
    #[arg(long)]
    pub top: bool,
    /// Set the bottom edge.
    #[arg(long)]
    pub bottom: bool,
    /// Set the left edge.
    #[arg(long)]
    pub left: bool,
    /// Set the right edge.
    #[arg(long)]
    pub right: bool,
    /// Set all four edges. Equivalent to passing all of
    /// `--top`/`--bottom`/`--left`/`--right`. Does **not** include
    /// `--inner-horizontal`/`--inner-vertical`, which need to be named
    /// explicitly.
    #[arg(long)]
    pub all: bool,
    /// Set the horizontal grid lines between rows within the range.
    #[arg(long)]
    pub inner_horizontal: bool,
    /// Set the vertical grid lines between columns within the range.
    #[arg(long)]
    pub inner_vertical: bool,

    /// Line style, shared by every edge this call sets.
    #[arg(long, value_enum, default_value_t = BorderStyle::Solid)]
    pub style: BorderStyle,
    /// Line color, `#RRGGBB`. Omitted takes solid black.
    #[arg(long, value_name = "HEX")]
    pub color: Option<String>,

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

/// `#[arg(default_value_t = ...)]` requires `Display`, which `ValueEnum`
/// does not provide for free.
impl std::fmt::Display for BorderStyle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.wire().to_ascii_lowercase().replace('_', "-"))
    }
}

impl UpdateBordersCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let sides = BorderSides {
            top: self.top || self.all,
            bottom: self.bottom || self.all,
            left: self.left || self.all,
            right: self.right || self.all,
            inner_horizontal: self.inner_horizontal,
            inner_vertical: self.inner_vertical,
        };
        let opts = FormatOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: FormatVerb::UpdateBorders {
                sheet: self.sheet,
                range: self.range,
                sides,
                style: self.style.wire().to_string(),
                color: self.color,
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_format(client, &opts, &self.output).await
    }
}

/// Merges a range into one cell, discarding every value but the top-left's.
#[derive(Parser)]
pub struct MergeCellsCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// A1 range to merge, optionally carrying its own `Sheet!` prefix.
    #[arg(long, value_name = "A1")]
    pub range: Option<String>,

    /// Sheet (tab) title. Supplies the prefix for a bare `--range`.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// How to merge: one cell for the whole range, one per row, or one per
    /// column.
    #[arg(long, value_enum, default_value_t = MergeType::All)]
    pub r#type: MergeType,

    /// Reports the gate verdict and, critically, every cell value the merge
    /// would discard — see `drive sheets merge-cells --help` before running
    /// without this flag first.
    #[arg(long)]
    pub dry_run: bool,

    #[command(flatten)]
    pub lease: crate::cli::drive::helpers::LeaseTokenArg,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl MergeCellsCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = FormatOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: FormatVerb::MergeCells {
                sheet: self.sheet,
                range: self.range,
                merge_type: self.r#type.wire().to_string(),
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_format(client, &opts, &self.output).await
    }
}

/// Splits a previously merged range back apart.
#[derive(Parser)]
pub struct UnmergeCellsCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// A1 range to unmerge, optionally carrying its own `Sheet!` prefix.
    #[arg(long, value_name = "A1")]
    pub range: Option<String>,

    /// Sheet (tab) title. Supplies the prefix for a bare `--range`.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

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

impl UnmergeCellsCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = FormatOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: FormatVerb::UnmergeCells {
                sheet: self.sheet,
                range: self.range,
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_format(client, &opts, &self.output).await
    }
}

/// Resizes rows or columns to fit their content.
#[derive(Parser)]
pub struct AutoResizeDimensionCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Title of the sheet to modify.
    #[arg(long, value_name = "NAME")]
    pub sheet: String,

    /// Rows or columns.
    #[arg(long, value_enum)]
    pub dimension: DimensionArg,

    /// 1-based first row/column, inclusive.
    #[arg(long, value_name = "N")]
    pub start: i64,

    /// 1-based last row/column, inclusive.
    #[arg(long, value_name = "N")]
    pub end: i64,

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

impl AutoResizeDimensionCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = FormatOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: FormatVerb::AutoResizeDimension {
                sheet: self.sheet,
                dimension: self.dimension.engine(),
                start: self.start,
                end: self.end,
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_format(client, &opts, &self.output).await
    }
}

/// Sets an explicit pixel width (columns) or height (rows).
#[derive(Parser)]
pub struct UpdateDimensionPropertiesCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Title of the sheet to modify.
    #[arg(long, value_name = "NAME")]
    pub sheet: String,

    /// Rows or columns.
    #[arg(long, value_enum)]
    pub dimension: DimensionArg,

    /// 1-based first row/column, inclusive.
    #[arg(long, value_name = "N")]
    pub start: i64,

    /// 1-based last row/column, inclusive.
    #[arg(long, value_name = "N")]
    pub end: i64,

    /// The new size, in pixels.
    #[arg(long, value_name = "N")]
    pub pixel_size: i64,

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

impl UpdateDimensionPropertiesCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = FormatOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: FormatVerb::UpdateDimensionProperties {
                sheet: self.sheet,
                dimension: self.dimension.engine(),
                start: self.start,
                end: self.end,
                pixel_size: self.pixel_size,
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_format(client, &opts, &self.output).await
    }
}

/// Shared tail for every formatting verb, mirroring
/// `structure.rs::run_structure` exactly.
async fn run_format(
    client: &DriveClient,
    opts: &FormatOptions,
    output: &OutputFormat,
) -> Result<()> {
    let sheets = SheetsClient::from_drive_client(client)?;
    let rules = helpers::active_account_rules()?;
    let outcome = format(client, &sheets, opts, &rules).await;
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn horizontal_align_wire_maps_every_variant() {
        assert_eq!(HorizontalAlign::Left.wire(), "LEFT");
        assert_eq!(HorizontalAlign::Center.wire(), "CENTER");
        assert_eq!(HorizontalAlign::Right.wire(), "RIGHT");
    }

    #[test]
    fn vertical_align_wire_maps_every_variant() {
        assert_eq!(VerticalAlign::Top.wire(), "TOP");
        assert_eq!(VerticalAlign::Middle.wire(), "MIDDLE");
        assert_eq!(VerticalAlign::Bottom.wire(), "BOTTOM");
    }

    #[test]
    fn hyperlink_display_type_wire_maps_every_variant() {
        assert_eq!(HyperlinkDisplayType::Linked.wire(), "LINKED");
        assert_eq!(HyperlinkDisplayType::PlainText.wire(), "PLAIN_TEXT");
    }

    #[test]
    fn text_direction_wire_maps_every_variant() {
        assert_eq!(TextDirection::LeftToRight.wire(), "LEFT_TO_RIGHT");
        assert_eq!(TextDirection::RightToLeft.wire(), "RIGHT_TO_LEFT");
    }

    #[test]
    fn wrap_strategy_wire_maps_every_variant() {
        assert_eq!(WrapStrategy::Overflow.wire(), "OVERFLOW_CELL");
        assert_eq!(WrapStrategy::Clip.wire(), "CLIP");
        assert_eq!(WrapStrategy::Wrap.wire(), "WRAP");
    }

    #[test]
    fn number_format_type_wire_maps_every_variant() {
        assert_eq!(NumberFormatType::Text.wire(), "TEXT");
        assert_eq!(NumberFormatType::Number.wire(), "NUMBER");
        assert_eq!(NumberFormatType::Percent.wire(), "PERCENT");
        assert_eq!(NumberFormatType::Currency.wire(), "CURRENCY");
        assert_eq!(NumberFormatType::Date.wire(), "DATE");
        assert_eq!(NumberFormatType::Time.wire(), "TIME");
        assert_eq!(NumberFormatType::DateTime.wire(), "DATE_TIME");
        assert_eq!(NumberFormatType::Scientific.wire(), "SCIENTIFIC");
    }

    #[test]
    fn border_style_wire_maps_every_variant() {
        assert_eq!(BorderStyle::Solid.wire(), "SOLID");
        assert_eq!(BorderStyle::SolidMedium.wire(), "SOLID_MEDIUM");
        assert_eq!(BorderStyle::SolidThick.wire(), "SOLID_THICK");
        assert_eq!(BorderStyle::Dashed.wire(), "DASHED");
        assert_eq!(BorderStyle::Dotted.wire(), "DOTTED");
        assert_eq!(BorderStyle::Double.wire(), "DOUBLE");
    }

    #[test]
    fn border_style_display_is_lowercase_hyphenated() {
        assert_eq!(BorderStyle::Solid.to_string(), "solid");
        assert_eq!(BorderStyle::SolidMedium.to_string(), "solid-medium");
        assert_eq!(BorderStyle::SolidThick.to_string(), "solid-thick");
        assert_eq!(BorderStyle::Dashed.to_string(), "dashed");
        assert_eq!(BorderStyle::Dotted.to_string(), "dotted");
        assert_eq!(BorderStyle::Double.to_string(), "double");
    }

    #[test]
    fn merge_type_wire_maps_every_variant() {
        assert_eq!(MergeType::All.wire(), "MERGE_ALL");
        assert_eq!(MergeType::Rows.wire(), "MERGE_ROWS");
        assert_eq!(MergeType::Columns.wire(), "MERGE_COLUMNS");
    }

    #[test]
    fn merge_type_display_is_lowercase_hyphenated() {
        assert_eq!(MergeType::All.to_string(), "merge-all");
        assert_eq!(MergeType::Rows.to_string(), "merge-rows");
        assert_eq!(MergeType::Columns.to_string(), "merge-columns");
    }

    #[test]
    fn dimension_arg_engine_maps_every_variant() {
        assert_eq!(DimensionArg::Rows.engine(), Dimension::Rows);
        assert_eq!(DimensionArg::Columns.engine(), Dimension::Columns);
    }
}
