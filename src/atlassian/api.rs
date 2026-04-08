//! Atlassian content API trait and shared types.
//!
//! Defines the [`AtlassianApi`] trait for abstracting over JIRA and
//! Confluence backends, plus the [`ContentItem`] and [`ContentMetadata`]
//! types used as the common read result.

use std::future::Future;
use std::pin::Pin;

use anyhow::Result;

use crate::atlassian::adf::AdfDocument;

/// A content item fetched from an Atlassian Cloud API.
#[derive(Debug, Clone)]
pub struct ContentItem {
    /// Identifier: JIRA issue key (e.g., "PROJ-123") or Confluence page ID.
    pub id: String,

    /// Title (JIRA summary or Confluence page title).
    pub title: String,

    /// Body as raw ADF JSON value (may be `None` when the field is null).
    pub body_adf: Option<serde_json::Value>,

    /// Backend-specific metadata that maps to frontmatter fields.
    pub metadata: ContentMetadata,
}

/// Backend-specific metadata for a content item.
#[derive(Debug, Clone)]
pub enum ContentMetadata {
    /// JIRA issue metadata.
    Jira {
        /// Issue status name.
        status: Option<String>,
        /// Issue type name (Bug, Story, Task, etc.).
        issue_type: Option<String>,
        /// Assignee display name.
        assignee: Option<String>,
        /// Priority name.
        priority: Option<String>,
        /// Labels.
        labels: Vec<String>,
    },
    /// Confluence page metadata.
    Confluence {
        /// Space key (e.g., "ENG").
        space_key: String,
        /// Page status ("current" or "draft").
        status: Option<String>,
        /// Page version number.
        version: Option<u32>,
        /// Parent page ID.
        parent_id: Option<String>,
    },
}

/// Trait for Atlassian content backends.
///
/// Follows the project's `AiClient` pattern: `Send + Sync` bounds with
/// boxed futures for async trait methods.
pub trait AtlassianApi: Send + Sync {
    /// Fetches a content item by its identifier.
    fn get_content<'a>(
        &'a self,
        id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<ContentItem>> + Send + 'a>>;

    /// Updates a content item's body and optionally its title.
    fn update_content<'a>(
        &'a self,
        id: &'a str,
        body_adf: &'a AdfDocument,
        title: Option<&'a str>,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

    /// Verifies authentication and returns a display name.
    fn verify_auth<'a>(&'a self) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;

    /// Returns the backend type name ("jira" or "confluence").
    fn backend_name(&self) -> &'static str;
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn content_metadata_jira_variant() {
        let meta = ContentMetadata::Jira {
            status: Some("Open".to_string()),
            issue_type: Some("Bug".to_string()),
            assignee: None,
            priority: Some("High".to_string()),
            labels: vec!["backend".to_string()],
        };
        match &meta {
            ContentMetadata::Jira { status, labels, .. } => {
                assert_eq!(status.as_deref(), Some("Open"));
                assert_eq!(labels.len(), 1);
            }
            _ => panic!("Expected Jira variant"),
        }
    }

    #[test]
    fn content_metadata_confluence_variant() {
        let meta = ContentMetadata::Confluence {
            space_key: "ENG".to_string(),
            status: Some("current".to_string()),
            version: Some(7),
            parent_id: None,
        };
        match &meta {
            ContentMetadata::Confluence {
                space_key, version, ..
            } => {
                assert_eq!(space_key, "ENG");
                assert_eq!(*version, Some(7));
            }
            _ => panic!("Expected Confluence variant"),
        }
    }

    #[test]
    fn content_item_fields() {
        let item = ContentItem {
            id: "PROJ-123".to_string(),
            title: "Fix the bug".to_string(),
            body_adf: None,
            metadata: ContentMetadata::Jira {
                status: None,
                issue_type: None,
                assignee: None,
                priority: None,
                labels: vec![],
            },
        };
        assert_eq!(item.id, "PROJ-123");
        assert_eq!(item.title, "Fix the bug");
        assert!(item.body_adf.is_none());
    }
}
