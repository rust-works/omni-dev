//! A1-notation-within-one-sheet → numeric [`GridRange`] conversion (issue
//! #1643).
//!
//! `a1.rs` deliberately never parses A1 grammar: every existing verb passes
//! an A1 *string* straight to the `values.*` endpoints, which resolve it
//! server-side, so a client-side parser could only ever reject something
//! the server would have accepted. The formatting, validation and
//! protection verbs are different: `repeatCell`, `updateBorders`,
//! `mergeCells`, `unmergeCells`, `setDataValidation` and
//! `addProtectedRange` all address a [`GridRange`] — a numeric,
//! zero-based, half-open rectangle — because that is the wire shape, not a
//! preference. Parsing is required here, not optional, exactly as
//! `structure.rs::dimension_range`'s 1-based-to-0-based conversion is
//! required for `insertDimension`. The same rule applies: one pure,
//! independently-tested conversion, never inlined at a call site.
//!
//! **A deliberately bounded grammar**, matching this feature's general
//! stance of a curated surface over full API coverage: a single cell
//! (`A1`), a bounded rectangle (`A1:D20`), a whole column or column span
//! (`A:A`, `A:D`), a whole row or row span (`5:5`, `5:20`), and a
//! column open-ended downward from a starting cell (`A5:A`). Anything else
//! — mixed forms Sheets itself may accept, like a row open-ended
//! rightward — is refused with a message naming the supported forms,
//! rather than guessed at.
//!
//! Takes the sheet's numeric id separately, like every other structural
//! verb's `--sheet` flag: the range string itself never carries a sheet
//! prefix here.

use crate::drive::sheets::a1;
use crate::drive::sheets::types::{GridRange, Sheet, Spreadsheet, ValueRange};

/// What one `:`-delimited token names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CellRef {
    /// A full cell reference (`A1`): zero-based column, zero-based row.
    Cell(i64, i64),
    /// A bare column letter (`A`, `AA`): zero-based column.
    Column(i64),
    /// A bare row number (`5`): zero-based row.
    Row(i64),
}

/// Converts `A`, `B`, … `Z`, `AA`, … into a zero-based column index.
///
/// Base-26 with no zero digit — `Z` is 25, `AA` is 26, not 0 — which is why
/// this can't be a direct radix conversion.
fn column_letters_to_index(letters: &str) -> Option<i64> {
    if letters.is_empty() || !letters.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    let mut index: i64 = 0;
    for b in letters.bytes() {
        let digit = i64::from(b.to_ascii_uppercase() - b'A' + 1);
        index = index.checked_mul(26)?.checked_add(digit)?;
    }
    Some(index - 1)
}

/// Zero-based column index → A1 letters (`0` → `"A"`, `26` → `"AA"`) — the
/// inverse of [`column_letters_to_index`], colocated with it so both
/// directions of the conversion share one tested implementation.
pub(crate) fn column_index_to_letters(mut index: i64) -> String {
    let mut letters = Vec::new();
    loop {
        letters.push((b'A' + (index % 26) as u8) as char);
        index = index / 26 - 1;
        if index < 0 {
            break;
        }
    }
    letters.iter().rev().collect()
}

/// Every non-blank cell within a `values.get` read, as its A1 address —
/// **never its value** (ADR-0083 §6: "counts and A1 locations, never
/// contents"). `row_offset`/`col_offset` are the read range's own start
/// row/column, since the API indexes a read relative to the range it was
/// asked for, not the sheet.
///
/// Shared by `paste.rs` and `auto_fill.rs` (previously two independent
/// copies) and `text_to_columns.rs`; a third private copy would let
/// ADR-0083 §6's "never a value" rule drift into a per-file convention
/// instead of one tested implementation. Whether the top-left/anchor cell
/// is included in the result is the caller's own decision — every existing
/// caller's written extent covers its whole read, so this function makes
/// no exception for it.
pub(crate) fn non_blank_locations(
    values: &ValueRange,
    row_offset: i64,
    col_offset: i64,
) -> Vec<String> {
    let mut locations = Vec::new();
    for (row_idx, row) in values.values.iter().enumerate() {
        for (col_idx, cell) in row.iter().enumerate() {
            let is_blank = cell.is_null() || cell.as_str().is_some_and(str::is_empty);
            if is_blank {
                continue;
            }
            locations.push(format!(
                "{}{}",
                column_index_to_letters(col_idx as i64 + col_offset),
                row_idx as i64 + row_offset + 1
            ));
        }
    }
    locations
}

