//! Atlassian Document Format (ADF) type definitions.
//!
//! Provides serde-compatible structs for the ADF JSON structure used by
//! JIRA Cloud REST API v3 and Confluence Cloud REST API v2.

use serde::{Deserialize, Serialize};

/// The root ADF document node.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AdfDocument {
    /// ADF version (always 1).
    pub version: u32,

    /// Node type (always "doc").
    #[serde(rename = "type")]
    pub doc_type: String,

    /// Top-level block content nodes.
    pub content: Vec<AdfNode>,
}

impl AdfDocument {
    /// Creates a new empty ADF document.
    #[must_use]
    pub fn new() -> Self {
        Self {
            version: 1,
            doc_type: "doc".to_string(),
            content: Vec::new(),
        }
    }
}

impl Default for AdfDocument {
    fn default() -> Self {
        Self::new()
    }
}

/// A node in the ADF tree.
///
/// Represents both block nodes (paragraph, heading, codeBlock, etc.)
/// and inline nodes (text, hardBreak, mention, etc.).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AdfNode {
    /// The node type identifier (e.g., "paragraph", "text", "heading").
    #[serde(rename = "type")]
    pub node_type: String,

    /// Node-specific attributes (e.g., heading level, code language).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attrs: Option<serde_json::Value>,

    /// Child content nodes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<Self>>,

    /// Text content (only present on text nodes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,

    /// Inline marks applied to this node (bold, italic, link, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub marks: Option<Vec<AdfMark>>,
}

impl AdfNode {
    /// Creates a text node with the given content.
    #[must_use]
    pub fn text(content: &str) -> Self {
        Self {
            node_type: "text".to_string(),
            attrs: None,
            content: None,
            text: Some(content.to_string()),
            marks: None,
        }
    }

    /// Creates a text node with marks applied.
    #[must_use]
    pub fn text_with_marks(content: &str, marks: Vec<AdfMark>) -> Self {
        Self {
            node_type: "text".to_string(),
            attrs: None,
            content: None,
            text: Some(content.to_string()),
            marks: if marks.is_empty() { None } else { Some(marks) },
        }
    }

    /// Creates a paragraph node with the given inline content.
    #[must_use]
    pub fn paragraph(content: Vec<Self>) -> Self {
        Self {
            node_type: "paragraph".to_string(),
            attrs: None,
            content: if content.is_empty() {
                None
            } else {
                Some(content)
            },
            text: None,
            marks: None,
        }
    }

    /// Creates a heading node.
    #[must_use]
    pub fn heading(level: u8, content: Vec<Self>) -> Self {
        Self {
            node_type: "heading".to_string(),
            attrs: Some(serde_json::json!({"level": level})),
            content: if content.is_empty() {
                None
            } else {
                Some(content)
            },
            text: None,
            marks: None,
        }
    }

    /// Creates a code block node.
    #[must_use]
    pub fn code_block(language: Option<&str>, text: &str) -> Self {
        Self {
            node_type: "codeBlock".to_string(),
            attrs: language.map(|lang| serde_json::json!({"language": lang})),
            content: Some(vec![Self::text(text)]),
            text: None,
            marks: None,
        }
    }

    /// Creates a blockquote node.
    #[must_use]
    pub fn blockquote(content: Vec<Self>) -> Self {
        Self {
            node_type: "blockquote".to_string(),
            attrs: None,
            content: Some(content),
            text: None,
            marks: None,
        }
    }

    /// Creates a horizontal rule node.
    #[must_use]
    pub fn rule() -> Self {
        Self {
            node_type: "rule".to_string(),
            attrs: None,
            content: None,
            text: None,
            marks: None,
        }
    }

    /// Creates a bullet list node.
    #[must_use]
    pub fn bullet_list(items: Vec<Self>) -> Self {
        Self {
            node_type: "bulletList".to_string(),
            attrs: None,
            content: Some(items),
            text: None,
            marks: None,
        }
    }

    /// Creates an ordered list node.
    #[must_use]
    pub fn ordered_list(items: Vec<Self>, start: Option<u32>) -> Self {
        Self {
            node_type: "orderedList".to_string(),
            attrs: start.map(|s| serde_json::json!({"order": s})),
            content: Some(items),
            text: None,
            marks: None,
        }
    }

    /// Creates a list item node.
    #[must_use]
    pub fn list_item(content: Vec<Self>) -> Self {
        Self {
            node_type: "listItem".to_string(),
            attrs: None,
            content: Some(content),
            text: None,
            marks: None,
        }
    }

