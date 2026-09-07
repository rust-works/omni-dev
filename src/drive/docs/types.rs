//! Wire types for the Docs v1 REST API.
//!
//! Field naming follows Docs' camelCase JSON via per-field
//! `#[serde(rename = "...")]`, mirroring `src/drive/types.rs` and
//! `src/drive/sheets/types.rs`. Only the subset the CLI actually renders is
//! modelled; every unmodelled field is tolerated and dropped, so a Google
//! response gaining a field never breaks a parse.
//!
//! Four shapes here are load-bearing and easy to get wrong:
//!
//! - **`startIndex` is absent, not `0`, for the first element.** Docs
//!   serialises proto3 JSON, which omits zero-valued integers, so every real
//!   `documents.get` has `body.content[0]` (the leading `sectionBreak`) with
//!   no `startIndex` key at all. Every index field is therefore
//!   `Option<i64>`, read through the `start_index()`/`end_index()`
//!   accessors. This is the direct analogue of `ValueRange::values` being
//!   absent-not-empty for a blank sheet.
//! - **Indices are UTF-16 code units, and `endIndex` is exclusive.** Not
//!   chars, not bytes. Any code deriving an index from a `&str` must use
//!   `s.encode_utf16().count()`. The difference is invisible in ASCII and
//!   wrong the moment a document contains an emoji or a CJK astral
//!   character, so nothing in this crate computes one — see
//!   `crate::drive::docs::structure`.
//! - **An empty message still serialises as `{}` when present.** A
//!   `sectionBreak` with no fields set arrives as `"sectionBreak": {}`, so
//!   the union variants this module does not inspect are modelled as
//!   `Option<serde_json::Value>`: `Some(Object({}))` when present, `None`
//!   when absent, which is exactly the discriminator semantics needed.
//! - **`tabs` and `body` are mutually exclusive.** With
//!   `includeTabsContent=true` the content lives under `tabs[]` and the
//!   top-level `body` is absent; without it, only `body` is populated. That
//!   is normalised once, in [`Document::resolved_tabs`], rather than at
//!   every call site.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// A document's full structural model, from `documents.get`.
///
/// Deliberately **not** `fields`-masked when fetched — unlike
/// `spreadsheets.get`, whose mask exists to keep every cell out of the
/// response. A Docs mask has to spell nesting depth out literally, and a
/// table may contain a table to arbitrary depth, so any fixed-depth mask
/// silently drops document text below its deepest named level. See
/// `crate::drive::docs::api`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Document {
    /// The document's id (echoes the one requested).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "documentId"
    )]
    pub document_id: Option<String>,
    /// The document's display title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The revision this response reflects.
    ///
    /// Google populates this **only for callers with edit access**, so a
    /// read-only caller sees `None`. It is the `writeControl`
    /// `requiredRevisionId` token a later `documents.batchUpdate` presents
    /// to refuse a write against a document that moved underneath it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "revisionId"
    )]
    pub revision_id: Option<String>,
    /// Legacy single-tab content, populated only when the request did not
    /// ask for tab content. Mutually exclusive with [`Self::tabs`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Body>,
    /// The tab tree, populated only when `includeTabsContent=true`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tabs: Vec<Tab>,
    /// Named ranges, keyed by name.
    ///
    /// The *stable* way to address a region: an index shifts on every
    /// insertion, a named range's name does not.
    #[serde(
        default,
        skip_serializing_if = "HashMap::is_empty",
        rename = "namedRanges"
    )]
    pub named_ranges: HashMap<String, NamedRanges>,
}

/// One tab's content, with the legacy single-`body` response normalised into
/// the same shape so callers never branch on which form arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedTab<'a> {
    /// The tab's id, or `None` for a legacy single-body document.
    pub tab_id: Option<&'a str>,
    /// The tab's title, or `None` for a legacy single-body document.
    pub title: Option<&'a str>,
    /// Depth in the tab tree; `0` for a top-level tab.
    pub nesting_level: i64,
    /// The tab's body, when it has one.
    pub body: Option<&'a Body>,
}

