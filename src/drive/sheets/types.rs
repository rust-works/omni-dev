//! Wire types for the Sheets v4 REST API.
//!
//! Field naming follows Sheets' camelCase JSON via per-field
//! `#[serde(rename = "...")]`, mirroring `src/drive/types.rs` and
//! `src/gmail/types.rs`. Only the subset the CLI actually renders is
//! modelled; every unmodelled field is tolerated and dropped, so a Google
//! response gaining a field never breaks a parse.
//!
//! Two shapes here are load-bearing and easy to get wrong:
//!
//! - **`ValueRange::values` is absent, not empty, for an empty sheet.** It
//!   needs `#[serde(default)]` or a blank tab fails the whole read.
//! - **Rows are ragged.** The API truncates trailing empty cells from each
//!   row, and trailing empty rows entirely, so row 1 may have 5 cells and
//!   row 2 only 2. Renderers must decide explicitly what to do about that
//!   rather than assuming a rectangle.

use serde::{Deserialize, Serialize};

/// A spreadsheet's metadata, from `spreadsheets.get`.
///
/// Always request an explicit `fields` mask when fetching this: the
/// unmasked response embeds **every cell of every sheet**, which on a large
/// workbook is an out-of-memory failure rather than a slow request.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Spreadsheet {
    /// The spreadsheet's id (echoes the one requested).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "spreadsheetId"
    )]
    pub spreadsheet_id: Option<String>,
    /// Workbook-level properties, notably its title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub properties: Option<SpreadsheetProperties>,
    /// The sheets (tabs) it contains, in workbook order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sheets: Vec<Sheet>,
}

impl Spreadsheet {
    /// The workbook title, or `""` when the `fields` mask omitted it.
    #[must_use]
    pub fn title(&self) -> &str {
        self.properties
            .as_ref()
            .map_or("", |props| props.title.as_str())
    }

    /// Every sheet title, in workbook order.
    #[must_use]
    pub fn sheet_titles(&self) -> Vec<String> {
        self.sheets
            .iter()
            .map(|sheet| sheet.title().to_string())
            .collect()
    }
}

/// Workbook-level properties.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpreadsheetProperties {
    /// The workbook's display title.
    #[serde(default)]
    pub title: String,
}

/// One sheet (tab) within a spreadsheet.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Sheet {
    /// This sheet's properties.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub properties: Option<SheetProperties>,
    /// Protected ranges on this sheet. Empty unless the caller requested
    /// them with a wider `fields` mask — `SPREADSHEET_FIELDS` (the mask
    /// every other call uses) omits this, so a plain `sheets info`/
    /// `structure` read never pays for it; only
    /// `SheetsApi::get_spreadsheet_with_protections` populates it.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        rename = "protectedRanges"
    )]
    pub protected_ranges: Vec<ProtectedRange>,
}

impl Sheet {
    /// This sheet's title, or `""` if absent.
    #[must_use]
    pub fn title(&self) -> &str {
        self.properties
            .as_ref()
            .map_or("", |props| props.title.as_str())
    }

    /// Whether the sheet is hidden in the UI.
    ///
    /// Hidden sheets are deliberately **included** in a full read: omitting
    /// them would silently drop data the caller asked for, and "fail
    /// visible" is the safer default for a data-extraction tool.
    #[must_use]
    pub fn hidden(&self) -> bool {
        self.properties
            .as_ref()
            .is_some_and(|props| props.hidden.unwrap_or(false))
    }
}

/// A sheet's properties.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SheetProperties {
    /// Stable numeric id, unique within the spreadsheet.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "sheetId")]
    pub sheet_id: Option<i64>,
    /// Display title — the string used to build an A1 prefix.
    #[serde(default)]
    pub title: String,
    /// Position in the workbook, zero-based.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<i64>,
    /// Whether the sheet is hidden in the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
    /// Grid dimensions, when the sheet is a grid.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "gridProperties"
    )]
    pub grid_properties: Option<GridProperties>,
}

/// A grid sheet's declared dimensions.
///
/// These are the sheet's *allocated* size, not the extent of its data — a
/// blank sheet still reports 1000 x 26.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GridProperties {
    /// Number of rows allocated.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "rowCount")]
    pub row_count: Option<i64>,
    /// Number of columns allocated.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "columnCount"
    )]
    pub column_count: Option<i64>,
}

/// Cell values for one range, from `values.get` / `values.batchGet`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ValueRange {
    /// The range these values came from, **server-normalised** (e.g.
    /// `'My Sheet'!A1:C7`).
    ///
    /// This is what a batch response must be matched on. Zipping a
    /// `batchGet` reply to the request list by index instead would silently
    /// misattribute one sheet's data to another the moment the server
    /// reorders, merges or drops a range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<String>,
    /// Row-major cell values. **Absent — not empty — for a sheet with no
    /// data**, hence `#[serde(default)]`.
    ///
    /// Rows are ragged: trailing empty cells are truncated per row, and
    /// trailing empty rows dropped entirely.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<Vec<serde_json::Value>>,
}

