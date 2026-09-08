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
use crate::drive::sheets::types::{GridRange, Spreadsheet};

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

/// Resolves an already-composed `--sheet`/`--range` string into a numeric
/// [`GridRange`]: the shared "split off the sheet prefix, find its id,
/// parse the bare range" pipeline every range-targeted verb in
/// `format.rs`/`protection.rs`/`validation.rs` needs. `invalid_range` and
/// `not_found` build the caller's own error variants, so this stays usable
/// across their differently-shaped result enums.
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
        return Err(invalid_range(format!(
            "'{composed}' does not name a sheet; pass --sheet, or a --range carrying its own \
             'Sheet!' prefix"
        )));
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
}
