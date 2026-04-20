//! JFM document format: YAML frontmatter + markdown body.
//!
//! Parses and renders documents in the format:
//! ```text
//! ---
//! type: jira
//! key: PROJ-123
//! summary: Issue title
//! ---
//!
//! Markdown body content here.
//! ```

use std::collections::BTreeMap;
use std::fmt::Write as _;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::atlassian::adf::AdfDocument;
use crate::atlassian::api::{ContentItem, ContentMetadata};
use crate::atlassian::client::{JiraCustomField, JiraIssue};
use crate::atlassian::convert::adf_to_markdown;
use crate::atlassian::error::AtlassianError;

/// A JFM document consisting of YAML frontmatter and a markdown body.
#[derive(Debug, Clone)]
pub struct JfmDocument {
    /// Parsed frontmatter metadata fields.
    pub frontmatter: JfmFrontmatter,

    /// Raw markdown body (not parsed — passed through to/from ADF conversion).
    pub body: String,
}

/// YAML frontmatter for a JFM document.
///
/// Dispatched by the `type` field to the appropriate backend-specific struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum JfmFrontmatter {
    /// JIRA issue frontmatter.
    #[serde(rename = "jira")]
    Jira(JiraFrontmatter),

    /// Confluence page frontmatter.
    #[serde(rename = "confluence")]
    Confluence(ConfluenceFrontmatter),
}

/// JIRA-specific frontmatter fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JiraFrontmatter {
    /// Atlassian instance base URL.
    pub instance: String,

    /// JIRA issue key (e.g., "PROJ-123"). Empty when creating a new issue.
    #[serde(default)]
    pub key: String,

    /// Project key (e.g., "PROJ"). Used when creating issues without an existing key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,

    /// Issue summary (title).
    pub summary: String,

    /// Issue status name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,

    /// Issue type name (Bug, Story, Task, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issue_type: Option<String>,

    /// Assignee display name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,

    /// Priority name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,

    /// Labels applied to the issue.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,

    /// Scalar custom field values keyed by human-readable field name.
    ///
    /// Populated when the issue was fetched with `--fields` or
    /// `--all-fields`. Rich-text (ADF) custom fields are rendered into the
    /// document body as extra sections instead.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub custom_fields: BTreeMap<String, serde_yaml::Value>,
}

/// Confluence-specific frontmatter fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfluenceFrontmatter {
    /// Atlassian instance base URL.
    pub instance: String,

    /// Confluence page ID. Empty when creating a new page.
    #[serde(default)]
    pub page_id: String,

    /// Page title.
    pub title: String,

    /// Space key (e.g., "ENG").
    pub space_key: String,

    /// Page status ("current" or "draft").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,

    /// Page version number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,

    /// Parent page ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
}

impl JfmFrontmatter {
    /// Returns the Atlassian instance URL.
    pub fn instance(&self) -> &str {
        match self {
            Self::Jira(fm) => &fm.instance,
            Self::Confluence(fm) => &fm.instance,
        }
    }

    /// Returns the content identifier (JIRA key or Confluence page ID).
    pub fn id(&self) -> &str {
        match self {
            Self::Jira(fm) => &fm.key,
            Self::Confluence(fm) => &fm.page_id,
        }
    }

    /// Returns the content title (JIRA summary or Confluence page title).
    pub fn title(&self) -> &str {
        match self {
            Self::Jira(fm) => &fm.summary,
            Self::Confluence(fm) => &fm.title,
        }
    }

    /// Returns the document type name.
    pub fn doc_type(&self) -> &str {
        match self {
            Self::Jira(_) => "jira",
            Self::Confluence(_) => "confluence",
        }
    }

    /// Returns the JIRA custom field scalar map, or `None` when the
    /// frontmatter is not a JIRA variant.
    pub fn jira_custom_fields(&self) -> Option<&BTreeMap<String, serde_yaml::Value>> {
        match self {
            Self::Jira(fm) => Some(&fm.custom_fields),
            Self::Confluence(_) => None,
        }
    }
}

/// Validates that a string looks like a JIRA issue key (e.g., "PROJ-123").
pub fn validate_issue_key(key: &str) -> Result<()> {
    let re =
        regex::Regex::new(r"^[A-Z][A-Z0-9]+-\d+$").context("Failed to compile issue key regex")?;
    if !re.is_match(key) {
        anyhow::bail!("Invalid JIRA issue key: '{key}'. Expected format: PROJ-123");
    }
    Ok(())
}

