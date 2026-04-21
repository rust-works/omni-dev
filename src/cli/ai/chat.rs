//! Interactive AI chat command.

use std::io::{self, Write};

use anyhow::Result;
use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode},
};

/// Interactive AI chat session.
#[derive(Parser)]
pub struct ChatCommand {
    /// AI model to use (overrides environment configuration).
    #[arg(long)]
    pub model: Option<String>,
}

impl ChatCommand {
    /// Executes the chat command.
    pub async fn execute(self) -> Result<()> {
        let ai_info = crate::utils::preflight::check_ai_credentials(self.model.as_deref())?;
        eprintln!(
            "Connected to {} (model: {})",
            ai_info.provider, ai_info.model
        );
        eprintln!("Enter to send, Shift+Enter for newline, Ctrl+D to exit.\n");

        let client = crate::claude::create_default_claude_client(self.model, None)?;

        chat_loop(&client).await
    }
}

/// Sends a single user message to the configured AI and returns the response.
///
/// Shared between the MCP `ai_chat` tool and any non-interactive CLI callers.
/// The function performs the same preflight credential check as the CLI chat
/// loop — on missing credentials it returns the preflight error verbatim so
/// MCP tool callers see the same diagnostic message the CLI would print.
///
/// `model` selects the AI model; `None` uses the environment default.
/// `system_prompt` defaults to `"You are a helpful assistant."` (matching the
/// CLI's default) when `None`.
pub async fn run_chat(
    message: &str,
    model: Option<String>,
    system_prompt: Option<String>,
) -> Result<String> {
    crate::utils::preflight::check_ai_credentials(model.as_deref())?;
    let client = crate::claude::create_default_claude_client(model, None)?;
    let system = system_prompt
        .as_deref()
        .unwrap_or("You are a helpful assistant.");
    client.send_message(system, message).await
}

async fn chat_loop(client: &crate::claude::client::ClaudeClient) -> Result<()> {
    let system_prompt = "You are a helpful assistant.";

    loop {
        let input = match read_user_input() {
            Ok(Some(text)) => text,
            Ok(None) => {
                eprintln!("\nGoodbye!");
                break;
            }
            Err(e) => {
                eprintln!("\nInput error: {e}");
                break;
            }
        };

        let trimmed = input.trim();
        if trimmed.is_empty() {
            continue;
        }

        let response = client.send_message(system_prompt, trimmed).await?;
        println!("{response}\n");
    }

    Ok(())
}

/// Guard that disables raw mode on drop.
struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

/// Reads multiline user input with "> " prompt.
///
/// Returns `Ok(Some(text))` on Enter, `Ok(None)` on Ctrl+D/Ctrl+C.
fn read_user_input() -> Result<Option<String>> {
    eprint!("> ");
    io::stderr().flush()?;

    enable_raw_mode()?;
    let _guard = RawModeGuard;

    let mut buffer = String::new();

    loop {
        if let Event::Key(key_event) = event::read()? {
            match key_event.code {
                KeyCode::Enter => {
                    if key_event.modifiers.contains(KeyModifiers::SHIFT) {
                        buffer.push('\n');
                        eprint!("\r\n... ");
                        io::stderr().flush()?;
                    } else {
                        eprint!("\r\n");
                        io::stderr().flush()?;
                        return Ok(Some(buffer));
                    }
                }
                KeyCode::Char('d') if key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                    if buffer.is_empty() {
                        return Ok(None);
                    }
                    eprint!("\r\n");
                    io::stderr().flush()?;
                    return Ok(Some(buffer));
                }
                KeyCode::Char('c') if key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(None);
                }
                KeyCode::Char(c) => {
                    buffer.push(c);
                    eprint!("{c}");
                    io::stderr().flush()?;
                }
                KeyCode::Backspace if buffer.pop().is_some() => {
                    eprint!("\x08 \x08");
                    io::stderr().flush()?;
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Env-isolation lock — tests in this module must serialise because they
    /// mutate `HOME` and every provider env var the preflight check reads.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    const KEYS: &[&str] = &[
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
    ];

    fn snapshot_env() -> Vec<(&'static str, Option<String>)> {
        let mut v: Vec<(&'static str, Option<String>)> =
            KEYS.iter().map(|k| (*k, std::env::var(k).ok())).collect();
        v.push(("HOME", std::env::var("HOME").ok()));
        v
    }

    fn restore_env(snap: Vec<(&'static str, Option<String>)>) {
        for (k, v) in snap {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
    }

    fn isolate_empty_home() -> tempfile::TempDir {
        let dir = {
            std::fs::create_dir_all("tmp").ok();
            tempfile::TempDir::new_in("tmp").unwrap()
        };
        std::env::set_var("HOME", dir.path());
        for k in KEYS {
            std::env::remove_var(k);
        }
        dir
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn run_chat_returns_error_when_credentials_missing() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let snap = snapshot_env();
        let _home = isolate_empty_home();

        let err = run_chat("hello", None, None).await.unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("API key not found") || msg.contains("not found"),
            "expected credential error, got: {msg}"
        );

        restore_env(snap);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn run_chat_bubbles_up_credential_error_with_custom_system_prompt() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let snap = snapshot_env();
        let _home = isolate_empty_home();

        // Custom system prompt should not bypass the preflight check.
        let err = run_chat("hello", None, Some("be terse".to_string()))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("not found"));

        restore_env(snap);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn run_chat_propagates_model_override_through_preflight() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let snap = snapshot_env();
        let _home = isolate_empty_home();

        // With explicit model override, the same credential check must still run.
        let err = run_chat("hello", Some("claude-sonnet-4-6".to_string()), None)
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("not found"));

        restore_env(snap);
    }

    /// Exercises the post-preflight code path (client construction and
    /// `send_message`) without requiring real AI credentials. Routes through
    /// Ollama mode, which skips the credential check, and points the client
    /// at a wiremock server that returns a canned OpenAI-compatible response.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn run_chat_happy_path_via_mocked_ollama_returns_response_text() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let snap = snapshot_env();
        let _home = isolate_empty_home();

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "test",
                    "object": "chat.completion",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "canned-response"},
                        "finish_reason": "stop"
                    }]
                })),
            )
            .mount(&server)
            .await;

        std::env::set_var("USE_OLLAMA", "true");
        std::env::set_var("OLLAMA_MODEL", "llama2");
        std::env::set_var("OLLAMA_BASE_URL", server.uri());

        let out = run_chat("hello", None, Some("be terse".to_string()))
            .await
            .unwrap();
        assert_eq!(out, "canned-response");

        restore_env(snap);
    }

    /// As above but with `system_prompt = None`, exercising the
    /// `.unwrap_or("You are a helpful assistant.")` default branch.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn run_chat_default_system_prompt_path_via_mocked_ollama() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let snap = snapshot_env();
        let _home = isolate_empty_home();

        let server = wiremock::MockServer::start().await;
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
            .mount(&server)
            .await;

        std::env::set_var("USE_OLLAMA", "true");
        std::env::set_var("OLLAMA_MODEL", "llama2");
        std::env::set_var("OLLAMA_BASE_URL", server.uri());

        let out = run_chat("hello", None, None).await.unwrap();
        assert_eq!(out, "ok");

        restore_env(snap);
    }
}
