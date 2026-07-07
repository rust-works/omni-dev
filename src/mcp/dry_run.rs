//! Shared helper for the `dry_run` parameter on mutating Atlassian MCP tools.
//!
//! When a mutating create/write/link tool such as `jira_create`, `jira_write`,
//! `jira_edit`, `jira_link`, `confluence_create`, or `confluence_write` is
//! called with `dry_run: true`, it performs all local resolution and ADF
//! validation but stops short of the network call, returning the **would-be HTTP
//! request** (method, path, body) as YAML instead. This lets an AI agent
//! validate required fields and JFM→ADF formatting before committing an
//! irreversible mutation — the MCP-side mirror of the CLI's `--dry-run` flag
//! (see issue #1048).
//!
//! Other tools expose dry-run previews without this HTTP-request helper. For
//! example, `confluence_comment_reanchor` computes a comment move preview, and
//! `git_twiddle_commits` formats its own amendment preview.
//!
//! The preview is rendered as YAML (the AI-friendly default per ADR-0020 /
//! ADR-0021) rather than printed, since the MCP server speaks JSON-RPC over
//! stdout and must never write to it directly.

use anyhow::{Context, Result};
use serde::Serialize;

/// A preview of the HTTP request a mutating tool would send, returned in place
/// of performing it when `dry_run: true`.
#[derive(Debug, Serialize)]
struct DryRunRequest {
    /// Always `true` — marks the output as a dry-run preview.
    dry_run: bool,
    /// HTTP method that would be used (e.g. `POST`, `PUT`, `DELETE`).
    method: &'static str,
    /// API path that would be hit, relative to the instance URL.
    path: String,
    /// Request body that would be sent. `None` for bodyless requests
    /// (e.g. link removal).
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<serde_json::Value>,
}

/// Renders a would-be request as YAML. `serde_json::Value` serializes cleanly
/// as nested YAML, so an ADF body renders inline and readably.
pub fn dry_run_request_yaml(
    method: &'static str,
    path: String,
    body: Option<serde_json::Value>,
) -> Result<String> {
    let req = DryRunRequest {
        dry_run: true,
        method,
        path,
        body,
    };
    serde_yaml::to_string(&req).context("Failed to serialize dry-run preview as YAML")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn renders_method_path_and_body() {
        let yaml = dry_run_request_yaml(
            "POST",
            "/rest/api/3/issue".to_string(),
            Some(serde_json::json!({ "fields": { "summary": "Hi" } })),
        )
        .unwrap();
        assert!(yaml.contains("dry_run: true"));
        assert!(yaml.contains("method: POST"));
        assert!(yaml.contains("path: /rest/api/3/issue"));
        assert!(yaml.contains("summary: Hi"));
    }

    #[test]
    fn omits_body_when_none() {
        let yaml =
            dry_run_request_yaml("DELETE", "/rest/api/3/issueLink/42".to_string(), None).unwrap();
        assert!(yaml.contains("method: DELETE"));
        assert!(yaml.contains("path: /rest/api/3/issueLink/42"));
        assert!(!yaml.contains("body"));
    }
}
