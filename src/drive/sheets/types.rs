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

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A spreadsheet's metadata, from `spreadsheets.get`.
///
/// Always request an explicit `fields` mask when fetching this: the
/// unmasked response embeds **every cell of every sheet**, which on a large
/// workbook is an out-of-memory failure rather than a slow request.
///
/// `PartialEq`-only, not `Eq` (issue #1793): a [`Sheet`]'s
/// `conditional_formats` can embed a `Color` (`f32`-based), which has no
/// meaningful `Eq` — see [`Color`]'s own doc comment.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
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
    /// Named ranges defined anywhere in the workbook. A named range is
    /// workbook-scoped, not sheet-scoped (unlike [`ProtectedRange`], which
    /// nests under the [`Sheet`] it protects), so this lives here rather
    /// than on `Sheet`. Empty unless the caller requested it with a wider
    /// `fields` mask — `SPREADSHEET_FIELDS` (the mask every other call uses)
    /// omits this; only `SheetsApi::get_spreadsheet_with_named_ranges`
    /// populates it.
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "namedRanges")]
    pub named_ranges: Vec<NamedRange>,
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

    /// Every sheet id, for sheets that carry one.
    #[must_use]
    pub fn sheet_ids(&self) -> std::collections::HashSet<i64> {
        self.sheets.iter().filter_map(Sheet::sheet_id).collect()
    }

    /// Whether any sheet in the workbook currently uses `title` as its
    /// display title — the collision check `drive sheets rename-sheet`/
    /// `duplicate-sheet` (via `structure.rs`'s `resolve_sheet`) and `drive
    /// lease restore`'s best-effort rename-back (issue #1676) share, so a
    /// future change to what counts as "the same title" (e.g.
    /// case-insensitive) lands in one place rather than drifting between
    /// two hand-written copies.
    ///
    /// A sheet with no `properties` never matches, not even `""` — it has no
    /// title at all, so `Sheet::title()`'s `""` fallback (meant for display)
    /// would otherwise falsely collide with an explicit empty-title request.
    #[must_use]
    pub fn has_sheet_titled(&self, title: &str) -> bool {
        self.sheets
            .iter()
            .filter_map(|sheet| sheet.properties.as_ref())
            .any(|props| props.title == title)
    }
}

/// Workbook-level properties.
///
/// `PartialEq`-only, not `Eq` (issue #1836): [`IterativeCalculationSettings`]
/// carries an `f64`, which has no meaningful `Eq` — see [`Spreadsheet`]'s own
/// doc comment for the same reasoning applied to [`Color`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SpreadsheetProperties {
    /// The workbook's display title.
    #[serde(default)]
    pub title: String,
    /// The workbook's locale, e.g. `"en_US"` (issue #1836's
    /// `update-workbook-properties --locale`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    /// The workbook's IANA time zone, e.g. `"America/New_York"`
    /// (`update-workbook-properties --time-zone`).
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "timeZone")]
    pub time_zone: Option<String>,
    /// How often the workbook recalculates
    /// (`update-workbook-properties --auto-recalc`).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "autoRecalc"
    )]
    pub auto_recalc: Option<RecalculationInterval>,
    /// Iterative-calculation settings, present only when iterative
    /// calculation is on (`update-workbook-properties --iterative-calculation`).
    /// See [`IterativeCalculationSettings`]'s doc comment for why presence,
    /// not a boolean field, is what "on" means.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "iterativeCalculationSettings"
    )]
    pub iterative_calculation_settings: Option<IterativeCalculationSettings>,
}

/// A workbook's automatic-recalculation interval (`autoRecalc`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum RecalculationInterval {
    /// Sheets' way of saying the field was never set. Never something
    /// `update-workbook-properties --auto-recalc` writes — see
    /// [`crate::cli::drive::sheets::structure::AutoRecalcArg`]'s doc comment
    /// for why the CLI-facing enum omits it.
    #[serde(rename = "RECALCULATION_INTERVAL_UNSPECIFIED")]
    Unspecified,
    /// Recalculate on every edit (Sheets' own default).
    #[serde(rename = "ON_CHANGE")]
    OnChange,
    /// Recalculate at most once a minute.
    #[serde(rename = "MINUTE")]
    Minute,
    /// Recalculate at most once an hour.
    #[serde(rename = "HOUR")]
    Hour,
}

impl RecalculationInterval {
    /// The wire spelling, also reused in human-readable output (the
    /// `update-workbook-properties` dry-run preview, the real-run
    /// confirmation, and the request log's `fields_changed`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unspecified => "RECALCULATION_INTERVAL_UNSPECIFIED",
            Self::OnChange => "ON_CHANGE",
            Self::Minute => "MINUTE",
            Self::Hour => "HOUR",
        }
    }
}

/// A workbook's iterative-calculation settings (`iterativeCalculationSettings`).
///
/// Sheets has no boolean "enabled" field: this object's mere *presence* on
/// [`SpreadsheetProperties`] is what turns iterative calculation on, and its
/// *absence* is what turns it off. `update-workbook-properties
/// --iterative-calculation off` therefore clears the field entirely (an
/// absent key in the field-masked request, via
/// [`SpreadsheetPropertiesUpdate::iterative_calculation_settings`]) rather
/// than writing some "disabled" value into it — there is no such value.
///
/// Turning iterative calculation on changes what a circular-reference
/// formula elsewhere in the workbook *evaluates to*: a value effect reached
/// indirectly, the same shape of concern ADR-0081 raised for named-range
/// deletion. It is still gated as `sheets-structure` rather than a data-
/// mutating operation, because — like a named-range deletion — no cell's
/// formula is itself changed, only what some formulas compute (issue #1836,
/// [ADR-0086](../../../docs/adrs/adr-0086-workbook-properties.md) §9).
///
/// `PartialEq`-only, not `Eq`: `convergence_threshold` is an `f64`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IterativeCalculationSettings {
    /// Maximum number of calculation rounds per recalculation. Omitted in a
    /// request takes Sheets' own default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_iterations: Option<i64>,
    /// The maximum change between two consecutive rounds that still counts
    /// as converged. Omitted in a request takes Sheets' own default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub convergence_threshold: Option<f64>,
}

impl IterativeCalculationSettings {
    /// A human-readable qualifier for whichever bounds are set, e.g.
    /// `" (max 50 iterations, threshold 0.01)"` — empty (not just the
    /// bounds omitted) when both take Sheets' own default. The leading
    /// space lets every call site append it directly after `"on"`.
    ///
    /// Shared by `update-workbook-properties`'s dry-run preview and
    /// real-run confirmation (`structure.rs::workbook_properties_summary`)
    /// and `sheets info`'s table render, so the two can never describe the
    /// same settings differently.
    #[must_use]
    pub fn describe_bounds(&self) -> String {
        match (self.max_iterations, self.convergence_threshold) {
            (Some(max), Some(threshold)) => {
                format!(" (max {max} iterations, threshold {threshold})")
            }
            (Some(max), None) => format!(" (max {max} iterations)"),
            (None, Some(threshold)) => format!(" (threshold {threshold})"),
            (None, None) => String::new(),
        }
    }
}

/// One sheet (tab) within a spreadsheet.
/// `PartialEq`-only, not `Eq` (issue #1793) — see [`Spreadsheet`]'s doc
/// comment.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
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
    /// This sheet's basic filter, if it has one. Empty unless the caller
    /// requested it with a wider `fields` mask — only
    /// `SheetsApi::get_spreadsheet_with_filter_views` populates it
    /// (issue #1794).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "basicFilter"
    )]
    pub basic_filter: Option<BasicFilter>,
    /// The named, id-addressed filter views on this sheet. Empty unless the
    /// caller requested them with a wider `fields` mask — only
    /// `SheetsApi::get_spreadsheet_with_filter_views` populates it
    /// (issue #1794).
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "filterViews")]
    pub filter_views: Vec<FilterView>,
    /// Conditional format rules on this sheet, in their API-defined
    /// evaluation order — the order `add-conditional-format`'s `--index`/
    /// `update-conditional-format`'s and `delete-conditional-format`'s
    /// `--index` address. Empty unless the caller requested a wider
    /// `fields` mask; only `SheetsApi::get_spreadsheet_with_conditional_formats`
    /// populates it (issue #1793).
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        rename = "conditionalFormats"
    )]
    pub conditional_formats: Vec<ConditionalFormatRule>,
    /// Charts embedded on this sheet. Empty unless the caller requested
    /// them with a wider `fields` mask — only
    /// `SheetsApi::get_spreadsheet_with_embedded_objects` populates it
    /// (issue #1797). `chartId`-addressed by `update-chart`/`delete-chart`;
    /// `list-charts` is how that id is discovered.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub charts: Vec<EmbeddedChart>,
    /// Slicers embedded on this sheet. Empty unless the caller requested
    /// them with a wider `fields` mask — only
    /// `SheetsApi::get_spreadsheet_with_embedded_objects` populates it
    /// (issue #1797). `slicerId`-addressed by `update-slicer`/
    /// `delete-slicer`; `list-slicers` is how that id is discovered.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub slicers: Vec<Slicer>,
    /// Banded ranges (alternating row/column colors) on this sheet. Empty
    /// unless the caller requested them with a wider `fields` mask — only
    /// `SheetsApi::get_spreadsheet_with_banding` populates it (issue
    /// #1832). `bandedRangeId`-addressed by `update-banding`/
    /// `delete-banding`; `list-bandings` is how that id is discovered.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        rename = "bandedRanges"
    )]
    pub banded_ranges: Vec<BandedRange>,
    /// Outline groups over row spans on this sheet — the collapsible +/-
    /// grouping bar. Empty unless the caller requested them with a wider
    /// `fields` mask — only
    /// `SheetsApi::get_spreadsheet_with_dimension_groups` populates it
    /// (issue #1833). Addressed by `(range, depth)`;
    /// `list-dimension-groups` is how both are discovered.
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "rowGroups")]
    pub row_groups: Vec<DimensionGroup>,
    /// Outline groups over column spans on this sheet. Same population and
    /// addressing as [`Self::row_groups`], the column axis.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        rename = "columnGroups"
    )]
    pub column_groups: Vec<DimensionGroup>,
    /// Grid data — cell-by-cell, in row-major chunks — read back only when
    /// the caller asked for it via a `fields` mask naming individual cell
    /// properties. Empty unless the caller requested a wider mask; only
    /// [`SheetsApi::get_spreadsheet_with_pivot_tables`](crate::drive::sheets::api::SheetsApi::get_spreadsheet_with_pivot_tables)/
    /// `SheetsApi::get_cell_pivot` populate it (issue #1798). Chunked
    /// rather than one flat grid because the API itself returns it that
    /// way — see [`GridData`]'s doc comment.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data: Vec<GridData>,
}

impl Sheet {
    /// This sheet's title, or `""` if absent.
    #[must_use]
    pub fn title(&self) -> &str {
        self.properties
            .as_ref()
            .map_or("", |props| props.title.as_str())
    }