impl Document {
    /// The document title, or `""` when absent.
    #[must_use]
    pub fn title(&self) -> &str {
        self.title.as_deref().unwrap_or("")
    }

    /// Every tab's content, depth-first through `childTabs`, falling back to
    /// the legacy top-level [`Self::body`] as a single anonymous tab.
    ///
    /// The fallback is what makes the `tabs`-xor-`body` split invisible to
    /// callers. A document fetched without `includeTabsContent` yields
    /// exactly one `ResolvedTab` whose `tab_id` and `title` are `None`.
    #[must_use]
    pub fn resolved_tabs(&self) -> Vec<ResolvedTab<'_>> {
        if self.tabs.is_empty() {
            return self.body.as_ref().map_or_else(Vec::new, |body| {
                vec![ResolvedTab {
                    tab_id: None,
                    title: None,
                    nesting_level: 0,
                    body: Some(body),
                }]
            });
        }
        let mut out = Vec::new();
        for tab in &self.tabs {
            push_tab(tab, 0, &mut out);
        }
        out
    }
}

/// Walks one tab and its `childTabs` depth-first, parent before children.
fn push_tab<'a>(tab: &'a Tab, depth: i64, out: &mut Vec<ResolvedTab<'a>>) {
    let props = tab.tab_properties.as_ref();
    out.push(ResolvedTab {
        tab_id: props.and_then(|p| p.tab_id.as_deref()),
        title: props.and_then(|p| p.title.as_deref()),
        // Prefer the server's own nesting level when it sent one; fall back
        // to the walk depth, which agrees with it for every well-formed
        // response and keeps a truncated one self-consistent.
        nesting_level: props.and_then(|p| p.nesting_level).unwrap_or(depth),
        body: tab.document_tab.as_ref().and_then(|dt| dt.body.as_ref()),
    });
    for child in &tab.child_tabs {
        push_tab(child, depth + 1, out);
    }
}

/// One tab of a document.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Tab {
    /// This tab's identity and position.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "tabProperties"
    )]
    pub tab_properties: Option<TabProperties>,
    /// The tab's content, when it is a document tab.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "documentTab"
    )]
    pub document_tab: Option<DocumentTab>,
    /// Nested tabs, recursively.
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "childTabs")]
    pub child_tabs: Vec<Self>,
}

/// A tab's identity and position within the tab tree.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TabProperties {
    /// The tab's id — a required component of every Docs edit address in a
    /// multi-tab document.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "tabId")]
    pub tab_id: Option<String>,
    /// The tab's display title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Zero-based position among its siblings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<i64>,
    /// Depth in the tab tree; `0` for a top-level tab.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "nestingLevel"
    )]
    pub nesting_level: Option<i64>,
    /// The parent tab's id, absent for a top-level tab.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "parentTabId"
    )]
    pub parent_tab_id: Option<String>,
}

/// The document-flavoured content of a tab.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DocumentTab {
    /// The tab's body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Body>,
    /// Named ranges scoped to this tab.
    #[serde(
        default,
        skip_serializing_if = "HashMap::is_empty",
        rename = "namedRanges"
    )]
    pub named_ranges: HashMap<String, NamedRanges>,
}

/// A document body — the main index segment.
///
/// Headers, footers and footnotes live in their own segments, addressed by
/// `segmentId`, and are **not** returned here. A Doc's header text is
/// therefore invisible to `drive docs read`; that is a documented gap, the
/// Docs analogue of "export gives you the first sheet only".
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Body {
    /// The body's structural elements, in index order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<StructuralElement>,
}

