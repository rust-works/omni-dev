//! Bedrock API client implementation for Claude.

use std::future::Future;
use std::pin::Pin;

use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};
use url::Url;

use super::{AiClient, AiClientMetadata};
use crate::claude::error::ClaudeError;

/// Bedrock API request message.
#[derive(Serialize)]
struct Message {
    role: String,
    content: String,
}

/// Bedrock API request body.
#[derive(Serialize)]
struct BedrockRequest {
    anthropic_version: String,
    max_tokens: i32,
    system: Option<String>,
    messages: Vec<Message>,
}

/// Bedrock API content.
#[derive(Deserialize, Debug)]
struct Content {
    #[serde(rename = "type")]
    content_type: String,
    text: String,
}

/// Bedrock API response.
#[derive(Deserialize, Debug)]
#[allow(dead_code)] // Fields populated by serde deserialization, not accessed directly
struct BedrockResponse {
    id: String,
    #[serde(rename = "type")]
    response_type: String,
    role: String,
    model: String,
    content: Vec<Content>,
    stop_reason: Option<String>,
    stop_sequence: Option<String>,
    usage: Option<Usage>,
}

/// Bedrock API usage statistics.
#[derive(Deserialize, Debug)]
#[allow(dead_code)] // Fields populated by serde deserialization, not accessed directly
struct Usage {
    input_tokens: i32,
    output_tokens: i32,
    #[serde(default)]
    cache_creation_input_tokens: i32,
    #[serde(default)]
    cache_read_input_tokens: i32,
}

/// Bedrock API client implementation for Claude.
pub struct BedrockAiClient {
    /// HTTP client for API requests.
    client: Client,
    /// Authorization token.
    auth_token: String,
    /// Model identifier.
    model: String,
    /// Base URL for the Bedrock API.
    base_url: String,
    /// Active beta header (key, value) if enabled.
    active_beta: Option<(String, String)>,
}

impl BedrockAiClient {
    /// Creates a new Bedrock AI client.
    pub fn new(
        model: String,
        auth_token: String,
        base_url: String,
        active_beta: Option<(String, String)>,
    ) -> Result<Self> {
        let client = super::build_http_client()?;

        Ok(Self {
            client,
            auth_token,
            model,
            base_url,
            active_beta,
        })
    }

    /// Returns the max tokens from the model registry.
    fn get_max_tokens(&self) -> i32 {
        super::registry_max_output_tokens(&self.model, &self.active_beta)
    }

    /// Builds the full API URL.
    fn get_api_url(&self) -> Result<String> {
        let mut url = Url::parse(&self.base_url)
            .map_err(|e| ClaudeError::NetworkError(format!("Invalid base URL: {e}")))?;

        // Ensure the base URL ends with a trailing slash to preserve all path components
        if !url.as_str().ends_with('/') {
            url.set_path(&format!("{}/", url.path()));
        }

        // Create the base URL with path to the model endpoint
        let model_url = url
            .join("model/")
            .map_err(|e| ClaudeError::NetworkError(format!("Failed to build API URL: {e}")))?;

        // Now properly URL-encode the model ID and add it to the path
        let encoded_model =
            url::form_urlencoded::byte_serialize(self.model.as_bytes()).collect::<String>();
        debug!(original_model = %self.model, encoded_model = %encoded_model, "URL-encoded model ID");

        // Join the encoded model and invoke endpoint
        let full_url = model_url
            .join(&format!("{encoded_model}/invoke"))
            .map_err(|e| {
                ClaudeError::NetworkError(format!(
                    "Failed to build API URL with encoded model: {e}"
                ))
            })?;

        debug!(base_url = %self.base_url, full_url = %full_url, "Constructed Bedrock API URL");

        Ok(full_url.to_string())
    }
}