    /// This sheet's stable numeric id, or `None` if the properties (or the
    /// id within them) are absent from the response.
    #[must_use]
    pub fn sheet_id(&self) -> Option<i64> {
        self.properties.as_ref()?.sheet_id
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
    /// Whether the sheet is laid out right-to-left (issue #1835). Not
    /// paired with a `tab_color_style` field here — see
    /// [`SheetPropertiesUpdate::tab_color_style`]'s doc comment for why the
    /// tab color is write-only and deliberately never read back onto this
    /// response type.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "rightToLeft"
    )]
    pub right_to_left: Option<bool>,
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
    /// Rows frozen at the top (issue #1835).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "frozenRowCount"
    )]
    pub frozen_row_count: Option<i64>,
    /// Columns frozen at the left (issue #1835).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "frozenColumnCount"
    )]
    pub frozen_column_count: Option<i64>,
    /// Whether gridlines are hidden in the UI (issue #1835).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "hideGridlines"
    )]
    pub hide_gridlines: Option<bool>,
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
/// validation, `duplicateSheet`, reorder/hide and, since issue #1795
/// ([ADR-0081](../../../docs/adrs/adr-0081.md) §4), developer-metadata
/// management, and extended again by issue #1796,
/// [ADR-0081](../../../docs/adrs/adr-0081.md) §2, with named-range
/// add/update/delete), the destructive ones behind
/// `DriveOperation::SheetsDelete` (issue #1623,
/// [ADR-0077](../../../docs/adrs/adr-0077-sheets-deletion-via-batchupdate.md)),
/// and the protected-range ones behind `DriveOperation::SheetsProtection`
/// (issue #1643, [ADR-0078](../../../docs/adrs/adr-0078.md) §2) — there is
/// still no raw `--requests` passthrough that could construct one of these
/// outside its gate. `structure.rs`'s `every_delete_verb_gates_on_sheets_delete_not_sheets_structure`/
/// `every_additive_verb_still_gates_on_sheets_structure` tests pin that every
/// verb reaches exactly one gate, never the other; `developer_metadata.rs`
/// has no dispatch table to get wrong in the first place — its `Set`/`Delete`
/// verbs share the single hardcoded `DriveOperation::SheetsStructure` call
/// site in `developer_metadata_inner`.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum BatchUpdateRequestItem {
    /// Add a new sheet to the workbook.
    AddSheet(AddSheetRequest),
    /// Change an existing sheet's properties — title (rename), index
    /// (reorder), hidden (hide/show), or, since issue #1835's
    /// `update-sheet-properties`, any of frozen rows/columns, hidden
    /// gridlines, tab color, or right-to-left layout. `rename-sheet`/
    /// `reorder-sheet`/`hide-sheet`/`show-sheet` each set exactly one field
    /// per request; `update-sheet-properties` may set several at once, but
    /// the request's `fields` mask always names exactly what was
    /// populated, one entry per field, so it can never blank a property it
    /// didn't mean to touch.
    UpdateSheetProperties(UpdateSheetPropertiesRequest),
    /// Change workbook-level properties — locale, time zone, auto-recalc
    /// and/or iterative calculation (`update-workbook-properties`, issue
    /// #1836). Gated by
    /// [`crate::drive::write_gate::DriveOperation::SheetsStructure`], like
    /// [`Self::UpdateSheetProperties`]: no cell's formula changes, only
    /// (for iterative calculation) what some formulas compute — see
    /// [`IterativeCalculationSettings`]'s doc comment.
    UpdateSpreadsheetProperties(UpdateSpreadsheetPropertiesRequest),
    /// Insert empty rows or columns, shifting existing ones.
    InsertDimension(InsertDimensionRequest),
    /// Insert empty cells into a rectangular range, shifting existing cells
    /// down or right within the grid.
    InsertRange(InsertRangeRequest),
    /// Delete an entire sheet from the workbook.
    DeleteSheet(DeleteSheetRequest),
    /// Delete whole rows or columns, shifting the remainder to close the gap.
    DeleteDimension(DeleteDimensionRequest),
    /// Move whole rows or columns to a new position within their sheet
    /// (`move-rows`/`move-columns`, issue #1834). Gated by
    /// [`crate::drive::write_gate::DriveOperation::SheetsStructure`] like
    /// the inserts, not `SheetsDelete`: a reordering discards nothing
    /// (ADR-0083's "additive/structural" group).
    MoveDimension(MoveDimensionRequest),
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
    /// Attach a new developer-metadata entry (`set-developer-metadata`,
    /// when no existing entry matched). Gated by
    /// [`crate::drive::write_gate::DriveOperation::SheetsStructure`], like
    /// every other additive verb — see [`DeveloperMetadata`]'s doc comment
    /// for the DOCUMENT-only guarantee (issue #1795, ADR-0081 §4).
    CreateDeveloperMetadata(CreateDeveloperMetadataRequest),
    /// Change an existing developer-metadata entry's value
    /// (`set-developer-metadata`, when an existing entry matched). Same
    /// gate as [`Self::CreateDeveloperMetadata`].
    UpdateDeveloperMetadata(UpdateDeveloperMetadataRequest),
    /// Remove developer metadata (`delete-developer-metadata`) — a
    /// third-party add-on's own state, not this spreadsheet's grid data, so
    /// it stays under `SheetsStructure` rather than `SheetsDelete` (ADR-0081
    /// §4). `developer_metadata.rs` reads back and reports every entry a
    /// request like this would remove before ever sending it.
    DeleteDeveloperMetadata(DeleteDeveloperMetadataRequest),
    /// Upsert a sheet's basic filter (`set-basic-filter`). Gated by
    /// `SheetsStructure` (issue #1794, ADR-0081): a filter hides rows,
    /// which is view state, not data.
    SetBasicFilter(SetBasicFilterRequest),
    /// Remove a sheet's basic filter (`clear-basic-filter`). Same gate as
    /// [`Self::SetBasicFilter`].
    ClearBasicFilter(ClearBasicFilterRequest),
    /// Add a named filter view (`add-filter-view`). Same gate as
    /// [`Self::SetBasicFilter`].
    AddFilterView(AddFilterViewRequest),
    /// Change an existing filter view's title, range, sort order, or hidden
    /// values (`update-filter-view`). Same gate as [`Self::SetBasicFilter`].
    UpdateFilterView(UpdateFilterViewRequest),
    /// Remove a filter view (`delete-filter-view`). Same gate as
    /// [`Self::SetBasicFilter`].
    DeleteFilterView(DeleteFilterViewRequest),
    /// Add a conditional format rule (`add-conditional-format`). Gated by
    /// `DriveOperation::SheetsStructure` — presentational, destroys no data
    /// (ADR-0081 §1, issue #1793).
    AddConditionalFormatRule(AddConditionalFormatRuleRequest),
    /// Replace an existing conditional format rule at an index
    /// (`update-conditional-format`). Same gate as
    /// [`Self::AddConditionalFormatRule`].
    UpdateConditionalFormatRule(UpdateConditionalFormatRuleRequest),
    /// Remove a conditional format rule at an index
    /// (`delete-conditional-format`). Same gate as
    /// [`Self::AddConditionalFormatRule`] — a presentational removal, not a
    /// *data* deletion.
    DeleteConditionalFormatRule(DeleteConditionalFormatRuleRequest),
    /// Add a named range (`add-named-range`). Gated
    /// `DriveOperation::SheetsStructure` — a label over a region, created
    /// without touching any cell's value.
    AddNamedRange(AddNamedRangeRequest),
    /// Change an existing named range's name and/or range
    /// (`update-named-range`). Same gate as [`Self::AddNamedRange`].
    UpdateNamedRange(UpdateNamedRangeRequest),
    /// Remove a named range (`delete-named-range`). Same gate as
    /// [`Self::AddNamedRange`] — it removes a label, not grid data; see
    /// [ADR-0081](../../../docs/adrs/adr-0081.md) §2 for why this stays
    /// `SheetsStructure` rather than `SheetsDelete`, and for the mandatory
    /// referencing-formula preview that mitigates the resulting `#NAME?`
    /// errors.
    DeleteNamedRange(DeleteNamedRangeRequest),
    /// Add a chart (`add-chart`). Gated by `DriveOperation::SheetsStructure`
    /// — additive, destroys no data (ADR-0081 §3, issue #1797).
    AddChart(AddChartRequest),
    /// Replace an existing chart's spec wholesale (`update-chart`). Same
    /// gate as [`Self::AddChart`]. Unlike `updateFilterView`, this request
    /// carries **no field mask** — see [`UpdateChartSpecRequest`]'s doc
    /// comment.
    UpdateChartSpec(UpdateChartSpecRequest),
    /// Add a slicer (`add-slicer`). Same gate as [`Self::AddChart`].
    AddSlicer(AddSlicerRequest),
    /// Change an existing slicer's spec (`update-slicer`). Same gate as
    /// [`Self::AddChart`]. Unlike [`Self::UpdateChartSpec`], this request
    /// *does* carry a field mask — see [`UpdateSlicerSpecRequest`].
    UpdateSlicerSpec(UpdateSlicerSpecRequest),
    /// Remove a chart or slicer (`delete-chart`/`delete-slicer`) — the one
    /// request shared by both, addressed by `objectId` alone with no
    /// discriminator naming which kind it is. Gated by
    /// `DriveOperation::SheetsStructure`, not `SheetsDelete`: ADR-0081 §3
    /// judges a chart/slicer the same *kind* of removal as
    /// `unmerge-cells`/`clear-data-validation` — a property of the sheet,
    /// not the sheet's data — despite being genuinely unrecoverable, which
    /// is why `embedded_object.rs` reads back and reports the object's
    /// spec before ever sending this.
    DeleteEmbeddedObject(DeleteEmbeddedObjectRequest),
    /// Move and/or resize an existing chart or slicer (`move-chart`/
    /// `move-slicer`). Gated by `DriveOperation::SheetsStructure` (issue
    /// #1837, [ADR-0081](../../../docs/adrs/adr-0081.md) §3) — the same
    /// operation as `add-chart`/`add-slicer`'s own placement, since a move
    /// discards no data.
    UpdateEmbeddedObjectPosition(UpdateEmbeddedObjectPositionRequest),
    /// Write a pivot table into (`add-pivot-table`) or clear one from
    /// (`delete-pivot-table`) a single anchor cell. The crate's only
    /// `updateCells` request — issue #1643's "`updateCells` stays unused"
    /// decision ([`RepeatCellData`]'s doc comment) was specifically about
    /// writing a *value*, which [`PivotCellData`] still cannot do (it has
    /// no `userEnteredValue` field, same guarantee as `RepeatCellData`).
    /// Gated by **both** `DriveOperation::SheetsWrite` and
    /// `DriveOperation::SheetsStructure` for `add-pivot-table` (a grid
    /// write with a server-computed extent, but also a named structural
    /// feature — ADR-0081 §5's union composition), and by `SheetsWrite`
    /// alone for `delete-pivot-table` (it only ever clears the one anchor
    /// cell's value, no structural effect).
    UpdateCells(UpdateCellsRequest),
    /// Add a new banded range (`add-banding`) — alternating row/column
    /// colors, presentation applied to a range. Gated by
    /// `DriveOperation::SheetsStructure` (issue #1832,
    /// [ADR-0082](../../../docs/adrs/adr-0082-banded-ranges.md)): the same
    /// "property of the sheet, not the sheet's data" reasoning as
    /// `unmerge-cells`/`clear-data-validation`.
    AddBanding(AddBandingRequest),
    /// Change an existing banded range's colors and/or range
    /// (`update-banding`). Same gate as [`Self::AddBanding`].
    UpdateBanding(UpdateBandingRequest),
    /// Remove a banded range (`delete-banding`). Same gate as
    /// [`Self::AddBanding`] — it removes presentation, not grid data.
    DeleteBanding(DeleteBandingRequest),
    /// Add a new outline group over a row or column span
    /// (`add-dimension-group`) — the collapsible +/- grouping bar. Gated by
    /// `DriveOperation::SheetsStructure` (issue #1833,
    /// [ADR-0084](../../../docs/adrs/adr-0084-dimension-groups.md)): the
    /// same "property of the sheet, not the sheet's data" reasoning as
    /// [`Self::AddBanding`].
    AddDimensionGroup(AddDimensionGroupRequest),
    /// Change an existing outline group's `collapsed` state
    /// (`update-dimension-group`). Same gate as [`Self::AddDimensionGroup`].
    UpdateDimensionGroup(UpdateDimensionGroupRequest),
    /// Remove an outline group (`delete-dimension-group`). Same gate as
    /// [`Self::AddDimensionGroup`] — it removes presentation, not grid data.
    DeleteDimensionGroup(DeleteDimensionGroupRequest),
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
/// Every caller but `update-sheet-properties` (issue #1835) populates
/// exactly the one [`SheetPropertiesUpdate`] field its `fields` string
/// names; `update-sheet-properties` may populate several at once (frozen
/// rows/columns, hidden gridlines, tab color, right-to-left), and its mask
/// still names exactly what was populated, field for field.
/// `structure.rs::build_request` is the single place that pairs the
/// properties and the mask, so they can never drift apart.
///
/// `PartialEq`-only, not `Eq`: [`SheetPropertiesUpdate`] embeds
/// [`ColorStyle`], which wraps [`Color`]'s `f32` channels.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct UpdateSheetPropertiesRequest {
    /// The properties to write. `sheet_id` selects the target.
    pub properties: SheetPropertiesUpdate,
    /// The field mask limiting what this request may change.
    pub fields: String,
}