    /// Creates a hard break node.
    #[must_use]
    pub fn hard_break() -> Self {
        Self {
            node_type: "hardBreak".to_string(),
            attrs: None,
            content: None,
            text: None,
            marks: None,
        }
    }

    /// Creates a table node.
    #[must_use]
    pub fn table(rows: Vec<Self>) -> Self {
        Self {
            node_type: "table".to_string(),
            attrs: None,
            content: Some(rows),
            text: None,
            marks: None,
        }
    }

    /// Creates a table node with attributes (layout, `isNumberColumnEnabled`).
    #[must_use]
    pub fn table_with_attrs(rows: Vec<Self>, attrs: serde_json::Value) -> Self {
        Self {
            node_type: "table".to_string(),
            attrs: Some(attrs),
            content: Some(rows),
            text: None,
            marks: None,
        }
    }

    /// Creates a table row node.
    #[must_use]
    pub fn table_row(cells: Vec<Self>) -> Self {
        Self {
            node_type: "tableRow".to_string(),
            attrs: None,
            content: Some(cells),
            text: None,
            marks: None,
        }
    }

    /// Creates a table header cell node.
    #[must_use]
    pub fn table_header(content: Vec<Self>) -> Self {
        Self {
            node_type: "tableHeader".to_string(),
            attrs: None,
            content: Some(content),
            text: None,
            marks: None,
        }
    }

    /// Creates a table header cell node with attributes (colspan, rowspan, background, colwidth).
    #[must_use]
    pub fn table_header_with_attrs(content: Vec<Self>, attrs: serde_json::Value) -> Self {
        Self {
            node_type: "tableHeader".to_string(),
            attrs: Some(attrs),
            content: Some(content),
            text: None,
            marks: None,
        }
    }

    /// Creates a table cell node.
    #[must_use]
    pub fn table_cell(content: Vec<Self>) -> Self {
        Self {
            node_type: "tableCell".to_string(),
            attrs: None,
            content: Some(content),
            text: None,
            marks: None,
        }
    }

    /// Creates a table cell node with attributes (colspan, rowspan, background, colwidth).
    #[must_use]
    pub fn table_cell_with_attrs(content: Vec<Self>, attrs: serde_json::Value) -> Self {
        Self {
            node_type: "tableCell".to_string(),
            attrs: Some(attrs),
            content: Some(content),
            text: None,
            marks: None,
        }
    }

    /// Creates a caption node (used inside tables).
    #[must_use]
    pub fn caption(content: Vec<Self>) -> Self {
        Self {
            node_type: "caption".to_string(),
            attrs: None,
            content: Some(content),
            text: None,
            marks: None,
        }
    }

    /// Creates an inline card node for a smart link (URL as both text and href).
    #[must_use]
    pub fn inline_card(url: &str) -> Self {
        Self {
            node_type: "inlineCard".to_string(),
            attrs: Some(serde_json::json!({"url": url})),
            content: None,
            text: None,
            marks: None,
        }
    }

    /// Creates a media single node wrapping an external image.
    #[must_use]
    pub fn media_single(url: &str, alt: Option<&str>) -> Self {
        let mut media_attrs = serde_json::json!({
            "type": "external",
            "url": url,
        });
        if let Some(alt_text) = alt {
            media_attrs["alt"] = serde_json::Value::String(alt_text.to_string());
        }
        Self {
            node_type: "mediaSingle".to_string(),
            attrs: Some(serde_json::json!({"layout": "center"})),
            content: Some(vec![Self {
                node_type: "media".to_string(),
                attrs: Some(media_attrs),
                content: None,
                text: None,
                marks: None,
            }]),
            text: None,
            marks: None,
        }
    }

    // ── Task lists ─────────────────────────────────────────────────

    /// Creates a task list node.
    #[must_use]
    pub fn task_list(items: Vec<Self>) -> Self {
        Self {
            node_type: "taskList".to_string(),
            attrs: Some(serde_json::json!({"localId": uuid_placeholder()})),
            content: Some(items),
            text: None,
            marks: None,
        }
    }

    /// Creates a task item node with state `"TODO"` or `"DONE"`.
    #[must_use]
    pub fn task_item(state: &str, content: Vec<Self>) -> Self {
        Self {
            node_type: "taskItem".to_string(),
            attrs: Some(serde_json::json!({
                "localId": uuid_placeholder(),
                "state": state,
            })),
            content: if content.is_empty() {
                None
            } else {
                Some(content)
            },
            text: None,
            marks: None,
        }
    }

    // ── Inline nodes ───────────────────────────────────────────────

    /// Creates an emoji node.
    #[must_use]
    pub fn emoji(short_name: &str) -> Self {
        Self {
            node_type: "emoji".to_string(),
            attrs: Some(serde_json::json!({"shortName": short_name})),
            content: None,
            text: None,
            marks: None,
        }
    }

