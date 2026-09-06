//! Flattening a document's structural tree into an index-ordered list.
//!
//! Pure and network-free, the way `crate::drive::sheets::a1` is: this is the
//! module where the subtle bugs live, so it must be testable without a
//! wiremock server. `body.content[]` is a recursive forest — a table holds
//! rows, which hold cells, which hold more structural elements, including
//! more tables — and both `drive docs info` and `drive docs read` need it
//! walked exactly once, the same way.
//!
//! Two rules govern the walk and both matter:
//!
//! - **A container is emitted before its contents.** A table yields one
//!   [`ElementKind::Table`] element carrying the table's own `[start, end)`
//!   — which is what addresses the whole table — followed by each cell's
//!   paragraphs. Without the container, a table is invisible in the flat
//!   list except as an unexplained index gap.
//! - **Text is trimmed of its paragraph terminator; indices never are.**
//!   Every Docs paragraph's final `textRun` ends in `\n`, and that newline
//!   occupies one index unit. Rendering it would put a blank line after
//!   every row; subtracting it from `end_index` would make the reported
//!   indices lie about the document. So exactly one of the two is adjusted.
//!
//! This module deliberately **computes no index of its own** — it only
//! reports what the server sent. Docs indices are UTF-16 code units and Rust
//! strings are UTF-8, so any index derived here would be wrong for
//! astral-plane characters in a way that corrupts silently rather than
//! failing. The write path avoids the problem the same way, by addressing
//! insertions with `endOfSegmentLocation` rather than a number.

use serde::Serialize;

use crate::drive::docs::types::{Body, ResolvedTab, StructuralElement};

/// What kind of thing a flattened element is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ElementKind {
    /// A paragraph, possibly a heading or a list item.
    Paragraph,
    /// A table container. Its cells' paragraphs follow it.
    Table,
    /// A section break.
    SectionBreak,
    /// A generated table of contents. Its entries follow it.
    TableOfContents,
}

impl ElementKind {
    /// The kebab-case label, matching the serde wire form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Paragraph => "paragraph",
            Self::Table => "table",
            Self::SectionBreak => "section-break",
            Self::TableOfContents => "table-of-contents",
        }
    }
}

/// One structural element, flattened out of the document tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DocElement {
    /// Inclusive start, in UTF-16 code units, within this tab's body segment.
    pub start_index: i64,
    /// Exclusive end, likewise.
    pub end_index: i64,
    /// Which kind of element this is.
    pub kind: ElementKind,
    /// Structural address: `[3]` is the 4th top-level element, `[3, 0, 1, 0]`
    /// the 1st element of row 0, cell 1 of that table. `path.len() - 1` is
    /// the nesting depth, which is what a renderer indents by.
    pub path: Vec<usize>,
    /// A paragraph's `namedStyleType` (`HEADING_1`, `NORMAL_TEXT`, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
    /// A list item's `listId`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_id: Option<String>,
    /// A table's row count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<i64>,
    /// A table's column count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub columns: Option<i64>,
    /// A paragraph's text, with its terminating newline removed. Empty for
    /// every non-paragraph kind.
    pub text: String,
}

impl DocElement {
    /// Nesting depth: `0` for a top-level element.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.path.len().saturating_sub(1)
    }
}

/// Walks `body.content` depth-first, emitting every structural element in
/// document-index order, containers before their contents.
#[must_use]
pub fn flatten(body: &Body) -> Vec<DocElement> {
    let mut out = Vec::new();
    walk(&body.content, &mut Vec::new(), &mut out);
    out
}

/// Recursive half of [`flatten`]. `path` is the address stack, pushed and
/// popped around each descent.
fn walk(content: &[StructuralElement], path: &mut Vec<usize>, out: &mut Vec<DocElement>) {
    for (index, element) in content.iter().enumerate() {
        path.push(index);
        emit(element, path, out);
        path.pop();
    }
}

