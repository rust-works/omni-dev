//! MCP tool handlers for stateless Atlassian operations.
//!
//! Tools in this module do not require an `AtlassianClient` — they are pure
//! conversion/validation helpers that can run without credentials. Anything
//! that talks to the Atlassian Cloud API belongs in `jira_tools.rs` or
//! `confluence_tools.rs`.

use anyhow::{Context, Result};
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock as Content},
    schemars, tool, tool_router, ErrorData as McpError,
};
use serde::Deserialize;

use crate::atlassian::adf::AdfDocument;
use crate::atlassian::convert::{adf_to_markdown_with_options, markdown_to_adf, RenderOptions};

use super::error::tool_error;
use super::server::OmniDevServer;

/// Parameters for the `atlassian_convert` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AtlassianConvertParams {
    /// The content to convert. For `to-adf` this is JFM markdown; for
    /// `from-adf` this is an ADF JSON document.
    pub content: String,
    /// Direction of the conversion: `to-adf` (markdown → ADF JSON) or
    /// `from-adf` (ADF JSON → markdown).
    pub direction: String,
    /// When `direction = to-adf`, emit compact JSON instead of pretty-printed.
    #[serde(default)]
    pub compact: Option<bool>,
    /// When `direction = from-adf`, strip `localId` attributes from output
    /// for better readability. For reading only — do NOT write stripped output
    /// back to Confluence, as `localId`s anchor inline comments and dropping
    /// them makes Confluence lose those comments.
    #[serde(default)]
    pub strip_local_ids: Option<bool>,
}

#[allow(missing_docs)] // #[tool_router] generates a pub `atlassian_tool_router` fn.
#[tool_router(router = atlassian_tool_router, vis = "pub")]
impl OmniDevServer {
    /// Tool: convert between JFM markdown and ADF JSON without touching the API.
    #[tool(
        description = "Convert between JFM (JIRA-Flavoured Markdown) and ADF (Atlassian \
                       Document Format) JSON. Bidirectional and fully offline — performs no network \
                       I/O and needs no Atlassian credentials. Set `direction` to \"to-adf\" to \
                       convert JFM markdown into an ADF JSON document (e.g. `# Title` becomes \
                       `{\"version\":1,\"type\":\"doc\",\"content\":[...]}`), or \"from-adf\" to \
                       render an ADF JSON document back into JFM markdown. Use it to preview or \
                       inspect the ADF a JIRA/Confluence write tool would send. Output is the \
                       converted document (JSON for to-adf, markdown for from-adf). Mirrors \
                       `omni-dev atlassian convert to-adf` / `from-adf`."
    )]
    pub async fn atlassian_convert(
        &self,
        Parameters(params): Parameters<AtlassianConvertParams>,
    ) -> Result<CallToolResult, McpError> {
        let output = run_convert(&params).map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(output)]))
    }
}

/// Runs the conversion described by `params`.
///
/// Split out of the tool handler so it can be unit-tested directly without
/// spinning up an MCP server.
fn run_convert(params: &AtlassianConvertParams) -> Result<String> {
    match params.direction.as_str() {
        "to-adf" => {
            let doc = markdown_to_adf(&params.content)?;
            let compact = params.compact.unwrap_or(false);
            if compact {
                serde_json::to_string(&doc).context("Failed to serialize ADF JSON")
            } else {
                serde_json::to_string_pretty(&doc).context("Failed to serialize ADF JSON")
            }
        }
        "from-adf" => {
            let doc = AdfDocument::from_json_str(&params.content)?;
            let opts = RenderOptions {
                strip_local_ids: params.strip_local_ids.unwrap_or(false),
            };
            adf_to_markdown_with_options(&doc, &opts)
        }
        other => anyhow::bail!("Invalid direction \"{other}\": must be \"to-adf\" or \"from-adf\""),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn to_adf_pretty_returns_multi_line_json() {
        let params = AtlassianConvertParams {
            content: "# Title\n\nBody text.".to_string(),
            direction: "to-adf".to_string(),
            compact: None,
            strip_local_ids: None,
        };
        let out = run_convert(&params).unwrap();
        assert!(out.contains("\"type\""));
        // Pretty-printed JSON contains newlines
        assert!(out.contains('\n'));
    }

    #[test]
    fn to_adf_compact_has_no_newlines() {
        let params = AtlassianConvertParams {
            content: "Plain body".to_string(),
            direction: "to-adf".to_string(),
            compact: Some(true),
            strip_local_ids: None,
        };
        let out = run_convert(&params).unwrap();
        assert!(out.contains("\"type\""));
        assert!(!out.contains('\n'), "compact JSON must not have newlines");
    }

    #[test]
    fn from_adf_returns_markdown() {
        let adf = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"Hello"}]}]}"#;
        let params = AtlassianConvertParams {
            content: adf.to_string(),
            direction: "from-adf".to_string(),
            compact: None,
            strip_local_ids: None,
        };
        let out = run_convert(&params).unwrap();
        assert!(out.contains("Hello"));
    }

    #[test]
    fn from_adf_strip_local_ids_flag_plumbed_through() {
        // ADF with a localId on the paragraph. The renderer honours the
        // strip_local_ids option; we just assert it runs without error when
        // the flag is set.
        let adf = r#"{
            "version": 1, "type": "doc",
            "content": [{
                "type": "paragraph",
                "attrs": {"localId": "abc-123"},
                "content": [{"type": "text", "text": "Body"}]
            }]
        }"#;
        let params = AtlassianConvertParams {
            content: adf.to_string(),
            direction: "from-adf".to_string(),
            compact: None,
            strip_local_ids: Some(true),
        };
        let out = run_convert(&params).unwrap();
        assert!(out.contains("Body"));
    }

    #[test]
    fn from_adf_invalid_json_errors() {
        let params = AtlassianConvertParams {
            content: "not json".to_string(),
            direction: "from-adf".to_string(),
            compact: None,
            strip_local_ids: None,
        };
        assert!(run_convert(&params).is_err());
    }

    #[test]
    fn unknown_direction_errors() {
        let params = AtlassianConvertParams {
            content: "x".to_string(),
            direction: "sideways".to_string(),
            compact: None,
            strip_local_ids: None,
        };
        let err = run_convert(&params).unwrap_err();
        assert!(err.to_string().contains("direction"));
    }

    #[test]
    fn tool_router_registers_atlassian_convert() {
        let router = OmniDevServer::atlassian_tool_router();
        assert!(router.has_route("atlassian_convert"));
    }

    // ── Tool handler bodies ────────────────────────────────────────

    use rmcp::handler::server::wrapper::Parameters;

    #[tokio::test(flavor = "current_thread")]
    async fn atlassian_convert_handler_to_adf_success() {
        let server = OmniDevServer::new();
        let result = server
            .atlassian_convert(Parameters(AtlassianConvertParams {
                content: "# Title\n\nBody.".to_string(),
                direction: "to-adf".to_string(),
                compact: None,
                strip_local_ids: None,
            }))
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn atlassian_convert_handler_invalid_direction_returns_tool_error() {
        let server = OmniDevServer::new();
        let result = server
            .atlassian_convert(Parameters(AtlassianConvertParams {
                content: "x".to_string(),
                direction: "sideways".to_string(),
                compact: None,
                strip_local_ids: None,
            }))
            .await;
        let err = result.unwrap_err();
        assert!(err.message.contains("direction"));
    }
}
