//! AI client trait and metadata definitions.

pub mod bedrock;
pub mod claude;
pub mod openai;

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Client;

use crate::claude::error::ClaudeError;
use crate::claude::model_config::get_model_registry;

/// HTTP request timeout for AI API calls.
///
/// Set to 5 minutes to accommodate large prompts and long model responses
/// (up to 64k output tokens) while preventing indefinite hangs.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// Metadata about an AI client implementation.
#[derive(Clone, Debug)]
pub struct AiClientMetadata {
    /// Service provider name.
    pub provider: String,
    /// Model identifier.
    pub model: String,
    /// Maximum context length supported.
    pub max_context_length: usize,
    /// Maximum token response length supported.
    pub max_response_length: usize,
    /// Active beta header, if any: (key, value).
    pub active_beta: Option<(String, String)>,
}

/// Prompt formatting families for AI providers.
///
/// Determines provider-specific prompt behaviour (e.g., how template
/// instructions are phrased). Parse once at the boundary via
/// [`AiClientMetadata::prompt_style`] and match on the enum downstream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptStyle {
    /// Claude models handle "literal template" instructions correctly.
    Claude,
    /// OpenAI-compatible models (OpenAI, Ollama) need different formatting.
    OpenAi,
}

impl AiClientMetadata {
    /// Derives the prompt style from the provider name.
    ///
    /// Matches against the exact strings set by each [`AiClient`] implementation:
    /// - `"OpenAI"` and `"Ollama"` → [`PromptStyle::OpenAi`]
    /// - `"Anthropic"` and `"Anthropic Bedrock"` → [`PromptStyle::Claude`]
    ///
    /// Unrecognised provider strings default to [`PromptStyle::Claude`].
    #[must_use]
    pub fn prompt_style(&self) -> PromptStyle {
        match self.provider.as_str() {
            "OpenAI" | "Ollama" => PromptStyle::OpenAi,
            _ => PromptStyle::Claude,
        }
    }
}

// ── Shared helpers for AI client implementations ────────────────────

/// Builds an HTTP client with the standard request timeout.
pub(crate) fn build_http_client() -> Result<Client> {
    Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("Failed to build HTTP client")
}

/// Returns the maximum output tokens for a model from the registry,
/// respecting beta overrides.
#[must_use]
pub(crate) fn registry_max_output_tokens(
    model: &str,
    active_beta: &Option<(String, String)>,
) -> i32 {
    let registry = get_model_registry();
    if let Some((_, value)) = active_beta {
        registry.get_max_output_tokens_with_beta(model, value) as i32
    } else {
        registry.get_max_output_tokens(model) as i32
    }
}

/// Returns the (input context length, max response length) for a model
/// from the registry, respecting beta overrides.
#[must_use]
pub(crate) fn registry_model_limits(
    model: &str,
    active_beta: &Option<(String, String)>,
) -> (usize, usize) {
    let registry = get_model_registry();
    match active_beta {
        Some((_, value)) => (
            registry.get_input_context_with_beta(model, value),
            registry.get_max_output_tokens_with_beta(model, value),
        ),
        None => (
            registry.get_input_context(model),
            registry.get_max_output_tokens(model),
        ),
    }
}

/// Checks an HTTP response for error status and returns a structured error
/// if non-success.
///
/// On success, returns the response unchanged for further processing.
/// On failure, reads the error body and returns a
/// [`ClaudeError::ApiRequestFailed`].
pub(crate) async fn check_error_response(response: reqwest::Response) -> Result<reqwest::Response> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let error_text = response.text().await.unwrap_or_else(|e| {
        tracing::debug!("Failed to read error response body: {e}");
        String::new()
    });
    Err(ClaudeError::ApiRequestFailed(format!("HTTP {status}: {error_text}")).into())
}

/// Logs successful text extraction from an AI API response.
pub(crate) fn log_response_success(provider: &str, result: &Result<String>) {
    if let Ok(text) = result {
        tracing::debug!(
            response_len = text.len(),
            "Successfully extracted text content from {} API response",
            provider
        );
        tracing::debug!(
            response_content = %text,
            "{} API response content",
            provider
        );
    }
}

/// Trait for AI service clients.
pub trait AiClient: Send + Sync {
    /// Sends a request to the AI service and returns the raw response.
    fn send_request<'a>(
        &'a self,
        system_prompt: &'a str,
        user_prompt: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;

    /// Returns metadata about the AI client implementation.
    fn get_metadata(&self) -> AiClientMetadata;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(provider: &str) -> AiClientMetadata {
        AiClientMetadata {
            provider: provider.to_string(),
            model: "test-model".to_string(),
            max_context_length: 1024,
            max_response_length: 1024,
            active_beta: None,
        }
    }

    #[test]
    fn prompt_style_openai() {
        assert_eq!(meta("OpenAI").prompt_style(), PromptStyle::OpenAi);
    }

    #[test]
    fn prompt_style_ollama() {
        assert_eq!(meta("Ollama").prompt_style(), PromptStyle::OpenAi);
    }

    #[test]
    fn prompt_style_anthropic() {
        assert_eq!(meta("Anthropic").prompt_style(), PromptStyle::Claude);
    }

    #[test]
    fn prompt_style_bedrock() {
        assert_eq!(
            meta("Anthropic Bedrock").prompt_style(),
            PromptStyle::Claude
        );
    }

    #[test]
    fn prompt_style_unknown_defaults_to_claude() {
        assert_eq!(meta("SomeNewProvider").prompt_style(), PromptStyle::Claude);
    }

    /// Ensure case-sensitive matching: "openai" (lowercase) is not a known provider
    /// string and must not silently match as OpenAI.
    #[test]
    fn prompt_style_case_sensitive() {
        assert_eq!(meta("openai").prompt_style(), PromptStyle::Claude);
        assert_eq!(meta("ollama").prompt_style(), PromptStyle::Claude);
    }
}