impl ValueRange {
    /// The widest row, i.e. the column count needed to render this range as
    /// a rectangle.
    #[must_use]
    pub fn width(&self) -> usize {
        self.values.iter().map(Vec::len).max().unwrap_or(0)
    }
}

impl crate::cli::drive::format::JsonlSerialize for Spreadsheet {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> anyhow::Result<()> {
        crate::cli::drive::format::write_scalar_jsonl(self, out)
    }
}

/// Envelope for `values.batchGet`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BatchGetValuesResponse {
    /// One entry per requested range, in the order the server chose — match
    /// on each entry's own `range`, never on request order.
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "valueRanges")]
    pub value_ranges: Vec<ValueRange>,
}

/// Response to `values.update`.
///
/// The counts are what the request log records as context, so
/// `omni-dev log --query kind:drivemutation` can answer "what did that write
/// actually touch" rather than only "a write happened".
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateValuesResponse {
    /// The range that was written, server-normalised.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "updatedRange"
    )]
    pub updated_range: Option<String>,
    /// Rows written.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "updatedRows"
    )]
    pub updated_rows: Option<i64>,
    /// Columns written.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "updatedColumns"
    )]
    pub updated_columns: Option<i64>,
    /// Cells written.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "updatedCells"
    )]
    pub updated_cells: Option<i64>,
}

/// Response to `values.append`.
///
/// Note the nesting: the counts live under `updates`, not at the top level,
/// which is the one shape difference from `values.update`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppendValuesResponse {
    /// The table range the append targeted, before the new rows.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "tableRange"
    )]
    pub table_range: Option<String>,
    /// What was actually written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updates: Option<UpdateValuesResponse>,
}

/// Response to `values.clear`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClearValuesResponse {
    /// The range that was cleared, server-normalised.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "clearedRange"
    )]
    pub cleared_range: Option<String>,
}

/// One `spreadsheets.batchUpdate` request.
///
/// **Externally tagged**, which is exactly Sheets' own wire shape:
/// `{"addSheet": {...}}`. Every variant here is reachable only through its
/// owning module's gated engine: the additive verbs behind
/// `DriveOperation::SheetsStructure` (issue #1613,
/// [ADR-0075](../../../docs/adrs/adr-0075.md), extended by issue #1643,
/// [ADR-0078](../../../docs/adrs/adr-0078.md), with formatting, data
/// validation, `duplicateSheet` and reorder/hide), the destructive ones
/// behind `DriveOperation::SheetsDelete` (issue #1623,
/// [ADR-0077](../../../docs/adrs/adr-0077-sheets-deletion-via-batchupdate.md)),
/// and the protected-range ones behind `DriveOperation::SheetsProtection`
/// (issue #1643, [ADR-0078](../../../docs/adrs/adr-0078.md) §2) — there is
/// still no raw `--requests` passthrough that could construct one of these
/// outside its gate. `structure.rs`'s `every_delete_verb_gates_on_sheets_delete_not_sheets_structure`/
/// `every_additive_verb_still_gates_on_sheets_structure` tests pin that every
/// verb reaches exactly one gate, never the other.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum BatchUpdateRequestItem {
    /// Add a new sheet to the workbook.
    AddSheet(AddSheetRequest),
    /// Change an existing sheet's properties — title (rename), index
    /// (reorder) or hidden (hide/show); exactly one per request, matching
    /// the request's own single-field `fields` mask.
    UpdateSheetProperties(UpdateSheetPropertiesRequest),
    /// Insert empty rows or columns, shifting existing ones.
    InsertDimension(InsertDimensionRequest),
    /// Delete an entire sheet from the workbook.
    DeleteSheet(DeleteSheetRequest),
    /// Delete whole rows or columns, shifting the remainder to close the gap.
    DeleteDimension(DeleteDimensionRequest),
    /// Delete a rectangular cell range, shifting the remainder to close the
    /// gap along one axis.
    DeleteRange(DeleteRangeRequest),
    /// Copy an existing sheet within the same workbook.
    DuplicateSheet(DuplicateSheetRequest),
    /// Apply a cell format across a range (`format-cells`). Never carries a
    /// value — see [`RepeatCellData`].
    RepeatCell(RepeatCellRequest),
    /// Set border lines on a range's edges (`update-borders`).
    UpdateBorders(UpdateBordersRequest),
    /// Merge a range into one cell (`merge-cells`) — the one request this
    /// enum models that discards data; see `format.rs` module docs.
    MergeCells(MergeCellsRequest),
    /// Split a previously merged range back apart (`unmerge-cells`).
    UnmergeCells(UnmergeCellsRequest),
    /// Resize rows/columns to fit their content (`auto-resize-dimension`).
    AutoResizeDimensions(AutoResizeDimensionsRequest),
    /// Set explicit row height / column width (`update-dimension-properties`).
    UpdateDimensionProperties(UpdateDimensionPropertiesRequest),
    /// Set (or, with an absent rule, clear) a data validation rule on a
    /// range (`set-data-validation` / `clear-data-validation`).
    SetDataValidation(SetDataValidationRequest),
    /// Add a protected range (`protect-range`). Gated by
    /// [`crate::drive::write_gate::DriveOperation::SheetsProtection`], not
    /// `SheetsStructure` — see that variant's doc comment.
    AddProtectedRange(AddProtectedRangeRequest),
    /// Change an existing protected range's description, warning-only flag,
    /// or editor list (`update-protection`). Same gate as
    /// [`Self::AddProtectedRange`].
    UpdateProtectedRange(UpdateProtectedRangeRequest),
    /// Remove a protected range (`unprotect-range`). Same gate as
    /// [`Self::AddProtectedRange`] — a permission removal, not a *data*
    /// deletion, which is why it needs no `SheetsDelete` grant.
    DeleteProtectedRange(DeleteProtectedRangeRequest),
}

