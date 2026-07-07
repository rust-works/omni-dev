//! MCP tool handlers for configuration and credential introspection.

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock as Content},
    schemars, tool, tool_router, ErrorData as McpError,
};
use serde::Deserialize;

use super::error::tool_error;
use super::server::OmniDevServer;

/// Parameters for `config_models_show` (none — placeholder for future extensibility).
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct ConfigModelsShowParams {}

/// Parameters for `atlassian_auth_status` (none).
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct AtlassianAuthStatusParams {}

#[allow(missing_docs)] // #[tool_router] generates a pub `config_tool_router` fn.
#[tool_router(router = config_tool_router, vis = "pub")]
impl OmniDevServer {
    /// Returns the embedded `models.yaml` describing every AI model the CLI knows about.
    #[tool(
        description = "Return the embedded `models.yaml` listing every supported AI model the CLI \
                       knows about, with each model's identifier, token limits (input context and \
                       max output tokens), and provider. Use this to discover the valid `model` \
                       values accepted by `ai_chat` and the git tools. Takes no arguments. \
                       Read-only. Output is YAML. Mirrors `omni-dev config models show \
                       --embedded-only` (the plain `show` additionally merges user/project \
                       overrides; this tool returns the embedded catalog only)."
    )]
    pub async fn config_models_show(
        &self,
        Parameters(_params): Parameters<ConfigModelsShowParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = crate::claude::model_config::MODELS_YAML.to_string();
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Reports which Atlassian credential scopes have credentials configured.
    ///
    /// Boolean presence flags only — never returns secret values.
    #[tool(
        description = "Report which Atlassian credential scopes have credentials configured. \
                       Returns boolean presence flags only — NEVER includes the email, API \
                       token, or any other secret. The instance URL (non-secret) is returned \
                       verbatim. Checks local configuration only; it does NOT call the Atlassian \
                       API to validate the credentials (unlike `omni-dev atlassian auth status`, \
                       which signs in and prints the authenticated user). Takes no arguments. \
                       Read-only. Output is YAML."
    )]
    pub async fn atlassian_auth_status(
        &self,
        Parameters(_params): Parameters<AtlassianAuthStatusParams>,
    ) -> Result<CallToolResult, McpError> {
        let status = crate::atlassian::auth::status();
        let yaml = serde_yaml::to_string(&status)
            .map_err(|e| tool_error(anyhow::anyhow!("failed to serialize auth status: {e}")))?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn config_models_show_params_accepts_empty_object() {
        let _p: ConfigModelsShowParams = serde_json::from_str("{}").unwrap();
    }

    #[test]
    fn atlassian_auth_status_params_accepts_empty_object() {
        let _p: AtlassianAuthStatusParams = serde_json::from_str("{}").unwrap();
    }
}