/// Emits one element, then descends into it if it is a container.
fn emit(element: &StructuralElement, path: &mut Vec<usize>, out: &mut Vec<DocElement>) {
    let base = |kind: ElementKind| DocElement {
        start_index: element.start_index(),
        end_index: element.end_index(),
        kind,
        path: path.clone(),
        style: None,
        list_id: None,
        rows: None,
        columns: None,
        text: String::new(),
    };

    if let Some(paragraph) = &element.paragraph {
        out.push(DocElement {
            style: paragraph.named_style_type().map(ToString::to_string),
            list_id: paragraph.bullet.as_ref().and_then(|b| b.list_id.clone()),
            text: strip_paragraph_terminator(&paragraph.text()),
            ..base(ElementKind::Paragraph)
        });
        return;
    }

    if let Some(table) = &element.table {
        out.push(DocElement {
            rows: table.rows,
            columns: table.columns,
            ..base(ElementKind::Table)
        });
        for (row_index, row) in table.table_rows.iter().enumerate() {
            path.push(row_index);
            for (cell_index, cell) in row.table_cells.iter().enumerate() {
                path.push(cell_index);
                walk(&cell.content, path, out);
                path.pop();
            }
            path.pop();
        }
        return;
    }

    if let Some(toc) = &element.table_of_contents {
        out.push(base(ElementKind::TableOfContents));
        walk(&toc.content, path, out);
        return;
    }

    if element.section_break.is_some() {
        out.push(base(ElementKind::SectionBreak));
    }
    // An element matching none of the above is a variant this crate does not
    // model. Dropping it is deliberate: emitting a kindless row would be
    // noise, and inventing a kind for it would be a guess.
}

/// Removes the single trailing `\n` every Docs paragraph carries.
///
/// Exactly one newline, not a `trim_end`: a paragraph whose text genuinely
/// ends in a soft line break (`\v`) or trailing spaces keeps them, because
/// they are content the user typed.
fn strip_paragraph_terminator(text: &str) -> String {
    text.strip_suffix('\n').unwrap_or(text).to_string()
}

/// A heading paragraph, for the `docs info` outline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Heading {
    /// The `namedStyleType` that made it a heading.
    pub style: String,
    /// Inclusive start, in UTF-16 code units.
    pub start_index: i64,
    /// Exclusive end, likewise.
    pub end_index: i64,
    /// The heading's text, terminator stripped.
    pub text: String,
}

/// Aggregate counts and the heading outline for one tab.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TabOutline {
    /// The tab's id, absent for a legacy single-body document.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
    /// The tab's title, likewise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Depth in the tab tree.
    pub nesting_level: i64,
    /// The tab's length in UTF-16 code units — the highest `endIndex` seen,
    /// and so the upper bound on any valid index within it.
    pub characters: i64,
    /// How many paragraphs it contains, at every nesting depth.
    pub paragraphs: usize,
    /// How many tables.
    pub tables: usize,
    /// How many section breaks.
    pub section_breaks: usize,
    /// How many generated tables of contents.
    pub tables_of_contents: usize,
    /// Every heading, in document order.
    pub headings: Vec<Heading>,
}

/// Whether a `namedStyleType` denotes a heading for outline purposes.
///
/// `TITLE` and `SUBTITLE` count: they are what a document's top-level
/// structure actually uses, and omitting them makes the outline of a
/// well-formed document start at its second section.
fn is_heading_style(style: &str) -> bool {
    style.starts_with("HEADING_") || style == "TITLE" || style == "SUBTITLE"
}

