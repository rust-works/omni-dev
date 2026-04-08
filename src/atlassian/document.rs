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

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::atlassian::adf::AdfDocument;
use crate::atlassian::api::{ContentItem, ContentMetadata};
use crate::atlassian::client::JiraIssue;
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

    /// JIRA issue key (e.g., "PROJ-123").
    pub key: String,

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
}

/// Confluence-specific frontmatter fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfluenceFrontmatter {
    /// Atlassian instance base URL.
    pub instance: String,

    /// Confluence page ID.
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
pub fn issue_to_jfm_document(issue: &JiraIssue, instance_url: &str) -> Result<JfmDocument> {
    let body = if let Some(ref adf_value) = issue.description_adf {
        let adf_doc: AdfDocument =
            serde_json::from_value(adf_value.clone()).context("Failed to parse ADF description")?;
        adf_to_markdown(&adf_doc)?
    } else {
        String::new()
    };

    Ok(JfmDocument {
        frontmatter: JfmFrontmatter::Jira(JiraFrontmatter {
            instance: instance_url.to_string(),
            key: issue.key.clone(),
            summary: issue.summary.clone(),
            status: issue.status.clone(),
            issue_type: issue.issue_type.clone(),
            assignee: issue.assignee.clone(),
            priority: issue.priority.clone(),
            labels: issue.labels.clone(),
        }),
        body,
    })
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
            summary: item.title.clone(),
            status: status.clone(),
            issue_type: issue_type.clone(),
            assignee: assignee.clone(),
            priority: priority.clone(),
            labels: labels.clone(),
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
                summary: "Fix the bug".to_string(),
                status: None,
                issue_type: None,
                assignee: None,
                priority: None,
                labels: vec![],
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
                summary: "Round trip test".to_string(),
                status: Some("Open".to_string()),
                issue_type: Some("Bug".to_string()),
                assignee: None,
                priority: None,
                labels: vec!["test".to_string()],
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
                summary: "Minimal".to_string(),
                status: None,
                issue_type: None,
                assignee: None,
                priority: None,
                labels: vec![],
            }),
            body: String::new(),
        };

        let output = doc.render().unwrap();
        assert!(!output.contains("status:"));
        assert!(!output.contains("issue_type:"));
        assert!(!output.contains("labels:"));
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