/// Body of `spreadsheets.batchUpdate`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct BatchUpdateRequest {
    /// The requests to apply. Every v1 structural verb sends exactly one,
    /// which is what makes partial application unobservable.
    pub requests: Vec<BatchUpdateRequestItem>,
}

/// `AddSheetRequest` — wraps the new sheet's properties.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AddSheetRequest {
    /// Properties of the sheet to add.
    pub properties: NewSheetProperties,
}

/// The properties of a sheet being added.
///
/// Distinct from [`SheetProperties`], which is a *response* type: this one
/// omits `sheetId` (the server assigns it) and every field is optional
/// except the title, so an omitted `--rows` inherits Sheets' own default
/// rather than us inventing one.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct NewSheetProperties {
    /// The new sheet's title.
    pub title: String,
    /// Zero-based position in the workbook. Omitted means "append".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<i64>,
    /// Initial grid dimensions. Omitted means Sheets' default (1000 x 26).
    #[serde(skip_serializing_if = "Option::is_none", rename = "gridProperties")]
    pub grid_properties: Option<GridProperties>,
}

/// `UpdateSheetPropertiesRequest`.
///
/// `fields` is a field mask and is **not** optional: an empty mask is an
/// error, and a mask naming more than we set would blank the unnamed fields.
/// Every caller populates exactly the one [`SheetPropertiesUpdate`] field its
/// `fields` string names — `structure.rs::build_request` is the single
/// place that pairs the two, so they can never drift apart.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct UpdateSheetPropertiesRequest {
    /// The properties to write. `sheet_id` selects the target.
    pub properties: SheetPropertiesUpdate,
    /// The field mask limiting what this request may change.
    pub fields: String,
}

/// The mutable subset of a sheet's properties this crate can set.
///
/// Deliberately not [`SheetProperties`]: reusing the response type would
/// serialise whatever else it happened to carry, and a field mask widened by
/// accident is how an unintended property gets overwritten.
///
/// Every field but `sheet_id` is optional and every caller sets exactly one
/// of them — `title` (rename), `index` (reorder) or `hidden` (hide/show) —
/// alongside a `fields` mask naming that one field. Modelling all three on
/// one struct, rather than one struct per verb, is what lets `resolve_sheet`/
/// `validate_verb_args`/`build_request`'s existing `updateSheetProperties`
/// plumbing serve all three without triplicating it.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct SheetPropertiesUpdate {
    /// Which sheet to modify.
    #[serde(rename = "sheetId")]
    pub sheet_id: i64,
    /// The new title, for a rename.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The new zero-based position, for a reorder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<i64>,
    /// Whether the sheet should be hidden, for hide/show.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
}

/// `DuplicateSheetRequest`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DuplicateSheetRequest {
    /// The sheet to copy.
    #[serde(rename = "sourceSheetId")]
    pub source_sheet_id: i64,
    /// Where the copy lands. `None` takes Sheets' own default — confirmed
    /// against the live API to be the *front* of the workbook (index 0),
    /// unlike [`NewSheetProperties::index`], whose `None` really does
    /// append.
    #[serde(skip_serializing_if = "Option::is_none", rename = "insertSheetIndex")]
    pub insert_sheet_index: Option<i64>,
    /// The copy's title. `None` takes Sheets' own "Copy of X" default.
    #[serde(skip_serializing_if = "Option::is_none", rename = "newSheetName")]
    pub new_sheet_name: Option<String>,
}

/// Which axis a [`DimensionRange`] runs along.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum Dimension {
    /// Rows.
    #[serde(rename = "ROWS")]
    Rows,
    /// Columns.
    #[serde(rename = "COLUMNS")]
    Columns,
}