    /// Creates a status badge node.
    #[must_use]
    pub fn status(text: &str, color: &str) -> Self {
        Self {
            node_type: "status".to_string(),
            attrs: Some(serde_json::json!({
                "text": text,
                "color": color,
                "localId": uuid_placeholder(),
            })),
            content: None,
            text: None,
            marks: None,
        }
    }

    /// Creates a date node from an ISO 8601 date string.
    #[must_use]
    pub fn date(timestamp: &str) -> Self {
        Self {
            node_type: "date".to_string(),
            attrs: Some(serde_json::json!({"timestamp": timestamp})),
            content: None,
            text: None,
            marks: None,
        }
    }

    /// Creates a placeholder node.
    #[must_use]
    pub fn placeholder(text: &str) -> Self {
        Self {
            node_type: "placeholder".to_string(),
            attrs: Some(serde_json::json!({"text": text})),
            content: None,
            text: None,
            marks: None,
        }
    }

    /// Creates a mention node.
    #[must_use]
    pub fn mention(id: &str, display_text: &str) -> Self {
        Self {
            node_type: "mention".to_string(),
            attrs: Some(serde_json::json!({
                "id": id,
                "text": display_text,
            })),
            content: None,
            text: None,
            marks: None,
        }
    }

    // ── Block cards and embeds ─────────────────────────────────────

    /// Creates a block card node (smart link displayed as a block).
    #[must_use]
    pub fn block_card(url: &str) -> Self {
        Self {
            node_type: "blockCard".to_string(),
            attrs: Some(serde_json::json!({"url": url})),
            content: None,
            text: None,
            marks: None,
        }
    }

    /// Creates an embed card node.
    #[must_use]
    pub fn embed_card(
        url: &str,
        layout: Option<&str>,
        original_height: Option<f64>,
        width: Option<f64>,
    ) -> Self {
        let mut attrs = serde_json::json!({"url": url});
        if let Some(l) = layout {
            attrs["layout"] = serde_json::Value::String(l.to_string());
        }
        if let Some(h) = original_height {
            attrs["originalHeight"] = serde_json::json!(h);
        }
        if let Some(w) = width {
            attrs["width"] = serde_json::json!(w);
        }
        Self {
            node_type: "embedCard".to_string(),
            attrs: Some(attrs),
            content: None,
            text: None,
            marks: None,
        }
    }

    // ── Panels and expand ──────────────────────────────────────────

    /// Creates a panel node.
    #[must_use]
    pub fn panel(panel_type: &str, content: Vec<Self>) -> Self {
        Self {
            node_type: "panel".to_string(),
            attrs: Some(serde_json::json!({"panelType": panel_type})),
            content: Some(content),
            text: None,
            marks: None,
        }
    }

    /// Creates an expand (collapsible) node.
    #[must_use]
    pub fn expand(title: Option<&str>, content: Vec<Self>) -> Self {
        let attrs = title.map(|t| serde_json::json!({"title": t}));
        Self {
            node_type: "expand".to_string(),
            attrs,
            content: Some(content),
            text: None,
            marks: None,
        }
    }

    /// Creates a nested expand node.
    #[must_use]
    pub fn nested_expand(title: Option<&str>, content: Vec<Self>) -> Self {
        let attrs = title.map(|t| serde_json::json!({"title": t}));
        Self {
            node_type: "nestedExpand".to_string(),
            attrs,
            content: Some(content),
            text: None,
            marks: None,
        }
    }

    // ── Layout ─────────────────────────────────────────────────────

    /// Creates a layout section node.
    #[must_use]
    pub fn layout_section(columns: Vec<Self>) -> Self {
        Self {
            node_type: "layoutSection".to_string(),
            attrs: None,
            content: Some(columns),
            text: None,
            marks: None,
        }
    }

    /// Creates a layout column node.
    #[must_use]
    pub fn layout_column(width: f64, content: Vec<Self>) -> Self {
        Self {
            node_type: "layoutColumn".to_string(),
            attrs: Some(serde_json::json!({"width": width})),
            content: Some(content),
            text: None,
            marks: None,
        }
    }

    // ── Decision lists ─────────────────────────────────────────────

    /// Creates a decision list node.
    #[must_use]
    pub fn decision_list(items: Vec<Self>) -> Self {
        Self {
            node_type: "decisionList".to_string(),
            attrs: Some(serde_json::json!({"localId": uuid_placeholder()})),
            content: Some(items),
            text: None,
            marks: None,
        }
    }