/// Whether every bound of `grid` is set — i.e. it names a fixed rectangle
/// rather than an open-ended column/row span (`A:A`, `5:5`).
pub(crate) fn is_bounded(grid: &GridRange) -> bool {
    grid.start_row_index.is_some()
        && grid.end_row_index.is_some()
        && grid.start_column_index.is_some()
        && grid.end_column_index.is_some()
}

/// Finds a sheet's numeric id by title. Shared by every verb that resolves
/// a `--sheet` name (or a range's sheet prefix) against a freshly-fetched
/// workbook — `not_found` builds the caller's own "sheet not found" error
/// variant.
pub(crate) fn find_sheet_id<E>(
    workbook: &Spreadsheet,
    title: &str,
    not_found: impl FnOnce(String, Vec<String>) -> E,
) -> Result<i64, E> {
    workbook
        .sheets
        .iter()
        .filter_map(|sheet| sheet.properties.as_ref())
        .find(|props| props.title == title)
        .and_then(|props| props.sheet_id)
        .ok_or_else(|| not_found(title.to_string(), workbook.sheet_titles()))
}

/// The reverse of [`find_sheet_id`]: a sheet's title by its numeric id, or
/// `None` if the workbook carries no sheet with that id. Needed by any
/// list verb (e.g. `list-named-ranges`) that renders a workbook-scoped
/// object's `sheetId` back into a human-readable title.
pub(crate) fn sheet_title_by_id(workbook: &Spreadsheet, sheet_id: i64) -> Option<String> {
    find_sheet_by_id(workbook, sheet_id).map(|sheet| sheet.title().to_string())
}

/// A workbook's sheet by its numeric id, or `None` if it carries none with
/// that id. The `&Sheet` counterpart to [`sheet_title_by_id`], for a caller
/// that needs more than the title (e.g. `embedded_object.rs`'s `move-chart`/
/// `move-slicer`, which renders a destination anchor via
/// `describe_overlay_position(sheet, ..)`, issue #1837).
pub(crate) fn find_sheet_by_id(workbook: &Spreadsheet, sheet_id: i64) -> Option<&Sheet> {
    workbook
        .sheets
        .iter()
        .find(|sheet| sheet.sheet_id() == Some(sheet_id))
}

/// Whether `grid` names exactly one cell.
///
/// Shared by every verb whose API request takes a `GridCoordinate` rather
/// than a `GridRange` — `add-pivot-table`'s anchor, `cut-paste`'s and
/// `paste-data`'s destination — so the refusal they make of a multi-cell
/// argument rests on one tested definition.
pub(crate) fn is_single_cell(grid: &GridRange) -> bool {
    is_bounded(grid)
        && grid.end_row_index.unwrap_or(0) - grid.start_row_index.unwrap_or(0) == 1
        && grid.end_column_index.unwrap_or(0) - grid.start_column_index.unwrap_or(0) == 1
}

/// Renders a fully-bounded, non-empty [`GridRange`] back to an A1 string
/// for a `values.get` read, e.g. `'Q1'!A1:B2`.
///
/// `None` for a range that isn't fully bounded, or that is empty on either
/// axis (`end <= start`): neither names a rectangle a read could describe,
/// and the 1-based inclusive end this has to render would run *before* the
/// start — `column_index_to_letters(-1)` for a zero-column range, which is
/// not a column at all. Returning `None` rather than a bad string keeps
/// that case a caller's decision instead of an invalid range on the wire
/// (issue #1839).
pub(crate) fn bounded_range_to_a1(sheet_title: &str, grid: &GridRange) -> Option<String> {
    let (Some(r0), Some(r1), Some(c0), Some(c1)) = (
        grid.start_row_index,
        grid.end_row_index,
        grid.start_column_index,
        grid.end_column_index,
    ) else {
        return None;
    };
    if r1 <= r0 || c1 <= c0 {
        return None;
    }
    let start = format!("{}{}", column_index_to_letters(c0), r0 + 1);
    let end = format!("{}{}", column_index_to_letters(c1 - 1), r1);
    a1::compose(Some(sheet_title), Some(&format!("{start}:{end}"))).ok()
}