impl Dimension {
    /// The wire spelling, also used in the request log's `dimension_range`
    /// context field and in human-readable output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rows => "ROWS",
            Self::Columns => "COLUMNS",
        }
    }

    /// Singular noun for human-readable output ("row" / "column").
    #[must_use]
    pub fn noun(self) -> &'static str {
        match self {
            Self::Rows => "row",
            Self::Columns => "column",
        }
    }
}

/// A half-open, **zero-based** span of rows or columns.
///
/// The zero-based half-open shape is the API's, not the CLI's: `--at` is
/// 1-based and inclusive, matching A1 and the spreadsheet UI. The single
/// conversion lives in `structure.rs::dimension_range`, never at a call
/// site.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DimensionRange {
    /// The sheet the span belongs to.
    #[serde(rename = "sheetId")]
    pub sheet_id: i64,
    /// Rows or columns.
    pub dimension: Dimension,
    /// First index, inclusive, zero-based.
    #[serde(rename = "startIndex")]
    pub start_index: i64,
    /// Last index, **exclusive**, zero-based.
    #[serde(rename = "endIndex")]
    pub end_index: i64,
}

/// A rectangular cell range, addressed numerically.
///
/// The shape every `batchUpdate` request that targets a *range* (as
/// opposed to a whole dimension span like [`DimensionRange`]) uses:
/// `repeatCell`, `updateBorders`, `mergeCells`, `unmergeCells`,
/// `setDataValidation`, `addProtectedRange`, `deleteRange`.
/// `structure.rs::grid_range` is `deleteRange`'s one caller, and always
/// sends a fully-bounded value (every field `Some`) — an open-ended delete
/// on one axis degenerates into `deleteDimension` semantics and is out of
/// scope for it, exactly as [`DeleteDimensionRequest`] already covers.
///
/// Zero-based and half-open on both axes, matching the API. Each bound is
/// independently optional: `None` on both row bounds means "every row" (a
/// whole-column reference, `A:A`); `None` on both column bounds means "every
/// column" (`5:5`); `None` on all four means the entire sheet. The single
/// A1-to-`GridRange` conversion lives in `grid_range::parse_grid_range`,
/// never at a call site — see its module docs for why this is new work
/// `a1.rs` deliberately doesn't do.
///
/// Also `Deserialize`, unlike most other outbound-only request pieces in
/// this file: `ProtectedRange` (a *response* shape) embeds one, since a
/// protected range is itself addressed by the `GridRange` it covers.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GridRange {
    /// The sheet this range belongs to.
    #[serde(default, rename = "sheetId")]
    pub sheet_id: i64,
    /// First row, inclusive, zero-based. `None` means "from the first row".
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "startRowIndex"
    )]
    pub start_row_index: Option<i64>,
    /// Last row, **exclusive**, zero-based. `None` means "to the last row".
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "endRowIndex"
    )]
    pub end_row_index: Option<i64>,
    /// First column, inclusive, zero-based. `None` means "from the first
    /// column".
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "startColumnIndex"
    )]
    pub start_column_index: Option<i64>,
    /// Last column, **exclusive**, zero-based. `None` means "to the last
    /// column".
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "endColumnIndex"
    )]
    pub end_column_index: Option<i64>,
}

/// An RGB color, 0.0-1.0 per channel — the Sheets API's own float scale,
/// not the 0-255 a hex string like `#RRGGBB` suggests. `format.rs`'s hex
/// parser is the one conversion site.
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq)]
pub struct Color {
    /// Red channel, 0.0-1.0.
    pub red: f32,
    /// Green channel, 0.0-1.0.
    pub green: f32,
    /// Blue channel, 0.0-1.0.
    pub blue: f32,
}

// `Color` has no meaningful `Eq`: it holds `f32`, so this crate's other
// request types (all `derive(PartialEq, Eq)`) can't include it directly
// without losing `Eq` themselves — hence the `PartialEq`-only requests
// below that embed it.

/// `ColorStyle` — the modern wrapper Sheets expects around a plain
/// [`Color`]. Only the `rgbColor` arm is modelled; the API's alternative
/// `themeColor` arm has no CLI flag surface in this feature.
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq)]
pub struct ColorStyle {
    /// The explicit RGB color.
    #[serde(rename = "rgbColor")]
    pub rgb_color: Color,
}

/// The mutable subset of a cell's text formatting this crate can set —
/// `CellFormat.textFormat`.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct TextFormat {
    /// Bold.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bold: Option<bool>,
    /// Italic.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub italic: Option<bool>,
    /// Strikethrough.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strikethrough: Option<bool>,
    /// Underline.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub underline: Option<bool>,
    /// Point size.
    #[serde(skip_serializing_if = "Option::is_none", rename = "fontSize")]
    pub font_size: Option<i64>,
    /// Text color.
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "foregroundColorStyle"
    )]
    pub foreground_color_style: Option<ColorStyle>,
}