/// `UpdateSpreadsheetPropertiesRequest` (`update-workbook-properties`, issue
/// #1836).
///
/// Same field-mask discipline as [`UpdateSheetPropertiesRequest`]: `fields`
/// is not optional, and `structure.rs::build_request` is the single place
/// that pairs it with [`SpreadsheetPropertiesUpdate`] so the two can never
/// drift apart. Unlike a sheet-properties update this one has no id to
/// select a target — the workbook itself is the target, addressed by the
/// `spreadsheetId` already in the URL.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct UpdateSpreadsheetPropertiesRequest {
    /// The properties to write.
    pub properties: SpreadsheetPropertiesUpdate,
    /// The field mask limiting what this request may change.
    pub fields: String,
}

/// The mutable subset of a workbook's properties this crate can set
/// (`update-workbook-properties`, issue #1836).
///
/// Deliberately not [`SpreadsheetProperties`] itself, for the same reason
/// [`SheetPropertiesUpdate`] is not [`SheetProperties`]: reusing the response
/// type would serialise whatever else it happened to carry, and a field mask
/// widened by accident is how an unintended property gets overwritten. Every
/// field is independently optional and `structure.rs::build_request` names
/// in `fields` exactly the ones the caller actually set — never more, since
/// an unset field named in the mask would blank it rather than leave it
/// alone.
///
/// `iterative_calculation_settings` is `None` both when the caller never
/// touched iterative calculation (in which case `fields` omits
/// `iterativeCalculationSettings` and this value is never serialised) and
/// when the caller explicitly turned it off (in which case `fields` *does*
/// name it, so the omitted key clears the property) — `structure.rs`'s
/// `IterativeCalculationToggle` is what tells the two apart before the mask
/// is built. `PartialEq`-only, not `Eq`, for the same reason
/// [`IterativeCalculationSettings`] is.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct SpreadsheetPropertiesUpdate {
    /// The new locale, for `--locale`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    /// The new time zone, for `--time-zone`.
    #[serde(skip_serializing_if = "Option::is_none", rename = "timeZone")]
    pub time_zone: Option<String>,
    /// The new recalculation interval, for `--auto-recalc`.
    #[serde(skip_serializing_if = "Option::is_none", rename = "autoRecalc")]
    pub auto_recalc: Option<RecalculationInterval>,
    /// The new iterative-calculation settings, for `--iterative-calculation
    /// on`; `None` to leave it untouched *or* to turn it off — see this
    /// struct's own doc comment for how `fields` disambiguates the two.
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "iterativeCalculationSettings"
    )]
    pub iterative_calculation_settings: Option<IterativeCalculationSettings>,
}

/// Body of `spreadsheets.sheets.copyTo`.
///
/// Copies one sheet into another spreadsheet, creating a new sheet there
/// with a server-assigned id (never the source's), so no destination-side
/// id collision is possible.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CopySheetToAnotherSpreadsheetRequest {
    /// The spreadsheet to copy the sheet into.
    #[serde(rename = "destinationSpreadsheetId")]
    pub destination_spreadsheet_id: String,
}

/// The mutable subset of a sheet's properties this crate can set.
///
/// Deliberately not [`SheetProperties`]: reusing the response type would
/// serialise whatever else it happened to carry, and a field mask widened by
/// accident is how an unintended property gets overwritten.
///
/// Every field but `sheet_id` is optional. `title` (rename), `index`
/// (reorder) and `hidden` (hide/show) each have their own single-field
/// caller, which sets exactly that field alongside a `fields` mask naming
/// it. `update-sheet-properties` (issue #1835) is different: it is the
/// first caller that may populate several fields — `grid_properties`,
/// `tab_color_style`, `right_to_left` — in one request, each still paired
/// with its own mask entry. Modelling every settable property on one
/// struct, rather than one struct per verb, is what lets `resolve_sheet`/
/// `validate_verb_args`/`build_request`'s existing `updateSheetProperties`
/// plumbing serve all of them without triplicating it.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
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
    /// Frozen rows/columns and hidden gridlines, for `update-sheet-properties`
    /// (issue #1835).
    #[serde(skip_serializing_if = "Option::is_none", rename = "gridProperties")]
    pub grid_properties: Option<GridPropertiesUpdate>,
    /// The new tab color, for `update-sheet-properties` (issue #1835).
    ///
    /// Deliberately write-only: [`SheetProperties`] (the response type)
    /// never carries this field, even though the API returns
    /// `tabColorStyle` on a sheet that has one. Adding it there would strip
    /// `Eq` from `SheetProperties`, and transitively from `Sheet` and
    /// `Spreadsheet`, for no benefit — a dry-run preview only ever needs to
    /// state what the color *becomes*, never what it was, since (unlike
    /// frozen rows/columns) there is no meaningful "before" number to show
    /// next to a swatch. `--clear-tab-color` (the CLI layer) leaves this
    /// `None` while still naming `tabColorStyle` in the `fields` mask —
    /// that is how the API is told to clear a field rather than leave it
    /// unset.
    #[serde(skip_serializing_if = "Option::is_none", rename = "tabColorStyle")]
    pub tab_color_style: Option<ColorStyle>,
    /// Whether the sheet should be right-to-left, for `update-sheet-properties`
    /// (issue #1835).
    #[serde(skip_serializing_if = "Option::is_none", rename = "rightToLeft")]
    pub right_to_left: Option<bool>,
}

/// The `gridProperties` subset [`SheetPropertiesUpdate`] can set — frozen
/// rows/columns and hidden gridlines (issue #1835).
///
/// Mirrors [`SheetPropertiesUpdate`]'s relationship to [`SheetProperties`]:
/// a request-side type distinct from the response-side [`GridProperties`]
/// so a field mask widened by accident can't blank `rowCount`/`columnCount`,
/// which this type has no way to even name.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct GridPropertiesUpdate {
    /// Rows to freeze at the top.
    #[serde(skip_serializing_if = "Option::is_none", rename = "frozenRowCount")]
    pub frozen_row_count: Option<i64>,
    /// Columns to freeze at the left.
    #[serde(skip_serializing_if = "Option::is_none", rename = "frozenColumnCount")]
    pub frozen_column_count: Option<i64>,
    /// Whether gridlines should be hidden.
    #[serde(skip_serializing_if = "Option::is_none", rename = "hideGridlines")]
    pub hide_gridlines: Option<bool>,
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
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
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

    /// The request log's `dimension_range` context value for a **1-based
    /// inclusive** span on this axis, e.g. `"ROWS 5:7"`. The one formatter
    /// for that key — `structure.rs` (insert/delete-dimension verbs) and
    /// `dimension_group.rs` both write it, and neither may spell it
    /// differently.
    #[must_use]
    pub fn span_label(self, start: i64, end: i64) -> String {
        format!("{} {start}:{end}", self.as_str())
    }
}

/// A half-open, **zero-based** span of rows or columns.
///
/// The zero-based half-open shape is the API's, not the CLI's: `--at` is
/// 1-based and inclusive, matching A1 and the spreadsheet UI. The single
/// conversion lives in `structure.rs::dimension_range`, never at a call
/// site.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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

/// An outline group over a [`DimensionRange`] — the collapsible +/-
/// grouping bar (issue #1833,
/// [ADR-0084](../../../docs/adrs/adr-0084-dimension-groups.md)).
///
/// `depth` is server-derived, never client-supplied: `addDimensionGroup`'s
/// request carries only `range`, and the server computes the new group's
/// depth from its overlap with existing groups on the same axis (a
/// superset increments an existing group's depth; a subset creates a new,
/// deeper one; a partial overlap widens the existing group to the union
/// and creates a new, deeper one over the intersection). This crate does
/// not predict that outcome — see the `dimension_group.rs` module doc for
/// why.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DimensionGroup {
    /// The span this group covers.
    pub range: DimensionRange,
    /// Nesting level — server-derived, **1-based**: `1` for a group nested
    /// in no other, `2` for one whose range lies wholly inside a depth-1
    /// group's, and so on (the API reference's own examples number a lone
    /// group `depth 1`). `0` never occurs in a reply.
    #[serde(default)]
    pub depth: i64,
    /// Whether the group is collapsed (its member rows/columns hidden).
    #[serde(default)]
    pub collapsed: bool,
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
///
/// Also `Deserialize` (issue #1793): `ConditionalFormatRule` reads existing
/// rules back via `list-conditional-formats`, so a color nested inside a
/// `BooleanRule`/`GradientRule` must round-trip, unlike `format-cells`'
/// purely outbound use of the same type.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
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
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct ColorStyle {
    /// The explicit RGB color.
    #[serde(rename = "rgbColor")]
    pub rgb_color: Color,
}

/// The mutable subset of a cell's text formatting this crate can set —
/// `CellFormat.textFormat`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
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
    /// Font family name, e.g. `"Arial"`.
    #[serde(skip_serializing_if = "Option::is_none", rename = "fontFamily")]
    pub font_family: Option<String>,
}

/// A cell's display number format — `CellFormat.numberFormat`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct NumberFormat {
    /// One of Sheets' type strings (`"TEXT"`, `"NUMBER"`, `"PERCENT"`,
    /// `"CURRENCY"`, `"DATE"`, `"TIME"`, `"DATE_TIME"`, `"SCIENTIFIC"`).
    #[serde(rename = "type")]
    pub format_type: String,
    /// The display pattern, e.g. `"#,##0.00"`.
    pub pattern: String,
}

/// `CellFormat.textRotation` — a union.
///
/// Exactly one of `angle`/`vertical` is ever set, never both: this wire
/// shape mirrors the API directly, but the engine only ever builds it from
/// `format::TextRotationFlag`, whose two variants make the "both set" state
/// unrepresentable one layer up.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TextRotation {
    /// Rotation angle in degrees, -90 to 90.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub angle: Option<i32>,
    /// Whether the text is stacked vertically instead of rotated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vertical: Option<bool>,
}

/// `CellFormat.padding` — each side is independently optional.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Padding {
    /// Top padding, in pixels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top: Option<i32>,
    /// Right padding, in pixels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub right: Option<i32>,
    /// Bottom padding, in pixels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bottom: Option<i32>,
    /// Left padding, in pixels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left: Option<i32>,
}