/// Clips `grid` to the rows and columns its sheet currently has, or `None`
/// when nothing of it lies inside them.
///
/// A `values.get` over a range past the grid's edge is refused by the API
/// ("exceeds grid limits"), so a preview that computes an extent reaching
/// beyond the sheet — which `paste.rs` deliberately can, since the written
/// extent is a property of the request, not of the sheet — must read the
/// part that exists rather than ask for the part that doesn't (issue
/// #1839). An axis the API reports no count for is left as-is: there's
/// nothing to clip against.
pub(crate) fn clamp_to_sheet(workbook: &Spreadsheet, grid: &GridRange) -> Option<GridRange> {
    let sheet = find_sheet_by_id(workbook, grid.sheet_id)?;
    let props = sheet
        .properties
        .as_ref()
        .and_then(|p| p.grid_properties.as_ref());
    let clip = |end: Option<i64>, count: Option<i64>| match (end, count) {
        (Some(end), Some(count)) => Some(end.min(count)),
        (end, _) => end,
    };
    let clamped = GridRange {
        sheet_id: grid.sheet_id,
        start_row_index: grid.start_row_index,
        end_row_index: clip(grid.end_row_index, props.and_then(|g| g.row_count)),
        start_column_index: grid.start_column_index,
        end_column_index: clip(grid.end_column_index, props.and_then(|g| g.column_count)),
    };
    let empty = |start: Option<i64>, end: Option<i64>| matches!((start, end), (Some(start), Some(end)) if end <= start);
    if empty(clamped.start_row_index, clamped.end_row_index)
        || empty(clamped.start_column_index, clamped.end_column_index)
    {
        return None;
    }
    Some(clamped)
}

/// Renders a numeric [`GridRange`] as a compact 1-based description for a
/// list verb's human-readable output — e.g. `"sheetId 0, rows 1-5, cols
/// 1-2"`, `"sheetId 0, rows 5+"` for a bound left open at one end (the
/// `A5:A` grammar `parse_grid_range` accepts), or `"sheetId 0 (whole
/// sheet)"` when every bound is `None`. Shared by `list-protections`/
/// `list-named-ranges`, which were previously two independent copies of
/// this function — one of which silently dropped an open-ended bound
/// instead of showing it, rather than falling back to "whole sheet".
pub(crate) fn render_grid_range(range: &GridRange) -> String {
    let rows = describe_bound(range.start_row_index, range.end_row_index, "rows");
    let cols = describe_bound(range.start_column_index, range.end_column_index, "cols");
    if rows.is_empty() && cols.is_empty() {
        format!("sheetId {} (whole sheet)", range.sheet_id)
    } else {
        format!("sheetId {}{rows}{cols}", range.sheet_id)
    }
}

/// One axis of [`render_grid_range`]: `start`/`end` are zero-based,
/// `end` exclusive, exactly like [`GridRange`]'s own fields.
fn describe_bound(start: Option<i64>, end: Option<i64>, label: &str) -> String {
    match (start, end) {
        (Some(start), Some(end)) => format!(", {label} {}-{end}", start + 1),
        (Some(start), None) => format!(", {label} {}+", start + 1),
        (None, Some(end)) => format!(", {label} 1-{end}"),
        (None, None) => String::new(),
    }
}