/// Converts a [`JiraIssue`] into a [`JfmDocument`] with YAML frontmatter.
///
/// Custom fields present on the issue are partitioned: rich-text (ADF)
/// fields become additional JFM sections appended to the body (prefixed
/// with an HTML-comment tag so the `write` round-trip can identify them),
/// while scalar fields are serialized into the frontmatter's
/// `custom_fields` map keyed by human-readable name.
pub fn issue_to_jfm_document(issue: &JiraIssue, instance_url: &str) -> Result<JfmDocument> {
    let mut body = if let Some(ref adf_value) = issue.description_adf {
        let adf_doc: AdfDocument =
            serde_json::from_value(adf_value.clone()).context("Failed to parse ADF description")?;
        adf_to_markdown(&adf_doc)?
    } else {
        String::new()
    };

    let mut custom_scalars: BTreeMap<String, serde_yaml::Value> = BTreeMap::new();
    for field in &issue.custom_fields {
        render_custom_field(field, &mut body, &mut custom_scalars)?;
    }

    Ok(JfmDocument {
        frontmatter: JfmFrontmatter::Jira(JiraFrontmatter {
            instance: instance_url.to_string(),
            key: issue.key.clone(),
            project: None,
            summary: issue.summary.clone(),
            status: issue.status.clone(),
            issue_type: issue.issue_type.clone(),
            assignee: issue.assignee.clone(),
            priority: issue.priority.clone(),
            labels: issue.labels.clone(),
            custom_fields: custom_scalars,
        }),
        body,
    })
}

/// Renders a single custom field into either the body (rich text) or the
/// frontmatter scalar map.
fn render_custom_field(
    field: &JiraCustomField,
    body: &mut String,
    scalars: &mut BTreeMap<String, serde_yaml::Value>,
) -> Result<()> {
    if is_adf_document(&field.value) {
        let adf_doc: AdfDocument = serde_json::from_value(field.value.clone())
            .with_context(|| format!("Failed to parse ADF value for {}", field.id))?;
        let section_md = adf_to_markdown(&adf_doc)?;
        append_custom_section(body, field, &section_md);
    } else if let Some(scalar) = extract_custom_field_scalar(&field.value) {
        scalars.insert(field.name.clone(), scalar);
    }
    // Otherwise the field is null or an unrecognized shape — omit it.
    Ok(())
}

/// Appends a rich-text custom field to the body as a tagged section.
fn append_custom_section(body: &mut String, field: &JiraCustomField, section_md: &str) {
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    if !body.is_empty() {
        body.push('\n');
    }
    let _ = write!(
        body,
        "---\n<!-- field: {} ({}) -->\n\n{}",
        field.name, field.id, section_md
    );
    if !body.ends_with('\n') {
        body.push('\n');
    }
}

/// A single rich-text custom field section parsed out of a JFM body.
///
/// Emitted by [`issue_to_jfm_document`] via [`append_custom_section`] and
/// recovered by [`JfmDocument::split_custom_sections`] so the write path
/// can round-trip a field through `markdown_to_adf` for upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomFieldSection {
    /// Human-readable field name captured from the `<!-- field: Name (id) -->` tag.
    pub name: String,

    /// Stable field ID captured from the tag (e.g., `customfield_19300`).
    pub id: String,

    /// Markdown body of the section (no trailing newline guarantees).
    pub body: String,
}

/// Splits a JFM body into the primary body and any trailing custom-field
/// sections.
///
/// Recognizes the separator emitted by [`append_custom_section`]: a line
/// containing only `---` followed by a line matching
/// `<!-- field: <name> (<id>) -->`. Everything before the first such
/// separator is the primary body; each subsequent separator starts a new
/// section whose content runs up to the next separator (or end of input).
pub(crate) fn split_custom_sections(body: &str) -> (String, Vec<CustomFieldSection>) {
    // (marker_start, content_start, name, id)
    let mut markers: Vec<(usize, usize, String, String)> = Vec::new();
    let mut cursor = 0;

    while cursor < body.len() {
        let Some(marker_start) = find_next_marker(body, cursor) else {
            break;
        };

        // Require exactly "---" on its own line, followed by "\n" (or "\r\n").
        let after_dashes = marker_start + 3;
        let after_nl = if body[after_dashes..].starts_with("\r\n") {
            after_dashes + 2
        } else if body[after_dashes..].starts_with('\n') {
            after_dashes + 1
        } else {
            cursor = after_dashes;
            continue;
        };

        if let Some((name, id, content_start)) = parse_field_tag_line(body, after_nl) {
            markers.push((marker_start, content_start, name, id));
            cursor = content_start;
        } else {
            cursor = after_nl;
        }
    }

    if markers.is_empty() {
        return (body.to_string(), Vec::new());
    }

    let first_marker = markers[0].0;
    let main_body = body[..first_marker].trim_end_matches('\n').to_string();

    let mut sections = Vec::with_capacity(markers.len());
    for i in 0..markers.len() {
        let (_marker, content_start, name, id) = &markers[i];
        let content_end = markers.get(i + 1).map_or(body.len(), |next| next.0);
        let raw = &body[*content_start..content_end];
        let trimmed = raw.trim_matches('\n').to_string();
        sections.push(CustomFieldSection {
            name: name.clone(),
            id: id.clone(),
            body: trimmed,
        });
    }

    (main_body, sections)
}

/// Finds the next line-anchored `---` at or after `from`. A marker is
/// either at the very start of `body` or preceded by a newline.
fn find_next_marker(body: &str, from: usize) -> Option<usize> {
    if from == 0 && body.starts_with("---") {
        return Some(0);
    }
    body[from..]
        .find("\n---")
        .map(|rel| from + rel + 1)
        .filter(|p| *p + 3 <= body.len())
}