/// The mutable subset of `CellFormat` this crate can set via `repeatCell`.
///
/// Every field is optional and `format.rs::build_fields_mask` names exactly
/// the ones populated — the same discipline `SheetPropertiesUpdate` follows
/// for `updateSheetProperties`.
///
/// Also `Deserialize` (issue #1793): `BooleanRule.format` reads an existing
/// conditional-format rule's format back via `list-conditional-formats`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
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
    /// Rotation of text in the cell.
    #[serde(skip_serializing_if = "Option::is_none", rename = "textRotation")]
    pub text_rotation: Option<TextRotation>,
    /// `"LINKED"` / `"PLAIN_TEXT"`.
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "hyperlinkDisplayType"
    )]
    pub hyperlink_display_type: Option<String>,
    /// Cell padding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub padding: Option<Padding>,
    /// `"LEFT_TO_RIGHT"` / `"RIGHT_TO_LEFT"`.
    #[serde(skip_serializing_if = "Option::is_none", rename = "textDirection")]
    pub text_direction: Option<String>,
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
/// sides `--top`/`--bottom`/`--left`/`--right`/`--all`/
/// `--inner-horizontal`/`--inner-vertical` actually named.
/// `innerHorizontal`/`innerVertical` are the grid lines *between* cells in
/// a multi-cell range, deliberately left out of `--all` since they don't
/// apply to a single-cell range the way the four outer edges do.
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
    /// Horizontal grid lines between rows within the range.
    #[serde(skip_serializing_if = "Option::is_none", rename = "innerHorizontal")]
    pub inner_horizontal: Option<Border>,
    /// Vertical grid lines between columns within the range.
    #[serde(skip_serializing_if = "Option::is_none", rename = "innerVertical")]
    pub inner_vertical: Option<Border>,
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
///
/// Sheets models this as a union: exactly one of `userEnteredValue` (a
/// literal string, parsed against the cell's type at evaluation time) or
/// `relativeDate` (one of Sheets' `RelativeDate` enum strings, used by the
/// date conditions' relative forms) is ever present.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConditionValue {
    /// A literal value, present unless `relative_date` is.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "userEnteredValue"
    )]
    pub user_entered_value: Option<String>,
    /// A `RelativeDate` enum string (`"TODAY"`, `"PAST_WEEK"`, …), present
    /// unless `user_entered_value` is.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "relativeDate"
    )]
    pub relative_date: Option<String>,
}

/// A data validation rule's condition — `DataValidationRule.condition`.
///
/// Only the handful of `type`s `validation.rs` builds are ever constructed
/// here (`ONE_OF_LIST`, `ONE_OF_RANGE`, `NUMBER_BETWEEN`/`NUMBER_NOT_BETWEEN`,
/// the numeric comparators, `TEXT_*`, `DATE_*`, `BLANK`/`NOT_BLANK`,
/// `BOOLEAN`, `CUSTOM_FORMULA`), but the type itself is a plain string so a
/// rule this crate didn't create (read back via `list-protections`' wider
/// fields mask, or a workbook edited outside this tool) still round-trips
/// rather than failing to parse.
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

/// One column's filter criteria within a [`BasicFilter`] or [`FilterView`]
/// (issue #1794).
///
/// **`hiddenValues` only** — the literal "uncheck a value in the dropdown"
/// filter, which is by far the most common real use. Sheets' full
/// `FilterCriteria` also supports a `condition` field carrying the same
/// `BooleanCondition` vocabulary `validation.rs` curates for data
/// validation; that is a documented cut for this issue, not a silent gap —
/// see `filter.rs`'s module docs.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FilterCriteria {
    /// Values to hide in this column.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        rename = "hiddenValues"
    )]
    pub hidden_values: Vec<String>,
}

/// Sort direction within a [`SortSpec`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SortOrder {
    /// Low to high.
    Ascending,
    /// High to low.
    Descending,
}

/// One column's sort order within a [`BasicFilter`] or [`FilterView`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct SortSpec {
    /// The 0-based column index within the sheet — absolute, not relative
    /// to the filter's own range.
    #[serde(rename = "dimensionIndex")]
    pub dimension_index: i64,
    /// The direction to sort that column.
    #[serde(rename = "sortOrder")]
    pub sort_order: SortOrder,
}

/// A sheet's basic filter.
///
/// As `setBasicFilter` builds one and `spreadsheets.get` (with the wider
/// filter-views `fields` mask) reads one back. A sheet has at most one, so
/// `set-basic-filter` is an upsert and `clear-basic-filter` needs only a
/// `sheetId` (see [`ClearBasicFilterRequest`]) — issue #1794.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BasicFilter {
    /// The filtered range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<GridRange>,
    /// Sort order applied on top of the filter, in priority order.
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "sortSpecs")]
    pub sort_specs: Vec<SortSpec>,
    /// Per-column criteria, keyed by the 0-based column index as a string
    /// (the wire format's own map key shape). `BTreeMap`, not `HashMap`, so
    /// serialization is deterministic.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub criteria: BTreeMap<String, FilterCriteria>,
}

/// `SetBasicFilterRequest` — upserts the sheet's basic filter, replacing any
/// existing one wholesale (issue #1794).
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct SetBasicFilterRequest {
    /// The filter to set. `range.sheet_id` selects the target sheet.
    pub filter: BasicFilter,
}

/// `ClearBasicFilterRequest` — a basic filter needs no identifier beyond the
/// sheet, since a sheet has at most one (issue #1794).
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct ClearBasicFilterRequest {
    /// The sheet to clear the basic filter from.
    #[serde(rename = "sheetId")]
    pub sheet_id: i64,
}

/// A named, id-addressed filter view.
///
/// As `addFilterView`/`updateFilterView` build one and `spreadsheets.get`
/// (with the wider filter-views `fields` mask) reads one back (issue
/// #1794). Unlike the basic filter, a sheet may have many;
/// `filter_view_id` is the stable handle `list-filter-views` discovers.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FilterView {
    /// The server-assigned stable id. Absent on an `add-filter-view`
    /// request this crate is building; always present on one read back or
    /// on an `update-filter-view`/`delete-filter-view` request.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "filterViewId"
    )]
    pub filter_view_id: Option<i64>,
    /// A human-readable name for the view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The filtered range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<GridRange>,
    /// Sort order applied on top of the filter, in priority order.
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "sortSpecs")]
    pub sort_specs: Vec<SortSpec>,
    /// Per-column criteria, keyed by the 0-based column index as a string.
    /// See [`BasicFilter::criteria`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub criteria: BTreeMap<String, FilterCriteria>,
}

/// `AddFilterViewRequest`.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct AddFilterViewRequest {
    /// The view to create. `filter_view_id` is left unset — the server
    /// assigns it, only knowable from the reply.
    pub filter: FilterView,
}

/// `UpdateFilterViewRequest`.
///
/// Reuses [`FilterView`] itself, unlike `update-protection`'s
/// `ProtectedRangeUpdate` split: every field here (`title`/`range`/
/// `sortSpecs`/`criteria`) is independently updatable, and `filter_view_id`
/// must be set to select the target, so there is no "response-only field a
/// caller could accidentally overwrite" hazard to guard against.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct UpdateFilterViewRequest {
    /// The properties to write. `filter_view_id` selects the target.
    pub filter: FilterView,
    /// The field mask limiting what this request may change. Sheets
    /// replaces `sortSpecs`/`criteria` wholesale when named, never merging
    /// per-entry — `filter.rs` computes the full resulting state
    /// client-side before sending, the same way `protection.rs` does for
    /// `editors`.
    pub fields: String,
}

/// `DeleteFilterViewRequest`.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct DeleteFilterViewRequest {
    /// Which filter view to remove.
    #[serde(rename = "filterId")]
    pub filter_id: i64,
}

/// A conditional format rule — one entry of a [`Sheet`]'s
/// `conditional_formats` (issue #1793).
///
/// Index-addressed within that list, not id-addressed like
/// [`ProtectedRange`]: `add-conditional-format`/`update-conditional-format`/
/// `delete-conditional-format`'s `--index` refers to this rule's ordinal
/// position, which shifts when an earlier-indexed rule is deleted —
/// `list-conditional-formats` exists to make that position discoverable
/// immediately before acting on it.
///
/// Exactly one of `boolean_rule`/`gradient_rule` is ever present, mirroring
/// the API's own union; this crate never constructs both, but tolerates
/// reading back a rule with neither modelled field set if a future Sheets
/// rule type this crate doesn't build is present.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ConditionalFormatRule {
    /// The range(s) this rule applies to.
    pub ranges: Vec<GridRange>,
    /// A condition-triggered format.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "booleanRule"
    )]
    pub boolean_rule: Option<BooleanRule>,
    /// A color-scale format.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "gradientRule"
    )]
    pub gradient_rule: Option<GradientRule>,
}

/// `BooleanRule` — applies `format` to every cell in the rule's ranges for
/// which `condition` evaluates true.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BooleanRule {
    /// What triggers the format. The same wire shape as
    /// [`DataValidationRule::condition`], but built from
    /// `conditional_format::FormatCondition` rather than `validation::Condition`
    /// — the two surfaces support an overlapping but different set of
    /// condition types (see `conditional_format.rs`'s module doc).
    pub condition: BooleanCondition,
    /// The format to apply when `condition` is true. Reused verbatim from
    /// `format-cells`/`format.rs`.
    pub format: CellFormat,
}

/// `GradientRule` — a three-point color scale. `midpoint` absent means a
/// plain two-color (min/max) scale.
///
/// A documented cut (issue #1793), mirroring `validation.rs`'s "curate
/// rather than chase every enum value" stance: the two endpoints are always
/// anchored `MIN`/`MAX`. Sheets also allows an endpoint to be anchored at an
/// explicit `NUMBER`/`PERCENT`/`PERCENTILE` value, which this crate doesn't
/// build — `docs/drive.md` names this gap.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GradientRule {
    /// The color at the low end of the scale (anchored `MIN`).
    #[serde(rename = "minColorStyle")]
    pub min_color_style: ColorStyle,
    /// The color at an optional midpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub midpoint: Option<InterpolationPoint>,
    /// The color at the high end of the scale (anchored `MAX`).
    #[serde(rename = "maxColorStyle")]
    pub max_color_style: ColorStyle,
}

/// One color/anchor pair of a [`GradientRule`]'s midpoint —
/// `InterpolationPoint`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct InterpolationPoint {
    /// The color at this point.
    #[serde(rename = "colorStyle")]
    pub color_style: ColorStyle,
    /// One of Sheets' `NUMBER`/`PERCENT`/`PERCENTILE` (`MIN`/`MAX` are
    /// reserved for [`GradientRule`]'s fixed endpoints and never appear
    /// here). A plain string, the same tolerate-unmodelled stance as
    /// [`BooleanCondition::condition_type`] — a rule read back with a type
    /// this crate doesn't build still round-trips.
    #[serde(rename = "type")]
    pub point_type: String,
    /// The threshold value, untouched — Sheets parses it at evaluation
    /// time, the same trust-the-caller stance the rest of this module
    /// takes.
    pub value: String,
}

/// `AddConditionalFormatRuleRequest`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AddConditionalFormatRuleRequest {
    /// The rule to add.
    pub rule: ConditionalFormatRule,
    /// Where to insert it — the sheet's current `conditional_formats.len()`
    /// appends.
    pub index: i64,
}

/// `UpdateConditionalFormatRuleRequest`.
///
/// Only the "replace the rule at this index" form is modelled — Sheets also
/// supports moving a rule from `index` to a `newIndex` without changing its
/// content, which this crate doesn't build (a documented cut: the issue
/// asks for position-addressed update/delete, not reordering).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct UpdateConditionalFormatRuleRequest {
    /// The rule's new content.
    pub rule: ConditionalFormatRule,
    /// Which rule, by ordinal position, to replace.
    pub index: i64,
}

/// `DeleteConditionalFormatRuleRequest`.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct DeleteConditionalFormatRuleRequest {
    /// Which sheet the rule belongs to.
    #[serde(rename = "sheetId")]
    pub sheet_id: i64,
    /// Which rule, by ordinal position, to remove.
    pub index: i64,
}