/// A cell's display number format — `CellFormat.numberFormat`.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct NumberFormat {
    /// One of Sheets' type strings (`"TEXT"`, `"NUMBER"`, `"PERCENT"`,
    /// `"CURRENCY"`, `"DATE"`, `"TIME"`, `"DATE_TIME"`, `"SCIENTIFIC"`).
    #[serde(rename = "type")]
    pub format_type: String,
    /// The display pattern, e.g. `"#,##0.00"`.
    pub pattern: String,
}

/// The mutable subset of `CellFormat` this crate can set via `repeatCell`.
///
/// Every field is optional and `format.rs::build_fields_mask` names exactly
/// the ones populated — the same discipline `SheetPropertiesUpdate` follows
/// for `updateSheetProperties`.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct CellFormat {
    /// Text formatting (bold, italic, color, …).
    #[serde(skip_serializing_if = "Option::is_none", rename = "textFormat")]
    pub text_format: Option<TextFormat>,
    /// Cell background color.
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "backgroundColorStyle"
    )]
    pub background_color_style: Option<ColorStyle>,
    /// `"LEFT"` / `"CENTER"` / `"RIGHT"`.
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "horizontalAlignment"
    )]
    pub horizontal_alignment: Option<String>,
    /// `"TOP"` / `"MIDDLE"` / `"BOTTOM"`.
    #[serde(skip_serializing_if = "Option::is_none", rename = "verticalAlignment")]
    pub vertical_alignment: Option<String>,
    /// Display number format.
    #[serde(skip_serializing_if = "Option::is_none", rename = "numberFormat")]
    pub number_format: Option<NumberFormat>,
    /// `"OVERFLOW_CELL"` / `"CLIP"` / `"WRAP"`.
    #[serde(skip_serializing_if = "Option::is_none", rename = "wrapStrategy")]
    pub wrap_strategy: Option<String>,
}

/// The `cell` payload of a `repeatCellRequest` — deliberately **has no
/// `userEnteredValue` field**.
///
/// That is what makes `format-cells` structurally incapable of writing a
/// value: there is nowhere on this type to put one, regardless of what a
/// caller asks for (issue #1643's decision that `updateCells` stays
/// unused).
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct RepeatCellData {
    /// The format to apply.
    #[serde(rename = "userEnteredFormat")]
    pub user_entered_format: CellFormat,
}

/// `RepeatCellRequest`.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct RepeatCellRequest {
    /// Which cells this applies to.
    pub range: GridRange,
    /// The format to repeat across the range.
    pub cell: RepeatCellData,
    /// The field mask limiting what this request may change — built from
    /// exactly the `CellFormat` fields populated above it, prefixed
    /// `userEnteredFormat.` per the API's dotted-path convention.
    pub fields: String,
}

/// One border edge — `Border`.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Border {
    /// `"SOLID"` / `"SOLID_MEDIUM"` / `"SOLID_THICK"` / `"DASHED"` /
    /// `"DOTTED"` / `"DOUBLE"`.
    pub style: String,
    /// The line color.
    #[serde(rename = "colorStyle")]
    pub color_style: ColorStyle,
}

/// `UpdateBordersRequest`.
///
/// Each side is independently optional — an unset side is left exactly as
/// it was, which is what lets `format.rs::update-borders` touch only the
/// sides `--top`/`--bottom`/`--left`/`--right`/`--all` actually named.
/// `innerHorizontal`/`innerVertical` (the grid lines *between* cells in a
/// multi-cell range) have no flag in v1 — a documented cut, not a silent
/// gap.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct UpdateBordersRequest {
    /// Which cells this applies to.
    pub range: GridRange,
    /// Top edge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top: Option<Border>,
    /// Bottom edge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bottom: Option<Border>,
    /// Left edge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left: Option<Border>,
    /// Right edge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub right: Option<Border>,
}

/// `MergeCellsRequest`.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct MergeCellsRequest {
    /// Which cells to merge.
    pub range: GridRange,
    /// `"MERGE_ALL"` / `"MERGE_COLUMNS"` / `"MERGE_ROWS"`.
    #[serde(rename = "mergeType")]
    pub merge_type: String,
}

/// `UnmergeCellsRequest`.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct UnmergeCellsRequest {
    /// Which cells to unmerge.
    pub range: GridRange,
}

/// `AutoResizeDimensionsRequest`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AutoResizeDimensionsRequest {
    /// Which rows or columns to resize.
    pub dimensions: DimensionRange,
}

/// The mutable subset of a dimension's properties this crate can set —
/// currently only pixel size.
///
/// `hiddenByUser` has no flag in v1: sheet-level hide/show
/// (`structure.rs::SetSheetVisibility`) already covers the common case, and
/// per-row/column hiding is a documented follow-up.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct DimensionProperties {
    /// Width (for columns) or height (for rows), in pixels.
    #[serde(rename = "pixelSize")]
    pub pixel_size: i64,
}