/// Parses an HTML-comment field tag at `start` and returns
/// `(name, id, content_start)` where `content_start` is the byte offset
/// immediately after the comment line's newline.
fn parse_field_tag_line(body: &str, start: usize) -> Option<(String, String, usize)> {
    let rest = body.get(start..)?;
    let line_end = rest.find('\n').unwrap_or(rest.len());
    let line = rest[..line_end].trim_end_matches('\r');

    let after_open = line.strip_prefix("<!--")?.trim_start();
    let after_field = after_open.strip_prefix("field:")?.trim_start();
    let close_idx = after_field.rfind("-->")?;
    let inner = after_field[..close_idx].trim_end();

    let paren_open = inner.rfind('(')?;
    let name = inner[..paren_open].trim().to_string();
    let rest_part = inner.get(paren_open + 1..)?;
    let paren_close = rest_part.rfind(')')?;
    let id = rest_part[..paren_close].trim().to_string();

    if name.is_empty() || id.is_empty() {
        return None;
    }

    let next_line_start = (start + line_end + 1).min(body.len());
    Some((name, id, next_line_start))
}

/// Returns `true` if `value` has the shape of an Atlassian Document Format
/// document (`{"type":"doc","version":_,"content":[...]}`).
fn is_adf_document(value: &serde_json::Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    obj.get("type").and_then(|t| t.as_str()) == Some("doc")
        && obj.contains_key("version")
        && obj.contains_key("content")
}

/// Converts a custom field's raw JSON value into a scalar YAML
/// representation suitable for frontmatter serialization.
///
/// - Option/select objects (`{self, value, id}`) collapse to their
///   `value` string.
/// - User-picker objects collapse to their `displayName`.
/// - Arrays recurse per element, dropping null/unknown entries.
/// - Primitives (bool/number/string) pass through unchanged.
/// - Unknown objects pass through as a structured YAML mapping.
/// - Null returns `None`.
fn extract_custom_field_scalar(value: &serde_json::Value) -> Option<serde_yaml::Value> {
    use serde_json::Value as J;
    match value {
        J::Null => None,
        J::Bool(_) | J::Number(_) | J::String(_) => json_to_yaml(value),
        J::Array(items) => {
            let extracted: Vec<_> = items
                .iter()
                .filter_map(extract_custom_field_scalar)
                .collect();
            if extracted.is_empty() {
                None
            } else {
                Some(serde_yaml::Value::Sequence(extracted))
            }
        }
        J::Object(map) => {
            if let Some(v) = map.get("value").and_then(|v| v.as_str()) {
                Some(serde_yaml::Value::String(v.to_string()))
            } else if let Some(name) = map.get("displayName").and_then(|v| v.as_str()) {
                Some(serde_yaml::Value::String(name.to_string()))
            } else {
                json_to_yaml(value)
            }
        }
    }
}

fn json_to_yaml(value: &serde_json::Value) -> Option<serde_yaml::Value> {
    serde_yaml::to_value(value).ok()
}

/// Converts a [`ContentItem`] into a [`JfmDocument`] with YAML frontmatter.
///
/// Dispatches on the [`ContentMetadata`] variant to populate the correct
/// frontmatter fields for JIRA or Confluence content.
pub fn content_item_to_document(item: &ContentItem, instance_url: &str) -> Result<JfmDocument> {
    let body = if let Some(ref adf_value) = item.body_adf {
        let adf_doc: AdfDocument =
            serde_json::from_value(adf_value.clone()).context("Failed to parse ADF description")?;
        adf_to_markdown(&adf_doc)?
    } else {
        String::new()
    };

    let frontmatter = match &item.metadata {
        ContentMetadata::Jira {
            status,
            issue_type,
            assignee,
            priority,
            labels,
        } => JfmFrontmatter::Jira(JiraFrontmatter {
            instance: instance_url.to_string(),
            key: item.id.clone(),
            project: None,
            summary: item.title.clone(),
            status: status.clone(),
            issue_type: issue_type.clone(),
            assignee: assignee.clone(),
            priority: priority.clone(),
            labels: labels.clone(),
            custom_fields: BTreeMap::new(),
        }),
        ContentMetadata::Confluence {
            space_key,
            status,
            version,
            parent_id,
        } => JfmFrontmatter::Confluence(ConfluenceFrontmatter {
            instance: instance_url.to_string(),
            page_id: item.id.clone(),
            title: item.title.clone(),
            space_key: space_key.clone(),
            status: status.clone(),
            version: *version,
            parent_id: parent_id.clone(),
        }),
    };

    Ok(JfmDocument { frontmatter, body })
}