/// A named range, as `addNamedRange` builds one and `spreadsheets.get` (with
/// the wider named-ranges `fields` mask) reads one back.
///
/// Unlike [`ProtectedRange::protected_range_id`] (an `i64`), Sheets assigns
/// `namedRangeId` as a **string** — don't copy the protected-range id type
/// here.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct NamedRange {
    /// The server-assigned stable id. Absent on a request this crate is
    /// building (the server assigns it); always present on one read back.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "namedRangeId"
    )]
    pub named_range_id: Option<String>,
    /// The name, unique workbook-wide.
    #[serde(default)]
    pub name: String,
    /// The range the name refers to. Always present — unlike
    /// [`ProtectedRange::range`], a named range has no "whole workbook"
    /// case, though `range` with every bound unset still covers a whole
    /// sheet.
    #[serde(default)]
    pub range: GridRange,
}

/// `AddNamedRangeRequest`.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct AddNamedRangeRequest {
    /// The name and range to add. `named_range_id` is left unset — the
    /// server assigns it, only knowable from the reply.
    #[serde(rename = "namedRange")]
    pub named_range: NamedRange,
}

/// `UpdateNamedRangeRequest`.
///
/// Reuses [`NamedRange`] directly, unlike `update-protection`'s
/// [`ProtectedRangeUpdate`]: a named range has no field `update-named-range`
/// must exclude — both `name` and `range` are freely re-settable, so there
/// is no risk of a caller accidentally attempting to change something the
/// API forbids changing this way.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct UpdateNamedRangeRequest {
    /// The properties to write. `named_range_id` selects the target.
    #[serde(rename = "namedRange")]
    pub named_range: NamedRange,
    /// The field mask limiting what this request may change.
    pub fields: String,
}

/// `DeleteNamedRangeRequest`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DeleteNamedRangeRequest {
    /// Which named range to remove.
    #[serde(rename = "namedRangeId")]
    pub named_range_id: String,
}

// ── Charts and slicers (issue #1797) ────────────────────────────────────
//
// `ChartSpec` is the largest union in the Sheets API — roughly 15 chart
// types, each with its own nested spec. This crate models only what it
// builds or needs to read back to merge an update: `basicChart` (the
// COLUMN/BAR/LINE/AREA/SCATTER subset `embedded_object.rs` supports) and
// `pieChart`. Every other named field of `ChartSpec`/`BasicChartSpec`/
// `BasicChartAxis`/`BasicChartSeries`/`PieChartSpec` — `titleTextFormat`,
// `backgroundColor`, `hiddenDimensionStrategy`, per-series `styleOverrides`,
// the other 13 chart-type variants (`bubbleChart`, `histogramChart`, …) —
// is **not modelled as a named field**. Instead each of these types carries
// `#[serde(flatten)] extra: BTreeMap<String, serde_json::Value>`, which does
// two jobs at once: reading an existing chart preserves every field this
// crate doesn't touch losslessly (essential for `update-chart`, which sends
// a *whole* replacement spec — see [`UpdateChartSpecRequest`]), and finding
// a key like `"histogramChart"` in `ChartSpec::extra` is exactly how
// `embedded_object.rs::merge_chart_spec` detects an unsupported existing
// chart and refuses rather than silently discarding it.
//
// `BTreeMap`, not `HashMap`, for the same determinism reason `jev`'s wire
// types use it (STYLE, `src/jev/protocol.rs`).
//
// `serde_json::Value` itself derives `Eq` (unlike an `f64` on its own), so
// most of these types derive `Eq` too. The exception is `PieChartSpec`
// (its `pie_hole` is a plain `f64`) and everything that transitively embeds
// it — `ChartSpec`, `EmbeddedChart`, and the requests/replies built from
// them — which stay `PartialEq`-only, the same reason [`Color`] forces
// `PartialEq`-only up through everything that embeds *it*.

/// One chart embedded on a sheet — `EmbeddedChart`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct EmbeddedChart {
    /// The server-assigned stable id. Absent on an `add-chart` request this
    /// crate is building; always present on one read back or on an
    /// `update-chart`/`delete-chart` request.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "chartId")]
    pub chart_id: Option<i64>,
    /// The chart's content and styling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<ChartSpec>,
    /// Where the chart is anchored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<EmbeddedObjectPosition>,
}

/// A chart's content and styling — `ChartSpec`. See the module-level note
/// above for which chart types this crate models and why unmodelled fields
/// round-trip through `extra`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ChartSpec {
    /// The chart's title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The chart's subtitle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    /// A basic (column/bar/line/area/scatter) chart's configuration.
    /// Mutually exclusive with [`Self::pie_chart`] and with every
    /// unmodelled chart-type field that may be present in `extra`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "basicChart"
    )]
    pub basic_chart: Option<BasicChartSpec>,
    /// A pie chart's configuration. Mutually exclusive with
    /// [`Self::basic_chart`].
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "pieChart")]
    pub pie_chart: Option<PieChartSpec>,
    /// Every other `ChartSpec` field this crate doesn't model — including
    /// the other 13 chart-type variants — preserved verbatim across a
    /// read-merge-write cycle. See the module-level note above.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// `BasicChartSpec` — the COLUMN/BAR/LINE/AREA/SCATTER subset this crate
/// supports (issue #1797's chosen v1 cut).
///
/// `COMBO` and `STEPPED_AREA` are also valid wire values for `chartType`,
/// and reading one back is tolerated (it lands here, not in `extra`, since
/// `chartType` is a named field) — but
/// `embedded_object.rs::merge_chart_spec` refuses to *update* one, since
/// `COMBO` needs a per-series `type` this crate doesn't model.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BasicChartSpec {
    /// `"COLUMN"`, `"BAR"`, `"LINE"`, `"AREA"`, `"SCATTER"` (this crate's
    /// supported set), or another wire value read back from an existing
    /// chart this crate didn't create.
    #[serde(rename = "chartType")]
    pub chart_type: String,
    /// Where the legend is drawn, e.g. `"BOTTOM_LEGEND"`, `"NONE"`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "legendPosition"
    )]
    pub legend_position: Option<String>,
    /// `"NOT_STACKED"`, `"STACKED"`, or `"PERCENT_STACKED"`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "stackedType"
    )]
    pub stacked_type: Option<String>,
    /// How many leading rows/columns of the source range are headers rather
    /// than data.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "headerCount"
    )]
    pub header_count: Option<i64>,
    /// The chart's axes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub axis: Vec<BasicChartAxis>,
    /// The domain (typically the x-axis / category column).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub domains: Vec<BasicChartDomain>,
    /// The data series.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub series: Vec<BasicChartSeries>,
    /// Every other `BasicChartSpec` field this crate doesn't model
    /// (`threeDimensional`, `interpolateNulls`, `lineSmoothing`, …).
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// One axis of a [`BasicChartSpec`] — `BasicChartAxis`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BasicChartAxis {
    /// Which physical axis this is — `"BOTTOM_AXIS"`, `"LEFT_AXIS"`, or
    /// `"RIGHT_AXIS"`. `--horizontal-axis-title`/`--vertical-axis-title`
    /// map to `BOTTOM_AXIS`/`LEFT_AXIS` literally — the physical axis, not
    /// the domain/series role, since a BAR chart's domain axis is
    /// `LEFT_AXIS`.
    pub position: String,
    /// The axis title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Every other `BasicChartAxis` field this crate doesn't model
    /// (`format`, `viewWindowOptions`, …).
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// One domain of a [`BasicChartSpec`] — `BasicChartDomain`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BasicChartDomain {
    /// The domain's source data.
    pub domain: ChartData,
    /// Every other `BasicChartDomain` field this crate doesn't model
    /// (`reversed`).
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// One series of a [`BasicChartSpec`] — `BasicChartSeries`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BasicChartSeries {
    /// The series' source data.
    pub series: ChartData,
    /// Which axis (by position) this series plots against — for a chart
    /// with more than one axis on the same side.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "targetAxis"
    )]
    pub target_axis: Option<String>,
    /// Every other `BasicChartSeries` field this crate doesn't model
    /// (`type` — COMBO-only, `color`, `lineStyle`, `styleOverrides`, …).
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// A pie chart's configuration — `PieChartSpec`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PieChartSpec {
    /// The category labels (the pie's slices).
    pub domain: ChartData,
    /// The values (the slices' sizes).
    pub series: ChartData,
    /// Where the legend is drawn.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "legendPosition"
    )]
    pub legend_position: Option<String>,
    /// `0.0`-`1.0`: the radius of the center hole, as a fraction of the
    /// pie's radius. `0.0` (or absent) is a solid pie; anything above `0`
    /// is a donut.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "pieHole")]
    pub pie_hole: Option<f64>,
    /// Every other `PieChartSpec` field this crate doesn't model
    /// (`threeDimensional`).
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// One chart series or domain's source data — `ChartData`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChartData {
    /// The cell range(s) this series/domain reads from.
    #[serde(rename = "sourceRange")]
    pub source_range: ChartSourceRange,
    /// Every other `ChartData` field this crate doesn't model
    /// (`aggregateType`, `groupRule`, `columnReference`).
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// The cell range(s) backing one [`ChartData`] — `ChartSourceRange`.
///
/// A `Vec` on the wire (Sheets allows discontiguous sources for one
/// series), though this crate's builders only ever populate one.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChartSourceRange {
    /// The range(s).
    pub sources: Vec<GridRange>,
}

/// Where an [`EmbeddedChart`] or [`Slicer`] is anchored —
/// `EmbeddedObjectPosition`.
///
/// A three-way union on the wire: exactly one of `overlay_position`/
/// `new_sheet` is set on a chart this crate builds (a slicer can only ever
/// be `overlay_position` — the API has no `newSheet`/own-`sheetId` slicer
/// placement).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EmbeddedObjectPosition {
    /// The chart occupies its own sheet, whose id this names. Read-back
    /// only — this crate never sets it directly; use `new_sheet` to
    /// request one.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "sheetId")]
    pub sheet_id: Option<i64>,
    /// The object floats over an existing sheet, anchored to a cell.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "overlayPosition"
    )]
    pub overlay_position: Option<OverlayPosition>,
    /// Request a brand-new sheet to hold this chart alone. Write-only
    /// (`--new-sheet`); the server never echoes `true` back — a
    /// server-created chart sheet reads back with `sheet_id` set instead.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "newSheet")]
    pub new_sheet: Option<bool>,
}

/// An object floating over a sheet, anchored to a cell — `OverlayPosition`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OverlayPosition {
    /// The cell the object's top-left corner is anchored to.
    #[serde(rename = "anchorCell")]
    pub anchor_cell: GridCoordinate,
    /// Additional horizontal offset from the anchor cell, in pixels.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "offsetXPixels"
    )]
    pub offset_x_pixels: Option<i64>,
    /// Additional vertical offset from the anchor cell, in pixels.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "offsetYPixels"
    )]
    pub offset_y_pixels: Option<i64>,
    /// The object's width in pixels. Absent means the API's own default.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "widthPixels"
    )]
    pub width_pixels: Option<i64>,
    /// The object's height in pixels. Absent means the API's own default.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "heightPixels"
    )]
    pub height_pixels: Option<i64>,
}

/// A single grid cell, addressed by sheet and 0-based row/column —
/// `GridCoordinate`. Distinct from [`GridRange`] (a rectangular span): an
/// anchor is one cell, never a range.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GridCoordinate {
    /// The sheet this coordinate is on.
    #[serde(rename = "sheetId")]
    pub sheet_id: i64,
    /// 0-based row.
    #[serde(rename = "rowIndex")]
    pub row_index: i64,
    /// 0-based column.
    #[serde(rename = "columnIndex")]
    pub column_index: i64,
}

