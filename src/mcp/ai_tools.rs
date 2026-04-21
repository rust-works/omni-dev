//! MCP tool handlers for AI operations and Claude skills management.

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    schemars, tool, tool_router, ErrorData as McpError,
};
use serde::Deserialize;

use super::error::tool_error;
use super::server::OmniDevServer;
use crate::cli::ai::{self, SkillsFormat};

/// Parameters for `ai_chat`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AiChatParams {
    /// User message to send to the AI.
    pub message: String,
    /// Optional model identifier (e.g., `claude-sonnet-4-6`).
    #[serde(default)]
    pub model: Option<String>,
    /// Optional system prompt; defaults to `"You are a helpful assistant."`.
    #[serde(default)]
    pub system_prompt: Option<String>,
}

/// Output format selector for the `claude_skills_*` tools.
#[derive(Debug, Default, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SkillsOutputFormat {
    /// Human-readable text (default).
    #[default]
    Text,
    /// Machine-readable YAML.
    Yaml,
}

impl From<SkillsOutputFormat> for SkillsFormat {
    fn from(value: SkillsOutputFormat) -> Self {
        match value {
            SkillsOutputFormat::Text => Self::Text,
            SkillsOutputFormat::Yaml => Self::Yaml,
        }
    }
}

/// Parameters shared by `claude_skills_sync` and `claude_skills_clean`.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct ClaudeSkillsMutateParams {
    /// When true, also operate on every worktree belonging to the target repository.
    #[serde(default)]
    pub worktrees: bool,
    /// Output format: `"text"` (default) or `"yaml"`.
    #[serde(default)]
    pub format: SkillsOutputFormat,
}

/// Parameters for `claude_skills_status` (identical shape to mutate tools).
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct ClaudeSkillsStatusParams {
    /// When true, also inspect every worktree belonging to the target repository.
    #[serde(default)]
    pub worktrees: bool,
    /// Output format: `"text"` (default) or `"yaml"`.
    #[serde(default)]
    pub format: SkillsOutputFormat,
}