/// Counts and outlines one tab.
#[must_use]
pub fn outline(tab: &ResolvedTab<'_>) -> TabOutline {
    let mut out = TabOutline {
        tab_id: tab.tab_id.map(ToString::to_string),
        title: tab.title.map(ToString::to_string),
        nesting_level: tab.nesting_level,
        ..TabOutline::default()
    };
    let Some(body) = tab.body else {
        return out;
    };
    for element in flatten(body) {
        out.characters = out.characters.max(element.end_index);
        match element.kind {
            ElementKind::Paragraph => {
                out.paragraphs += 1;
                if let Some(style) = element.style.as_deref().filter(|s| is_heading_style(s)) {
                    out.headings.push(Heading {
                        style: style.to_string(),
                        start_index: element.start_index,
                        end_index: element.end_index,
                        text: element.text.clone(),
                    });
                }
            }
            ElementKind::Table => out.tables += 1,
            ElementKind::SectionBreak => out.section_breaks += 1,
            ElementKind::TableOfContents => out.tables_of_contents += 1,
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::docs::types::Document;

    fn body(json: serde_json::Value) -> Body {
        serde_json::from_value(json).unwrap()
    }

    fn paragraph(start: i64, end: i64, text: &str, style: &str) -> serde_json::Value {
        serde_json::json!({
            "startIndex": start, "endIndex": end,
            "paragraph": {
                "elements": [{"textRun": {"content": text}}],
                "paragraphStyle": {"namedStyleType": style},
            },
        })
    }

    #[test]
    fn flatten_of_an_empty_body_is_empty() {
        assert!(flatten(&body(serde_json::json!({"content": []}))).is_empty());
    }

    #[test]
    fn flatten_emits_elements_in_index_order() {
        let elements = flatten(&body(serde_json::json!({"content": [
            {"endIndex": 1, "sectionBreak": {}},
            paragraph(1, 10, "Overview\n", "HEADING_1"),
            paragraph(10, 20, "Body text\n", "NORMAL_TEXT"),
        ]})));
        assert_eq!(
            elements.iter().map(|e| e.start_index).collect::<Vec<_>>(),
            vec![0, 1, 10]
        );
        assert_eq!(elements[0].kind, ElementKind::SectionBreak);
        assert_eq!(elements[1].kind, ElementKind::Paragraph);
    }

    /// The paragraph terminator is dropped from `text` and kept in
    /// `end_index`. Adjusting both, or neither, is the bug this pins.
    #[test]
    fn flatten_strips_the_terminator_from_text_but_not_from_end_index() {
        let elements = flatten(&body(serde_json::json!({
            "content": [paragraph(1, 10, "Overview\n", "HEADING_1")],
        })));
        assert_eq!(elements[0].text, "Overview");
        assert_eq!(elements[0].end_index, 10);
    }

    /// Trailing content the user actually typed survives; only the single
    /// paragraph terminator goes.
    #[test]
    fn flatten_strips_exactly_one_newline_not_a_trim_end() {
        let elements = flatten(&body(serde_json::json!({
            "content": [paragraph(1, 10, "text  \u{b}\n", "NORMAL_TEXT")],
        })));
        assert_eq!(elements[0].text, "text  \u{b}");
    }

    #[test]
    fn flatten_joins_multiple_text_runs_in_one_paragraph() {
        let elements = flatten(&body(serde_json::json!({"content": [{
            "startIndex": 1, "endIndex": 20,
            "paragraph": {"elements": [
                {"textRun": {"content": "Hello "}},
                {"textRun": {"content": "bold"}},
                {"textRun": {"content": " world\n"}},
            ]},
        }]})));
        assert_eq!(elements[0].text, "Hello bold world");
    }

    /// An inline image consumes index space but contributes no text, so the
    /// paragraph's text is shorter than its span. That is correct.
    #[test]
    fn flatten_counts_an_inline_object_as_index_space_with_no_text() {
        let elements = flatten(&body(serde_json::json!({"content": [{
            "startIndex": 1, "endIndex": 5,
            "paragraph": {"elements": [
                {"textRun": {"content": "a"}},
                {"inlineObjectElement": {"inlineObjectId": "kix.1"}},
                {"textRun": {"content": "b\n"}},
            ]},
        }]})));
        assert_eq!(elements[0].text, "ab");
        assert_eq!(elements[0].end_index - elements[0].start_index, 4);
    }

    /// Without the container row a table is invisible in the flat list
    /// except as an index gap, and nothing addresses the table as a whole.
    #[test]
    fn flatten_emits_the_table_container_before_its_cell_paragraphs() {
        let elements = flatten(&body(serde_json::json!({"content": [{
            "startIndex": 1, "endIndex": 40,
            "table": {
                "rows": 1, "columns": 2,
                "tableRows": [{"tableCells": [
                    {"content": [paragraph(3, 10, "Name\n", "NORMAL_TEXT")]},
                    {"content": [paragraph(10, 20, "Value\n", "NORMAL_TEXT")]},
                ]}],
            },
        }]})));
        assert_eq!(elements.len(), 3);
        assert_eq!(elements[0].kind, ElementKind::Table);
        assert_eq!(elements[0].rows, Some(1));
        assert_eq!(elements[0].columns, Some(2));
        assert_eq!(elements[1].text, "Name");
        assert_eq!(elements[2].text, "Value");
    }

    #[test]
    fn flatten_records_a_path_for_a_cell_paragraph() {
        let elements = flatten(&body(serde_json::json!({"content": [{
            "startIndex": 1, "endIndex": 40,
            "table": {"rows": 1, "columns": 2, "tableRows": [{"tableCells": [
                {"content": [paragraph(3, 10, "a\n", "NORMAL_TEXT")]},
                {"content": [paragraph(10, 20, "b\n", "NORMAL_TEXT")]},
            ]}]},
        }]})));
        assert_eq!(elements[0].path, vec![0]);
        assert_eq!(elements[0].depth(), 0);
        // [table, row 0, cell 1, element 0]
        assert_eq!(elements[2].path, vec![0, 0, 1, 0]);
        assert_eq!(elements[2].depth(), 3);
    }

    #[test]
    fn flatten_recurses_into_a_nested_table() {
        let elements = flatten(&body(serde_json::json!({"content": [{
            "startIndex": 1, "endIndex": 60,
            "table": {"rows": 1, "columns": 1, "tableRows": [{"tableCells": [{"content": [{
                "startIndex": 3, "endIndex": 50,
                "table": {"rows": 1, "columns": 1, "tableRows": [{"tableCells": [{
                    "content": [paragraph(5, 12, "deep\n", "NORMAL_TEXT")],
                }]}]},
            }]}]}]},
        }]})));
        let kinds: Vec<_> = elements.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ElementKind::Table,
                ElementKind::Table,
                ElementKind::Paragraph
            ]
        );
        assert_eq!(elements[2].text, "deep");
        // outer table, row, cell, inner table, row, cell, paragraph — a
        // 7-segment path, so 6 levels below the top.
        assert_eq!(elements[2].path, vec![0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(elements[2].depth(), 6);
    }

    #[test]
    fn flatten_records_a_list_id_for_a_bullet() {
        let elements = flatten(&body(serde_json::json!({"content": [{
            "startIndex": 1, "endIndex": 8,
            "paragraph": {
                "elements": [{"textRun": {"content": "item\n"}}],
                "bullet": {"listId": "kix.l1", "nestingLevel": 0},
            },
        }]})));
        assert_eq!(elements[0].list_id.as_deref(), Some("kix.l1"));
    }

    #[test]
    fn flatten_walks_a_table_of_contents() {
        let elements = flatten(&body(serde_json::json!({"content": [{
            "startIndex": 1, "endIndex": 30,
            "tableOfContents": {"content": [paragraph(2, 12, "Overview\n", "NORMAL_TEXT")]},
        }]})));
        assert_eq!(elements[0].kind, ElementKind::TableOfContents);
        assert_eq!(elements[1].text, "Overview");
    }

    /// Indices come from the server and are UTF-16 code units, so an emoji
    /// spans two of them while being one `char`. Nothing here recomputes
    /// them; this pins that they are passed through untouched.
    #[test]
    fn indices_are_utf16_code_units_not_chars() {
        let elements = flatten(&body(serde_json::json!({
            // "😀" is 1 char, 2 UTF-16 code units, 4 UTF-8 bytes, so the
            // server reports a span of 3 for the emoji plus its newline.
            "content": [paragraph(1, 4, "\u{1F600}\n", "NORMAL_TEXT")],
        })));
        assert_eq!(elements[0].end_index - elements[0].start_index, 3);
        assert_eq!(elements[0].text.chars().count(), 1);
        assert_eq!(elements[0].text.encode_utf16().count(), 2);
        assert_eq!(elements[0].text.len(), 4);
    }

    fn outline_of(json: serde_json::Value) -> TabOutline {
        let doc: Document = serde_json::from_value(json).unwrap();
        let tabs = doc.resolved_tabs();
        outline(&tabs[0])
    }

    #[test]
    fn outline_counts_each_element_kind() {
        let counts = outline_of(serde_json::json!({"body": {"content": [
            {"endIndex": 1, "sectionBreak": {}},
            paragraph(1, 10, "A\n", "HEADING_1"),
            paragraph(10, 20, "b\n", "NORMAL_TEXT"),
            {"startIndex": 20, "endIndex": 40, "table": {
                "rows": 1, "columns": 1,
                "tableRows": [{"tableCells": [{"content": [
                    paragraph(22, 30, "c\n", "NORMAL_TEXT")]}]}],
            }},
        ]}}));
        assert_eq!(counts.section_breaks, 1);
        assert_eq!(counts.tables, 1);
        // Three paragraphs: two top-level plus the one inside the cell.
        assert_eq!(counts.paragraphs, 3);
    }

    #[test]
    fn outline_lists_headings_in_document_order_with_their_index_ranges() {
        let counts = outline_of(serde_json::json!({"body": {"content": [
            paragraph(1, 10, "Overview\n", "HEADING_1"),
            paragraph(10, 20, "prose\n", "NORMAL_TEXT"),
            paragraph(20, 30, "Goals\n", "HEADING_2"),
        ]}}));
        assert_eq!(counts.headings.len(), 2);
        assert_eq!(counts.headings[0].text, "Overview");
        assert_eq!(counts.headings[0].style, "HEADING_1");
        assert_eq!(counts.headings[0].start_index, 1);
        assert_eq!(counts.headings[1].text, "Goals");
        assert_eq!(counts.headings[1].end_index, 30);
    }

    /// A well-formed document's top-level structure uses `TITLE`; omitting
    /// it would make the outline start at the second section.
    #[test]
    fn outline_treats_title_and_subtitle_as_headings() {
        let counts = outline_of(serde_json::json!({"body": {"content": [
            paragraph(1, 10, "The Title\n", "TITLE"),
            paragraph(10, 20, "A subtitle\n", "SUBTITLE"),
            paragraph(20, 30, "prose\n", "NORMAL_TEXT"),
        ]}}));
        let styles: Vec<_> = counts.headings.iter().map(|h| h.style.as_str()).collect();
        assert_eq!(styles, vec!["TITLE", "SUBTITLE"]);
    }

    #[test]
    fn outline_characters_is_the_highest_end_index() {
        let counts = outline_of(serde_json::json!({"body": {"content": [
            paragraph(1, 10, "a\n", "NORMAL_TEXT"),
            paragraph(10, 137, "b\n", "NORMAL_TEXT"),
        ]}}));
        assert_eq!(counts.characters, 137);
    }

    #[test]
    fn outline_of_an_empty_tab_is_all_zeroes() {
        let counts = outline_of(serde_json::json!({"body": {"content": []}}));
        assert_eq!(counts.characters, 0);
        assert_eq!(counts.paragraphs, 0);
        assert!(counts.headings.is_empty());
    }

    #[test]
    fn outline_carries_the_tabs_identity() {
        let doc: Document = serde_json::from_value(serde_json::json!({"tabs": [{
            "tabProperties": {"tabId": "t.0", "title": "Overview", "nestingLevel": 0},
            "documentTab": {"body": {"content": [paragraph(1, 10, "a\n", "NORMAL_TEXT")]}},
        }]}))
        .unwrap();
        let counts = outline(&doc.resolved_tabs()[0]);
        assert_eq!(counts.tab_id.as_deref(), Some("t.0"));
        assert_eq!(counts.title.as_deref(), Some("Overview"));
        assert_eq!(counts.paragraphs, 1);
    }

    #[test]
    fn element_kind_labels_match_the_serde_wire_form() {
        for kind in [
            ElementKind::Paragraph,
            ElementKind::Table,
            ElementKind::SectionBreak,
            ElementKind::TableOfContents,
        ] {
            let wire = serde_json::to_value(kind).unwrap();
            assert_eq!(wire.as_str().unwrap(), kind.as_str());
        }
    }
}