/// A slicer embedded on a sheet — `Slicer`.
///
/// Filters an existing range or pivot table interactively, the same
/// `FilterCriteria` vocabulary `filter.rs` curates (`hiddenValues` only —
/// see that module's doc comment for the condition-based cut this crate
/// makes uniformly).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Slicer {
    /// The server-assigned stable id. Absent on an `add-slicer` request
    /// this crate is building; always present on one read back or on an
    /// `update-slicer`/`delete-slicer` request.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "slicerId")]
    pub slicer_id: Option<i64>,
    /// The slicer's filtering configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<SlicerSpec>,
    /// Where the slicer is anchored. Always `overlay_position` — a slicer
    /// cannot occupy its own sheet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<EmbeddedObjectPosition>,
}

/// A slicer's filtering configuration — `SlicerSpec`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SlicerSpec {
    /// The range the slicer filters.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "dataRange")]
    pub data_range: Option<GridRange>,
    /// Which values to hide, within `column_index`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "filterCriteria"
    )]
    pub filter_criteria: Option<FilterCriteria>,
    /// The column within `data_range` the criteria apply to. See
    /// `embedded_object.rs`'s `--column` doc comment for the
    /// absolute-vs-relative indexing this needs to be verified against a
    /// live account.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "columnIndex"
    )]
    pub column_index: Option<i64>,
    /// Whether this slicer also filters pivot tables built from
    /// `data_range`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "applyToPivotTables"
    )]
    pub apply_to_pivot_tables: Option<bool>,
    /// A human-readable name for the slicer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Every other `SlicerSpec` field this crate doesn't model
    /// (`textFormat`, `backgroundColor`, `horizontalAlignment`, …).
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// `AddChartRequest`.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct AddChartRequest {
    /// The chart to create. `chart_id` is left unset — the server assigns
    /// it, only knowable from the reply.
    pub chart: EmbeddedChart,
}

/// `UpdateChartSpecRequest`.
///
/// **No `fields` mask** — unlike every other update request in this file,
/// Sheets' `updateChartSpec` replaces the chart's entire `ChartSpec`
/// wholesale; there is no way to name "just the title". `embedded_object.rs`
/// fetches the existing spec, merges the caller's flags onto it (preserving
/// every unmodelled field via `extra`), and sends the full result here —
/// the same reason `update-filter-view` computes a full `sortSpecs`/
/// `criteria` client-side, except here it isn't optional: there is no
/// narrower request to fall back to.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct UpdateChartSpecRequest {
    /// Which chart to update.
    #[serde(rename = "chartId")]
    pub chart_id: i64,
    /// The chart's new, complete spec.
    pub spec: ChartSpec,
}

/// `AddSlicerRequest`.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct AddSlicerRequest {
    /// The slicer to create. `slicer_id` is left unset — the server assigns
    /// it, only knowable from the reply.
    pub slicer: Slicer,
}

/// `UpdateSlicerSpecRequest`.
///
/// Unlike [`UpdateChartSpecRequest`], this request **does** carry a field
/// mask — `updateSlicerSpec` supports partial updates, so
/// `embedded_object.rs` only ever names the fields the caller actually set.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct UpdateSlicerSpecRequest {
    /// Which slicer to update.
    #[serde(rename = "slicerId")]
    pub slicer_id: i64,
    /// The fields to write. `data_range`/`title`/`apply_to_pivot_tables`
    /// are independently settable; `filter_criteria` and `column_index`
    /// travel together (a criteria value is meaningless without knowing
    /// which column it filters).
    pub spec: SlicerSpec,
    /// The field mask limiting what this request may change.
    pub fields: String,
}

/// `DeleteEmbeddedObjectRequest` — removes a chart or a slicer; the API
/// gives no way to name which kind `object_id` refers to, and none is
/// needed (issue #1797).
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct DeleteEmbeddedObjectRequest {
    /// Which chart or slicer to remove.
    #[serde(rename = "objectId")]
    pub object_id: i64,
}

/// `UpdateEmbeddedObjectPositionRequest` — moves and/or resizes an existing
/// chart or slicer (`move-chart`/`move-slicer`, issue #1837).
///
/// **The `fields` mask is rooted at `newPosition.overlayPosition`, not at
/// `newPosition`** — the API's own rule ("the root `newPosition.overlayPosition`
/// is implied and should not be specified"), and the one place this request
/// differs from every other masked request in this file, where the mask is
/// rooted at the request's own payload field (cf. [`UpdateSlicerSpecRequest`],
/// whose mask is rooted at `spec`). So a move sends `anchorCell`, never
/// `overlayPosition.anchorCell`. The mask is used **only** when
/// `new_position.overlay_position` is set — a `newSheet` move sends no
/// `fields` at all (`skip_serializing_if = "String::is_empty"`), since there
/// is nothing under `overlayPosition` to name.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct UpdateEmbeddedObjectPositionRequest {
    /// Which chart or slicer to move.
    #[serde(rename = "objectId")]
    pub object_id: i64,
    /// The position to write.
    #[serde(rename = "newPosition")]
    pub new_position: EmbeddedObjectPosition,
    /// The field mask limiting what this request may change, relative to
    /// `newPosition.overlayPosition`. Empty (and omitted on the wire) for a
    /// `--new-sheet` move.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub fields: String,
}

/// `updateCells` — issue #1798's `add-pivot-table`/`delete-pivot-table`,
/// the crate's only user of this request (see
/// [`BatchUpdateRequestItem::UpdateCells`]'s doc comment).
///
/// Always exactly one row of one cell: a pivot table has exactly one
/// anchor, so `rows` is a single-element `Vec` rather than a genuine grid
/// — kept as `Vec` because that is the wire shape, not because this crate
/// ever sends more than one.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct UpdateCellsRequest {
    /// The anchor cell.
    pub start: GridCoordinate,
    /// Always one [`PivotRowData`] of one [`PivotCellData`].
    pub rows: Vec<PivotRowData>,
    /// Always the literal `"pivotTable"` — the field mask that makes this
    /// request touch nothing but the anchor's `pivotTable` property,
    /// preserving whatever value or formatting the cell already had.
    pub fields: String,
}

/// One row of [`UpdateCellsRequest::rows`].
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct PivotRowData {
    /// The row's cells.
    pub values: Vec<PivotCellData>,
}

/// The `cell` payload of an `updateCells` request — deliberately **has no
/// `userEnteredValue` field**.
///
/// Same guarantee [`RepeatCellData`]'s doc comment describes: there is
/// nowhere on this type to put a literal value, so `add-pivot-table`
/// cannot be used to write one regardless of what a caller asks for.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct PivotCellData {
    /// The pivot table to write, or `None` to clear an existing one
    /// (`delete-pivot-table`) — serializes as `{}` when absent, which is
    /// what `fields: "pivotTable"` interprets as "clear this property".
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "pivotTable"
    )]
    pub pivot_table: Option<PivotTable>,
}

/// A pivot table definition — `PivotTable`.
///
/// Also `Deserialize`: unlike most other request-only pieces in this
/// file, a pivot table is read back too, via
/// [`CellSnapshot::pivot_table`], so `list-pivot-tables` and
/// `delete-pivot-table`'s "currently:" preview can round-trip one.
///
/// **A curated surface, not full API coverage** — mirroring
/// `conditional_format.rs`'s own stance. Not modelled: the deprecated
/// `criteria` field (`filterSpecs` replaces it), `PivotGroup.groupRule`
/// (date/number bucketing), `PivotGroup.valueBucket`/`valueMetadata`
/// (sort-by-value-column and collapsed-group state), data-source pivots
/// (`dataSourceId`/`dataExecutionStatus`), and
/// `PivotValue.calculatedDisplayType`. `docs/drive.md` names each gap.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PivotTable {
    /// The range this pivot table is built from.
    pub source: GridRange,
    /// Row grouping(s), outermost first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rows: Vec<PivotGroup>,
    /// Column grouping(s), outermost first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub columns: Vec<PivotGroup>,
    /// Aggregated value column(s).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<PivotValue>,
    /// Source-row filters, by column offset.
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "filterSpecs")]
    pub filter_specs: Vec<PivotFilterSpec>,
    /// `"HORIZONTAL"` (values as columns, the API default) or
    /// `"VERTICAL"` (values as rows). Absent means the API default.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "valueLayout"
    )]
    pub value_layout: Option<String>,
}

/// One row or column grouping of a [`PivotTable`] — `PivotGroup`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PivotGroup {
    /// Which source column this groups by, 0-based and relative to
    /// [`PivotTable::source`]'s first column — not an absolute sheet
    /// column index.
    #[serde(rename = "sourceColumnOffset")]
    pub source_column_offset: i64,
    /// Whether to show a totals row/column for this grouping. Always sent
    /// explicitly (never omitted) since the API's own default (`true`)
    /// would otherwise silently differ from a caller's expectation.
    #[serde(rename = "showTotals")]
    pub show_totals: bool,
    /// `"ASCENDING"` or `"DESCENDING"`. Absent means the API default
    /// (source order).
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "sortOrder")]
    pub sort_order: Option<String>,
}

/// One aggregated value column of a [`PivotTable`] — `PivotValue`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PivotValue {
    /// Which source column this aggregates, 0-based and relative to
    /// [`PivotTable::source`]'s first column.
    #[serde(rename = "sourceColumnOffset")]
    pub source_column_offset: i64,
    /// One of Sheets' `SUM`/`COUNTA`/`COUNT`/`COUNTUNIQUE`/`AVERAGE`/
    /// `MAX`/`MIN`/`MEDIAN`/`PRODUCT`/`STDEV`/`STDEVP`/`VAR`/`VARP`
    /// (`CUSTOM`, a value driven by a formula rather than a source column,
    /// is a documented cut). A plain string, the same tolerate-unmodelled
    /// stance as [`InterpolationPoint::point_type`].
    #[serde(rename = "summarizeFunction")]
    pub summarize_function: String,
    /// An optional display name overriding the default
    /// (`"<function> of <column>"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// A source-row filter on one column of a [`PivotTable`] —
/// `PivotFilterSpec`.
///
/// Only the `filterCriteria` form is modelled (a fixed allow-list of
/// values); the newer `dataSourceColumnReference`, meaningful only for
/// data-source pivots, is a documented cut alongside them.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PivotFilterSpec {
    /// Which source column this filters, 0-based and relative to
    /// [`PivotTable::source`]'s first column.
    #[serde(rename = "columnOffsetIndex")]
    pub column_offset_index: i64,
    /// The filter itself.
    #[serde(rename = "filterCriteria")]
    pub filter_criteria: PivotFilterCriteria,
}

/// [`PivotFilterSpec`]'s filter.
///
/// Only `visibleValues` (a fixed allow-list of raw string values) is
/// modelled; the API's condition-based form (`BooleanCondition`,
/// mirroring data validation) is a documented cut.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PivotFilterCriteria {
    /// The raw source values that pass this filter.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub visible_values: Vec<String>,
}

/// One row-major chunk of a sheet's cell data — `GridData`.
///
/// The API chunks rather than returning one flat grid because a `fields`
/// mask can request disjoint ranges in one call; this crate only ever
/// requests one contiguous range at a time, so `Sheet::data` is a
/// single-element `Vec` in practice, but the type stays a `Vec` because
/// that is the wire shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GridData {
    /// The zero-based row this chunk starts at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_row: Option<i64>,
    /// The zero-based column this chunk starts at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_column: Option<i64>,
    /// The rows themselves.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub row_data: Vec<RowData>,
}

/// One row of [`GridData::row_data`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RowData {
    /// The row's cells, in the `fields` mask the caller requested — every
    /// property not asked for is simply absent, even one that exists on
    /// the real cell.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<CellSnapshot>,
}

