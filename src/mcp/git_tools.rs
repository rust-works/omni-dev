//! MCP tool handlers for git operations.
//!
//! Each handler delegates to the same `run_*` function that the CLI uses, so
//! the MCP surface and the CLI share a single implementation.

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    schemars, tool, tool_router, ErrorData as McpError,
};
use serde::Deserialize;

use super::error::tool_error;
use super::server::OmniDevServer;

/// Parameters for the `git_view_commits` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GitViewCommitsParams {
    /// Commit range to analyze (e.g., `HEAD~3..HEAD`, `abc123..def456`).
    /// Defaults to `HEAD` when omitted.
    #[serde(default)]
    pub range: Option<String>,
    /// Path to the git repository. Defaults to the current working directory.
    #[serde(default)]
    pub repo_path: Option<String>,
}

#[allow(missing_docs)] // #[tool_router] generates a pub `git_tool_router` fn.
#[tool_router(router = git_tool_router, vis = "pub")]
impl OmniDevServer {
    /// Tool: analyse commits in a range and return repository information as YAML.
    #[tool(
        description = "Analyze commits in a range and return repository information as YAML. \
                       Mirrors `omni-dev git commit message view`."
    )]
    pub async fn git_view_commits(
        &self,
        Parameters(params): Parameters<GitViewCommitsParams>,
    ) -> Result<CallToolResult, McpError> {
        let range = params.range.as_deref().unwrap_or("HEAD").to_string();
        let repo_path = params.repo_path.clone();

        let yaml = tokio::task::spawn_blocking(move || {
            crate::cli::git::run_view(&range, repo_path.as_deref())
        })
        .await
        .map_err(|e| tool_error(anyhow::anyhow!("join error: {e}")))?
        .map_err(tool_error)?;

        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }
}