/// One structural element: exactly one of the variant fields is present, and
/// which one is the discriminator.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct StructuralElement {
    /// Inclusive start, in UTF-16 code units. Absent means `0`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "startIndex"
    )]
    pub start_index: Option<i64>,
    /// Exclusive end, in UTF-16 code units.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "endIndex")]
    pub end_index: Option<i64>,
    /// A paragraph.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paragraph: Option<Paragraph>,
    /// A table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table: Option<Table>,
    /// A table of contents.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "tableOfContents"
    )]
    pub table_of_contents: Option<TableOfContents>,
    /// A section break. Carries nothing this crate reads, so it is kept as
    /// raw JSON purely as a present/absent discriminator.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "sectionBreak"
    )]
    pub section_break: Option<serde_json::Value>,
}

impl StructuralElement {
    /// Inclusive start index, treating an absent field as `0`.
    #[must_use]
    pub fn start_index(&self) -> i64 {
        self.start_index.unwrap_or(0)
    }

    /// Exclusive end index, treating an absent field as `0`.
    #[must_use]
    pub fn end_index(&self) -> i64 {
        self.end_index.unwrap_or(0)
    }
}

/// A paragraph and its inline elements.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Paragraph {
    /// The paragraph's inline elements, in index order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub elements: Vec<ParagraphElement>,
    /// The paragraph's style, notably its `namedStyleType`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "paragraphStyle"
    )]
    pub paragraph_style: Option<ParagraphStyle>,
    /// Present when the paragraph is a list item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bullet: Option<Bullet>,
}

impl Paragraph {
    /// The paragraph's text, concatenating every `textRun` in order.
    ///
    /// Concatenation is not optional: a paragraph with one bolded word is
    /// three text runs, and reading only the first silently truncates it.
    ///
    /// The trailing `\n` every Docs paragraph carries is **kept** here —
    /// stripping is the renderer's job, because the newline occupies one
    /// index unit and dropping it from the text without dropping it from
    /// `end_index` is what makes the two disagree.
    #[must_use]
    pub fn text(&self) -> String {
        self.elements
            .iter()
            .filter_map(|el| el.text_run.as_ref())
            .map(|run| run.content.as_str())
            .collect()
    }

    /// The paragraph's `namedStyleType` (`HEADING_1`, `NORMAL_TEXT`, …).
    #[must_use]
    pub fn named_style_type(&self) -> Option<&str> {
        self.paragraph_style
            .as_ref()
            .and_then(|style| style.named_style_type.as_deref())
    }
}

/// A paragraph's style.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParagraphStyle {
    /// `HEADING_1`, `TITLE`, `NORMAL_TEXT`, …
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "namedStyleType"
    )]
    pub named_style_type: Option<String>,
    /// The heading's id, when this paragraph is a heading.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "headingId")]
    pub heading_id: Option<String>,
}

/// A list-item marker.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Bullet {
    /// The list this item belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "listId")]
    pub list_id: Option<String>,
    /// Nesting depth within that list.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "nestingLevel"
    )]
    pub nesting_level: Option<i64>,
}

/// One inline element of a paragraph.
///
/// Only `textRun` carries text this crate reads. The rest are modelled as
/// present/absent so an element that consumes index space without
/// contributing text — an inline image, a page break — is not mistaken for
/// nothing at all.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParagraphElement {
    /// Inclusive start, in UTF-16 code units. Absent means `0`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "startIndex"
    )]
    pub start_index: Option<i64>,
    /// Exclusive end, in UTF-16 code units.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "endIndex")]
    pub end_index: Option<i64>,
    /// A run of text with uniform styling.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "textRun")]
    pub text_run: Option<TextRun>,
    /// An inline image or drawing. Consumes one index unit, contributes no
    /// text — so its paragraph's text is shorter than its index span, which
    /// is correct and must not be "fixed".
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "inlineObjectElement"
    )]
    pub inline_object_element: Option<InlineObjectElement>,
    /// A page break.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "pageBreak")]
    pub page_break: Option<serde_json::Value>,
    /// A column break.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "columnBreak"
    )]
    pub column_break: Option<serde_json::Value>,
    /// A horizontal rule.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "horizontalRule"
    )]
    pub horizontal_rule: Option<serde_json::Value>,
    /// A footnote reference.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "footnoteReference"
    )]
    pub footnote_reference: Option<serde_json::Value>,
    /// Auto-text such as a page number.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "autoText")]
    pub auto_text: Option<serde_json::Value>,
    /// An equation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equation: Option<serde_json::Value>,
    /// A person mention.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub person: Option<serde_json::Value>,
    /// A rich link chip.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "richLink")]
    pub rich_link: Option<serde_json::Value>,
}