/// One cell, read back only for the properties a caller's `fields` mask
/// requested — `CellSnapshot`.
///
/// Distinct from [`PivotCellData`] (the *write*-side, value-incapable
/// type) precisely so this read-only type can carry
/// [`Self::formatted_value`] without ever creating a path for a literal
/// value to be written back through it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CellSnapshot {
    /// The pivot table anchored at this cell, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pivot_table: Option<PivotTable>,
    /// The cell's value as displayed in the UI — used only to describe an
    /// occupied anchor in a refusal/dry-run message, never parsed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub formatted_value: Option<String>,
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

/// `MoveDimensionRequest` — moves the rows or columns spanned by `source` to
/// a new position within the same sheet (`move-rows`/`move-columns`, issue
/// #1834).
///
/// Nothing is discarded: the moved rows/columns take their values with them
/// and the rows/columns between the source and the destination shift to
/// close the gap. Reuses [`DimensionRange`] as-is, exactly as
/// [`DeleteDimensionRequest`] does.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MoveDimensionRequest {
    /// The rows or columns to move.
    pub source: DimensionRange,
    /// The zero-based index the source is moved *in front of*, in the
    /// sheet's coordinates **before** the source is removed from the grid —
    /// the API's own definition, so a downward move lands `source.len()`
    /// short of this number. The CLI's 1-based `--before` is converted in
    /// `structure.rs::move_destination_index`, never at a call site.
    #[serde(rename = "destinationIndex")]
    pub destination_index: i64,
}

/// Which axis a range operation shifts cells along.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum ShiftDimension {
    /// Cells below a range shift up after deletion or down after insertion.
    #[serde(rename = "ROWS")]
    Rows,
    /// Cells to the right of a range shift left after deletion or right after
    /// insertion.
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

/// `InsertRangeRequest` — inserts empty cells into `range`, shifting existing
/// cells along `shift_dimension` to make room.
///
/// Cells shifted past the sheet's grid extent are dropped by the Sheets API.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct InsertRangeRequest {
    /// The empty cells to insert.
    pub range: GridRange,
    /// Which way existing cells shift to make room.
    #[serde(rename = "shiftDimension")]
    pub shift_dimension: ShiftDimension,
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

/// A banded range — alternating row or column colors applied to a range
/// (issue #1832, [ADR-0082](../../../docs/adrs/adr-0082-banded-ranges.md)).
///
/// v1 sets at most one of `row_properties`/`column_properties` per
/// `add-banding`/`update-banding` call, selected by `--axis`; the API
/// itself allows both on the same range simultaneously (a checkerboard
/// effect), which this crate does not expose — a documented cut, not a
/// silent gap.
///
/// `PartialEq`-only, not `Eq`: embeds [`ColorStyle`], which wraps the
/// `f32`-based [`Color`] — see [`Color`]'s own doc comment.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BandedRange {
    /// The server-assigned stable id. Absent on an `add-banding` request
    /// this crate is building; always present on one read back or on an
    /// `update-banding`/`delete-banding` request.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "bandedRangeId"
    )]
    pub banded_range_id: Option<i64>,
    /// The banded range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<GridRange>,
    /// Row-banding colors, when banding alternates by row.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "rowProperties"
    )]
    pub row_properties: Option<BandingProperties>,
    /// Column-banding colors, when banding alternates by column.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "columnProperties"
    )]
    pub column_properties: Option<BandingProperties>,
}

/// The colors of one axis of a [`BandedRange`] — `BandingProperties`.
///
/// Only the modern `*ColorStyle` fields are modelled, matching
/// `format-cells`'s precedent verbatim: the API's plain `headerColor`/
/// `firstBandColor`/`secondBandColor`/`footerColor` fields are deprecated
/// in favor of their `*ColorStyle` counterparts, which take precedence when
/// both are set, so the plain fields are never worth sending. Within each
/// `ColorStyle`, only the `rgbColor` arm is modelled — see [`ColorStyle`]'s
/// own doc comment.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BandingProperties {
    /// The header row/column's color, if distinct from the alternating
    /// bands.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "headerColorStyle"
    )]
    pub header_color_style: Option<ColorStyle>,
    /// The first band's color.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "firstBandColorStyle"
    )]
    pub first_band_color_style: Option<ColorStyle>,
    /// The second (alternating) band's color.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "secondBandColorStyle"
    )]
    pub second_band_color_style: Option<ColorStyle>,
    /// The footer row/column's color, if distinct from the alternating
    /// bands.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "footerColorStyle"
    )]
    pub footer_color_style: Option<ColorStyle>,
}

/// `AddBandingRequest`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AddBandingRequest {
    /// The banded range to create. `banded_range_id` is left unset — the
    /// server assigns it, only knowable from the reply.
    #[serde(rename = "bandedRange")]
    pub banded_range: BandedRange,
}

/// `UpdateBandingRequest`.
///
/// Reuses [`BandedRange`] itself, like `UpdateFilterViewRequest`: every
/// field here (`range`/`rowProperties`/`columnProperties`) is
/// independently updatable, and `banded_range_id` must be set to select the
/// target.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct UpdateBandingRequest {
    /// The properties to write. `banded_range_id` selects the target.
    #[serde(rename = "bandedRange")]
    pub banded_range: BandedRange,
    /// The field mask limiting what this request may change.
    pub fields: String,
}

/// `DeleteBandingRequest`.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct DeleteBandingRequest {
    /// Which banded range to remove.
    #[serde(rename = "bandedRangeId")]
    pub banded_range_id: i64,
}

/// `AddDimensionGroupRequest`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AddDimensionGroupRequest {
    /// The span to create a group over. The server derives the new
    /// group's `depth` from this; see [`DimensionGroup`]'s doc comment.
    pub range: DimensionRange,
}

/// `UpdateDimensionGroupRequest`.
///
/// `collapsed` is the only field this crate ever changes — `range` is the
/// group's identity and `depth` is server-derived, so both are always sent
/// unchanged from the group `update-dimension-group` resolved against,
/// with `fields` naming only `"collapsed"`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct UpdateDimensionGroupRequest {
    /// The group to update, identified by its (unchanged) `range` and
    /// `depth`, carrying the new `collapsed` value.
    #[serde(rename = "dimensionGroup")]
    pub dimension_group: DimensionGroup,
    /// The field mask limiting what this request may change. Always
    /// `"collapsed"` — the only field [`Self::dimension_group`] ever
    /// updates.
    pub fields: String,
}

/// `DeleteDimensionGroupRequest`.
///
/// Addressed by `range` alone, like the API — `depth` plays no part in
/// which group is removed. See the `dimension_group.rs` module doc for why
/// this crate requires an **exact** range match rather than exposing the
/// API's partial-overlap decrement semantics.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DeleteDimensionGroupRequest {
    /// The span whose group should be removed.
    pub range: DimensionRange,
}

/// Response to `spreadsheets.batchUpdate`.
///
/// Only `replies` is modelled, and only the `addSheet` arm of it: the new
/// sheet's server-assigned `sheetId` is the one fact the response carries
/// that the request did not already know, and the request log records it.
///
/// `PartialEq`-only, not `Eq` since #1797 — see [`BatchUpdateReply`]'s doc
/// comment.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct BatchUpdateResponse {
    /// One reply per request, in request order. Replies for requests with
    /// nothing to report are empty objects, not omitted.
    #[serde(default)]
    pub replies: Vec<BatchUpdateReply>,
}

/// One reply within a [`BatchUpdateResponse`].
///
/// `PartialEq`-only, not `Eq` (issue #1797): [`AddChartReply`] embeds a
/// [`ChartSpec`], which can carry a `PieChartSpec::pie_hole` (`f64`) — the
/// same reason [`Spreadsheet`] lost `Eq` in #1793.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
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
    /// Present only for an `addFilterView` request — carries the
    /// server-assigned `filterViewId`, only knowable from the reply
    /// (issue #1794).
    #[serde(default, rename = "addFilterView")]
    pub add_filter_view: Option<AddFilterViewReply>,
    /// Present only for an `addNamedRange` request — carries the
    /// server-assigned `namedRangeId`, only knowable from the reply.
    #[serde(default, rename = "addNamedRange")]
    pub add_named_range: Option<AddNamedRangeReply>,
    /// Present only for an `addChart` request — carries the server-assigned
    /// `chartId`, only knowable from the reply (issue #1797).
    #[serde(default, rename = "addChart")]
    pub add_chart: Option<AddChartReply>,
    /// Present only for an `addSlicer` request — carries the
    /// server-assigned `slicerId`, only knowable from the reply
    /// (issue #1797).
    #[serde(default, rename = "addSlicer")]
    pub add_slicer: Option<AddSlicerReply>,
    /// Present only for an `addBanding` request — carries the
    /// server-assigned `bandedRangeId`, only knowable from the reply
    /// (issue #1832).
    #[serde(default, rename = "addBanding")]
    pub add_banding: Option<AddBandingReply>,
}

/// The `addProtectedRange` arm of a [`BatchUpdateReply`].
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct AddProtectedRangeReply {
    /// The created protection, including its assigned `protectedRangeId`.
    #[serde(default, rename = "protectedRange")]
    pub protected_range: Option<ProtectedRange>,
}

/// The `addFilterView` arm of a [`BatchUpdateReply`] (issue #1794).
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct AddFilterViewReply {
    /// The created view, including its assigned `filterViewId`.
    #[serde(default)]
    pub filter: Option<FilterView>,
}

/// The `addNamedRange` arm of a [`BatchUpdateReply`].
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct AddNamedRangeReply {
    /// The created named range, including its assigned `namedRangeId`.
    #[serde(default, rename = "namedRange")]
    pub named_range: Option<NamedRange>,
}

/// The `addChart` arm of a [`BatchUpdateReply`] (issue #1797).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct AddChartReply {
    /// The created chart, including its assigned `chartId`.
    #[serde(default)]
    pub chart: Option<EmbeddedChart>,
}

/// The `addSlicer` arm of a [`BatchUpdateReply`] (issue #1797).
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct AddSlicerReply {
    /// The created slicer, including its assigned `slicerId`.
    #[serde(default)]
    pub slicer: Option<Slicer>,
}

/// The `addBanding` arm of a [`BatchUpdateReply`] (issue #1832).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct AddBandingReply {
    /// The created banded range, including its assigned `bandedRangeId`.
    #[serde(default, rename = "bandedRange")]
    pub banded_range: Option<BandedRange>,
}

/// The `addSheet`/`duplicateSheet` arm of a [`BatchUpdateReply`].
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct AddSheetReply {
    /// The created sheet's properties, including its assigned `sheetId`.
    #[serde(default)]
    pub properties: Option<SheetProperties>,
}

/// The wire literal every developer-metadata request this crate sends
/// carries as `visibility` — issue #1795,
/// [ADR-0081](../../../docs/adrs/adr-0081.md) §4.
///
/// `PROJECT`-visibility metadata belongs to whatever OAuth client created
/// it and is never this tool's to read or write. `developer_metadata.rs`
/// is the only module that constructs this, and it never reads it from a
/// flag — there is no `--visibility` option on any of the three CLI
/// verbs, so no user input can reach anything but this literal.
pub const DOCUMENT_VISIBILITY: &str = "DOCUMENT";

/// Where a developer-metadata entry is attached — `DeveloperMetadataLocation`.
///
/// Sheets models this as a union of `spreadsheet`/`sheetId`/
/// `dimensionRange`; exactly one is set by the one function that builds it
/// (`developer_metadata.rs::resolve_location`), matching the
/// [`ConditionValue`] convention for unions elsewhere in this module —
/// discipline enforced by the builder, not the type.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeveloperMetadataLocation {
    /// `true` when this entry is attached to the whole spreadsheet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spreadsheet: Option<bool>,
    /// Set when attached to a whole sheet.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "sheetId")]
    pub sheet_id: Option<i64>,
    /// Set when attached to a row or column span.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "dimensionRange"
    )]
    pub dimension_range: Option<DimensionRange>,
}

