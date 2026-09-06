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
/// `{"addSheet": {...}}`. That is also why this type is the enforcement
/// point for issue #1613's central safety property — the destructive
/// requests (`deleteSheet`, `deleteDimension`, `deleteRange`) have no
/// variant here, so no caller anywhere in the crate can construct one. See
/// `structure.rs`'s `no_destructive_request_is_reachable` guard.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum BatchUpdateRequestItem {
    /// Add a new sheet to the workbook.
    AddSheet(AddSheetRequest),
    /// Change an existing sheet's properties (we only ever send `title`).
    UpdateSheetProperties(UpdateSheetPropertiesRequest),
    /// Insert empty rows or columns, shifting existing ones.
    InsertDimension(InsertDimensionRequest),
}

/// Body of `spreadsheets.batchUpdate`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
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
/// We always send `"title"` and only ever populate the title.
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
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SheetPropertiesUpdate {
    /// Which sheet to modify.
    #[serde(rename = "sheetId")]
    pub sheet_id: i64,
    /// The new title.
    pub title: String,
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
}

/// The `addSheet` arm of a [`BatchUpdateReply`].
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