/// `UpdateDimensionPropertiesRequest`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct UpdateDimensionPropertiesRequest {
    /// Which rows or columns to modify.
    pub range: DimensionRange,
    /// The properties to write.
    pub properties: DimensionProperties,
    /// The field mask limiting what this request may change. Always
    /// `"pixelSize"` today — the only field [`DimensionProperties`] models.
    pub fields: String,
}

/// One literal value a [`BooleanCondition`] compares against.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConditionValue {
    /// The literal, always sent/read as a string (Sheets parses it against
    /// the cell's type at evaluation time).
    #[serde(default, rename = "userEnteredValue")]
    pub user_entered_value: String,
}

/// A data validation rule's condition — `DataValidationRule.condition`.
///
/// Only the handful of `type`s `validation.rs` builds are ever constructed
/// here (`ONE_OF_LIST`, `NUMBER_BETWEEN`, `BOOLEAN`, `CUSTOM_FORMULA`), but
/// the type itself is a plain string so a rule this crate didn't create
/// (read back via `list-protections`' wider fields mask, or a workbook
/// edited outside this tool) still round-trips rather than failing to
/// parse.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BooleanCondition {
    /// Sheets' condition type string.
    #[serde(default, rename = "type")]
    pub condition_type: String,
    /// The condition's operands, when it has any (`BOOLEAN` with no
    /// argument has none).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<ConditionValue>,
}

/// `DataValidationRule`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DataValidationRule {
    /// What the entered value is checked against.
    #[serde(default)]
    pub condition: BooleanCondition,
    /// Shown as a tooltip on the cell.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "inputMessage"
    )]
    pub input_message: Option<String>,
    /// `true` rejects an invalid entry outright; `false`/absent shows a
    /// warning but allows it. Mirrors the API's own default (`false`) when
    /// omitted, so `set-data-validation`'s default (`--show-warning` unset)
    /// sends an explicit `true` rather than relying on that default holding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

/// `SetDataValidationRequest`.
///
/// `rule: None` **clears** the range's validation — the API's own
/// documented meaning for an absent `rule`, which is exactly what
/// `clear-data-validation` sends and why [`Self::rule`] is
/// `skip_serializing_if`-omitted rather than sent as an explicit `null`.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct SetDataValidationRequest {
    /// Which cells this applies to.
    pub range: GridRange,
    /// The rule to set, or `None` to clear.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule: Option<DataValidationRule>,
}

/// The editors of a protected range — `Editors`. Only the explicit
/// per-user list is modelled; Sheets' `domainUsersCanEdit` and `groups`
/// have no flag in v1.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProtectedRangeEditors {
    /// Email addresses granted an exemption from the protection.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub users: Vec<String>,
}

/// A protected range, as `addProtectedRange` builds one and
/// `spreadsheets.get` (with the wider protections `fields` mask) reads one
/// back.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProtectedRange {
    /// The server-assigned stable id. Absent on a request this crate is
    /// building (the server assigns it); always present on one read back.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "protectedRangeId"
    )]
    pub protected_range_id: Option<i64>,
    /// The protected cells. Every bound `None` protects the whole sheet
    /// (`protect-range --whole-sheet`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<GridRange>,
    /// A human-readable note about why the range is protected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `true` blocks an edit outright; `false`/absent only warns.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "warningOnly"
    )]
    pub warning_only: Option<bool>,
    /// Who may edit the protected cells despite the protection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editors: Option<ProtectedRangeEditors>,
}

/// `AddProtectedRangeRequest`.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct AddProtectedRangeRequest {
    /// The range to protect. `protected_range_id` is left unset — the
    /// server assigns it, only knowable from the reply.
    #[serde(rename = "protectedRange")]
    pub protected_range: ProtectedRange,
}

/// The mutable subset of a protected range `update-protection` can change.
///
/// Deliberately not [`ProtectedRange`]: reusing the response type would let
/// a caller accidentally attempt to overwrite `range`, which Sheets does
/// not allow this request to change at all — resizing a protection means
/// deleting and re-adding it, not updating.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct ProtectedRangeUpdate {
    /// Which protected range to modify.
    #[serde(rename = "protectedRangeId")]
    pub protected_range_id: i64,
    /// The new description, when changing it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The new warning-only flag, when changing it.
    #[serde(skip_serializing_if = "Option::is_none", rename = "warningOnly")]
    pub warning_only: Option<bool>,
    /// The new, complete editor list, when changing it. Sheets has no
    /// incremental add/remove — `protection.rs` computes this as the full
    /// resulting set from the range's current editors plus `--add-editor`/
    /// minus `--remove-editor`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub editors: Option<ProtectedRangeEditors>,
}