    /// Creates a decision item node.
    #[must_use]
    pub fn decision_item(state: &str, content: Vec<Self>) -> Self {
        Self {
            node_type: "decisionItem".to_string(),
            attrs: Some(serde_json::json!({
                "localId": uuid_placeholder(),
                "state": state,
            })),
            content: Some(content),
            text: None,
            marks: None,
        }
    }

    // ── Extensions ─────────────────────────────────────────────────

    /// Creates a void (block) extension node.
    #[must_use]
    pub fn extension(
        extension_type: &str,
        extension_key: &str,
        params: Option<serde_json::Value>,
    ) -> Self {
        let mut attrs = serde_json::json!({
            "extensionType": extension_type,
            "extensionKey": extension_key,
        });
        if let Some(p) = params {
            attrs["parameters"] = p;
        }
        Self {
            node_type: "extension".to_string(),
            attrs: Some(attrs),
            content: None,
            text: None,
            marks: None,
        }
    }

    /// Creates a bodied extension node (extension with block content).
    #[must_use]
    pub fn bodied_extension(extension_type: &str, extension_key: &str, content: Vec<Self>) -> Self {
        Self {
            node_type: "bodiedExtension".to_string(),
            attrs: Some(serde_json::json!({
                "extensionType": extension_type,
                "extensionKey": extension_key,
            })),
            content: Some(content),
            text: None,
            marks: None,
        }
    }

    /// Creates an inline extension node.
    #[must_use]
    pub fn inline_extension(
        extension_type: &str,
        extension_key: &str,
        fallback_text: Option<&str>,
    ) -> Self {
        Self {
            node_type: "inlineExtension".to_string(),
            attrs: Some(serde_json::json!({
                "extensionType": extension_type,
                "extensionKey": extension_key,
            })),
            content: None,
            text: fallback_text.map(String::from),
            marks: None,
        }
    }
}

/// Returns the default placeholder for nodes that require a `localId`.
/// Empty string is used because Confluence itself emits `localId: ""`
/// for auto-generated nodes; both `""` and the nil UUID
/// `"00000000-0000-0000-0000-000000000000"` are treated as
/// non-significant by the rendering layer.
fn uuid_placeholder() -> String {
    String::new()
}

/// An inline mark applied to a text node.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdfMark {
    /// The mark type (e.g., "strong", "em", "code", "link", "strike").
    #[serde(rename = "type")]
    pub mark_type: String,

    /// Mark-specific attributes (e.g., href for links).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attrs: Option<serde_json::Value>,
}

impl AdfMark {
    /// Creates a strong (bold) mark.
    #[must_use]
    pub fn strong() -> Self {
        Self {
            mark_type: "strong".to_string(),
            attrs: None,
        }
    }

    /// Creates an emphasis (italic) mark.
    #[must_use]
    pub fn em() -> Self {
        Self {
            mark_type: "em".to_string(),
            attrs: None,
        }
    }

    /// Creates an inline code mark.
    #[must_use]
    pub fn code() -> Self {
        Self {
            mark_type: "code".to_string(),
            attrs: None,
        }
    }

    /// Creates a strikethrough mark.
    #[must_use]
    pub fn strike() -> Self {
        Self {
            mark_type: "strike".to_string(),
            attrs: None,
        }
    }

    /// Creates a link mark with the given URL.
    #[must_use]
    pub fn link(href: &str) -> Self {
        Self {
            mark_type: "link".to_string(),
            attrs: Some(serde_json::json!({"href": href})),
        }
    }

    /// Creates an underline mark.
    #[must_use]
    pub fn underline() -> Self {
        Self {
            mark_type: "underline".to_string(),
            attrs: None,
        }
    }

    /// Creates an annotation mark (inline comment highlight).
    #[must_use]
    pub fn annotation(id: &str, annotation_type: &str) -> Self {
        Self {
            mark_type: "annotation".to_string(),
            attrs: Some(serde_json::json!({"id": id, "annotationType": annotation_type})),
        }
    }

    /// Creates a text color mark.
    #[must_use]
    pub fn text_color(color: &str) -> Self {
        Self {
            mark_type: "textColor".to_string(),
            attrs: Some(serde_json::json!({"color": color})),
        }
    }

    /// Creates a background color mark.
    #[must_use]
    pub fn background_color(color: &str) -> Self {
        Self {
            mark_type: "backgroundColor".to_string(),
            attrs: Some(serde_json::json!({"color": color})),
        }
    }

    /// Creates a subscript or superscript mark.
    #[must_use]
    pub fn subsup(kind: &str) -> Self {
        Self {
            mark_type: "subsup".to_string(),
            attrs: Some(serde_json::json!({"type": kind})),
        }
    }

