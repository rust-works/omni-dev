//! Bedrock API client implementation for Claude

use crate::claude::{
    ai_client::{AiClient, AiClientMetadata},
    error::ClaudeError,
};
use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use url::Url;

/// Bedrock API request message
#[derive(Serialize)]
struct Message {
    role: String,
    content: String,
}

/// Bedrock API request body
#[derive(Serialize)]
struct BedrockRequest {
    anthropic_version: String,
    max_tokens: i32,
    messages: Vec<Message>,
}

/// Bedrock API content
#[derive(Deserialize, Debug)]
struct Content {
    #[serde(rename = "type")]
    content_type: String,
    text: String,
}

/// Bedrock API response
#[derive(Deserialize, Debug)]
#[allow(dead_code)]
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

/// Bedrock API usage statistics
#[derive(Deserialize, Debug)]
#[allow(dead_code)]
struct Usage {
    input_tokens: i32,
    output_tokens: i32,
    #[serde(default)]
    cache_creation_input_tokens: i32,
    #[serde(default)]
    cache_read_input_tokens: i32,
}

/// Bedrock API client implementation for Claude
pub struct BedrockAiClient {
    /// HTTP client for API requests
    client: Client,
    /// Authorization token
    auth_token: String,
    /// Model identifier
    model: String,
    /// Base URL for the Bedrock API
    base_url: String,
}

impl BedrockAiClient {
    /// Create a new Bedrock AI client
    pub fn new(model: String, auth_token: String, base_url: String) -> Self {
        let client = Client::new();

        Self {
            client,
            auth_token,
            model,
            base_url,
        }
    }

    /// Create a model-specific token limit
    fn get_max_tokens(&self) -> i32 {
        if self.model.contains("sonnet") {
            8192
        } else if self.model.contains("opus") {
            12288
        } else if self.model.contains("haiku") {
            4096
        } else {
            4000 // default
        }
    }

    /// Build the full API URL
    fn get_api_url(&self) -> Result<String> {
        let mut url = Url::parse(&self.base_url)
            .map_err(|e| ClaudeError::NetworkError(format!("Invalid base URL: {}", e)))?;

        // Ensure the base URL ends with a trailing slash to preserve all path components
        if !url.as_str().ends_with('/') {
            url.set_path(&format!("{}/", url.path()));
        }

        // Create the base URL with path to the model endpoint
        let model_url = url.join("model/")
            .map_err(|e| ClaudeError::NetworkError(format!("Failed to build API URL: {}", e)))?;

        // Now properly URL-encode the model ID and add it to the path
        let encoded_model = url::form_urlencoded::byte_serialize(self.model.as_bytes()).collect::<String>();

        // Join the encoded model and invoke endpoint
        let full_url = model_url.join(&format!("{}/invoke", encoded_model))
            .map_err(|e| ClaudeError::NetworkError(format!("Failed to build API URL with encoded model: {}", e)))?;

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
            // For Bedrock API, we need to include system prompt as a system message
            let mut messages = vec![];

            // Add system message if provided
            if !system_prompt.is_empty() {
                messages.push(Message {
                    role: "system".to_string(),
                    content: system_prompt.to_string(),
                });
            }

            // Add user message
            messages.push(Message {
                role: "user".to_string(),
                content: user_prompt.to_string(),
            });

            // Build request to Bedrock API
            let request = BedrockRequest {
                anthropic_version: "bedrock-2023-05-31".to_string(),
                max_tokens: self.get_max_tokens(),
                messages,
            };

            // Get the API URL
            let api_url = self.get_api_url()?;

            // Send request to Bedrock API
            let response = self
                .client
                .post(&api_url)
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {}", self.auth_token))
                .json(&request)
                .send()
                .await
                .map_err(|e| ClaudeError::NetworkError(e.to_string()))?;

            if !response.status().is_success() {
                let status = response.status();
                let error_text = response.text().await.unwrap_or_default();
                return Err(ClaudeError::ApiRequestFailed(format!(
                    "HTTP {}: {}",
                    status, error_text
                ))
                .into());
            }

            let bedrock_response: BedrockResponse = response
                .json()
                .await
                .map_err(|e| ClaudeError::InvalidResponseFormat(e.to_string()))?;

            // Extract text content from response
            bedrock_response
                .content
                .first()
                .filter(|c| c.content_type == "text")
                .map(|c| c.text.clone())
                .ok_or_else(|| {
                    ClaudeError::InvalidResponseFormat("No text content in response".to_string())
                        .into()
                })
        })
    }

    fn get_metadata(&self) -> AiClientMetadata {
        // Determine context length based on model
        let (max_context_length, max_response_length) = if self.model.contains("sonnet") {
            (180000, 8192)
        } else if self.model.contains("opus") {
            (200000, 12288)
        } else if self.model.contains("haiku") {
            (150000, 4096)
        } else {
            (100000, 4000) // default for older models
        };

        AiClientMetadata {
            provider: "Anthropic Bedrock".to_string(),
            model: self.model.clone(),
            max_context_length,
            max_response_length,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_api_url() {
        let client = BedrockAiClient::new(
            "us.anthropic.claude-3-7-sonnet-20250219-v1:0".to_string(),
            "test_token".to_string(),
            "https://bedrock-api.com/bedrock".to_string(),
        );

        let url = client.get_api_url().unwrap();
        assert_eq!(
            url,
            "https://bedrock-api.com/bedrock/model/us.anthropic.claude-3-7-sonnet-20250219-v1%3A0/invoke"
        );
    }

    #[test]
    fn test_get_max_tokens() {
        // Test sonnet model
        let client = BedrockAiClient::new(
            "claude-3-sonnet".to_string(),
            "test_token".to_string(),
            "https://example.com".to_string(),
        );
        assert_eq!(client.get_max_tokens(), 8192);

        // Test opus model
        let client = BedrockAiClient::new(
            "claude-3-opus".to_string(),
            "test_token".to_string(),
            "https://example.com".to_string(),
        );
        assert_eq!(client.get_max_tokens(), 12288);

        // Test haiku model
        let client = BedrockAiClient::new(
            "claude-3-haiku".to_string(),
            "test_token".to_string(),
            "https://example.com".to_string(),
        );
        assert_eq!(client.get_max_tokens(), 4096);

        // Test unknown model
        let client = BedrockAiClient::new(
            "unknown-model".to_string(),
            "test_token".to_string(),
            "https://example.com".to_string(),
        );
        assert_eq!(client.get_max_tokens(), 4000);
    }
}
