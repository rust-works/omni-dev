//! `drive sheets read-cell-format` engine — reports a range's per-cell
//! `userEnteredFormat`, note and data-validation rule (issue #1878).
//!
//! Exists to close [ADR-0083](../../../docs/adrs/adr-0083.md) §5's
//! gate-moving question in tooling rather than by eye: "a verb live-verified
//! to move or write formatting resolves both `sheets-write` and
//! `sheets-structure`" is an obligation the grid-mutation tranche has and,
//! until this verb existed, no tooling to discharge — nothing under
//! `omni-dev drive sheets` read a cell's format back.
//!
//! Read-only, so — like `read.rs` — it consults no write gate. See that
//! module's doc comment for why: [ADR-0071](../../../docs/adrs/adr-0071.md)
//! §11 records read-path gate enforcement as a deliberate, still-open
//! fast-follow across the whole Drive surface, not something this command
//! opts out of on its own.

use anyhow::Result;
use serde::Serialize;

use crate::cli::drive::format::{write_scalar_jsonl, JsonlSerialize};
use crate::drive::sheets::a1;
use crate::drive::sheets::api::SheetsApi;
use crate::drive::sheets::format::format_hex_color;
use crate::drive::sheets::grid_range;
use crate::drive::sheets::types::{
    CellFormatSnapshot, ColorStyleSnapshot, DataValidationRule, GridData,
};

/// Refuse a bounded range covering more cells than this budget.
///
/// An `includeGridData`-shaped response grows with the range requested, not
/// with how much of it is actually populated, so the narrow `fields` mask
/// alone does not bound the cost the way it does for `sheets read`'s
/// values-only fetch. Errors rather than truncating — the same posture as
/// `read.rs::MAX_SHEETS_PER_READ`.
///
/// Only ever enforced against a **bounded** range (`A1:D10`); an open-ended
/// one (`A:A`, `5:20`) is let through unchecked; a follow-up could size-check
/// those against the returned response instead of refusing up front.
const MAX_CELLS_PER_READ_CELL_FORMAT: i64 = 50_000;

/// Per-call options.
#[derive(Debug, Clone)]
pub struct ReadCellFormatOptions {
    /// Spreadsheet id.
    pub spreadsheet_id: String,
    /// A sheet title, supplying a prefix for a bare `range`.
    pub sheet: Option<String>,
    /// The A1 range to read, which may carry its own `Sheet!` prefix.
    /// Required — a whole-workbook read is out of scope for v1 (see the
    /// module doc comment).
    pub range: String,
}

/// One non-default cell's formatting, note and/or data validation rule.
///
/// A cell present in the response but carrying none of the three — the
/// common case for most of a real range — is never turned into an entry;
/// see [`entries_from_grid`].
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CellFormatEntry {
    /// The cell's absolute A1 address within its sheet (e.g. `"B3"`).
    pub cell: String,
    /// The cell's user-entered format, when it carries a non-default one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<CellFormatSnapshot>,
    /// The cell's note, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The cell's data validation rule, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_validation: Option<DataValidationRule>,
}

/// One sheet's non-default cells within the requested range.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SheetCellFormats {
    /// The sheet title, when it could be determined.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The composed range that was read.
    pub range: String,
    /// Every non-default cell, in row-major order.
    pub cells: Vec<CellFormatEntry>,
}

/// The result of a `read-cell-format` call.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CellFormatOutcome {
    /// The spreadsheet that was read.
    pub spreadsheet_id: String,
    /// One entry — v1 always reads exactly one range on one sheet; the
    /// `Vec` shape mirrors [`crate::drive::sheets::read::ReadOutcome`] so a
    /// future whole-sheet/whole-workbook mode could extend this without a
    /// breaking output-shape change.
    pub sheets: Vec<SheetCellFormats>,
}

