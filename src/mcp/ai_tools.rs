//! MCP tool handlers for AI operations and Claude skills management.

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock as Content},
    schemars, tool, tool_router, ErrorData as McpError,
};
use serde::Deserialize;

use super::content_input::require_content_input;
use super::error::tool_error;
use super::server::OmniDevServer;
use crate::cli::ai::{self, SkillsFormat};

/// Parameters for `ai_chat`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AiChatParams {
    /// User message to send to the AI, e.g. `"Summarise this diff in one
    /// sentence."`. Sent as a single turn; there is no conversation history.
    /// Mutually exclusive with `message_path`; exactly one is required.
    #[serde(default)]
    pub message: Option<String>,
    /// Filesystem path the server reads the message from, instead of `message`.
    /// Prefer this when the message is already on disk (e.g. a large prompt or
    /// document) — it avoids re-emitting it inline. Mutually exclusive with
    /// `message`.
    #[serde(default)]
    pub message_path: Option<String>,
    /// Optional model identifier (e.g., `claude-sonnet-4-6`). When omitted,
    /// the backend's environment-configured default model is used; call
    /// `config_models_show` to see the identifiers the CLI recognises.
    #[serde(default)]
    pub model: Option<String>,
    /// Optional system prompt; defaults to `"You are a helpful assistant."`.
    /// MCP-only: the interactive `omni-dev ai chat` CLI has no equivalent flag,
    /// so this override is reachable only through the tool.
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
                       and return its response. Non-streaming, single-turn. Optionally override the \
                       model (`model`) and the system prompt (`system_prompt`). On missing \
                       credentials, returns a tool error containing the same diagnostic the CLI \
                       would print. Mirrors `omni-dev ai chat` in one-shot form — that CLI command \
                       is interactive and has no `system_prompt` flag, so this tool is the only way \
                       to set a custom system prompt. Supply the message as `message` (inline) OR \
                       `message_path` (a filesystem path the server reads) — not both; prefer the \
                       path form when the message is already on disk."
    )]
    pub async fn ai_chat(
        &self,
        Parameters(params): Parameters<AiChatParams>,
    ) -> Result<CallToolResult, McpError> {
        let message = require_content_input(
            params.message.as_deref(),
            params.message_path.as_deref(),
            "message",
        )
        .map_err(tool_error)?;
        // Fall back to `settings.mcp.default_model` when the caller omits `model`
        // (issue #620); the tool parameter still takes precedence when supplied.
        let model = params
            .model
            .or_else(|| crate::utils::settings::Settings::load_mcp().default_model);
        let response = ai::run_chat(&message, model, params.system_prompt)
            .await
            .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(response)]))
    }

    /// Syncs Claude skills from the current repository into the current repo (and optionally its worktrees).
    #[tool(
        description = "Sync Claude Code skills from the current repository (the MCP server's \
                       current working directory) into target worktrees. MUTATES THE FILESYSTEM: \
                       creates symlinks inside `.claude/skills/` (e.g. \
                       `.claude/skills/my-skill -> ../../../.claude/skills/my-skill`) and upserts a \
                       managed block in `.git/info/exclude`. Operates relative to the server \
                       process's cwd — not cross-project. Use `claude_skills_clean` to reverse this \
                       and `claude_skills_status` to inspect the result without changing anything. \
                       Mirrors `omni-dev ai claude skills sync`."
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
        description = "Remove the skill symlinks under `.claude/skills/` and the managed exclude \
                       block created by a prior `claude_skills_sync` — the inverse of that tool. \
                       MUTATES THE FILESYSTEM. Real files (non-symlinks) are preserved, never \
                       deleted. Operates relative to the server process's cwd. Use \
                       `claude_skills_status` first if you want to see what would be removed. \
                       Mirrors `omni-dev ai claude skills clean`."
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
        description = "Report the skill symlinks under `.claude/skills/` and the managed \
                       exclude-block entries left by prior `claude_skills_sync` runs. READ-ONLY — \
                       changes nothing, so it is the safe way to preview before calling \
                       `claude_skills_sync` or `claude_skills_clean`. Operates relative to the \
                       server process's cwd. Mirrors `omni-dev ai claude skills status`."
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
        assert_eq!(params.message.as_deref(), Some("hi"));
        assert!(params.message_path.is_none());
        assert!(params.model.is_none());
        assert!(params.system_prompt.is_none());
    }

    #[test]
    fn ai_chat_params_deserializes_all_fields() {
        let params: AiChatParams = serde_json::from_str(
            r#"{"message":"hi","model":"claude-sonnet-4-6","system_prompt":"be helpful"}"#,
        )
        .unwrap();
        assert_eq!(params.message.as_deref(), Some("hi"));
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
            .filter_map(|c| match c {
                rmcp::model::ContentBlock::Text(t) => Some(t.text.as_str()),
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
                message: Some("hi".to_string()),
                message_path: None,
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
                message: Some("hi".to_string()),
                message_path: None,
                model: None,
                system_prompt: None,
            }))
            .await
            .unwrap_err();
        assert!(err.message.to_lowercase().contains("not found"));

        restore_ai_env(snap);
    }

    /// Writes `settings.json` under the given (tempdir) home so `load_mcp()`
    /// picks it up, with the supplied `mcp.default_model`.
    fn write_mcp_default_model(home: &std::path::Path, model: &str) {
        let dir = home.join(".omni-dev");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("settings.json"),
            format!(r#"{{"mcp":{{"default_model":"{model}"}}}}"#),
        )
        .unwrap();
    }

    /// Mounts a stub chat-completions endpoint and returns the running server;
    /// the model actually sent is asserted afterwards via `received_requests`.
    async fn mount_chat_stub() -> wiremock::MockServer {
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "test",
                    "object": "chat.completion",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "ok"},
                        "finish_reason": "stop"
                    }]
                })),
            )
            .mount(&mock)
            .await;
        mock
    }

    async fn model_sent_on_wire(mock: &wiremock::MockServer) -> String {
        // The Ollama backend also issues context-length probe requests, so pick
        // the chat-completions request out of the recorded set by its `messages`
        // field rather than assuming a single request.
        let received = mock.received_requests().await.expect("recorded requests");
        let chat = received
            .iter()
            .filter_map(|r| serde_json::from_slice::<serde_json::Value>(&r.body).ok())
            .find(|b| b.get("messages").is_some())
            .expect("a chat-completions request carrying messages");
        chat["model"].as_str().expect("model on wire").to_string()
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn ai_chat_uses_settings_default_model_when_param_absent() {
        // issue #620: `mcp.default_model` supplies the model when the tool's
        // `model` parameter is omitted.
        let _guard = AI_CHAT_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let snap = snapshot_ai_env();
        let home = tempdir();
        std::env::set_var("HOME", home.path());
        for k in AI_CHAT_KEYS.iter().filter(|k| **k != "HOME") {
            std::env::remove_var(k);
        }
        write_mcp_default_model(home.path(), "settings-model");

        let mock = mount_chat_stub().await;
        std::env::set_var("USE_OLLAMA", "true");
        std::env::set_var("OLLAMA_BASE_URL", mock.uri());

        let server = OmniDevServer::new();
        let result = server
            .ai_chat(Parameters(AiChatParams {
                message: Some("hi".to_string()),
                message_path: None,
                model: None,
                system_prompt: None,
            }))
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false));
        assert_eq!(model_sent_on_wire(&mock).await, "settings-model");

        restore_ai_env(snap);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn ai_chat_tool_model_param_overrides_settings_default() {
        // The tool's explicit `model` still wins over `mcp.default_model`.
        let _guard = AI_CHAT_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let snap = snapshot_ai_env();
        let home = tempdir();
        std::env::set_var("HOME", home.path());
        for k in AI_CHAT_KEYS.iter().filter(|k| **k != "HOME") {
            std::env::remove_var(k);
        }
        write_mcp_default_model(home.path(), "settings-model");

        let mock = mount_chat_stub().await;
        std::env::set_var("USE_OLLAMA", "true");
        std::env::set_var("OLLAMA_BASE_URL", mock.uri());

        let server = OmniDevServer::new();
        let result = server
            .ai_chat(Parameters(AiChatParams {
                message: Some("hi".to_string()),
                message_path: None,
                model: Some("param-model".to_string()),
                system_prompt: None,
            }))
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false));
        assert_eq!(model_sent_on_wire(&mock).await, "param-model");

        restore_ai_env(snap);
    }
}