impl AiClient for BedrockAiClient {
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
                "Preparing Bedrock API request"
            );

            debug!(
                system_prompt = %system_prompt,
                user_prompt = %user_prompt,
                "Bedrock API request content"
            );

            // For Bedrock API, the system prompt is a separate parameter
            let messages = vec![Message {
                role: "user".to_string(),
                content: user_prompt.to_string(),
            }];

            // Build request to Bedrock API
            let request = BedrockRequest {
                anthropic_version: "bedrock-2023-05-31".to_string(),
                max_tokens: self.get_max_tokens(),
                // Add system prompt as a separate field if it exists
                system: if !system_prompt.is_empty() {
                    Some(system_prompt.to_string())
                } else {
                    None
                },
                messages,
            };

            debug!(
                system_prompt_len = system_prompt.len(),
                max_tokens = self.get_max_tokens(),
                "Built Bedrock request payload"
            );

            // Get the API URL
            let api_url = self.get_api_url()?;

            // Log the URL for debugging purposes
            info!(url = %api_url, "Sending request to Bedrock API");
            debug!(model = %self.model, "Using model for Bedrock request");

            // Send request to Bedrock API
            let mut builder = self
                .client
                .post(&api_url)
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {}", self.auth_token));

            if let Some((ref key, ref value)) = self.active_beta {
                debug!(header_key = %key, header_value = %value, "Adding beta header to Bedrock API request");
                builder = builder.header(key, value);
            }

            let response = builder
                .json(&request)
                .send()
                .await
                .map_err(|e| ClaudeError::NetworkError(e.to_string()))?;

            let response = super::check_error_response(response).await?;

            let bedrock_response: BedrockResponse = response
                .json()
                .await
                .map_err(|e| ClaudeError::InvalidResponseFormat(e.to_string()))?;

            debug!(
                response_id = %bedrock_response.id,
                response_type = %bedrock_response.response_type,
                content_count = bedrock_response.content.len(),
                stop_reason = ?bedrock_response.stop_reason,
                usage = ?bedrock_response.usage,
                "Received Bedrock API response"
            );

            // Extract text content from response
            let result = bedrock_response
                .content
                .first()
                .filter(|c| c.content_type == "text")
                .map(|c| c.text.clone())
                .ok_or_else(|| {
                    ClaudeError::InvalidResponseFormat("No text content in response".to_string())
                        .into()
                });

            super::log_response_success("Bedrock", &result);

            result
        })
    }

    fn get_metadata(&self) -> AiClientMetadata {
        let (max_context_length, max_response_length) =
            super::registry_model_limits(&self.model, &self.active_beta);

        AiClientMetadata {
            provider: "Anthropic Bedrock".to_string(),
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
    fn get_api_url() {
        let client = BedrockAiClient::new(
            "us.anthropic.claude-3-7-sonnet-20250219-v1:0".to_string(),
            "test_token".to_string(),
            "https://bedrock-api.com/bedrock".to_string(),
            None,
        )
        .unwrap();

        let url = client.get_api_url().unwrap();
        assert_eq!(
            url,
            "https://bedrock-api.com/bedrock/model/us.anthropic.claude-3-7-sonnet-20250219-v1%3A0/invoke"
        );
    }

    #[test]
    fn get_max_tokens() {
        // Test legacy Claude 3 Opus
        let client = BedrockAiClient::new(
            "claude-3-opus-20240229".to_string(),
            "test_token".to_string(),
            "https://example.com".to_string(),
            None,
        )
        .unwrap();
        assert_eq!(client.get_max_tokens(), 4096); // Correct legacy limit

        // Test Claude Sonnet 4
        let client = BedrockAiClient::new(
            "claude-sonnet-4-20250514".to_string(),
            "test_token".to_string(),
            "https://example.com".to_string(),
            None,
        )
        .unwrap();
        assert_eq!(client.get_max_tokens(), 64000); // New high limit

        // Test unknown model falls back to provider defaults
        let client = BedrockAiClient::new(
            "claude-unknown-model".to_string(),
            "test_token".to_string(),
            "https://example.com".to_string(),
            None,
        )
        .unwrap();
        assert_eq!(client.get_max_tokens(), 4096); // Claude provider default
    }

    #[test]
    fn get_api_url_with_trailing_slash() {
        let client = BedrockAiClient::new(
            "us.anthropic.claude-3-7-sonnet-20250219-v1:0".to_string(),
            "test_token".to_string(),
            "https://bedrock-api.com/bedrock/".to_string(),
            None,
        )
        .unwrap();

        let url = client.get_api_url().unwrap();
        assert_eq!(
            url,
            "https://bedrock-api.com/bedrock/model/us.anthropic.claude-3-7-sonnet-20250219-v1%3A0/invoke"
        );
    }

    #[test]
    fn get_api_url_simple_model() {
        let client = BedrockAiClient::new(
            "claude-sonnet-4-20250514".to_string(),
            "test_token".to_string(),
            "https://bedrock-api.com/api".to_string(),
            None,
        )
        .unwrap();

        let url = client.get_api_url().unwrap();
        assert!(url.contains("model/claude-sonnet-4-20250514/invoke"));
    }

    #[test]
    fn get_metadata_without_beta() {
        let client = BedrockAiClient::new(
            "claude-sonnet-4-20250514".to_string(),
            "token".to_string(),
            "https://example.com".to_string(),
            None,
        )
        .unwrap();
        let metadata = client.get_metadata();
        assert_eq!(metadata.provider, "Anthropic Bedrock");
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
        let client = BedrockAiClient::new(
            "claude-sonnet-4-20250514".to_string(),
            "token".to_string(),
            "https://example.com".to_string(),
            beta,
        )
        .unwrap();
        let metadata = client.get_metadata();
        assert!(metadata.active_beta.is_some());
    }

    #[test]
    fn bedrock_client_new() {
        let client = BedrockAiClient::new(
            "claude-sonnet-4-20250514".to_string(),
            "my-token".to_string(),
            "https://bedrock.us-east-1.amazonaws.com".to_string(),
            None,
        )
        .unwrap();
        assert_eq!(client.model, "claude-sonnet-4-20250514");
        assert_eq!(client.auth_token, "my-token");
        assert_eq!(client.base_url, "https://bedrock.us-east-1.amazonaws.com");
        assert!(client.active_beta.is_none());
    }
}
