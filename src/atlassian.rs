//! Atlassian integration: JIRA and Confluence API clients, ADF/JFM conversion.
//!
//! Provides bidirectional conversion between markdown with YAML frontmatter
//! and Atlassian Document Format (ADF), plus Atlassian Cloud REST API clients
//! for JIRA and Confluence.

pub mod adf;
pub mod adf_attr_schema;
pub mod adf_hints;
pub mod adf_mark_schema;
pub mod adf_schema;
pub mod adf_validated;
pub mod api;
pub mod attrs;
pub mod auth;
pub mod client;
pub mod confluence_api;
pub mod convert;
pub mod create;
pub mod custom_fields;
pub mod diff;
pub mod diff_format;
pub mod directive;
pub mod document;
pub mod error;
pub mod inline_comment;
pub mod jira_api;
