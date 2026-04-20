//! Atlassian integration: JIRA and Confluence API clients, ADF/JFM conversion.
//!
//! Provides bidirectional conversion between markdown with YAML frontmatter
//! and Atlassian Document Format (ADF), plus Atlassian Cloud REST API clients
//! for JIRA and Confluence.

pub mod adf;
pub mod api;
pub mod attrs;
pub mod auth;
pub mod client;
pub mod confluence_api;
pub mod convert;
pub mod custom_fields;
pub mod directive;
pub mod document;
pub mod error;
pub mod jira_api;