/// `UpdateProtectedRangeRequest`.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct UpdateProtectedRangeRequest {
    /// The properties to write. `protected_range_id` selects the target.
    #[serde(rename = "protectedRange")]
    pub protected_range: ProtectedRangeUpdate,
    /// The field mask limiting what this request may change.
    pub fields: String,
}

/// `DeleteProtectedRangeRequest`.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct DeleteProtectedRangeRequest {
    /// Which protected range to remove.
    #[serde(rename = "protectedRangeId")]
    pub protected_range_id: i64,
}

/// `InsertDimensionRequest`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct InsertDimensionRequest {
    /// Where to insert.
    pub range: DimensionRange,
    /// Whether the inserted rows/columns inherit formatting from the ones
    /// before them rather than after. Always `false` here: `true` is
    /// rejected outright by the API when `start_index` is 0, so a fixed
    /// `false` is the only value that works for every legal `--at`.
    #[serde(rename = "inheritFromBefore")]
    pub inherit_from_before: bool,
}

/// `DeleteSheetRequest` — names the sheet to remove entirely.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DeleteSheetRequest {
    /// The sheet to delete.
    #[serde(rename = "sheetId")]
    pub sheet_id: i64,
}

/// `DeleteDimensionRequest` — deletes the rows or columns spanned by `range`.
///
/// Shifts the remainder to close the gap. Reuses [`DimensionRange`] as-is:
/// the request shape is identical to [`InsertDimensionRequest`]'s `range`,
/// just interpreted as removal instead of insertion.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DeleteDimensionRequest {
    /// The rows or columns to delete.
    pub range: DimensionRange,
}

/// Which axis a [`DeleteRangeRequest`] shifts remaining cells along to fill
/// the gap left by a deleted rectangular range.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum ShiftDimension {
    /// Cells below the deleted range shift up.
    #[serde(rename = "ROWS")]
    Rows,
    /// Cells to the right of the deleted range shift left.
    #[serde(rename = "COLUMNS")]
    Columns,
}

impl ShiftDimension {
    /// The wire spelling, also used in human-readable output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rows => "ROWS",
            Self::Columns => "COLUMNS",
        }
    }
}

/// `DeleteRangeRequest` — deletes the cells in `range`, shifting the
/// remainder along `shift_dimension` to close the gap.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DeleteRangeRequest {
    /// The cells to delete.
    pub range: GridRange,
    /// Which way to shift the remaining cells afterward.
    #[serde(rename = "shiftDimension")]
    pub shift_dimension: ShiftDimension,
}

/// Response to `spreadsheets.batchUpdate`.
///
/// Only `replies` is modelled, and only the `addSheet` arm of it: the new
/// sheet's server-assigned `sheetId` is the one fact the response carries
/// that the request did not already know, and the request log records it.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct BatchUpdateResponse {
    /// One reply per request, in request order. Replies for requests with
    /// nothing to report are empty objects, not omitted.
    #[serde(default)]
    pub replies: Vec<BatchUpdateReply>,
}

/// One reply within a [`BatchUpdateResponse`].
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct BatchUpdateReply {
    /// Present only for an `addSheet` request.
    #[serde(default, rename = "addSheet")]
    pub add_sheet: Option<AddSheetReply>,
    /// Present only for a `duplicateSheet` request. Same wire shape as
    /// `addSheet`'s reply (`{"properties": {...}}`), so it reuses
    /// [`AddSheetReply`] rather than a near-identical twin type.
    #[serde(default, rename = "duplicateSheet")]
    pub duplicate_sheet: Option<AddSheetReply>,
    /// Present only for an `addProtectedRange` request — carries the
    /// server-assigned `protectedRangeId`, only knowable from the reply.
    #[serde(default, rename = "addProtectedRange")]
    pub add_protected_range: Option<AddProtectedRangeReply>,
}

/// The `addProtectedRange` arm of a [`BatchUpdateReply`].
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct AddProtectedRangeReply {
    /// The created protection, including its assigned `protectedRangeId`.
    #[serde(default, rename = "protectedRange")]
    pub protected_range: Option<ProtectedRange>,
}

