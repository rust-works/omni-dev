//! Bounded table edits resolved from the revision-pinned inline snapshot.
//! One request per invocation prevents index shifts from request ordering.

use serde::Serialize;

use super::anchor::{self, AnchorError, Side};
use super::types::{Document, StructuralElement};
use super::write_types::{
    DocsRequest, InsertTableColumnRequest, InsertTableRequest, InsertTableRowRequest, Location,
    TableCellLocation, TableDimensionRequest,
};
use crate::drive::write_gate::DriveOperation;

/// The five supported table operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TableVerb {
    /// Insert an empty table.
    InsertTable,
    /// Insert a row.
    InsertRow,
    /// Insert a column.
    InsertColumn,
    /// Remove a row, retaining the table.
    DeleteRow,
    /// Remove a column, retaining the table.
    DeleteColumn,
}

impl TableVerb {
    /// CLI and audit spelling.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::InsertTable => "insert-table",
            Self::InsertRow => "insert-table-row",
            Self::InsertColumn => "insert-table-column",
            Self::DeleteRow => "delete-table-row",
            Self::DeleteColumn => "delete-table-column",
        }
    }

    /// Independent consent for additions and destructive edits.
    #[must_use]
    pub const fn gate_operation(self) -> DriveOperation {
        match self {
            Self::DeleteRow | Self::DeleteColumn => DriveOperation::DocsTableDelete,
            _ => DriveOperation::DocsStructure,
        }
    }

    pub(super) const fn log_operation(self) -> &'static str {
        match self {
            Self::InsertTable => "docs-insert-table",
            Self::InsertRow => "docs-insert-table-row",
            Self::InsertColumn => "docs-insert-table-column",
            Self::DeleteRow => "docs-delete-table-row",
            Self::DeleteColumn => "docs-delete-table-column",
        }
    }
}

/// Typed caller intent. No raw index or batch escape hatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableEdit {
    /// Insert next to a unique top-level body anchor.
    Insert {
        /// Literal anchor.
        anchor: String,
        /// Before or after the anchor.
        side: Side,
        /// Empty rows to create.
        rows: i64,
        /// Empty columns to create.
        columns: i64,
        /// Case-sensitive anchor matching.
        match_case: bool,
    },
    /// Edit the row/column containing a unique cell anchor.
    Dimension {
        /// Operation (InsertTable is rejected here).
        verb: TableVerb,
        /// Literal unique text in the reference cell.
        cell: String,
        /// Below/right for insertion; unused for deletion.
        after: bool,
        /// Case-sensitive matching.
        match_case: bool,
    },
}

impl TableEdit {
    /// The operation this intent describes.
    #[must_use]
    pub const fn verb(&self) -> TableVerb {
        match self {
            Self::Insert { .. } => TableVerb::InsertTable,
            Self::Dimension { verb, .. } => *verb,
        }
    }

    pub(super) fn validate(&self) -> Result<(), String> {
        let anchor = match self {
            Self::Insert {
                anchor,
                rows,
                columns,
                ..
            } => {
                // Bound allocation explicitly; no unbounded empty grid creation.
                if *rows < 1
                    || *columns < 1
                    || rows.checked_mul(*columns).is_none_or(|n| n > 10_000)
                {
                    return Err(
                        "table must have positive dimensions and at most 10000 cells".into(),
                    );
                }
                anchor
            }
            Self::Dimension {
                verb: TableVerb::InsertTable,
                ..
            } => {
                return Err("table insertion requires dimensions and a body anchor".into());
            }
            Self::Dimension { cell, .. } => cell,
        };
        if anchor.is_empty() || anchor.contains(['\n', '\r']) {
            return Err("anchor must be nonempty and within one paragraph".into());
        }
        Ok(())
    }
}

/// A table operation's exact resolved effect, without user prose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TablePreview {
    /// The operation.
    pub operation: TableVerb,
    /// Insertion point for a new table, table start for dimension edits.
    pub location: Location,
    /// Reference row, for dimension edits.
    pub row_index: Option<i64>,
    /// Reference column, for dimension edits.
    pub column_index: Option<i64>,
    /// Existing rows (zero for a new table).
    pub rows_before: i64,
    /// Existing columns (zero for a new table).
    pub columns_before: i64,
    /// Resulting row count.
    pub rows_after: i64,
    /// Resulting column count.
    pub columns_after: i64,
    /// New row/column goes below/right of the reference cell.
    pub insert_after: bool,
    /// Google inserts a newline before a new table.
    pub preceding_newline: bool,
}

