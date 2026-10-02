//! Tolerant models for the Slides object graph. Unknown fields are ignored.
use serde::{Deserialize, Serialize};

/// The complete presentation snapshot, including templates and notes.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Presentation {
    /// Drive/Slides presentation identity.
    pub presentation_id: String,
    /// Presentation title.
    pub title: Option<String>,
    /// Opaque editor-only revision token.
    pub revision_id: Option<String>,
    /// Page dimensions in the API's units.
    pub page_size: Option<serde_json::Value>,
    /// Ordinary slides, in display order.
    pub slides: Vec<Page>,
    /// Layout templates, excluded from replacement.
    pub layouts: Vec<Page>,
    /// Master templates, excluded from replacement.
    pub masters: Vec<Page>,
}

/// A page with object-addressed elements.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Page {
    /// Stable page identity.
    pub object_id: String,
    /// API page type.
    pub page_type: Option<String>,
    /// Direct elements on this page.
    pub page_elements: Vec<PageElement>,
    /// Notes belonging to an ordinary slide.
    pub slide_properties: Option<SlideProperties>,
}

/// Slide-specific relationships.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SlideProperties {
    /// The separately addressed notes page.
    pub notes_page: Option<Box<Page>>,
}

/// A page element; future kinds deserialize as an unknown element.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PageElement {
    /// Stable element identity.
    pub object_id: String,
    /// Shape text, when this is a shape.
    pub shape: Option<Shape>,
    /// Cells, when this is a table.
    pub table: Option<Table>,
    /// Presence identifies an image without exposing its content URL.
    pub image: Option<serde_json::Value>,
    /// Recursively grouped elements.
    pub element_group: Option<ElementGroup>,
}

/// Child elements of a group.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ElementGroup {
    /// Direct children.
    pub children: Vec<PageElement>,
}

/// A shape's text.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Shape {
    /// All text runs in this shape.
    pub text: Option<Text>,
}

/// Text runs are joined within one text object, never across objects.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Text {
    /// Runs and paragraph markers in API order.
    pub text_elements: Vec<TextElement>,
}
impl Text {
    /// The textual content, preserving split-run continuity.
    pub fn content(&self) -> String {
        self.text_elements
            .iter()
            .filter_map(|e| e.text_run.as_ref())
            .map(|r| r.content.as_str())
            .collect()
    }
}

/// A text run or a non-text marker.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TextElement {
    /// Actual text; absent on paragraph markers and auto-text.
    pub text_run: Option<TextRun>,
}

/// Content of one text run.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct TextRun {
    /// Unicode text, including its trailing newline when present.
    pub content: String,
}

/// A table's rows.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Table {
    /// Rows in API order.
    pub table_rows: Vec<TableRow>,
}

/// One table row.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TableRow {
    /// Cells in API order, including merged cells.
    pub table_cells: Vec<TableCell>,
}

/// A table cell with its true grid location (important for merged cells).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TableCell {
    /// Cell text.
    pub text: Option<Text>,
    /// Explicit grid coordinates; fallback to array position when omitted.
    pub location: Option<CellLocation>,
}

/// Zero-based grid coordinates.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct CellLocation {
    /// Row number.
    pub row_index: usize,
    /// Column number.
    pub column_index: usize,
}