/// The `addSheet`/`duplicateSheet` arm of a [`BatchUpdateReply`].
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct AddSheetReply {
    /// The created sheet's properties, including its assigned `sheetId`.
    #[serde(default)]
    pub properties: Option<SheetProperties>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn shift_dimension_as_str_matches_the_wire_spelling() {
        assert_eq!(ShiftDimension::Rows.as_str(), "ROWS");
        assert_eq!(ShiftDimension::Columns.as_str(), "COLUMNS");
    }

    #[test]
    fn spreadsheet_parses_a_realistic_fields_masked_response() {
        let json = serde_json::json!({
            "spreadsheetId": "sheet-1",
            "properties": {"title": "Budget"},
            "sheets": [
                {"properties": {"sheetId": 0, "title": "Q1", "index": 0,
                                "gridProperties": {"rowCount": 1000, "columnCount": 26}}},
                {"properties": {"sheetId": 7, "title": "My Sheet", "index": 1, "hidden": true}},
            ],
        });
        let parsed: Spreadsheet = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.title(), "Budget");
        assert_eq!(parsed.sheet_titles(), vec!["Q1", "My Sheet"]);
        assert!(!parsed.sheets[0].hidden());
        assert!(parsed.sheets[1].hidden());
        assert_eq!(
            parsed.sheets[0]
                .properties
                .as_ref()
                .unwrap()
                .grid_properties
                .as_ref()
                .unwrap()
                .row_count,
            Some(1000)
        );
    }

    #[test]
    fn spreadsheet_tolerates_unmodelled_fields() {
        let json = serde_json::json!({
            "spreadsheetId": "s",
            "somethingNew": {"nested": true},
            "sheets": [{"properties": {"title": "A"}, "conditionalFormats": []}],
        });
        let parsed: Spreadsheet = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.sheet_titles(), vec!["A"]);
    }

    #[test]
    fn spreadsheet_title_is_empty_when_the_fields_mask_omitted_it() {
        let parsed: Spreadsheet = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(parsed.title(), "");
        assert!(parsed.sheet_titles().is_empty());
    }

    #[test]
    fn value_range_absent_values_parses_as_empty_not_an_error() {
        // The shape an empty sheet actually returns: `values` is missing.
        let json = serde_json::json!({"range": "'Blank'!A1:Z1000"});
        let parsed: ValueRange = serde_json::from_value(json).unwrap();
        assert!(parsed.values.is_empty());
        assert_eq!(parsed.width(), 0);
        assert_eq!(parsed.range.as_deref(), Some("'Blank'!A1:Z1000"));
    }

    #[test]
    fn value_range_width_is_the_widest_ragged_row() {
        let json = serde_json::json!({
            "range": "S!A1:C2",
            "values": [["a", "b", "c"], ["d"]],
        });
        let parsed: ValueRange = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.width(), 3);
        assert_eq!(parsed.values[1].len(), 1, "raggedness is preserved as-is");
    }

    #[test]
    fn value_range_preserves_non_string_cell_types() {
        // UNFORMATTED_VALUE yields JSON numbers and bools, not strings.
        let json = serde_json::json!({"range": "S!A1:C1", "values": [[1234.5, true, "x"]]});
        let parsed: ValueRange = serde_json::from_value(json).unwrap();
        assert!(parsed.values[0][0].is_number());
        assert!(parsed.values[0][1].is_boolean());
        assert!(parsed.values[0][2].is_string());
    }

    #[test]
    fn batch_get_response_parses_and_defaults_to_empty() {
        let json = serde_json::json!({
            "valueRanges": [
                {"range": "'A'!A1:B1", "values": [["1", "2"]]},
                {"range": "'B'!A1:A1"},
            ],
        });
        let parsed: BatchGetValuesResponse = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.value_ranges.len(), 2);
        assert!(parsed.value_ranges[1].values.is_empty());

        let empty: BatchGetValuesResponse = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(empty.value_ranges.is_empty());
    }
    #[test]
    fn update_values_response_parses_the_counts() {
        let json = serde_json::json!({
            "spreadsheetId": "s", "updatedRange": "'Q1'!A1:B2",
            "updatedRows": 2, "updatedColumns": 2, "updatedCells": 4,
        });
        let parsed: UpdateValuesResponse = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.updated_range.as_deref(), Some("'Q1'!A1:B2"));
        assert_eq!(parsed.updated_cells, Some(4));
    }

    #[test]
    fn append_values_response_nests_the_counts_under_updates() {
        // The one shape difference from values.update — reading the counts
        // from the top level here would silently log zeroes.
        let json = serde_json::json!({
            "tableRange": "'Q1'!A1:B3",
            "updates": {"updatedRange": "'Q1'!A4:B4", "updatedRows": 1, "updatedCells": 2},
        });
        let parsed: AppendValuesResponse = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.table_range.as_deref(), Some("'Q1'!A1:B3"));
        assert_eq!(parsed.updates.unwrap().updated_cells, Some(2));
    }

    #[test]
    fn clear_values_response_parses_the_cleared_range() {
        let json = serde_json::json!({"spreadsheetId": "s", "clearedRange": "'Q1'!A1:Z999"});
        let parsed: ClearValuesResponse = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.cleared_range.as_deref(), Some("'Q1'!A1:Z999"));
    }

    #[test]
    fn write_responses_tolerate_missing_counts() {
        let parsed: UpdateValuesResponse = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(parsed.updated_cells, None);
        let parsed: AppendValuesResponse = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(parsed.updates.is_none());
    }
}