impl JsonlSerialize for CellFormatOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Reads a range's cell-level formatting.
pub async fn read_cell_formats(
    api: &SheetsApi<'_>,
    opts: &ReadCellFormatOptions,
) -> Result<CellFormatOutcome> {
    anyhow::ensure!(
        !opts.range.trim().is_empty(),
        "--range must not be empty; a whole-workbook read is out of scope for read-cell-format"
    );
    let composed = a1::compose(opts.sheet.as_deref(), Some(opts.range.as_str()))?;
    check_cell_budget(&composed)?;

    let workbook = api
        .get_cell_formats(&opts.spreadsheet_id, &composed)
        .await?;

    // `sheets.data` is scoped by the `ranges` query parameter to just the
    // targeted sheet (see `get_cell_formats`'s doc comment), so the first
    // sheet carrying any chunk at all is always the one that was read —
    // the same reasoning `pivot.rs::cell_snapshot_at` relies on.
    let sheet = workbook.sheets.iter().find(|sheet| !sheet.data.is_empty());
    let (title, cells) = match sheet {
        Some(sheet) => (
            Some(sheet.title().to_string()),
            sheet.data.iter().flat_map(entries_from_grid).collect(),
        ),
        None => (opts.sheet.clone(), Vec::new()),
    };

    Ok(CellFormatOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        sheets: vec![SheetCellFormats {
            title,
            range: composed,
            cells,
        }],
    })
}

/// Refuses a bounded range over [`MAX_CELLS_PER_READ_CELL_FORMAT`] cells,
/// before any HTTP call. `composed` may carry a `Sheet!` prefix; only the
/// bare A1 portion is parsed. An unbounded or unparseable range (a defined
/// name, say) is let through — this cap only ever refuses what it can
/// actually compute, the same "never guess" stance `a1.rs` documents.
fn check_cell_budget(composed: &str) -> Result<()> {
    let bare = a1::split_sheet_prefix(composed).map_or(composed, |(_, rest)| rest);
    let Ok(grid) = grid_range::parse_grid_range(0, bare) else {
        return Ok(());
    };
    if !grid_range::is_bounded(&grid) {
        return Ok(());
    }
    // `is_bounded` guarantees every bound is `Some`.
    let rows = grid.end_row_index.unwrap_or_default() - grid.start_row_index.unwrap_or_default();
    let cols =
        grid.end_column_index.unwrap_or_default() - grid.start_column_index.unwrap_or_default();
    let cells = rows.saturating_mul(cols);
    anyhow::ensure!(
        cells <= MAX_CELLS_PER_READ_CELL_FORMAT,
        "range covers {cells} cells, over the {MAX_CELLS_PER_READ_CELL_FORMAT}-cell budget for \
         read-cell-format; narrow --range"
    );
    Ok(())
}

/// Extracts every non-default cell from one grid-data chunk, as absolute A1
/// addresses within the sheet — `start_row`/`start_column` are the chunk's
/// own offset (the API indexes a read relative to the range it was asked
/// for, not the sheet), the same convention
/// `grid_range::non_blank_locations` documents.
fn entries_from_grid(grid: &GridData) -> Vec<CellFormatEntry> {
    let start_row = grid.start_row.unwrap_or(0);
    let start_column = grid.start_column.unwrap_or(0);
    let mut entries = Vec::new();
    for (row_idx, row) in grid.row_data.iter().enumerate() {
        for (col_idx, cell) in row.values.iter().enumerate() {
            if cell.user_entered_format.is_none()
                && cell.note.is_none()
                && cell.data_validation.is_none()
            {
                continue;
            }
            let cell_address = format!(
                "{}{}",
                grid_range::column_index_to_letters(start_column + col_idx as i64),
                start_row + row_idx as i64 + 1
            );
            entries.push(CellFormatEntry {
                cell: cell_address,
                format: cell.user_entered_format.clone(),
                note: cell.note.clone(),
                data_validation: cell.data_validation.clone(),
            });
        }
    }
    entries
}

/// Renders one color, `#RRGGBB` for the `rgbColor` arm or `theme:<NAME>` for
/// the `themeColor` arm — never guessing black for a theme color the way
/// defaulting an absent `rgb_color` would.
fn render_color_style(style: &ColorStyleSnapshot) -> String {
    match (style.rgb_color, style.theme_color.as_deref()) {
        (Some(color), _) => format_hex_color(color),
        (None, Some(theme)) => format!("theme:{theme}"),
        (None, None) => "?".to_string(),
    }
}