    /// Creates an alignment mark for block nodes.
    #[must_use]
    pub fn alignment(align: &str) -> Self {
        Self {
            mark_type: "alignment".to_string(),
            attrs: Some(serde_json::json!({"align": align})),
        }
    }

    /// Creates an indentation mark for block nodes.
    #[must_use]
    pub fn indentation(level: u32) -> Self {
        Self {
            mark_type: "indentation".to_string(),
            attrs: Some(serde_json::json!({"level": level})),
        }
    }

    /// Creates a breakout mark for block nodes.
    #[must_use]
    pub fn breakout(mode: &str, width: Option<u32>) -> Self {
        let mut attrs = serde_json::json!({"mode": mode});
        if let Some(w) = width {
            attrs["width"] = serde_json::json!(w);
        }
        Self {
            mark_type: "breakout".to_string(),
            attrs: Some(attrs),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn empty_document_serialization() {
        let doc = AdfDocument::new();
        let json = serde_json::to_string(&doc).unwrap();
        assert!(json.contains(r#""version":1"#));
        assert!(json.contains(r#""type":"doc""#));
    }

    #[test]
    fn document_with_paragraph() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![AdfNode::text("Hello world")])],
        };
        let json = serde_json::to_value(&doc).unwrap();
        let content = json["content"][0].clone();
        assert_eq!(content["type"], "paragraph");
        assert_eq!(content["content"][0]["text"], "Hello world");
    }

    #[test]
    fn text_with_marks() {
        let node = AdfNode::text_with_marks("bold text", vec![AdfMark::strong()]);
        let json = serde_json::to_value(&node).unwrap();
        assert_eq!(json["marks"][0]["type"], "strong");
    }

    #[test]
    fn heading_with_level() {
        let node = AdfNode::heading(2, vec![AdfNode::text("Title")]);
        let json = serde_json::to_value(&node).unwrap();
        assert_eq!(json["attrs"]["level"], 2);
        assert_eq!(json["content"][0]["text"], "Title");
    }

    #[test]
    fn code_block_with_language() {
        let node = AdfNode::code_block(Some("rust"), "fn main() {}");
        let json = serde_json::to_value(&node).unwrap();
        assert_eq!(json["attrs"]["language"], "rust");
        assert_eq!(json["content"][0]["text"], "fn main() {}");
    }

    #[test]
    fn link_mark_attributes() {
        let mark = AdfMark::link("https://example.com");
        let json = serde_json::to_value(&mark).unwrap();
        assert_eq!(json["attrs"]["href"], "https://example.com");
    }

    #[test]
    fn real_jira_adf_deserialization() {
        let adf_json = r#"{
            "version": 1,
            "type": "doc",
            "content": [
                {
                    "type": "paragraph",
                    "content": [
                        {"type": "text", "text": "Hello "},
                        {"type": "text", "text": "world", "marks": [{"type": "strong"}]}
                    ]
                },
                {
                    "type": "heading",
                    "attrs": {"level": 2},
                    "content": [
                        {"type": "text", "text": "Section"}
                    ]
                }
            ]
        }"#;

        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        assert_eq!(doc.version, 1);
        assert_eq!(doc.content.len(), 2);
        assert_eq!(doc.content[0].node_type, "paragraph");
        assert_eq!(doc.content[1].node_type, "heading");
    }

    #[test]
    fn round_trip_serialization() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![
                AdfNode::heading(1, vec![AdfNode::text("Title")]),
                AdfNode::paragraph(vec![
                    AdfNode::text("Normal "),
                    AdfNode::text_with_marks("bold", vec![AdfMark::strong()]),
                    AdfNode::text(" text"),
                ]),
                AdfNode::code_block(Some("rust"), "let x = 1;"),
                AdfNode::rule(),
            ],
        };