impl JfmDocument {
    /// Parses a JFM document from a string.
    ///
    /// Expects the format: `---\n<yaml frontmatter>\n---\n<markdown body>`
    pub fn parse(input: &str) -> Result<Self> {
        let trimmed = input.trim_start();

        if !trimmed.starts_with("---") {
            return Err(AtlassianError::InvalidDocument(
                "Document must start with '---' frontmatter delimiter".to_string(),
            )
            .into());
        }

        // Find the closing '---' delimiter (skip the opening one)
        let after_opening = &trimmed[3..];
        let after_opening = after_opening.strip_prefix('\n').unwrap_or(after_opening);

        let closing_pos = after_opening.find("\n---").ok_or_else(|| {
            AtlassianError::InvalidDocument(
                "Missing closing '---' frontmatter delimiter".to_string(),
            )
        })?;

        let frontmatter_yaml = &after_opening[..closing_pos];
        let after_closing = &after_opening[closing_pos + 4..]; // skip "\n---"

        // Strip the first newline after the closing delimiter
        let body = after_closing
            .strip_prefix('\n')
            .unwrap_or(after_closing)
            .to_string();

        let frontmatter: JfmFrontmatter = serde_yaml::from_str(frontmatter_yaml)
            .context("Failed to parse JFM frontmatter YAML")?;

        Ok(Self { frontmatter, body })
    }

    /// Renders the document back to a string with YAML frontmatter and markdown body.
    pub fn render(&self) -> Result<String> {
        let frontmatter_yaml = serde_yaml::to_string(&self.frontmatter)
            .context("Failed to serialize JFM frontmatter to YAML")?;

        let mut output = String::new();
        output.push_str("---\n");
        output.push_str(&frontmatter_yaml);
        output.push_str("---\n");
        if !self.body.is_empty() {
            output.push('\n');
            output.push_str(&self.body);
            // Ensure trailing newline
            if !self.body.ends_with('\n') {
                output.push('\n');
            }
        }

        Ok(output)
    }