/// Renders one entry as a compact, diff-friendly table line — the
/// before/after-`diff` workflow ADR-0083 §5's verification is built around.
/// Note *content* is never shown here (only its presence) — the same
/// "counts and locations, never contents" stance ADR-0083 §6 documents for
/// `non_blank_locations`; the full text is available via `-o json`/`-o yaml`.
pub(crate) fn render_line(entry: &CellFormatEntry) -> String {
    let mut attrs: Vec<String> = Vec::new();
    if let Some(format) = &entry.format {
        if let Some(bg) = &format.background_color_style {
            attrs.push(format!("bg={}", render_color_style(bg)));
        }
        if let Some(text) = &format.text_format {
            if text.bold == Some(true) {
                attrs.push("bold".to_string());
            }
            if text.italic == Some(true) {
                attrs.push("italic".to_string());
            }
            if text.strikethrough == Some(true) {
                attrs.push("strikethrough".to_string());
            }
            if text.underline == Some(true) {
                attrs.push("underline".to_string());
            }
            if let Some(fg) = &text.foreground_color_style {
                attrs.push(format!("fg={}", render_color_style(fg)));
            }
        }
        if let Some(number) = &format.number_format {
            if !number.format_type.is_empty() {
                if number.pattern.is_empty() {
                    attrs.push(format!("number={}", number.format_type));
                } else {
                    attrs.push(format!(
                        "number={}:{:?}",
                        number.format_type, number.pattern
                    ));
                }
            }
        }
        if let Some(align) = &format.horizontal_alignment {
            attrs.push(format!("align={align}"));
        }
    }
    if entry.note.is_some() {
        attrs.push("note".to_string());
    }
    if let Some(validation) = &entry.data_validation {
        if validation.condition.condition_type.is_empty() {
            attrs.push("validation".to_string());
        } else {
            attrs.push(format!(
                "validation={}",
                validation.condition.condition_type
            ));
        }
    }
    if attrs.is_empty() {
        entry.cell.clone()
    } else {
        format!("{}  {}", entry.cell, attrs.join(" "))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::sheets::types::{
        BooleanCondition, CellSnapshot, Color, NumberFormat, RowData, TextFormatSnapshot,
    };

    // ── check_cell_budget ──────────────────────────────────────────────

    #[test]
    fn check_cell_budget_allows_a_range_within_the_cap() {
        assert!(check_cell_budget("A1:D10").is_ok());
        assert!(check_cell_budget("'My Sheet'!A1:D10").is_ok());
    }

    #[test]
    fn check_cell_budget_refuses_a_bounded_range_over_the_cap() {
        // 1,000 rows * 1,000 cols = 1,000,000 cells, well over the cap.
        let err = check_cell_budget("A1:ALL1000").unwrap_err();
        assert!(err.to_string().contains("cell budget"), "{err}");
    }

    #[test]
    fn check_cell_budget_lets_an_unbounded_range_through() {
        assert!(check_cell_budget("A:A").is_ok());
        assert!(check_cell_budget("5:20").is_ok());
        assert!(check_cell_budget("A5:A").is_ok());
    }

    #[test]
    fn check_cell_budget_lets_an_unparseable_range_through() {
        // A defined name, or anything else `parse_grid_range` refuses —
        // the server is authoritative, per `a1.rs`'s stance.
        assert!(check_cell_budget("MyNamedRange").is_ok());
    }

    // ── entries_from_grid ──────────────────────────────────────────────

    fn cell_with_format() -> CellSnapshot {
        CellSnapshot {
            user_entered_format: Some(CellFormatSnapshot {
                background_color_style: Some(ColorStyleSnapshot {
                    rgb_color: Some(Color {
                        red: 1.0,
                        green: 0.0,
                        blue: 0.0,
                    }),
                    theme_color: None,
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn entries_from_grid_skips_a_cell_carrying_no_format_note_or_validation() {
        let grid = GridData {
            start_row: Some(0),
            start_column: Some(0),
            row_data: vec![RowData {
                values: vec![CellSnapshot::default()],
            }],
        };
        assert_eq!(entries_from_grid(&grid), Vec::new());
    }

    #[test]
    fn entries_from_grid_addresses_cells_absolutely_from_a_non_zero_offset() {
        // Offset past Z, to exercise the AA rollover too.
        let grid = GridData {
            start_row: Some(4),
            start_column: Some(26),
            row_data: vec![RowData {
                values: vec![CellSnapshot::default(), cell_with_format()],
            }],
        };
        let entries = entries_from_grid(&grid);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].cell, "AB5");
    }

    #[test]
    fn entries_from_grid_handles_ragged_rows() {
        let grid = GridData {
            start_row: Some(0),
            start_column: Some(0),
            row_data: vec![
                RowData { values: vec![] },
                RowData {
                    values: vec![cell_with_format()],
                },
            ],
        };
        let entries = entries_from_grid(&grid);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].cell, "A2");
    }

    #[test]
    fn entries_from_grid_includes_a_note_only_cell() {
        let grid = GridData {
            start_row: Some(0),
            start_column: Some(0),
            row_data: vec![RowData {
                values: vec![CellSnapshot {
                    note: Some("hello".to_string()),
                    ..Default::default()
                }],
            }],
        };
        let entries = entries_from_grid(&grid);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].note.as_deref(), Some("hello"));
        assert!(entries[0].format.is_none());
    }

    #[test]
    fn entries_from_grid_includes_a_validation_only_cell() {
        let grid = GridData {
            start_row: Some(0),
            start_column: Some(0),
            row_data: vec![RowData {
                values: vec![CellSnapshot {
                    data_validation: Some(DataValidationRule {
                        condition: BooleanCondition {
                            condition_type: "BOOLEAN".to_string(),
                            values: vec![],
                        },
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
            }],
        };
        let entries = entries_from_grid(&grid);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].data_validation.is_some());
    }

    // ── render_color_style ─────────────────────────────────────────────

    #[test]
    fn render_color_style_renders_black_with_every_channel_omitted() {
        let style = ColorStyleSnapshot {
            rgb_color: Some(Color::default()),
            theme_color: None,
        };
        assert_eq!(render_color_style(&style), "#000000");
    }

    #[test]
    fn render_color_style_renders_a_partial_channel() {
        let style = ColorStyleSnapshot {
            rgb_color: Some(Color {
                red: 1.0,
                ..Default::default()
            }),
            theme_color: None,
        };
        assert_eq!(render_color_style(&style), "#FF0000");
    }

    #[test]
    fn render_color_style_renders_a_theme_color_by_name_never_as_black() {
        let style = ColorStyleSnapshot {
            rgb_color: None,
            theme_color: Some("TEXT".to_string()),
        };
        assert_eq!(render_color_style(&style), "theme:TEXT");
    }

    // ── render_line ────────────────────────────────────────────────────

    #[test]
    fn render_line_covers_background_bold_number_note_and_validation() {
        let entry = CellFormatEntry {
            cell: "B3".to_string(),
            format: Some(CellFormatSnapshot {
                background_color_style: Some(ColorStyleSnapshot {
                    rgb_color: Some(Color {
                        red: 1.0,
                        ..Default::default()
                    }),
                    theme_color: None,
                }),
                text_format: Some(TextFormatSnapshot {
                    bold: Some(true),
                    ..Default::default()
                }),
                number_format: Some(NumberFormat {
                    format_type: "CURRENCY".to_string(),
                    pattern: "$#,##0".to_string(),
                }),
                horizontal_alignment: None,
            }),
            note: Some("a note".to_string()),
            data_validation: Some(DataValidationRule {
                condition: BooleanCondition {
                    condition_type: "ONE_OF_LIST".to_string(),
                    values: vec![],
                },
                ..Default::default()
            }),
        };
        let line = render_line(&entry);
        assert!(line.starts_with("B3  "), "{line}");
        assert!(line.contains("bg=#FF0000"), "{line}");
        assert!(line.contains("bold"), "{line}");
        assert!(line.contains("number=CURRENCY:\"$#,##0\""), "{line}");
        assert!(line.contains("note"), "{line}");
        assert!(line.contains("validation=ONE_OF_LIST"), "{line}");
        // Note content is never shown — only its presence.
        assert!(!line.contains("a note"), "{line}");
    }

    #[test]
    fn render_line_with_nothing_set_is_bare_cell_address() {
        let entry = CellFormatEntry {
            cell: "A1".to_string(),
            format: None,
            note: None,
            data_validation: None,
        };
        assert_eq!(render_line(&entry), "A1");
    }
}