/// A run of text with uniform styling.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TextRun {
    /// The run's text, including any trailing newline.
    #[serde(default)]
    pub content: String,
}

/// A reference to an inline object.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct InlineObjectElement {
    /// The object's id, resolvable against the document's `inlineObjects`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "inlineObjectId"
    )]
    pub inline_object_id: Option<String>,
}

/// A table.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Table {
    /// Row count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<i64>,
    /// Column count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub columns: Option<i64>,
    /// The table's rows.
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "tableRows")]
    pub table_rows: Vec<TableRow>,
}

/// One row of a table.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TableRow {
    /// Inclusive start, in UTF-16 code units.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "startIndex"
    )]
    pub start_index: Option<i64>,
    /// Exclusive end, in UTF-16 code units.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "endIndex")]
    pub end_index: Option<i64>,
    /// The row's cells.
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "tableCells")]
    pub table_cells: Vec<TableCell>,
}

/// One cell of a table row.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TableCell {
    /// Inclusive start, in UTF-16 code units.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "startIndex"
    )]
    pub start_index: Option<i64>,
    /// Exclusive end, in UTF-16 code units.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "endIndex")]
    pub end_index: Option<i64>,
    /// The cell's own structural elements — where the recursion lives, since
    /// a cell may contain another table.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<StructuralElement>,
}

/// A table of contents.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TableOfContents {
    /// The generated entries, as structural elements.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<StructuralElement>,
}

/// Every named range sharing one name.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct NamedRanges {
    /// The shared name.
    #[serde(default)]
    pub name: String,
    /// The ranges carrying it.
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "namedRanges")]
    pub named_ranges: Vec<NamedRange>,
}

/// One named range.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct NamedRange {
    /// The range's id.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "namedRangeId"
    )]
    pub named_range_id: Option<String>,
    /// The range's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The index spans it covers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ranges: Vec<Range>,
}