    /// Splits the body into the primary markdown and any trailing custom-field
    /// sections emitted by the custom-field-aware read path.
    ///
    /// Non-destructive — returns owned strings and leaves `self.body`
    /// unchanged so `render()` continues to produce a lossless round-trip.
    pub fn split_custom_sections(&self) -> (String, Vec<CustomFieldSection>) {
        split_custom_sections(&self.body)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_document() {
        let input = "---\ntype: jira\ninstance: https://org.atlassian.net\nkey: PROJ-123\nsummary: Fix the bug\n---\n\nThis is the description.\n";
        let doc = JfmDocument::parse(input).unwrap();
        assert_eq!(doc.frontmatter.doc_type(), "jira");
        assert_eq!(doc.frontmatter.id(), "PROJ-123");
        assert_eq!(doc.frontmatter.title(), "Fix the bug");
        assert_eq!(doc.body, "\nThis is the description.\n");
    }

    #[test]
    fn parse_with_optional_fields() {
        let input = "---\ntype: jira\ninstance: https://org.atlassian.net\nkey: PROJ-456\nsummary: A story\nstatus: In Progress\nissue_type: Story\nassignee: Alice\npriority: High\nlabels:\n  - backend\n  - auth\n---\n\nDescription here.\n";
        let doc = JfmDocument::parse(input).unwrap();
        match &doc.frontmatter {
            JfmFrontmatter::Jira(fm) => {
                assert_eq!(fm.status.as_deref(), Some("In Progress"));
                assert_eq!(fm.issue_type.as_deref(), Some("Story"));
                assert_eq!(fm.assignee.as_deref(), Some("Alice"));
                assert_eq!(fm.priority.as_deref(), Some("High"));
                assert_eq!(fm.labels, vec!["backend", "auth"]);
            }
            _ => panic!("Expected Jira frontmatter"),
        }
    }

    #[test]
    fn parse_empty_body() {
        let input = "---\ntype: jira\ninstance: https://org.atlassian.net\nkey: PROJ-1\nsummary: Empty\n---\n";
        let doc = JfmDocument::parse(input).unwrap();
        assert_eq!(doc.body, "");
    }

    #[test]
    fn parse_body_with_triple_dashes() {
        let input = "---\ntype: jira\ninstance: https://org.atlassian.net\nkey: PROJ-1\nsummary: Dashes\n---\n\nContent with --- dashes in it.\n";
        let doc = JfmDocument::parse(input).unwrap();
        assert!(doc.body.contains("--- dashes"));
    }

    #[test]
    fn parse_missing_opening_delimiter() {
        let input = "type: jira\nkey: PROJ-1\n";
        let result = JfmDocument::parse(input);
        assert!(result.is_err());
    }

    #[test]
    fn parse_missing_closing_delimiter() {
        let input = "---\ntype: jira\nkey: PROJ-1\n";
        let result = JfmDocument::parse(input);
        assert!(result.is_err());
    }

    #[test]
    fn render_basic_document() {
        let doc = JfmDocument {
            frontmatter: JfmFrontmatter::Jira(JiraFrontmatter {
                instance: "https://org.atlassian.net".to_string(),
                key: "PROJ-123".to_string(),
                project: None,
                summary: "Fix the bug".to_string(),
                status: None,
                issue_type: None,
                assignee: None,
                priority: None,
                labels: vec![],
                custom_fields: BTreeMap::new(),
            }),
            body: "Description here.".to_string(),
        };

        let output = doc.render().unwrap();
        assert!(output.starts_with("---\n"));
        assert!(output.contains("key: PROJ-123"));
        assert!(output.contains("summary: Fix the bug"));
        assert!(output.contains("---\n\nDescription here.\n"));
    }

    #[test]
    fn render_round_trip() {
        let doc = JfmDocument {
            frontmatter: JfmFrontmatter::Jira(JiraFrontmatter {
                instance: "https://org.atlassian.net".to_string(),
                key: "PROJ-789".to_string(),
                project: None,
                summary: "Round trip test".to_string(),
                status: Some("Open".to_string()),
                issue_type: Some("Bug".to_string()),
                assignee: None,
                priority: None,
                labels: vec!["test".to_string()],
                custom_fields: BTreeMap::new(),
            }),
            body: "# Heading\n\nSome text.\n".to_string(),
        };

        let rendered = doc.render().unwrap();
        let restored = JfmDocument::parse(&rendered).unwrap();

        assert_eq!(doc.frontmatter.id(), restored.frontmatter.id());
        assert_eq!(doc.frontmatter.title(), restored.frontmatter.title());
        match &restored.frontmatter {
            JfmFrontmatter::Jira(fm) => {
                assert_eq!(fm.status.as_deref(), Some("Open"));
            }
            _ => panic!("Expected Jira frontmatter"),
        }
        assert!(restored.body.contains("# Heading"));
        assert!(restored.body.contains("Some text."));
    }

    // ── validate_issue_key tests ────────��────────────────────────────

    #[test]
    fn valid_issue_keys() {
        assert!(validate_issue_key("PROJ-123").is_ok());
        assert!(validate_issue_key("AB-1").is_ok());
        assert!(validate_issue_key("A1B-999").is_ok());
    }

    #[test]
    fn invalid_issue_keys() {
        assert!(validate_issue_key("proj-123").is_err());
        assert!(validate_issue_key("PROJ").is_err());
        assert!(validate_issue_key("PROJ-").is_err());
        assert!(validate_issue_key("-123").is_err());
        assert!(validate_issue_key("").is_err());
    }

    // ── issue_to_jfm_document tests ─────────��─────────────────────────

    fn sample_issue() -> JiraIssue {
        JiraIssue {
            key: "TEST-42".to_string(),
            summary: "Fix the widget".to_string(),
            description_adf: Some(serde_json::json!({
                "version": 1,
                "type": "doc",
                "content": [{
                    "type": "paragraph",
                    "content": [{"type": "text", "text": "Hello world"}]
                }]
            })),
            status: Some("Open".to_string()),
            issue_type: Some("Bug".to_string()),
            assignee: Some("Alice".to_string()),
            priority: Some("High".to_string()),
            labels: vec!["backend".to_string()],
            custom_fields: Vec::new(),
        }
    }

    #[test]
    fn issue_to_jfm_with_description() {
        let issue = sample_issue();
        let doc = issue_to_jfm_document(&issue, "https://org.atlassian.net").unwrap();
        assert_eq!(doc.frontmatter.id(), "TEST-42");
        assert_eq!(doc.frontmatter.title(), "Fix the widget");
        match &doc.frontmatter {
            JfmFrontmatter::Jira(fm) => {
                assert_eq!(fm.status.as_deref(), Some("Open"));
                assert_eq!(fm.issue_type.as_deref(), Some("Bug"));
            }
            _ => panic!("Expected Jira frontmatter"),
        }
        assert!(doc.body.contains("Hello world"));
    }

    #[test]
    fn issue_to_jfm_without_description() {
        let mut issue = sample_issue();
        issue.description_adf = None;
        let doc = issue_to_jfm_document(&issue, "https://org.atlassian.net").unwrap();
        assert_eq!(doc.body, "");
    }

    #[test]
    fn issue_to_jfm_minimal_fields() {
        let issue = JiraIssue {
            key: "MIN-1".to_string(),
            summary: "Minimal".to_string(),
            description_adf: None,
            status: None,
            issue_type: None,
            assignee: None,
            priority: None,
            labels: vec![],
            custom_fields: Vec::new(),
        };
        let doc = issue_to_jfm_document(&issue, "https://test.atlassian.net").unwrap();
        assert_eq!(doc.frontmatter.instance(), "https://test.atlassian.net");
        match &doc.frontmatter {
            JfmFrontmatter::Jira(fm) => {
                assert!(fm.status.is_none());
                assert!(fm.labels.is_empty());
            }
            _ => panic!("Expected Jira frontmatter"),
        }
    }

    #[test]
    fn issue_to_jfm_renders_correctly() {
        let issue = sample_issue();
        let doc = issue_to_jfm_document(&issue, "https://org.atlassian.net").unwrap();
        let rendered = doc.render().unwrap();
        assert!(rendered.starts_with("---\n"));
        assert!(rendered.contains("key: TEST-42"));
        assert!(rendered.contains("Hello world"));
    }

    #[test]
    fn render_skips_none_and_empty_fields() {
        let doc = JfmDocument {
            frontmatter: JfmFrontmatter::Jira(JiraFrontmatter {
                instance: "https://org.atlassian.net".to_string(),
                key: "PROJ-1".to_string(),
                project: None,
                summary: "Minimal".to_string(),
                status: None,
                issue_type: None,
                assignee: None,
                priority: None,
                labels: vec![],
                custom_fields: BTreeMap::new(),
            }),
            body: String::new(),
        };

        let output = doc.render().unwrap();
        assert!(!output.contains("status:"));
        assert!(!output.contains("issue_type:"));
        assert!(!output.contains("labels:"));
    }

    // ── Custom field helpers ────────────────────────────────────────

    #[test]
    fn is_adf_document_detects_doc_shape() {
        let adf = serde_json::json!({
            "type": "doc",
            "version": 1,
            "content": [{"type": "paragraph", "content": []}]
        });
        assert!(is_adf_document(&adf));
    }

    #[test]
    fn is_adf_document_rejects_scalar_and_other_objects() {
        assert!(!is_adf_document(&serde_json::json!("string")));
        assert!(!is_adf_document(&serde_json::json!(42)));
        assert!(!is_adf_document(&serde_json::json!({"type": "option"})));
        assert!(!is_adf_document(&serde_json::json!({
            "type": "doc", "version": 1
        })));
    }

    #[test]
    fn extract_scalar_passes_through_primitives() {
        assert_eq!(
            extract_custom_field_scalar(&serde_json::json!(7)),
            Some(serde_yaml::Value::from(7_i64))
        );
        assert_eq!(
            extract_custom_field_scalar(&serde_json::json!("hello")),
            Some(serde_yaml::Value::String("hello".to_string()))
        );
        assert_eq!(
            extract_custom_field_scalar(&serde_json::json!(true)),
            Some(serde_yaml::Value::Bool(true))
        );
        assert_eq!(extract_custom_field_scalar(&serde_json::Value::Null), None);
    }

    #[test]
    fn extract_scalar_collapses_option_object_to_value_string() {
        let value = serde_json::json!({
            "self": "https://example.atlassian.net/rest/api/3/customFieldOption/12345",
            "value": "Unplanned",
            "id": "12345"
        });
        assert_eq!(
            extract_custom_field_scalar(&value),
            Some(serde_yaml::Value::String("Unplanned".to_string()))
        );
    }

    #[test]
    fn extract_scalar_collapses_user_object_to_display_name() {
        let value = serde_json::json!({
            "accountId": "abc123",
            "displayName": "Alice",
            "emailAddress": "alice@example.com"
        });
        assert_eq!(
            extract_custom_field_scalar(&value),
            Some(serde_yaml::Value::String("Alice".to_string()))
        );
    }

    #[test]
    fn extract_scalar_recurses_into_arrays_and_drops_nulls() {
        let value = serde_json::json!([
            {"value": "A"},
            null,
            {"displayName": "Bob"},
            42
        ]);
        let extracted = extract_custom_field_scalar(&value).unwrap();
        assert_eq!(
            extracted,
            serde_yaml::Value::Sequence(vec![
                serde_yaml::Value::String("A".to_string()),
                serde_yaml::Value::String("Bob".to_string()),
                serde_yaml::Value::from(42_i64),
            ])
        );
    }

    #[test]
    fn extract_scalar_empty_array_returns_none() {
        let value = serde_json::json!([null, null]);
        assert_eq!(extract_custom_field_scalar(&value), None);
    }

    #[test]
    fn issue_with_scalar_custom_field_goes_to_frontmatter() {
        let issue = JiraIssue {
            key: "ACCS-1".to_string(),
            summary: "S".to_string(),
            description_adf: None,
            status: None,
            issue_type: None,
            assignee: None,
            priority: None,
            labels: vec![],
            custom_fields: vec![JiraCustomField {
                id: "customfield_10001".to_string(),
                name: "Planned / Unplanned Work".to_string(),
                value: serde_json::json!({"value": "Unplanned", "id": "42"}),
            }],
        };
        let doc = issue_to_jfm_document(&issue, "https://org.atlassian.net").unwrap();
        let rendered = doc.render().unwrap();
        assert!(rendered.contains("custom_fields:"));
        assert!(rendered.contains("Planned / Unplanned Work"));
        assert!(rendered.contains("Unplanned"));
        assert!(!rendered.contains("<!-- field:"));
    }

    #[test]
    fn issue_with_adf_custom_field_becomes_body_section() {
        let adf_value = serde_json::json!({
            "type": "doc",
            "version": 1,
            "content": [{
                "type": "paragraph",
                "content": [{"type": "text", "text": "Criterion one"}]
            }]
        });
        let issue = JiraIssue {
            key: "ACCS-1".to_string(),
            summary: "S".to_string(),
            description_adf: None,
            status: None,
            issue_type: None,
            assignee: None,
            priority: None,
            labels: vec![],
            custom_fields: vec![JiraCustomField {
                id: "customfield_19300".to_string(),
                name: "Acceptance Criteria".to_string(),
                value: adf_value,
            }],
        };
        let doc = issue_to_jfm_document(&issue, "https://org.atlassian.net").unwrap();
        let rendered = doc.render().unwrap();
        assert!(rendered.contains("<!-- field: Acceptance Criteria (customfield_19300) -->"));
        assert!(rendered.contains("Criterion one"));
        assert!(!rendered.contains("custom_fields:"));
    }

    #[test]
    fn issue_with_mixed_custom_fields() {
        let adf_value = serde_json::json!({
            "type": "doc",
            "version": 1,
            "content": [{"type": "paragraph", "content": [{"type": "text", "text": "AC body"}]}]
        });
        let issue = JiraIssue {
            key: "ACCS-1".to_string(),
            summary: "S".to_string(),
            description_adf: Some(serde_json::json!({
                "type": "doc",
                "version": 1,
                "content": [{"type": "paragraph", "content": [{"type": "text", "text": "Main"}]}]
            })),
            status: None,
            issue_type: None,
            assignee: None,
            priority: None,
            labels: vec![],
            custom_fields: vec![
                JiraCustomField {
                    id: "customfield_19300".to_string(),
                    name: "Acceptance Criteria".to_string(),
                    value: adf_value,
                },
                JiraCustomField {
                    id: "customfield_10001".to_string(),
                    name: "Sprint Label".to_string(),
                    value: serde_json::json!("Q1"),
                },
            ],
        };
        let doc = issue_to_jfm_document(&issue, "https://org.atlassian.net").unwrap();
        let rendered = doc.render().unwrap();
        assert!(rendered.contains("custom_fields:"));
        assert!(rendered.contains("Sprint Label: Q1"));
        assert!(rendered.contains("Main"));
        assert!(rendered.contains("<!-- field: Acceptance Criteria"));
        assert!(rendered.contains("AC body"));
    }

    // ── split_custom_sections ──────────────────────────────────────

    #[test]
    fn split_custom_sections_no_sections_returns_body_unchanged() {
        let (body, sections) = split_custom_sections("Hello world\n\nMore text\n");
        assert_eq!(body, "Hello world\n\nMore text\n");
        assert!(sections.is_empty());
    }

    #[test]
    fn split_custom_sections_extracts_single_section() {
        let input = "Main body\n\n---\n<!-- field: Acceptance Criteria (customfield_19300) -->\n\n- Item 1\n- Item 2\n";
        let (body, sections) = split_custom_sections(input);
        assert_eq!(body, "Main body");
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].name, "Acceptance Criteria");
        assert_eq!(sections[0].id, "customfield_19300");
        assert_eq!(sections[0].body, "- Item 1\n- Item 2");
    }

    #[test]
    fn split_custom_sections_extracts_multiple_sections() {
        let input = "Main\n\n---\n<!-- field: AC (customfield_1) -->\n\nAC body\n\n---\n<!-- field: Notes (customfield_2) -->\n\nNotes body\n";
        let (body, sections) = split_custom_sections(input);
        assert_eq!(body, "Main");
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].id, "customfield_1");
        assert_eq!(sections[0].body, "AC body");
        assert_eq!(sections[1].id, "customfield_2");
        assert_eq!(sections[1].body, "Notes body");
    }

    #[test]
    fn split_custom_sections_preserves_triple_dashes_inside_body() {
        // A `---` without the follow-up comment tag is just content, not a
        // section separator.
        let input =
            "Before\n\n---\n\nStill body\n\n---\n<!-- field: AC (customfield_1) -->\n\nSection\n";
        let (body, sections) = split_custom_sections(input);
        assert!(body.contains("Still body"));
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].body, "Section");
    }

    #[test]
    fn split_custom_sections_roundtrips_through_render() {
        let issue = JiraIssue {
            key: "TEST-1".to_string(),
            summary: "S".to_string(),
            description_adf: Some(serde_json::json!({
                "type": "doc", "version": 1,
                "content": [{"type":"paragraph","content":[{"type":"text","text":"Main"}]}]
            })),
            status: None,
            issue_type: None,
            assignee: None,
            priority: None,
            labels: vec![],
            custom_fields: vec![JiraCustomField {
                id: "customfield_19300".to_string(),
                name: "Acceptance Criteria".to_string(),
                value: serde_json::json!({
                    "type": "doc", "version": 1,
                    "content": [{"type":"paragraph","content":[{"type":"text","text":"AC line"}]}]
                }),
            }],
        };
        let doc = issue_to_jfm_document(&issue, "https://org.atlassian.net").unwrap();
        let rendered = doc.render().unwrap();
        let reparsed = JfmDocument::parse(&rendered).unwrap();
        let (body, sections) = reparsed.split_custom_sections();
        assert!(body.contains("Main"));
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].id, "customfield_19300");
        assert_eq!(sections[0].name, "Acceptance Criteria");
        assert!(sections[0].body.contains("AC line"));
    }

    #[test]
    fn issue_with_null_custom_field_is_omitted() {
        let issue = JiraIssue {
            key: "ACCS-1".to_string(),
            summary: "S".to_string(),
            description_adf: None,
            status: None,
            issue_type: None,
            assignee: None,
            priority: None,
            labels: vec![],
            custom_fields: vec![JiraCustomField {
                id: "customfield_99".to_string(),
                name: "Empty Field".to_string(),
                value: serde_json::Value::Null,
            }],
        };
        let doc = issue_to_jfm_document(&issue, "https://org.atlassian.net").unwrap();
        let rendered = doc.render().unwrap();
        assert!(!rendered.contains("custom_fields:"));
        assert!(!rendered.contains("Empty Field"));
    }

    // ── Confluence frontmatter tests ───���─────────────────────────────

    #[test]
    fn parse_confluence_document() {
        let input = "---\ntype: confluence\ninstance: https://org.atlassian.net\npage_id: '12345'\ntitle: Architecture Overview\nspace_key: ENG\nstatus: current\nversion: 7\n---\n\nPage body here.\n";
        let doc = JfmDocument::parse(input).unwrap();
        assert_eq!(doc.frontmatter.doc_type(), "confluence");
        assert_eq!(doc.frontmatter.id(), "12345");
        assert_eq!(doc.frontmatter.title(), "Architecture Overview");
        match &doc.frontmatter {
            JfmFrontmatter::Confluence(fm) => {
                assert_eq!(fm.space_key, "ENG");
                assert_eq!(fm.status.as_deref(), Some("current"));
                assert_eq!(fm.version, Some(7));
            }
            _ => panic!("Expected Confluence frontmatter"),
        }
    }

    #[test]
    fn render_confluence_document() {
        let doc = JfmDocument {
            frontmatter: JfmFrontmatter::Confluence(ConfluenceFrontmatter {
                instance: "https://org.atlassian.net".to_string(),
                page_id: "12345".to_string(),
                title: "Architecture Overview".to_string(),
                space_key: "ENG".to_string(),
                status: Some("current".to_string()),
                version: Some(7),
                parent_id: None,
            }),
            body: "Page body here.\n".to_string(),
        };

        let output = doc.render().unwrap();
        assert!(output.starts_with("---\n"));
        assert!(output.contains("type: confluence"));
        assert!(output.contains("page_id:"));
        assert!(output.contains("space_key: ENG"));
        assert!(output.contains("Page body here."));
    }

    #[test]
    fn confluence_round_trip() {
        let doc = JfmDocument {
            frontmatter: JfmFrontmatter::Confluence(ConfluenceFrontmatter {
                instance: "https://org.atlassian.net".to_string(),
                page_id: "99999".to_string(),
                title: "Round trip".to_string(),
                space_key: "DEV".to_string(),
                status: None,
                version: Some(3),
                parent_id: Some("88888".to_string()),
            }),
            body: "Content.\n".to_string(),
        };

        let rendered = doc.render().unwrap();
        let restored = JfmDocument::parse(&rendered).unwrap();
        assert_eq!(restored.frontmatter.id(), "99999");
        assert_eq!(restored.frontmatter.title(), "Round trip");
        match &restored.frontmatter {
            JfmFrontmatter::Confluence(fm) => {
                assert_eq!(fm.space_key, "DEV");
                assert_eq!(fm.version, Some(3));
                assert_eq!(fm.parent_id.as_deref(), Some("88888"));
            }
            _ => panic!("Expected Confluence frontmatter"),
        }
    }

    // ── content_item_to_document tests ───────────────────────────────

    #[test]
    fn content_item_jira_to_document() {
        let item = ContentItem {
            id: "PROJ-42".to_string(),
            title: "A JIRA issue".to_string(),
            body_adf: Some(serde_json::json!({
                "version": 1,
                "type": "doc",
                "content": [{
                    "type": "paragraph",
                    "content": [{"type": "text", "text": "Content"}]
                }]
            })),
            metadata: ContentMetadata::Jira {
                status: Some("Open".to_string()),
                issue_type: Some("Bug".to_string()),
                assignee: None,
                priority: None,
                labels: vec![],
            },
        };
        let doc = content_item_to_document(&item, "https://org.atlassian.net").unwrap();
        assert_eq!(doc.frontmatter.doc_type(), "jira");
        assert_eq!(doc.frontmatter.id(), "PROJ-42");
        assert!(doc.body.contains("Content"));
    }

    #[test]
    fn content_item_confluence_to_document() {
        let item = ContentItem {
            id: "12345".to_string(),
            title: "A Confluence page".to_string(),
            body_adf: None,
            metadata: ContentMetadata::Confluence {
                space_key: "ENG".to_string(),
                status: Some("current".to_string()),
                version: Some(5),
                parent_id: None,
            },
        };
        let doc = content_item_to_document(&item, "https://org.atlassian.net").unwrap();
        assert_eq!(doc.frontmatter.doc_type(), "confluence");
        assert_eq!(doc.frontmatter.id(), "12345");
        assert_eq!(doc.frontmatter.title(), "A Confluence page");
    }
}