impl DeveloperMetadataLocation {
    /// A location spanning the whole spreadsheet.
    #[must_use]
    pub fn spreadsheet() -> Self {
        Self {
            spreadsheet: Some(true),
            ..Self::default()
        }
    }

    /// A location spanning one whole sheet.
    #[must_use]
    pub fn sheet(sheet_id: i64) -> Self {
        Self {
            sheet_id: Some(sheet_id),
            ..Self::default()
        }
    }

    /// A location spanning a row or column range.
    #[must_use]
    pub fn dimension(range: DimensionRange) -> Self {
        Self {
            dimension_range: Some(range),
            ..Self::default()
        }
    }
}

/// The developer-metadata fields settable on create —
/// `CreateDeveloperMetadataRequest.developerMetadata`.
///
/// No `metadata_id` field (server-assigned) and `visibility` is always
/// [`DOCUMENT_VISIBILITY`] — see that constant's doc comment.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct NewDeveloperMetadata {
    /// The key this entry is looked up by.
    #[serde(rename = "metadataKey")]
    pub metadata_key: String,
    /// The value stored under `metadata_key`.
    #[serde(rename = "metadataValue")]
    pub metadata_value: String,
    /// Where this entry is attached.
    pub location: DeveloperMetadataLocation,
    /// Always [`DOCUMENT_VISIBILITY`].
    pub visibility: String,
}

/// `CreateDeveloperMetadataRequest`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CreateDeveloperMetadataRequest {
    /// The entry to create.
    #[serde(rename = "developerMetadata")]
    pub developer_metadata: NewDeveloperMetadata,
}

/// The mutable subset of an existing entry `set-developer-metadata` may
/// change on update — currently only its value.
///
/// Sheets' own `DeveloperMetadata` can also change `metadataKey`/
/// `location`/`visibility` via `update`, but this surface never offers
/// that: the key and location are how the entry was found, so changing
/// them out from under the same request would silently repoint a
/// different caller's lookup.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DeveloperMetadataValueUpdate {
    /// The new value.
    #[serde(rename = "metadataValue")]
    pub metadata_value: String,
}

/// `UpdateDeveloperMetadataRequest`.
///
/// Applies to every entry matched by `data_filters` — Sheets bulk-updates
/// by filter, not by id, which is what lets `set-developer-metadata`
/// resolve "does this key already exist here" and "update it" with the
/// same filter, no id round-trip needed.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct UpdateDeveloperMetadataRequest {
    /// Selects which existing entries this updates.
    #[serde(rename = "dataFilters")]
    pub data_filters: Vec<DataFilter>,
    /// The new value.
    #[serde(rename = "developerMetadata")]
    pub developer_metadata: DeveloperMetadataValueUpdate,
    /// The field mask limiting what this request may change. Always
    /// `"metadataValue"` — the only field [`DeveloperMetadataValueUpdate`]
    /// models.
    pub fields: String,
}

/// `DeleteDeveloperMetadataRequest`.
///
/// **Singular** `dataFilter`, matching the real API — one filter can match
/// and delete several entries in a single request, which is why
/// `delete-developer-metadata`'s preview lists every match rather than
/// assuming exactly one.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DeleteDeveloperMetadataRequest {
    /// Selects which existing entries this deletes.
    #[serde(rename = "dataFilter")]
    pub data_filter: DataFilter,
}

/// A lookup by key and/or location, restricted (by every caller in this
/// crate) to [`DOCUMENT_VISIBILITY`] — the `developerMetadataLookup` arm of
/// a [`DataFilter`].
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct DeveloperMetadataLookupFilter {
    /// Restricts the match to one location, when given.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "metadataLocation"
    )]
    pub metadata_location: Option<DeveloperMetadataLocation>,
    /// Restricts the match to one key, when given.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "metadataKey"
    )]
    pub metadata_key: Option<String>,
    /// Always `Some(DOCUMENT_VISIBILITY)` — every filter this crate builds
    /// restricts to document-visibility metadata; see that constant's doc
    /// comment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    /// `"EXACT_LOCATION"` when `metadata_location` is given (this crate
    /// never sends `"INTERSECTING_LOCATION"`, which would also match a
    /// dimension range straddling the requested one).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "locationMatchingStrategy"
    )]
    pub location_matching_strategy: Option<String>,
}

/// `DataFilter`, restricted to the one shape this crate ever sends: a
/// `developerMetadataLookup`.
///
/// Sheets' `DataFilter` also has `a1Range`/`gridRange` alternatives, never
/// constructed here — the same curated, not full-coverage, stance as
/// [`BooleanCondition`]'s doc comment.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DataFilter {
    /// The lookup this filter applies.
    #[serde(rename = "developerMetadataLookup")]
    pub developer_metadata_lookup: DeveloperMetadataLookupFilter,
}

/// One developer-metadata entry as read back from the server —
/// `DeveloperMetadata`, the response/search shape (has an id; the create
/// payload, [`NewDeveloperMetadata`], does not).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct DeveloperMetadata {
    /// The server-assigned id.
    #[serde(default, rename = "metadataId")]
    pub metadata_id: i64,
    /// The key this entry is looked up by.
    #[serde(default, rename = "metadataKey")]
    pub metadata_key: String,
    /// The value stored under `metadata_key`.
    #[serde(default, rename = "metadataValue")]
    pub metadata_value: String,
    /// Where this entry is attached.
    #[serde(default)]
    pub location: DeveloperMetadataLocation,
    /// The entry's visibility. `developer_metadata.rs` asserts this is
    /// always [`DOCUMENT_VISIBILITY`] on every entry it reads — the
    /// defense-in-depth half of the DOCUMENT-only guarantee, on top of
    /// every request already restricting the search itself (issue #1795,
    /// ADR-0081 §4).
    #[serde(default)]
    pub visibility: String,
}

/// Body of `spreadsheets.developerMetadata:search`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SearchDeveloperMetadataRequest {
    /// Every filter must match (Sheets ANDs a multi-entry list); this crate
    /// always sends exactly one.
    #[serde(rename = "dataFilters")]
    pub data_filters: Vec<DataFilter>,
}

/// Response to `spreadsheets.developerMetadata:search`.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct SearchDeveloperMetadataResponse {
    /// Every entry that matched, unwrapped from its `MatchedDeveloperMetadata`
    /// wrapper (the echoed `dataFilters` that wrapper also carries are not
    /// modelled — this crate already knows which filter it sent).
    #[serde(default, rename = "matchedDeveloperMetadata")]
    pub matched_developer_metadata: Vec<MatchedDeveloperMetadata>,
}

/// One match within a [`SearchDeveloperMetadataResponse`].
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct MatchedDeveloperMetadata {
    /// The matched entry.
    #[serde(rename = "developerMetadata")]
    pub developer_metadata: DeveloperMetadata,
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
    fn recalculation_interval_as_str_matches_the_wire_spelling() {
        assert_eq!(
            RecalculationInterval::Unspecified.as_str(),
            "RECALCULATION_INTERVAL_UNSPECIFIED"
        );
        assert_eq!(RecalculationInterval::OnChange.as_str(), "ON_CHANGE");
        assert_eq!(RecalculationInterval::Minute.as_str(), "MINUTE");
        assert_eq!(RecalculationInterval::Hour.as_str(), "HOUR");
    }

    #[test]
    fn iterative_calculation_settings_describe_bounds_covers_every_combination() {
        assert_eq!(
            IterativeCalculationSettings {
                max_iterations: Some(50),
                convergence_threshold: Some(0.01),
            }
            .describe_bounds(),
            " (max 50 iterations, threshold 0.01)"
        );
        assert_eq!(
            IterativeCalculationSettings {
                max_iterations: Some(50),
                convergence_threshold: None,
            }
            .describe_bounds(),
            " (max 50 iterations)"
        );
        assert_eq!(
            IterativeCalculationSettings {
                max_iterations: None,
                convergence_threshold: Some(0.01),
            }
            .describe_bounds(),
            " (threshold 0.01)"
        );
        assert_eq!(
            IterativeCalculationSettings::default().describe_bounds(),
            ""
        );
    }

    #[test]
    fn insert_range_serializes_the_column_shift_direction() {
        let request = BatchUpdateRequestItem::InsertRange(InsertRangeRequest {
            range: GridRange {
                sheet_id: 7,
                start_row_index: Some(1),
                end_row_index: Some(2),
                start_column_index: Some(3),
                end_column_index: Some(4),
            },
            shift_dimension: ShiftDimension::Columns,
        });
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            serde_json::json!({
                "insertRange": {
                    "range": {
                        "sheetId": 7,
                        "startRowIndex": 1,
                        "endRowIndex": 2,
                        "startColumnIndex": 3,
                        "endColumnIndex": 4,
                    },
                    "shiftDimension": "COLUMNS",
                },
            })
        );
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
            "sheets": [{"properties": {"title": "A"}, "somethingElseNew": 1}],
        });
        let parsed: Spreadsheet = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.sheet_titles(), vec!["A"]);
    }

    #[test]
    fn sheet_conditional_formats_round_trips_a_boolean_and_a_gradient_rule() {
        // issue #1793 — `conditionalFormats` used to be dropped as an
        // unmodelled field (see the fixture `spreadsheet_tolerates_unmodelled_fields`
        // predates this test with); confirm it's now real, in both shapes.
        let json = serde_json::json!({
            "sheets": [{
                "properties": {"title": "A"},
                "conditionalFormats": [
                    {
                        "ranges": [{"sheetId": 0, "startRowIndex": 0, "endRowIndex": 1}],
                        "booleanRule": {
                            "condition": {"type": "NUMBER_GREATER", "values": [{"userEnteredValue": "10"}]},
                            "format": {"backgroundColorStyle": {"rgbColor": {"red": 1.0, "green": 0.0, "blue": 0.0}}},
                        },
                    },
                    {
                        "ranges": [{"sheetId": 0, "startRowIndex": 1, "endRowIndex": 2}],
                        "gradientRule": {
                            "minColorStyle": {"rgbColor": {"red": 1.0, "green": 1.0, "blue": 1.0}},
                            "maxColorStyle": {"rgbColor": {"red": 0.0, "green": 1.0, "blue": 0.0}},
                        },
                    },
                ],
            }],
        });
        let parsed: Spreadsheet = serde_json::from_value(json).unwrap();
        let rules = &parsed.sheets[0].conditional_formats;
        assert_eq!(rules.len(), 2);
        assert_eq!(
            rules[0]
                .boolean_rule
                .as_ref()
                .unwrap()
                .condition
                .condition_type,
            "NUMBER_GREATER"
        );
        assert!(rules[0].gradient_rule.is_none());
        assert!(rules[1].boolean_rule.is_none());
        assert!(rules[1].gradient_rule.as_ref().unwrap().midpoint.is_none());
    }

    #[test]
    fn spreadsheet_title_is_empty_when_the_fields_mask_omitted_it() {
        let parsed: Spreadsheet = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(parsed.title(), "");
        assert!(parsed.sheet_titles().is_empty());
    }

    #[test]
    fn has_sheet_titled_skips_a_property_less_sheet() {
        // A sheet with no `properties` must never match, not even "" — it
        // has no title at all, unlike `Sheet::title()`'s `""` display
        // fallback (issue #1702).
        let json = serde_json::json!({
            "sheets": [
                {},
                {"properties": {"title": "Q1"}},
            ],
        });
        let parsed: Spreadsheet = serde_json::from_value(json).unwrap();
        assert!(!parsed.has_sheet_titled(""));
        assert!(parsed.has_sheet_titled("Q1"));
        assert!(!parsed.has_sheet_titled("nonexistent"));
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