        let json = serde_json::to_string(&doc).unwrap();
        let restored: AdfDocument = serde_json::from_str(&json).unwrap();
        assert_eq!(doc, restored);
    }

    #[test]
    fn skip_none_fields_in_serialization() {
        let node = AdfNode::text("hello");
        let json = serde_json::to_value(&node).unwrap();
        assert!(json.get("attrs").is_none());
        assert!(json.get("content").is_none());
        assert!(json.get("marks").is_none());
    }

    #[test]
    fn default_document() {
        let doc = AdfDocument::default();
        assert_eq!(doc.version, 1);
        assert_eq!(doc.doc_type, "doc");
        assert!(doc.content.is_empty());
    }

    #[test]
    fn empty_paragraph_no_content() {
        let node = AdfNode::paragraph(vec![]);
        assert!(node.content.is_none());
    }

    #[test]
    fn empty_heading_no_content() {
        let node = AdfNode::heading(1, vec![]);
        assert!(node.content.is_none());
    }

    #[test]
    fn text_with_empty_marks_is_none() {
        let node = AdfNode::text_with_marks("test", vec![]);
        assert!(node.marks.is_none());
    }

    #[test]
    fn code_block_no_language() {
        let node = AdfNode::code_block(None, "code");
        assert!(node.attrs.is_none());
        assert_eq!(
            node.content.as_ref().unwrap()[0].text.as_deref(),
            Some("code")
        );
    }

    #[test]
    fn ordered_list_with_start() {
        let node = AdfNode::ordered_list(vec![], Some(5));
        let attrs = node.attrs.as_ref().unwrap();
        assert_eq!(attrs["order"], 5);
    }

    #[test]
    fn ordered_list_no_start() {
        let node = AdfNode::ordered_list(vec![], None);
        assert!(node.attrs.is_none());
    }

    #[test]
    fn media_single_with_alt() {
        let node = AdfNode::media_single("https://img.url", Some("Alt text"));
        let media = &node.content.as_ref().unwrap()[0];
        let attrs = media.attrs.as_ref().unwrap();
        assert_eq!(attrs["url"], "https://img.url");
        assert_eq!(attrs["alt"], "Alt text");
    }

    #[test]
    fn media_single_no_alt() {
        let node = AdfNode::media_single("https://img.url", None);
        let media = &node.content.as_ref().unwrap()[0];
        let attrs = media.attrs.as_ref().unwrap();
        assert_eq!(attrs["url"], "https://img.url");
        assert!(attrs.get("alt").is_none());
    }

    #[test]
    fn mark_constructors() {
        assert_eq!(AdfMark::em().mark_type, "em");
        assert_eq!(AdfMark::code().mark_type, "code");
        assert_eq!(AdfMark::strike().mark_type, "strike");
    }

    #[test]
    fn table_structure() {
        let table = AdfNode::table(vec![AdfNode::table_row(vec![
            AdfNode::table_header(vec![AdfNode::paragraph(vec![AdfNode::text("H")])]),
            AdfNode::table_cell(vec![AdfNode::paragraph(vec![AdfNode::text("C")])]),
        ])]);
        assert_eq!(table.node_type, "table");
        let row = &table.content.as_ref().unwrap()[0];
        assert_eq!(row.node_type, "tableRow");
        let cells = row.content.as_ref().unwrap();
        assert_eq!(cells[0].node_type, "tableHeader");
        assert_eq!(cells[1].node_type, "tableCell");
    }

    #[test]
    fn blockquote_structure() {
        let bq = AdfNode::blockquote(vec![AdfNode::paragraph(vec![AdfNode::text("quoted")])]);
        assert_eq!(bq.node_type, "blockquote");
        assert_eq!(bq.content.as_ref().unwrap()[0].node_type, "paragraph");
    }

    #[test]
    fn hard_break_structure() {
        let br = AdfNode::hard_break();
        assert_eq!(br.node_type, "hardBreak");
        assert!(br.content.is_none());
        assert!(br.text.is_none());
    }

    #[test]
    fn rule_structure() {
        let rule = AdfNode::rule();
        assert_eq!(rule.node_type, "rule");
        assert!(rule.content.is_none());
    }

    // ── Additional node constructors ────────────────────────────────

    #[test]
    fn bullet_list_structure() {
        let item = AdfNode::list_item(vec![AdfNode::paragraph(vec![AdfNode::text("item")])]);
        let list = AdfNode::bullet_list(vec![item]);
        assert_eq!(list.node_type, "bulletList");
        assert_eq!(list.content.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn list_item_structure() {
        let item = AdfNode::list_item(vec![AdfNode::paragraph(vec![AdfNode::text("text")])]);
        assert_eq!(item.node_type, "listItem");
    }

    #[test]
    fn inline_card_structure() {
        let card = AdfNode::inline_card("https://example.com");
        assert_eq!(card.node_type, "inlineCard");
        assert_eq!(card.attrs.as_ref().unwrap()["url"], "https://example.com");
    }

    #[test]
    fn task_list_structure() {
        let item = AdfNode::task_item("TODO", vec![AdfNode::text("do this")]);
        let list = AdfNode::task_list(vec![item]);
        assert_eq!(list.node_type, "taskList");
        assert!(list.attrs.as_ref().unwrap()["localId"].is_string());
    }

    #[test]
    fn task_item_states() {
        let todo = AdfNode::task_item("TODO", vec![]);
        assert_eq!(todo.attrs.as_ref().unwrap()["state"], "TODO");

        let done = AdfNode::task_item("DONE", vec![]);
        assert_eq!(done.attrs.as_ref().unwrap()["state"], "DONE");
    }

    #[test]
    fn emoji_node() {
        let node = AdfNode::emoji(":thumbsup:");
        assert_eq!(node.node_type, "emoji");
        assert_eq!(node.attrs.as_ref().unwrap()["shortName"], ":thumbsup:");
    }

    #[test]
    fn status_node() {
        let node = AdfNode::status("In Progress", "blue");
        assert_eq!(node.node_type, "status");
        assert_eq!(node.attrs.as_ref().unwrap()["text"], "In Progress");
        assert_eq!(node.attrs.as_ref().unwrap()["color"], "blue");
    }

    #[test]
    fn date_node() {
        let node = AdfNode::date("1680307200000");
        assert_eq!(node.node_type, "date");
        assert_eq!(node.attrs.as_ref().unwrap()["timestamp"], "1680307200000");
    }

    #[test]
    fn mention_node() {
        let node = AdfNode::mention("user-123", "Alice");
        assert_eq!(node.node_type, "mention");
        assert_eq!(node.attrs.as_ref().unwrap()["id"], "user-123");
        assert_eq!(node.attrs.as_ref().unwrap()["text"], "Alice");
    }

    #[test]
    fn block_card_structure() {
        let card = AdfNode::block_card("https://example.com/page");
        assert_eq!(card.node_type, "blockCard");
        assert_eq!(
            card.attrs.as_ref().unwrap()["url"],
            "https://example.com/page"
        );
    }

    #[test]
    fn embed_card_with_all_options() {
        let card = AdfNode::embed_card(
            "https://example.com",
            Some("wide"),
            Some(732.0),
            Some(100.0),
        );
        let attrs = card.attrs.as_ref().unwrap();
        assert_eq!(attrs["url"], "https://example.com");
        assert_eq!(attrs["layout"], "wide");
        assert_eq!(attrs["originalHeight"], 732.0);
        assert_eq!(attrs["width"], 100.0);
    }

    #[test]
    fn embed_card_minimal() {
        let card = AdfNode::embed_card("https://example.com", None, None, None);
        let attrs = card.attrs.as_ref().unwrap();
        assert_eq!(attrs["url"], "https://example.com");
        assert!(attrs.get("layout").is_none());
        assert!(attrs.get("originalHeight").is_none());
        assert!(attrs.get("width").is_none());
    }

    #[test]
    fn panel_structure() {
        let panel = AdfNode::panel(
            "info",
            vec![AdfNode::paragraph(vec![AdfNode::text("note")])],
        );
        assert_eq!(panel.node_type, "panel");
        assert_eq!(panel.attrs.as_ref().unwrap()["panelType"], "info");
    }

    #[test]
    fn expand_with_title() {
        let node = AdfNode::expand(Some("Details"), vec![AdfNode::paragraph(vec![])]);
        assert_eq!(node.node_type, "expand");
        assert_eq!(node.attrs.as_ref().unwrap()["title"], "Details");
    }

    #[test]
    fn expand_without_title() {
        let node = AdfNode::expand(None, vec![AdfNode::paragraph(vec![])]);
        assert_eq!(node.node_type, "expand");
        assert!(node.attrs.is_none());
    }

    #[test]
    fn nested_expand_structure() {
        let node = AdfNode::nested_expand(Some("Inner"), vec![]);
        assert_eq!(node.node_type, "nestedExpand");
        assert_eq!(node.attrs.as_ref().unwrap()["title"], "Inner");
    }

    #[test]
    fn layout_section_and_column() {
        let col = AdfNode::layout_column(50.0, vec![AdfNode::paragraph(vec![])]);
        assert_eq!(col.node_type, "layoutColumn");
        assert_eq!(col.attrs.as_ref().unwrap()["width"], 50.0);

        let section = AdfNode::layout_section(vec![col]);
        assert_eq!(section.node_type, "layoutSection");
    }

    #[test]
    fn decision_list_and_item() {
        let item = AdfNode::decision_item("DECIDED", vec![AdfNode::text("yes")]);
        assert_eq!(item.attrs.as_ref().unwrap()["state"], "DECIDED");

        let list = AdfNode::decision_list(vec![item]);
        assert_eq!(list.node_type, "decisionList");
    }

    #[test]
    fn extension_with_params() {
        let node = AdfNode::extension(
            "com.atlassian.jira",
            "issue-list",
            Some(serde_json::json!({"jql": "project = PROJ"})),
        );
        assert_eq!(node.node_type, "extension");
        let attrs = node.attrs.as_ref().unwrap();
        assert_eq!(attrs["extensionType"], "com.atlassian.jira");
        assert_eq!(attrs["parameters"]["jql"], "project = PROJ");
    }

    #[test]
    fn extension_without_params() {
        let node = AdfNode::extension("com.atlassian.jira", "issue-list", None);
        let attrs = node.attrs.as_ref().unwrap();
        assert!(attrs.get("parameters").is_none());
    }

    #[test]
    fn bodied_extension_structure() {
        let node = AdfNode::bodied_extension(
            "com.atlassian.jira",
            "issue-list",
            vec![AdfNode::paragraph(vec![AdfNode::text("body")])],
        );
        assert_eq!(node.node_type, "bodiedExtension");
        assert!(node.content.is_some());
    }

    #[test]
    fn inline_extension_structure() {
        let node = AdfNode::inline_extension("com.test", "inline-key", Some("fallback"));
        assert_eq!(node.node_type, "inlineExtension");
        assert_eq!(node.text.as_deref(), Some("fallback"));
    }

    #[test]
    fn inline_extension_no_fallback() {
        let node = AdfNode::inline_extension("com.test", "inline-key", None);
        assert!(node.text.is_none());
    }

    #[test]
    fn table_with_attrs_structure() {
        let row = AdfNode::table_row(vec![]);
        let table = AdfNode::table_with_attrs(
            vec![row],
            serde_json::json!({"isNumberColumnEnabled": true, "layout": "default"}),
        );
        assert_eq!(table.node_type, "table");
        assert_eq!(table.attrs.as_ref().unwrap()["isNumberColumnEnabled"], true);
    }

    #[test]
    fn table_header_with_attrs_structure() {
        let header = AdfNode::table_header_with_attrs(
            vec![AdfNode::paragraph(vec![AdfNode::text("H")])],
            serde_json::json!({"colspan": 2, "background": "#deebff"}),
        );
        assert_eq!(header.node_type, "tableHeader");
        assert_eq!(header.attrs.as_ref().unwrap()["colspan"], 2);
    }

    #[test]
    fn table_cell_with_attrs_structure() {
        let cell = AdfNode::table_cell_with_attrs(
            vec![AdfNode::paragraph(vec![AdfNode::text("C")])],
            serde_json::json!({"rowspan": 3}),
        );
        assert_eq!(cell.node_type, "tableCell");
        assert_eq!(cell.attrs.as_ref().unwrap()["rowspan"], 3);
    }

    // ── Additional mark constructors ────────────────────────────────

    #[test]
    fn underline_mark() {
        let mark = AdfMark::underline();
        assert_eq!(mark.mark_type, "underline");
        assert!(mark.attrs.is_none());
    }

    #[test]
    fn text_color_mark() {
        let mark = AdfMark::text_color("#ff0000");
        assert_eq!(mark.mark_type, "textColor");
        assert_eq!(mark.attrs.as_ref().unwrap()["color"], "#ff0000");
    }

    #[test]
    fn background_color_mark() {
        let mark = AdfMark::background_color("#00ff00");
        assert_eq!(mark.mark_type, "backgroundColor");
        assert_eq!(mark.attrs.as_ref().unwrap()["color"], "#00ff00");
    }

    #[test]
    fn subsup_mark() {
        let mark = AdfMark::subsup("sub");
        assert_eq!(mark.mark_type, "subsup");
        assert_eq!(mark.attrs.as_ref().unwrap()["type"], "sub");
    }

    #[test]
    fn alignment_mark() {
        let mark = AdfMark::alignment("center");
        assert_eq!(mark.mark_type, "alignment");
        assert_eq!(mark.attrs.as_ref().unwrap()["align"], "center");
    }

    #[test]
    fn indentation_mark() {
        let mark = AdfMark::indentation(2);
        assert_eq!(mark.mark_type, "indentation");
        assert_eq!(mark.attrs.as_ref().unwrap()["level"], 2);
    }

    #[test]
    fn breakout_mark() {
        let mark = AdfMark::breakout("wide", None);
        assert_eq!(mark.mark_type, "breakout");
        assert_eq!(mark.attrs.as_ref().unwrap()["mode"], "wide");
        assert!(mark.attrs.as_ref().unwrap().get("width").is_none());
    }

    #[test]
    fn breakout_mark_with_width() {
        let mark = AdfMark::breakout("wide", Some(1200));
        assert_eq!(mark.mark_type, "breakout");
        assert_eq!(mark.attrs.as_ref().unwrap()["mode"], "wide");
        assert_eq!(mark.attrs.as_ref().unwrap()["width"], 1200);
    }
}