/// An index span within one segment of one tab.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Range {
    /// The segment; empty/absent means the document body.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "segmentId")]
    pub segment_id: Option<String>,
    /// The tab; empty/absent means the first tab.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "tabId")]
    pub tab_id: Option<String>,
    /// Inclusive start, in UTF-16 code units.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "startIndex"
    )]
    pub start_index: Option<i64>,
    /// Exclusive end, in UTF-16 code units.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "endIndex")]
    pub end_index: Option<i64>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn parse(json: serde_json::Value) -> Document {
        serde_json::from_value(json).unwrap()
    }

    /// The single highest-value test in this file. Docs serialises proto3
    /// JSON, which omits zero-valued integers, so the first element of every
    /// real document has no `startIndex` key. Reading it as anything but `0`
    /// shifts every reported index.
    #[test]
    fn a_missing_start_index_means_zero() {
        let doc = parse(serde_json::json!({
            "body": {"content": [{"endIndex": 1, "sectionBreak": {}}]},
        }));
        let el = &doc.body.as_ref().unwrap().content[0];
        assert_eq!(el.start_index, None, "the key really is absent");
        assert_eq!(el.start_index(), 0, "and the accessor reads it as zero");
        assert_eq!(el.end_index(), 1);
    }

    #[test]
    fn a_present_but_empty_section_break_is_still_recognised() {
        let doc = parse(serde_json::json!({
            "body": {"content": [{"sectionBreak": {}}]},
        }));
        let el = &doc.body.as_ref().unwrap().content[0];
        assert!(el.section_break.is_some(), "present-and-empty is Some");
        assert!(el.paragraph.is_none());
    }

    #[test]
    fn an_absent_section_break_is_none() {
        let doc = parse(serde_json::json!({
            "body": {"content": [{"paragraph": {"elements": []}}]},
        }));
        assert!(doc.body.as_ref().unwrap().content[0]
            .section_break
            .is_none());
    }

    #[test]
    fn text_run_content_defaults_to_empty() {
        let run: TextRun = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(run.content, "");
    }

    /// A paragraph with a bolded word is several text runs. Reading only the
    /// first silently truncates it.
    #[test]
    fn paragraph_text_joins_every_text_run_in_order() {
        let para: Paragraph = serde_json::from_value(serde_json::json!({
            "elements": [
                {"textRun": {"content": "Hello "}},
                {"textRun": {"content": "bold"}},
                {"textRun": {"content": " world\n"}},
            ],
        }))
        .unwrap();
        assert_eq!(para.text(), "Hello bold world\n");
    }

    /// An inline image consumes index space but contributes no text, so a
    /// paragraph's text is shorter than its index span. That is correct.
    #[test]
    fn paragraph_text_skips_a_non_text_element() {
        let para: Paragraph = serde_json::from_value(serde_json::json!({
            "elements": [
                {"textRun": {"content": "a"}},
                {"inlineObjectElement": {"inlineObjectId": "kix.1"}},
                {"textRun": {"content": "b\n"}},
            ],
        }))
        .unwrap();
        assert_eq!(para.text(), "ab\n");
        assert!(para.elements[1].inline_object_element.is_some());
    }

    #[test]
    fn resolved_tabs_synthesises_one_anonymous_tab_from_a_legacy_body() {
        let doc = parse(serde_json::json!({
            "title": "Legacy",
            "body": {"content": [{"endIndex": 1, "sectionBreak": {}}]},
        }));
        let tabs = doc.resolved_tabs();
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].tab_id, None);
        assert_eq!(tabs[0].title, None);
        assert_eq!(tabs[0].nesting_level, 0);
        assert!(tabs[0].body.is_some());
    }

    #[test]
    fn resolved_tabs_of_a_document_with_neither_body_nor_tabs_is_empty() {
        assert!(parse(serde_json::json!({"title": "Empty"}))
            .resolved_tabs()
            .is_empty());
    }

    #[test]
    fn resolved_tabs_walks_child_tabs_depth_first_parent_before_children() {
        let doc = parse(serde_json::json!({
            "tabs": [
                {
                    "tabProperties": {"tabId": "t.0", "title": "One", "nestingLevel": 0},
                    "documentTab": {"body": {"content": []}},
                    "childTabs": [{
                        "tabProperties": {"tabId": "t.0.a", "title": "One A", "nestingLevel": 1},
                        "documentTab": {"body": {"content": []}},
                    }],
                },
                {
                    "tabProperties": {"tabId": "t.1", "title": "Two", "nestingLevel": 0},
                    "documentTab": {"body": {"content": []}},
                },
            ],
        }));
        let ids: Vec<_> = doc.resolved_tabs().iter().map(|t| t.tab_id).collect();
        assert_eq!(ids, vec![Some("t.0"), Some("t.0.a"), Some("t.1")]);
        assert_eq!(doc.resolved_tabs()[1].nesting_level, 1);
    }

    /// A response that omits `nestingLevel` still gets a self-consistent
    /// depth from the walk, rather than collapsing every tab to level 0.
    #[test]
    fn resolved_tabs_falls_back_to_walk_depth_when_nesting_level_is_absent() {
        let doc = parse(serde_json::json!({
            "tabs": [{
                "tabProperties": {"tabId": "t.0"},
                "childTabs": [{"tabProperties": {"tabId": "t.0.a"}}],
            }],
        }));
        let tabs = doc.resolved_tabs();
        assert_eq!(tabs[0].nesting_level, 0);
        assert_eq!(tabs[1].nesting_level, 1);
    }

    /// `tabs` wins over `body`; they never both contribute.
    #[test]
    fn tabs_take_precedence_over_a_legacy_body() {
        let doc = parse(serde_json::json!({
            "body": {"content": [{"paragraph": {"elements": [{"textRun": {"content": "legacy\n"}}]}}]},
            "tabs": [{
                "tabProperties": {"tabId": "t.0"},
                "documentTab": {"body": {"content": []}},
            }],
        }));
        let tabs = doc.resolved_tabs();
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].tab_id, Some("t.0"));
    }

    #[test]
    fn nested_tables_parse_recursively() {
        let doc = parse(serde_json::json!({
            "body": {"content": [{
                "startIndex": 1, "endIndex": 40,
                "table": {
                    "rows": 1, "columns": 1,
                    "tableRows": [{"tableCells": [{"content": [{
                        "table": {"rows": 1, "columns": 1, "tableRows": [{"tableCells": [{
                            "content": [{"paragraph": {"elements": [
                                {"textRun": {"content": "deep\n"}}]}}],
                        }]}]},
                    }]}]}],
                },
            }]},
        }));
        let outer = doc.body.as_ref().unwrap().content[0]
            .table
            .as_ref()
            .unwrap();
        let inner = outer.table_rows[0].table_cells[0].content[0]
            .table
            .as_ref()
            .unwrap();
        let para = inner.table_rows[0].table_cells[0].content[0]
            .paragraph
            .as_ref()
            .unwrap();
        assert_eq!(para.text(), "deep\n");
    }

    #[test]
    fn named_ranges_parse_as_a_map_keyed_by_name() {
        let doc = parse(serde_json::json!({
            "namedRanges": {
                "intro": {
                    "name": "intro",
                    "namedRanges": [{
                        "namedRangeId": "nr.1", "name": "intro",
                        "ranges": [{"startIndex": 1, "endIndex": 20}],
                    }],
                },
            },
        }));
        let group = doc.named_ranges.get("intro").unwrap();
        assert_eq!(group.name, "intro");
        assert_eq!(group.named_ranges[0].ranges[0].end_index, Some(20));
    }

    /// A `revisionId` is present only for callers with edit access; a
    /// read-only caller sees `None`, and the write path refuses rather than
    /// falling back to an unleased write.
    #[test]
    fn a_document_without_edit_access_has_no_revision_id() {
        assert_eq!(parse(serde_json::json!({"title": "T"})).revision_id, None);
        let with = parse(serde_json::json!({"revisionId": "rev-1"}));
        assert_eq!(with.revision_id.as_deref(), Some("rev-1"));
    }

    #[test]
    fn document_tolerates_unmodelled_fields() {
        let doc = parse(serde_json::json!({
            "documentId": "d1",
            "title": "T",
            "documentStyle": {"marginTop": {"magnitude": 72.0}},
            "lists": {"kix.l1": {"listProperties": {}}},
            "inlineObjects": {"kix.i1": {}},
            "suggestionsViewMode": "PREVIEW_SUGGESTIONS_ACCEPTED",
            "body": {"content": [{
                "endIndex": 5,
                "paragraph": {
                    "elements": [{"textRun": {
                        "content": "hi\n",
                        "textStyle": {"bold": true},
                        "suggestedInsertionIds": ["s1"],
                    }}],
                    "paragraphStyle": {"namedStyleType": "NORMAL_TEXT", "direction": "LEFT_TO_RIGHT"},
                },
            }]},
        }));
        assert_eq!(doc.document_id.as_deref(), Some("d1"));
        let para = doc.body.as_ref().unwrap().content[0]
            .paragraph
            .as_ref()
            .unwrap();
        assert_eq!(para.text(), "hi\n");
        assert_eq!(para.named_style_type(), Some("NORMAL_TEXT"));
    }

    #[test]
    fn title_defaults_to_empty() {
        assert_eq!(parse(serde_json::json!({})).title(), "");
        assert_eq!(parse(serde_json::json!({"title": "T"})).title(), "T");
    }
}