#[allow(missing_docs)] // #[tool_router] generates a pub `ai_tool_router` fn.
#[tool_router(router = ai_tool_router, vis = "pub")]
impl OmniDevServer {
    /// Single-turn AI chat. Returns the assistant's response as text.
    #[tool(
        description = "Send a single message to the configured AI (Claude/OpenAI/Ollama/Bedrock) \
                       and return its response. Non-streaming, single-turn. On missing credentials, \
                       returns a tool error containing the same diagnostic the CLI would print. \
                       Mirrors `omni-dev ai chat` in one-shot form."
    )]
    pub async fn ai_chat(
        &self,
        Parameters(params): Parameters<AiChatParams>,
    ) -> Result<CallToolResult, McpError> {
        let response = ai::run_chat(&params.message, params.model, params.system_prompt)
            .await
            .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(response)]))
    }

    /// Syncs Claude skills from the current repository into the current repo (and optionally its worktrees).
    #[tool(
        description = "Sync Claude Code skills from the current repository (the MCP server's \
                       current working directory) into target worktrees. MUTATES THE FILESYSTEM: \
                       creates symlinks inside `.claude/skills/` and upserts a managed block in \
                       `.git/info/exclude`. Operates relative to the server process's cwd — not \
                       cross-project. Mirrors `omni-dev ai claude skills sync`."
    )]
    pub async fn claude_skills_sync(
        &self,
        Parameters(params): Parameters<ClaudeSkillsMutateParams>,
    ) -> Result<CallToolResult, McpError> {
        let worktrees = params.worktrees;
        let format = SkillsFormat::from(params.format);
        let output = tokio::task::spawn_blocking(move || ai::run_sync(None, worktrees, format))
            .await
            .map_err(|e| tool_error(anyhow::anyhow!("join error: {e}")))?
            .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Cleans Claude skill residue (symlinks and managed exclude block).
    #[tool(
        description = "Remove skill symlinks and the managed exclude block created by a prior \
                       `claude_skills_sync`. MUTATES THE FILESYSTEM. Operates relative to the \
                       server process's cwd. Mirrors `omni-dev ai claude skills clean`."
    )]
    pub async fn claude_skills_clean(
        &self,
        Parameters(params): Parameters<ClaudeSkillsMutateParams>,
    ) -> Result<CallToolResult, McpError> {
        let worktrees = params.worktrees;
        let format = SkillsFormat::from(params.format);
        let output = tokio::task::spawn_blocking(move || ai::run_clean(None, worktrees, format))
            .await
            .map_err(|e| tool_error(anyhow::anyhow!("join error: {e}")))?
            .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Reports Claude skill residue (symlinks and managed exclude block) — read-only.
    #[tool(
        description = "Report symlinks and managed exclude-block entries left by prior \
                       `claude_skills_sync` runs. Read-only. Operates relative to the server \
                       process's cwd. Mirrors `omni-dev ai claude skills status`."
    )]
    pub async fn claude_skills_status(
        &self,
        Parameters(params): Parameters<ClaudeSkillsStatusParams>,
    ) -> Result<CallToolResult, McpError> {
        let worktrees = params.worktrees;
        let format = SkillsFormat::from(params.format);
        let output = tokio::task::spawn_blocking(move || ai::run_status(None, worktrees, format))
            .await
            .map_err(|e| tool_error(anyhow::anyhow!("join error: {e}")))?
            .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(output)]))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn skills_output_format_defaults_to_text() {
        let fmt: SkillsOutputFormat = SkillsOutputFormat::default();
        matches!(fmt, SkillsOutputFormat::Text);
    }

    #[test]
    fn skills_output_format_converts_to_internal_format() {
        let as_internal: SkillsFormat = SkillsOutputFormat::Text.into();
        assert_eq!(as_internal, SkillsFormat::Text);
        let as_internal: SkillsFormat = SkillsOutputFormat::Yaml.into();
        assert_eq!(as_internal, SkillsFormat::Yaml);
    }

    #[test]
    fn skills_output_format_deserializes_from_lowercase() {
        let fmt: SkillsOutputFormat = serde_json::from_str(r#""text""#).unwrap();
        matches!(fmt, SkillsOutputFormat::Text);
        let fmt: SkillsOutputFormat = serde_json::from_str(r#""yaml""#).unwrap();
        matches!(fmt, SkillsOutputFormat::Yaml);
    }

    #[test]
    fn ai_chat_params_deserializes_with_minimal_fields() {
        let params: AiChatParams = serde_json::from_str(r#"{"message":"hi"}"#).unwrap();
        assert_eq!(params.message, "hi");
        assert!(params.model.is_none());
        assert!(params.system_prompt.is_none());
    }

    #[test]
    fn ai_chat_params_deserializes_all_fields() {
        let params: AiChatParams = serde_json::from_str(
            r#"{"message":"hi","model":"claude-sonnet-4-6","system_prompt":"be helpful"}"#,
        )
        .unwrap();
        assert_eq!(params.message, "hi");
        assert_eq!(params.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(params.system_prompt.as_deref(), Some("be helpful"));
    }

    #[test]
    fn skills_mutate_params_default() {
        let p: ClaudeSkillsMutateParams = serde_json::from_str("{}").unwrap();
        assert!(!p.worktrees);
    }

    #[test]
    fn skills_status_params_default() {
        let p: ClaudeSkillsStatusParams = serde_json::from_str("{}").unwrap();
        assert!(!p.worktrees);
    }

    fn extract_text(result: &rmcp::model::CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|c| match &c.raw {
                rmcp::model::RawContent::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn tempdir() -> tempfile::TempDir {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
        std::fs::create_dir_all(&root).ok();
        tempfile::TempDir::new_in(&root).unwrap()
    }

    // Handler-level cwd-based tests were intentionally omitted:
    // `claude_skills_*` read `std::env::current_dir()`, so exercising them
    // under a unit test would require process-wide chdir, which is inherently
    // racy against every other test in the binary that uses a relative
    // tempdir. The equivalent coverage comes from
    // `cli::ai::claude::skills::mod::skills_api_tests` (which drive
    // `run_sync`/`run_clean`/`run_status` with an explicit `base_dir`)
    // plus the duplex MCP integration test
    // `tests/mcp_integration_test.rs::claude_skills_status_returns_yaml_report`.

    /// Env-isolation lock for `ai_chat` handler tests — they mutate the
    /// provider env vars (USE_OLLAMA, OLLAMA_BASE_URL, …) so they need to
    /// serialise against each other and against `cli::ai::chat::tests`.
    static AI_CHAT_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    const AI_CHAT_KEYS: &[&str] = &[
        "USE_OPENAI",
        "USE_OLLAMA",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_API_KEY",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_BEDROCK_BASE_URL",
        "OPENAI_API_KEY",
        "OPENAI_AUTH_TOKEN",
        "OLLAMA_MODEL",
        "OLLAMA_BASE_URL",
        "ANTHROPIC_MODEL",
        "HOME",
    ];

    fn snapshot_ai_env() -> Vec<(&'static str, Option<String>)> {
        AI_CHAT_KEYS
            .iter()
            .map(|k| (*k, std::env::var(k).ok()))
            .collect()
    }

    fn restore_ai_env(snap: Vec<(&'static str, Option<String>)>) {
        for (k, v) in snap {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn ai_chat_handler_returns_assistant_text_via_mocked_ollama() {
        let _guard = AI_CHAT_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let snap = snapshot_ai_env();
        let home = tempdir();
        std::env::set_var("HOME", home.path());
        for k in AI_CHAT_KEYS.iter().filter(|k| **k != "HOME") {
            std::env::remove_var(k);
        }

        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "test",
                    "object": "chat.completion",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "mcp-ok"},
                        "finish_reason": "stop"
                    }]
                })),
            )
            .mount(&mock)
            .await;

        std::env::set_var("USE_OLLAMA", "true");
        std::env::set_var("OLLAMA_MODEL", "llama2");
        std::env::set_var("OLLAMA_BASE_URL", mock.uri());

        let server = OmniDevServer::new();
        let result = server
            .ai_chat(Parameters(AiChatParams {
                message: "hi".to_string(),
                model: None,
                system_prompt: Some("be terse".to_string()),
            }))
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false));
        assert_eq!(extract_text(&result), "mcp-ok");

        restore_ai_env(snap);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn ai_chat_handler_returns_tool_error_on_missing_credentials() {
        let _guard = AI_CHAT_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let snap = snapshot_ai_env();
        let home = tempdir();
        std::env::set_var("HOME", home.path());
        for k in AI_CHAT_KEYS.iter().filter(|k| **k != "HOME") {
            std::env::remove_var(k);
        }

        let server = OmniDevServer::new();
        let err = server
            .ai_chat(Parameters(AiChatParams {
                message: "hi".to_string(),
                model: None,
                system_prompt: None,
            }))
            .await
            .unwrap_err();
        assert!(err.message.to_lowercase().contains("not found"));

        restore_ai_env(snap);
    }
}