/// A table could not be safely addressed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "kebab-case")]
pub enum TableError {
    /// Anchor resolution refused the address.
    Anchor {
        /// Anchor refusal detail.
        error: AnchorError,
    },
    /// Anchor is outside the supported top-level paragraph/table boundary.
    UnsafeBoundary,
    /// Missing or inconsistent table/row/cell indices or dimensions.
    InvalidTable,
    /// A pending suggestion affects the table.
    SuggestedTable,
    /// A merged or nested table is outside this bounded tranche.
    UnsupportedTable,
    /// Removing the last dimension would delete the table.
    LastDimension,
    /// Invalid caller intent.
    InvalidIntent,
}

fn bounds(element: &StructuralElement, index: i64) -> bool {
    element
        .start_index
        .is_some_and(|start| start >= 1 && start <= index)
        && element.end_index.is_some_and(|end| index < end)
}

/// Build the single request and its preview together, before dry-run branches.
pub fn resolve(
    document: &Document,
    edit: &TableEdit,
) -> Result<(DocsRequest, TablePreview), TableError> {
    edit.validate().map_err(|_| TableError::InvalidIntent)?;
    let (needle, side, match_case) = match edit {
        TableEdit::Insert {
            anchor,
            side,
            match_case,
            ..
        } => (anchor, *side, *match_case),
        TableEdit::Dimension {
            cell, match_case, ..
        } => (cell, Side::Before, *match_case),
    };
    let point = anchor::resolve_insert(document, needle, side, "x", match_case)
        .map_err(|error| TableError::Anchor { error })?;
    let tabs = document.resolved_tabs();
    let body = tabs
        .iter()
        .find(|tab| tab.tab_id == point.tab_id.as_deref())
        .and_then(|tab| tab.body)
        .ok_or(TableError::UnsafeBoundary)?;
    let elements: Vec<_> = body
        .content
        .iter()
        .filter(|e| bounds(e, point.start_index))
        .collect();
    if elements.len() != 1 {
        return Err(TableError::UnsafeBoundary);
    }
    let element = elements[0];
    let verb = edit.verb();
    let mut preview = TablePreview {
        operation: verb,
        location: Location {
            index: point.start_index,
            tab_id: point.tab_id,
            segment_id: None,
        },
        row_index: None,
        column_index: None,
        rows_before: 0,
        columns_before: 0,
        rows_after: 0,
        columns_after: 0,
        insert_after: false,
        preceding_newline: false,
    };
    if let TableEdit::Insert { rows, columns, .. } = edit {
        if element.paragraph.is_none()
            || element.table.is_some()
            || element.table_of_contents.is_some()
            || element.section_break.is_some()
        {
            return Err(TableError::UnsafeBoundary);
        }
        preview.rows_after = *rows;
        preview.columns_after = *columns;
        preview.preceding_newline = true;
        return Ok((
            DocsRequest::InsertTable(InsertTableRequest {
                rows: *rows,
                columns: *columns,
                location: preview.location.clone(),
            }),
            preview,
        ));
    }
    let table = element.table.as_ref().ok_or(TableError::UnsafeBoundary)?;
    let json = serde_json::to_value(table).map_err(|_| TableError::InvalidTable)?;
    if table.has_pending_suggestions || super::types::has_suggestions(&json) {
        return Err(TableError::SuggestedTable);
    }
    let rows = table
        .rows
        .filter(|n| *n > 0)
        .ok_or(TableError::InvalidTable)?;
    let columns = table
        .columns
        .filter(|n| *n > 0)
        .ok_or(TableError::InvalidTable)?;
    if usize::try_from(rows).ok() != Some(table.table_rows.len()) {
        return Err(TableError::InvalidTable);
    }
    let start = element.start_index.ok_or(TableError::InvalidTable)?;
    let end = element.end_index.ok_or(TableError::InvalidTable)?;
    let mut previous_row_end = start + 1;
    let mut reference = None;
    for (r, row) in table.table_rows.iter().enumerate() {
        if usize::try_from(columns).ok() != Some(row.table_cells.len()) {
            return Err(TableError::UnsupportedTable);
        }
        let rs = row.start_index.ok_or(TableError::InvalidTable)?;
        let re = row.end_index.ok_or(TableError::InvalidTable)?;
        if rs < previous_row_end || re <= rs || re > end {
            return Err(TableError::InvalidTable);
        }
        previous_row_end = re;
        let mut previous_cell_end = rs + 1;
        for (c, cell) in row.table_cells.iter().enumerate() {
            let cs = cell.start_index.ok_or(TableError::InvalidTable)?;
            let ce = cell.end_index.ok_or(TableError::InvalidTable)?;
            if cs < previous_cell_end || ce <= cs || ce > re {
                return Err(TableError::InvalidTable);
            }
            previous_cell_end = ce;
            if cell.table_cell_style.as_ref().is_some_and(|style| {
                style.row_span.is_some_and(|n| n != 1) || style.column_span.is_some_and(|n| n != 1)
            }) {
                return Err(TableError::UnsupportedTable);
            }
            if cell.content.is_empty()
                || cell.content.iter().any(|e| {
                    e.paragraph.is_none()
                        || e.table.is_some()
                        || e.table_of_contents.is_some()
                        || e.section_break.is_some()
                })
            {
                return Err(TableError::UnsupportedTable);
            }
            let mut previous_paragraph_end = cs + 1;
            for paragraph in &cell.content {
                let ps = paragraph.start_index.ok_or(TableError::InvalidTable)?;
                let pe = paragraph.end_index.ok_or(TableError::InvalidTable)?;
                if ps != previous_paragraph_end || pe <= ps || pe > ce {
                    return Err(TableError::InvalidTable);
                }
                previous_paragraph_end = pe;
            }
            if previous_paragraph_end != ce {
                return Err(TableError::InvalidTable);
            }
            if cs <= preview.location.index && preview.location.index < ce {
                if reference.is_some() {
                    return Err(TableError::InvalidTable);
                }
                reference = Some((r as i64, c as i64));
            }
        }
    }
    let (row, column) = reference.ok_or(TableError::InvalidTable)?;
    preview.location.index = start;
    preview.row_index = Some(row);
    preview.column_index = Some(column);
    preview.rows_before = rows;
    preview.columns_before = columns;
    preview.rows_after = rows;
    preview.columns_after = columns;
    let cell = TableCellLocation {
        table_start_location: preview.location.clone(),
        row_index: row,
        column_index: column,
    };
    let TableEdit::Dimension { after, .. } = edit else {
        return Err(TableError::InvalidIntent);
    };
    let request = match verb {
        TableVerb::InsertRow => {
            preview.rows_after = rows.checked_add(1).ok_or(TableError::InvalidTable)?;
            preview.insert_after = *after;
            DocsRequest::InsertTableRow(InsertTableRowRequest {
                table_cell_location: cell,
                insert_below: *after,
            })
        }
        TableVerb::InsertColumn => {
            preview.columns_after = columns.checked_add(1).ok_or(TableError::InvalidTable)?;
            preview.insert_after = *after;
            DocsRequest::InsertTableColumn(InsertTableColumnRequest {
                table_cell_location: cell,
                insert_right: *after,
            })
        }
        TableVerb::DeleteRow => {
            if rows == 1 {
                return Err(TableError::LastDimension);
            }
            preview.rows_after -= 1;
            DocsRequest::DeleteTableRow(TableDimensionRequest {
                table_cell_location: cell,
            })
        }
        TableVerb::DeleteColumn => {
            if columns == 1 {
                return Err(TableError::LastDimension);
            }
            preview.columns_after -= 1;
            DocsRequest::DeleteTableColumn(TableDimensionRequest {
                table_cell_location: cell,
            })
        }
        TableVerb::InsertTable => return Err(TableError::InvalidIntent),
    };
    Ok((request, preview))
}

