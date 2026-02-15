//! Claude API client implementation.

use std::future::Future;
use std::pin::Pin;

use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use super::{AiClient, AiClientMetadata};
use crate::claude::{error::ClaudeError, model_config::get_model_registry};

/// Claude API request message.
#[derive(Serialize)]
struct Message {
    role: String,
    content: String,
}

/// Claude API request body.
#[derive(Serialize)]
struct ClaudeRequest {
    model: String,
    max_tokens: i32,
    system: String,
    messages: Vec<Message>,
}

/// Claude API response content.
#[derive(Deserialize)]
struct Content {
    #[serde(rename = "type")]
    content_type: String,
    text: String,
}

/// Claude API response.
#[derive(Deserialize)]
struct ClaudeResponse {
    content: Vec<Content>,
}

/// Claude API client implementation.
pub struct ClaudeAiClient {
    /// HTTP client for API requests.
    client: Client,
    /// API key for authentication.
    api_key: String,
    /// Model identifier.
    model: String,
    /// Active beta header (key, value) if enabled.
    active_beta: Option<(String, String)>,
}

impl ClaudeAiClient {
    /// Creates a new Claude AI client.
    pub fn new(
        model: String,
        api_key: String,
        active_beta: Option<(String, String)>,
    ) -> Result<Self> {
        let client = Client::builder()
            .timeout(super::REQUEST_TIMEOUT)
            .build()
            .context("Failed to build HTTP client")?;

        Ok(Self {
            client,
            api_key,
            model,
            active_beta,
        })
    }

    /// Returns the max tokens from the model registry.
    fn get_max_tokens(&self) -> i32 {
        let registry = get_model_registry();
        if let Some((_, ref value)) = self.active_beta {
            registry.get_max_output_tokens_with_beta(&self.model, value) as i32
        } else {
            registry.get_max_output_tokens(&self.model) as i32
        }
    }
}

impl AiClient for ClaudeAiClient {
    fn send_request<'a>(
        &'a self,
        system_prompt: &'a str,
        user_prompt: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
        // Use Box::pin to wrap the async block in a Pin<Box<...>>
        Box::pin(async move {
            debug!(
                system_prompt_len = system_prompt.len(),
                user_prompt_len = user_prompt.len(),
                model = %self.model,
                "Preparing Claude API request"
            );

            debug!(
                system_prompt = %system_prompt,
                user_prompt = %user_prompt,
                "Claude API request content"
            );

            // Build request to Claude API
            let request = ClaudeRequest {
                model: self.model.clone(),
                max_tokens: self.get_max_tokens(),
                system: system_prompt.to_string(),
                messages: vec![Message {
                    role: "user".to_string(),
                    content: user_prompt.to_string(),
                }],
            };

            info!(
                url = "https://api.anthropic.com/v1/messages",
                model = %self.model,
                max_tokens = self.get_max_tokens(),
                "Sending request to Claude API"
            );

            // Send request to Claude API
            let mut builder = self
                .client
                .post("https://api.anthropic.com/v1/messages")
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json");

            if let Some((ref key, ref value)) = self.active_beta {
                debug!(header_key = %key, header_value = %value, "Adding beta header to Claude API request");
                builder = builder.header(key, value);
            }

            let response = builder
                .json(&request)
                .send()
                .await
                .map_err(|e| ClaudeError::NetworkError(e.to_string()))?;

            if !response.status().is_success() {
                let status = response.status();
                let error_text = response.text().await.unwrap_or_else(|e| {
                    tracing::debug!("Failed to read error response body: {e}");
                    String::new()
                });
                return Err(
                    ClaudeError::ApiRequestFailed(format!("HTTP {status}: {error_text}")).into(),
                );
            }

            let claude_response: ClaudeResponse = response
                .json()
                .await
                .map_err(|e| ClaudeError::InvalidResponseFormat(e.to_string()))?;

            debug!(
                content_count = claude_response.content.len(),
                "Received Claude API response"
            );

            // Extract text content from response
            let result = claude_response
                .content
                .first()
                .filter(|c| c.content_type == "text")
                .map(|c| c.text.clone())
                .ok_or_else(|| {
                    ClaudeError::InvalidResponseFormat("No text content in response".to_string())
                        .into()
                });

            if let Ok(ref text) = result {
                debug!(
                    response_len = text.len(),
                    "Successfully extracted text content from Claude API response"
                );
                debug!(
                    response_content = %text,
                    "Claude API response content"
                );
            }

            result
        })
    }

    fn get_metadata(&self) -> AiClientMetadata {
        let registry = get_model_registry();
        let (max_context_length, max_response_length) = match &self.active_beta {
            Some((_, value)) => (
                registry.get_input_context_with_beta(&self.model, value),
                registry.get_max_output_tokens_with_beta(&self.model, value),
            ),
            None => (
                registry.get_input_context(&self.model),
                registry.get_max_output_tokens(&self.model),
            ),
        };

        AiClientMetadata {
            provider: "Anthropic".to_string(),
            model: self.model.clone(),
            max_context_length,
            max_response_length,
            active_beta: self.active_beta.clone(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn claude_client_new() {
        let client = ClaudeAiClient::new(
            "claude-sonnet-4-20250514".to_string(),
            "sk-ant-test".to_string(),
            None,
        )
        .unwrap();
        assert_eq!(client.model, "claude-sonnet-4-20250514");
        assert_eq!(client.api_key, "sk-ant-test");
        assert!(client.active_beta.is_none());
    }

    #[test]
    fn claude_client_with_beta() {
        let beta = Some((
            "anthropic-beta".to_string(),
            "output-128k-2025-02-19".to_string(),
        ));
        let client = ClaudeAiClient::new(
            "claude-sonnet-4-20250514".to_string(),
            "key".to_string(),
            beta,
        )
        .unwrap();
        assert!(client.active_beta.is_some());
        let (key, value) = client.active_beta.unwrap();
        assert_eq!(key, "anthropic-beta");
        assert_eq!(value, "output-128k-2025-02-19");
    }

    #[test]
    fn get_max_tokens_known_model() {
        let client = ClaudeAiClient::new(
            "claude-sonnet-4-20250514".to_string(),
            "key".to_string(),
            None,
        )
        .unwrap();
        let tokens = client.get_max_tokens();
        assert!(tokens > 0, "expected positive token limit, got {tokens}");
    }

    #[test]
    fn get_max_tokens_legacy_model() {
        let client = ClaudeAiClient::new(
            "claude-3-opus-20240229".to_string(),
            "key".to_string(),
            None,
        )
        .unwrap();
        assert_eq!(client.get_max_tokens(), 4096);
    }

    #[test]
    fn get_metadata_without_beta() {
        let client = ClaudeAiClient::new(
            "claude-sonnet-4-20250514".to_string(),
            "key".to_string(),
            None,
        )
        .unwrap();
        let metadata = client.get_metadata();
        assert_eq!(metadata.provider, "Anthropic");
        assert_eq!(metadata.model, "claude-sonnet-4-20250514");
        assert!(metadata.active_beta.is_none());
        assert!(metadata.max_context_length > 0);
        assert!(metadata.max_response_length > 0);
    }

    #[test]
    fn get_metadata_with_beta() {
        let beta = Some((
            "anthropic-beta".to_string(),
            "output-128k-2025-02-19".to_string(),
        ));
        let client = ClaudeAiClient::new(
            "claude-sonnet-4-20250514".to_string(),
            "key".to_string(),
            beta,
        )
        .unwrap();
        let metadata = client.get_metadata();
        assert!(metadata.active_beta.is_some());
    }
}