/// Resolves an already-composed `--sheet`/`--range` string into a numeric
/// [`GridRange`]: the shared "split off the sheet prefix, find its id,
/// parse the bare range" pipeline every range-targeted verb in
/// `format.rs`/`protection.rs`/`validation.rs`/`named_range.rs` needs.
/// `invalid_range` and `not_found` build the caller's own error variants,
/// so this stays usable across their differently-shaped result enums.
///
/// Returns the sheet's title alongside the range, since some callers (e.g.
/// `format.rs`'s merge-cells preview) need it again afterward.
pub(crate) fn resolve_grid_range<E>(
    workbook: &Spreadsheet,
    composed: &str,
    invalid_range: impl Fn(String) -> E,
    not_found: impl FnOnce(String, Vec<String>) -> E,
) -> Result<(String, GridRange), E> {
    let Some((title, bare_range)) = a1::split_sheet_prefix(composed) else {
        // `--sheet` alone (with no `--range`) composes to a bare quoted
        // sheet name with no `!` — a distinct, common mistake from a
        // genuinely malformed range, and one `--whole-sheet` names the fix
        // for directly, so it earns its own message rather than the
        // generic one below (which would otherwise tell a `--sheet` caller
        // to "pass --sheet" when they already did).
        let detail = if a1::is_whole_sheet_reference(composed) {
            format!(
                "'{composed}' names a sheet but not a range within it; pass --whole-sheet to \
                 target the whole sheet, or add --range to name a range inside it"
            )
        } else {
            format!(
                "'{composed}' does not name a sheet; pass --sheet, or a --range carrying its \
                 own 'Sheet!' prefix"
            )
        };
        return Err(invalid_range(detail));
    };
    let sheet_id = find_sheet_id(workbook, &title, not_found)?;
    let grid = parse_grid_range(sheet_id, bare_range).map_err(invalid_range)?;
    Ok((title, grid))
}

/// Parses one token (one side of a `:`, or a whole single-cell range) into
/// a [`CellRef`].
fn parse_cell_ref(token: &str) -> Option<CellRef> {
    let split_at = token.find(|c: char| c.is_ascii_digit());
    match split_at {
        None => column_letters_to_index(token).map(CellRef::Column),
        Some(0) => {
            let row: i64 = token.parse().ok()?;
            (row >= 1).then_some(CellRef::Row(row - 1))
        }
        Some(at) => {
            let (letters, digits) = token.split_at(at);
            let column = column_letters_to_index(letters)?;
            let row: i64 = digits.parse().ok()?;
            (row >= 1).then_some(CellRef::Cell(column, row - 1))
        }
    }
}

/// The forms [`parse_grid_range`] understands, for its error message.
const SUPPORTED_FORMS: &str = "a cell (A1), a bounded range (A1:D20), a whole column or \
    column span (A:A, A:D), a whole row or row span (5:5, 5:20), or a column open-ended \
    downward from a cell (A5:A)";