#[cfg(test)]
pub(crate) fn fixture() -> serde_json::Value {
    use serde_json::json;
    let mut index = 10;
    let start = index;
    index += 1;
    let mut rows = Vec::new();
    for names in [["😀 Alpha", "Beta"], ["Gamma", "Delta"]] {
        let rs = index;
        index += 1;
        let mut cells = Vec::new();
        for name in names {
            let cs = index;
            index += 1;
            let ps = index;
            let text = format!("{name}\n");
            index += text.encode_utf16().count() as i64;
            cells.push(json!({"startIndex":cs,"endIndex":index,"tableCellStyle":{"rowSpan":1,"columnSpan":1},"content":[{"startIndex":ps,"endIndex":index,"paragraph":{"elements":[{"startIndex":ps,"endIndex":index,"textRun":{"content":text}}]}}]}));
        }
        rows.push(json!({"startIndex":rs,"endIndex":index,"tableCells":cells}));
    }
    json!({"revisionId":"rev-table","tabs":[{"documentTab":{"body":{"content":[
        {"startIndex":1,"endIndex":10,"paragraph":{"elements":[{"startIndex":1,"endIndex":10,"textRun":{"content":"😀 Intro\n"}}]}},
        {"startIndex":start,"endIndex":index,"table":{"rows":2,"columns":2,"tableRows":rows}}
    ]}},"tabProperties":{"tabId":"child"}}]})
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::super::write_types::BatchUpdateDocumentRequest;
    use super::*;
    use serde_json::json;

    fn doc(value: serde_json::Value) -> Document {
        serde_json::from_value(value).unwrap()
    }
    fn dimension(verb: TableVerb) -> TableEdit {
        TableEdit::Dimension {
            verb,
            cell: "Alpha".into(),
            after: true,
            match_case: true,
        }
    }
    fn table(value: &mut serde_json::Value) -> &mut serde_json::Value {
        &mut value["tabs"][0]["documentTab"]["body"]["content"][1]["table"]
    }

    #[test]
    fn table_insertion_uses_utf16_indices_tab_identity_and_preceding_newline() {
        for (side, index) in [(Side::Before, 4), (Side::After, 9)] {
            let edit = TableEdit::Insert {
                anchor: "Intro".into(),
                side,
                rows: 2,
                columns: 3,
                match_case: true,
            };
            let (request, preview) = resolve(&doc(fixture()), &edit).unwrap();
            assert_eq!(preview.location.index, index);
            assert!(preview.preceding_newline);
            assert_eq!(
                (
                    preview.rows_before,
                    preview.columns_before,
                    preview.rows_after,
                    preview.columns_after
                ),
                (0, 0, 2, 3)
            );
            assert_eq!(
                serde_json::to_value(BatchUpdateDocumentRequest::new(request, "revision")).unwrap(),
                json!({"requests":[{"insertTable":{"rows":2,"columns":3,"location":{"index":index,"tabId":"child"}}}],"writeControl":{"requiredRevisionId":"revision"}})
            );
        }
    }

    #[test]
    fn all_dimension_wire_shapes_and_effects_are_exact() {
        for (verb, key, extra, dimensions) in [
            (
                TableVerb::InsertRow,
                "insertTableRow",
                Some("insertBelow"),
                (3, 2),
            ),
            (
                TableVerb::InsertColumn,
                "insertTableColumn",
                Some("insertRight"),
                (2, 3),
            ),
            (TableVerb::DeleteRow, "deleteTableRow", None, (1, 2)),
            (TableVerb::DeleteColumn, "deleteTableColumn", None, (2, 1)),
        ] {
            for after in [false, true] {
                let edit = TableEdit::Dimension {
                    verb,
                    cell: "Alpha".into(),
                    after,
                    match_case: true,
                };
                let (request, preview) = resolve(&doc(fixture()), &edit).unwrap();
                assert_eq!((preview.rows_after, preview.columns_after), dimensions);
                assert_eq!(
                    (preview.row_index, preview.column_index),
                    (Some(0), Some(0))
                );
                let wire =
                    serde_json::to_value(BatchUpdateDocumentRequest::new(request, "rev")).unwrap();
                let mut expected = json!({"tableCellLocation":{"tableStartLocation":{"index":10,"tabId":"child"},"rowIndex":0,"columnIndex":0}});
                if let Some(field) = extra {
                    expected[field] = json!(after);
                }
                assert_eq!(wire["requests"], json!([{key:expected}]));
                assert_eq!(wire["writeControl"], json!({"requiredRevisionId":"rev"}));
            }
        }
    }

    #[test]
    fn refuses_merged_nested_suggested_and_invalid_tables() {
        for (kind, expected) in [
            (0, TableError::UnsupportedTable),
            (1, TableError::UnsupportedTable),
            (2, TableError::SuggestedTable),
            (3, TableError::InvalidTable),
            (4, TableError::SuggestedTable),
            (5, TableError::UnsupportedTable),
        ] {
            let mut value = fixture();
            let t = table(&mut value);
            match kind {
                0 => t["tableRows"][0]["tableCells"][0]["tableCellStyle"]["rowSpan"] = json!(2),
                1 => t["tableRows"][1]["tableCells"][1]["content"][0]["table"] = json!({}),
                2 => t["tableRows"][1]["suggestedDeletionIds"] = json!(["suggestion"]),
                3 => t["tableRows"][0]["tableCells"][0]["endIndex"] = json!(1000),
                4 => {
                    t["tableRows"][1]["tableCells"][1]["content"][0]["paragraph"]["elements"][0]
                        ["textRun"]["suggestedInsertionIds"] = json!(["s"])
                }
                _ => {
                    t["tableRows"][1]["tableCells"]
                        .as_array_mut()
                        .unwrap()
                        .pop();
                }
            }
            assert_eq!(
                resolve(&doc(value), &dimension(TableVerb::DeleteRow)).unwrap_err(),
                expected,
                "kind {kind}"
            );
        }
    }

    #[test]
    fn refuses_last_dimension_and_body_cell_boundary_confusion() {
        let mut value = fixture();
        let t = table(&mut value);
        t["rows"] = json!(1);
        t["tableRows"].as_array_mut().unwrap().pop();
        assert_eq!(
            resolve(&doc(value), &dimension(TableVerb::DeleteRow)).unwrap_err(),
            TableError::LastDimension
        );
        let edit = TableEdit::Insert {
            anchor: "Alpha".into(),
            side: Side::After,
            rows: 1,
            columns: 1,
            match_case: true,
        };
        assert_eq!(
            resolve(&doc(fixture()), &edit).unwrap_err(),
            TableError::UnsafeBoundary
        );
        let edit = TableEdit::Dimension {
            verb: TableVerb::InsertRow,
            cell: "Intro".into(),
            after: false,
            match_case: true,
        };
        assert_eq!(
            resolve(&doc(fixture()), &edit).unwrap_err(),
            TableError::UnsafeBoundary
        );
        let mut value = fixture();
        let t = table(&mut value);
        t["columns"] = json!(1);
        for row in t["tableRows"].as_array_mut().unwrap() {
            row["tableCells"].as_array_mut().unwrap().pop();
        }
        assert_eq!(
            resolve(&doc(value), &dimension(TableVerb::DeleteColumn)).unwrap_err(),
            TableError::LastDimension
        );
    }

    #[test]
    fn invalid_missing_ambiguous_and_suggested_anchors_fail_closed() {
        for (needle, expected) in [
            ("", AnchorError::InvalidAnchor),
            ("missing", AnchorError::NotFound),
            ("a", AnchorError::Ambiguous { count: 5 }),
        ] {
            let edit = TableEdit::Dimension {
                verb: TableVerb::InsertRow,
                cell: needle.into(),
                after: false,
                match_case: true,
            };
            let error = resolve(&doc(fixture()), &edit).unwrap_err();
            if needle.is_empty() {
                assert_eq!(error, TableError::InvalidIntent);
            } else {
                assert_eq!(error, TableError::Anchor { error: expected });
            }
        }
        let mut value = fixture();
        table(&mut value)["tableRows"][0]["tableCells"][0]["content"][0]["paragraph"]["elements"]
            [0]["textRun"]["suggestedDeletionIds"] = json!(["s"]);
        assert_eq!(
            resolve(&doc(value), &dimension(TableVerb::InsertRow)).unwrap_err(),
            TableError::Anchor {
                error: AnchorError::SuggestedContent
            }
        );
    }
    #[test]
    fn unmodelled_inline_suggestions_and_bad_cell_paragraph_bounds_fail_closed() {
        let mut value = fixture();
        table(&mut value)["tableRows"][1]["tableCells"][1]["content"][0]["paragraph"]
            ["suggestedParagraphStyleChanges"] = json!({"s": {}});
        let document = doc(value);
        assert_eq!(
            resolve(&document, &dimension(TableVerb::DeleteColumn)).unwrap_err(),
            TableError::SuggestedTable
        );
        let mut value = fixture();
        table(&mut value)["tableRows"][0]["tableCells"][0]["startIndex"] = json!(1);
        assert_eq!(
            resolve(&doc(value), &dimension(TableVerb::DeleteColumn)).unwrap_err(),
            TableError::InvalidTable
        );
        let mut value = fixture();
        value["tabs"][0]["tabProperties"]["tabId"] = json!("");
        assert_eq!(
            resolve(&doc(value), &dimension(TableVerb::InsertRow)).unwrap_err(),
            TableError::Anchor {
                error: AnchorError::InvalidIndices
            }
        );
    }
}