/// Parses a bare (no sheet prefix) A1 range into a numeric [`GridRange`] on
/// `sheet_id`.
///
/// See the module docs for exactly which forms are accepted. Every rejected
/// input gets a message naming what *is* supported, matching `a1.rs`'s own
/// stance of never silently guessing.
pub(crate) fn parse_grid_range(sheet_id: i64, range: &str) -> Result<GridRange, String> {
    let range = range.trim();
    let bare = || GridRange {
        sheet_id,
        ..Default::default()
    };
    let mut parts = range.splitn(2, ':');
    let first = parts.next().unwrap_or_default();
    match parts.next() {
        None => match parse_cell_ref(first) {
            Some(CellRef::Cell(col, row)) => Ok(GridRange {
                start_row_index: Some(row),
                end_row_index: Some(row + 1),
                start_column_index: Some(col),
                end_column_index: Some(col + 1),
                ..bare()
            }),
            Some(CellRef::Column(col)) => Ok(GridRange {
                start_column_index: Some(col),
                end_column_index: Some(col + 1),
                ..bare()
            }),
            Some(CellRef::Row(row)) => Ok(GridRange {
                start_row_index: Some(row),
                end_row_index: Some(row + 1),
                ..bare()
            }),
            None => Err(format!(
                "'{range}' is not a recognised A1 range; expected {SUPPORTED_FORMS}"
            )),
        },
        Some(second) => {
            let (Some(start), Some(end)) = (parse_cell_ref(first), parse_cell_ref(second)) else {
                return Err(format!(
                    "'{range}' is not a recognised A1 range; expected {SUPPORTED_FORMS}"
                ));
            };
            match (start, end) {
                (CellRef::Cell(c1, r1), CellRef::Cell(c2, r2)) => Ok(GridRange {
                    start_row_index: Some(r1.min(r2)),
                    end_row_index: Some(r1.max(r2) + 1),
                    start_column_index: Some(c1.min(c2)),
                    end_column_index: Some(c1.max(c2) + 1),
                    ..bare()
                }),
                (CellRef::Column(c1), CellRef::Column(c2)) => Ok(GridRange {
                    start_column_index: Some(c1.min(c2)),
                    end_column_index: Some(c1.max(c2) + 1),
                    ..bare()
                }),
                (CellRef::Row(r1), CellRef::Row(r2)) => Ok(GridRange {
                    start_row_index: Some(r1.min(r2)),
                    end_row_index: Some(r1.max(r2) + 1),
                    ..bare()
                }),
                (CellRef::Cell(c1, r1), CellRef::Column(c2)) if c1 == c2 => Ok(GridRange {
                    start_row_index: Some(r1),
                    end_row_index: None,
                    start_column_index: Some(c1),
                    end_column_index: Some(c1 + 1),
                    ..bare()
                }),
                _ => Err(format!(
                    "'{range}' is not a recognised A1 range; expected {SUPPORTED_FORMS}"
                )),
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::sheets::types::{GridProperties, Sheet, SheetProperties};

    #[test]
    fn non_blank_locations_skips_blanks_and_offsets_addresses() {
        let values: ValueRange = serde_json::from_value(serde_json::json!({
            "values": [["a", "", "c"], [null, "d2"]]
        }))
        .unwrap();
        let locations = non_blank_locations(&values, 4, 2);
        assert_eq!(locations, vec!["C5", "E5", "D6"]);
    }

    #[test]
    fn non_blank_locations_includes_the_top_left_cell() {
        // Unlike `format.rs::discarded_from_values`, the anchor is not
        // skipped: every caller's written extent covers its whole read,
        // so the top-left cell is overwritten like any other. Moved here
        // with the function it pins, from `paste.rs`.
        let values: ValueRange = serde_json::from_value(serde_json::json!({
            "values": [["kept"]]
        }))
        .unwrap();
        assert_eq!(non_blank_locations(&values, 0, 0), vec!["A1"]);
    }

    #[test]
    fn non_blank_locations_never_carries_a_value() {
        let values: ValueRange = serde_json::from_value(serde_json::json!({
            "values": [["a-secret-value"]]
        }))
        .unwrap();
        let locations = non_blank_locations(&values, 0, 0);
        assert_eq!(locations, vec!["A1"]);
        assert!(!locations.iter().any(|loc| loc.contains("secret")));
    }

    #[test]
    fn column_letters_to_index_single_and_double_letters() {
        assert_eq!(column_letters_to_index("A"), Some(0));
        assert_eq!(column_letters_to_index("Z"), Some(25));
        assert_eq!(column_letters_to_index("AA"), Some(26));
        assert_eq!(column_letters_to_index("AZ"), Some(51));
        assert_eq!(column_letters_to_index("BA"), Some(52));
    }

    #[test]
    fn column_letters_to_index_rejects_non_letters() {
        assert_eq!(column_letters_to_index(""), None);
        assert_eq!(column_letters_to_index("1"), None);
        assert_eq!(column_letters_to_index("A1"), None);
    }

    #[test]
    fn a_single_cell() {
        let g = parse_grid_range(7, "B3").unwrap();
        assert_eq!(g.sheet_id, 7);
        assert_eq!(g.start_row_index, Some(2));
        assert_eq!(g.end_row_index, Some(3));
        assert_eq!(g.start_column_index, Some(1));
        assert_eq!(g.end_column_index, Some(2));
    }

    #[test]
    fn a_bounded_range() {
        let g = parse_grid_range(1, "A1:D20").unwrap();
        assert_eq!(g.start_row_index, Some(0));
        assert_eq!(g.end_row_index, Some(20));
        assert_eq!(g.start_column_index, Some(0));
        assert_eq!(g.end_column_index, Some(4));
    }

    #[test]
    fn a_bounded_range_normalises_a_reversed_pair() {
        let g = parse_grid_range(1, "D20:A1").unwrap();
        assert_eq!(g.start_row_index, Some(0));
        assert_eq!(g.end_row_index, Some(20));
        assert_eq!(g.start_column_index, Some(0));
        assert_eq!(g.end_column_index, Some(4));
    }

    #[test]
    fn a_whole_column() {
        let g = parse_grid_range(1, "C:C").unwrap();
        assert_eq!(g.start_row_index, None);
        assert_eq!(g.end_row_index, None);
        assert_eq!(g.start_column_index, Some(2));
        assert_eq!(g.end_column_index, Some(3));
    }

    #[test]
    fn a_column_span() {
        let g = parse_grid_range(1, "A:C").unwrap();
        assert_eq!(g.start_column_index, Some(0));
        assert_eq!(g.end_column_index, Some(3));
        assert_eq!(g.start_row_index, None);
    }

    #[test]
    fn a_whole_row() {
        let g = parse_grid_range(1, "5:5").unwrap();
        assert_eq!(g.start_row_index, Some(4));
        assert_eq!(g.end_row_index, Some(5));
        assert_eq!(g.start_column_index, None);
        assert_eq!(g.end_column_index, None);
    }

    #[test]
    fn a_row_span() {
        let g = parse_grid_range(1, "5:20").unwrap();
        assert_eq!(g.start_row_index, Some(4));
        assert_eq!(g.end_row_index, Some(20));
    }

    #[test]
    fn a_column_open_ended_downward_from_a_cell() {
        let g = parse_grid_range(1, "A5:A").unwrap();
        assert_eq!(g.start_row_index, Some(4));
        assert_eq!(g.end_row_index, None);
        assert_eq!(g.start_column_index, Some(0));
        assert_eq!(g.end_column_index, Some(1));
    }

    #[test]
    fn mismatched_column_open_end_is_rejected() {
        let err = parse_grid_range(1, "A5:B").unwrap_err();
        assert!(err.contains("not a recognised A1 range"), "{err}");
    }

    #[test]
    fn a_row_open_ended_rightward_is_out_of_the_supported_grammar() {
        let err = parse_grid_range(1, "A5:5").unwrap_err();
        assert!(err.contains("expected"), "{err}");
    }

    #[test]
    fn garbage_is_rejected_with_a_message_naming_supported_forms() {
        let err = parse_grid_range(1, "!!!").unwrap_err();
        assert!(err.contains("column span"), "{err}");
    }

    #[test]
    fn empty_string_is_rejected() {
        assert!(parse_grid_range(1, "").is_err());
    }

    #[test]
    fn whitespace_is_trimmed() {
        let g = parse_grid_range(1, "  A1:B2  ").unwrap();
        assert_eq!(g.start_row_index, Some(0));
    }

    #[test]
    fn column_index_to_letters_round_trips_column_letters_to_index() {
        for (index, letters) in [(0, "A"), (25, "Z"), (26, "AA"), (51, "AZ"), (52, "BA")] {
            assert_eq!(column_index_to_letters(index), letters);
            assert_eq!(column_letters_to_index(letters), Some(index));
        }
    }

    #[test]
    fn is_bounded_requires_all_four_indices() {
        let bounded = parse_grid_range(1, "A1:B2").unwrap();
        assert!(is_bounded(&bounded));
        let open_column = parse_grid_range(1, "A:A").unwrap();
        assert!(!is_bounded(&open_column));
    }

    #[test]
    fn a_bare_column_letter_with_no_colon() {
        let g = parse_grid_range(1, "C").unwrap();
        assert_eq!(g.start_column_index, Some(2));
        assert_eq!(g.end_column_index, Some(3));
        assert_eq!(g.start_row_index, None);
        assert_eq!(g.end_row_index, None);
    }

    #[test]
    fn a_bare_row_number_with_no_colon() {
        let g = parse_grid_range(1, "5").unwrap();
        assert_eq!(g.start_row_index, Some(4));
        assert_eq!(g.end_row_index, Some(5));
        assert_eq!(g.start_column_index, None);
        assert_eq!(g.end_column_index, None);
    }

    #[test]
    fn a_range_with_one_unparseable_side_is_rejected() {
        let err = parse_grid_range(1, "A1:!!!").unwrap_err();
        assert!(err.contains("not a recognised A1 range"), "{err}");
    }

    #[test]
    fn is_single_cell_accepts_one_cell_and_refuses_a_range_or_an_open_end() {
        assert!(is_single_cell(&parse_grid_range(1, "A1").unwrap()));
        assert!(!is_single_cell(&parse_grid_range(1, "A1:A2").unwrap()));
        assert!(!is_single_cell(&parse_grid_range(1, "A1:B1").unwrap()));
        assert!(!is_single_cell(&parse_grid_range(1, "A:A").unwrap()));
    }

    #[test]
    fn bounded_range_to_a1_round_trips_a_rectangle() {
        let grid = parse_grid_range(7, "B2:D5").unwrap();
        assert_eq!(
            bounded_range_to_a1("Q1", &grid).as_deref(),
            Some("'Q1'!B2:D5")
        );
    }

    #[test]
    fn bounded_range_to_a1_refuses_an_open_ended_or_empty_range() {
        assert_eq!(
            bounded_range_to_a1("Q1", &parse_grid_range(1, "A:A").unwrap()),
            None
        );
        // The zero-by-zero case: rendering it would ask
        // `column_index_to_letters` for column -1.
        let empty = GridRange {
            sheet_id: 1,
            start_row_index: Some(0),
            end_row_index: Some(0),
            start_column_index: Some(0),
            end_column_index: Some(0),
        };
        assert_eq!(bounded_range_to_a1("Q1", &empty), None);
    }

    fn workbook_with_grid(sheet_id: i64, rows: i64, columns: i64) -> Spreadsheet {
        Spreadsheet {
            sheets: vec![Sheet {
                properties: Some(SheetProperties {
                    sheet_id: Some(sheet_id),
                    title: "Q1".to_string(),
                    grid_properties: Some(GridProperties {
                        row_count: Some(rows),
                        column_count: Some(columns),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn clamp_to_sheet_clips_an_extent_past_the_grids_edge() {
        let workbook = workbook_with_grid(1, 10, 5);
        let clamped = clamp_to_sheet(&workbook, &parse_grid_range(1, "A1:Z100").unwrap()).unwrap();
        assert_eq!(clamped.end_row_index, Some(10));
        assert_eq!(clamped.end_column_index, Some(5));
    }

    #[test]
    fn clamp_to_sheet_leaves_an_extent_inside_the_grid_alone() {
        let workbook = workbook_with_grid(1, 10, 5);
        let grid = parse_grid_range(1, "A1:B2").unwrap();
        assert_eq!(clamp_to_sheet(&workbook, &grid), Some(grid));
    }

    #[test]
    fn clamp_to_sheet_is_none_when_nothing_of_the_range_exists() {
        let workbook = workbook_with_grid(1, 10, 5);
        // Starts past the last row: clipping leaves an empty rectangle,
        // which is no range at all rather than a zero-height one.
        assert_eq!(
            clamp_to_sheet(&workbook, &parse_grid_range(1, "A20:B30").unwrap()),
            None
        );
    }

    #[test]
    fn clamp_to_sheet_leaves_an_axis_with_no_reported_count_alone() {
        let workbook = workbook_with_sheet(1, "Q1");
        let grid = parse_grid_range(1, "A1:Z100").unwrap();
        assert_eq!(clamp_to_sheet(&workbook, &grid), Some(grid));
    }

    #[test]
    fn clamp_to_sheet_is_none_for_a_sheet_the_workbook_does_not_carry() {
        let workbook = workbook_with_grid(1, 10, 5);
        assert_eq!(
            clamp_to_sheet(&workbook, &parse_grid_range(99, "A1:B2").unwrap()),
            None
        );
    }

    fn workbook_with_sheet(sheet_id: i64, title: &str) -> Spreadsheet {
        Spreadsheet {
            sheets: vec![Sheet {
                properties: Some(SheetProperties {
                    sheet_id: Some(sheet_id),
                    title: title.to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn resolve_grid_range_rejects_a_range_with_no_sheet_prefix() {
        let workbook = workbook_with_sheet(0, "Q1");
        let err = resolve_grid_range(
            &workbook,
            "A1:B2",
            |detail| detail,
            |t, a| format!("{t} {a:?}"),
        )
        .unwrap_err();
        assert!(err.contains("does not name a sheet"), "{err}");
    }

    #[test]
    fn resolve_grid_range_points_a_sheet_only_composition_at_whole_sheet() {
        // `--sheet Q1` alone composes to `'Q1'` (a1::compose's
        // sheet-only arm) — a distinct mistake from a malformed range,
        // deserving its own message rather than "pass --sheet" when the
        // caller already did.
        let workbook = workbook_with_sheet(0, "Q1");
        let err = resolve_grid_range(
            &workbook,
            "'Q1'",
            |detail| detail,
            |t, a| format!("{t} {a:?}"), // omni-dev: coverage ignore-line reason="a sheet-only composition always fails at the split_sheet_prefix check above; not_found is never reached in this test"
        )
        .unwrap_err();
        assert!(err.contains("--whole-sheet"), "{err}");
    }

    #[test]
    fn sheet_title_by_id_finds_the_matching_sheet() {
        let workbook = workbook_with_sheet(3, "Q1");
        assert_eq!(sheet_title_by_id(&workbook, 3), Some("Q1".to_string()));
        assert_eq!(sheet_title_by_id(&workbook, 99), None);
    }

    #[test]
    fn find_sheet_by_id_finds_the_matching_sheet() {
        let workbook = workbook_with_sheet(3, "Q1");
        assert_eq!(find_sheet_by_id(&workbook, 3).map(Sheet::title), Some("Q1"));
        assert!(find_sheet_by_id(&workbook, 99).is_none());
    }

    #[test]
    fn render_grid_range_whole_sheet_when_all_bounds_none() {
        let range = GridRange {
            sheet_id: 0,
            ..Default::default()
        };
        assert_eq!(render_grid_range(&range), "sheetId 0 (whole sheet)");
    }

    #[test]
    fn render_grid_range_rows_and_cols() {
        let range = GridRange {
            sheet_id: 3,
            start_row_index: Some(0),
            end_row_index: Some(5),
            start_column_index: Some(0),
            end_column_index: Some(2),
        };
        assert_eq!(render_grid_range(&range), "sheetId 3, rows 1-5, cols 1-2");
    }

    #[test]
    fn render_grid_range_shows_an_open_ended_bound_rather_than_dropping_it() {
        // The `A5:A` grammar: bounded start, open end. Regression test for
        // the bug where this silently rendered as if unbounded.
        let range = GridRange {
            sheet_id: 0,
            start_row_index: Some(4),
            end_row_index: None,
            start_column_index: Some(0),
            end_column_index: Some(1),
        };
        assert_eq!(render_grid_range(&range), "sheetId 0, rows 5+, cols 1-1");
    }

    #[test]
    fn render_grid_range_shows_an_open_start_bound() {
        let range = GridRange {
            sheet_id: 0,
            start_row_index: None,
            end_row_index: Some(5),
            ..Default::default()
        };
        assert_eq!(render_grid_range(&range), "sheetId 0, rows 1-5");
    }
}
