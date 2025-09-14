//! Claude API client implementation

use crate::claude::{ai_client::{AiClient, AiClientMetadata}, error::ClaudeError};
use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;

/// Claude API request message
#[derive(Serialize)]
struct Message {
    role: String,
    content: String,
}

/// Claude API request body
#[derive(Serialize)]
struct ClaudeRequest {
    model: String,
    max_tokens: i32,
    system: String,
    messages: Vec<Message>,
}

/// Claude API response content
#[derive(Deserialize)]
struct Content {
    #[serde(rename = "type")]
    content_type: String,
    text: String,
}

/// Claude API response
#[derive(Deserialize)]
struct ClaudeResponse {
    content: Vec<Content>,
}

/// Claude API client implementation
pub struct ClaudeAiClient {
    /// HTTP client for API requests
    client: Client,
    /// API key for authentication
    api_key: String,
    /// Model identifier
    model: String,
}

impl ClaudeAiClient {
    /// Create a new Claude AI client
    pub fn new(model: String, api_key: String) -> Self {
        let client = Client::new();

        Self {
            client,
            api_key,
            model,
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
}

impl AiClient for ClaudeAiClient {
    fn send_request<'a>(
        &'a self,
        system_prompt: &'a str,
        user_prompt: &'a str
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
        // Use Box::pin to wrap the async block in a Pin<Box<...>>
        Box::pin(async move {
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

            // Send request to Claude API
            let response = self
                .client
                .post("https://api.anthropic.com/v1/messages")
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .json(&request)
                .send()
                .await
                .map_err(|e| ClaudeError::NetworkError(e.to_string()))?;

            if !response.status().is_success() {
                let status = response.status();
                let error_text = response.text().await.unwrap_or_default();
                return Err(
                    ClaudeError::ApiRequestFailed(format!("HTTP {}: {}", status, error_text)).into(),
                );
            }

            let claude_response: ClaudeResponse = response
                .json()
                .await
                .map_err(|e| ClaudeError::InvalidResponseFormat(e.to_string()))?;

            // Extract text content from response
            claude_response
                .content
                .first()
                .filter(|c| c.content_type == "text")
                .map(|c| c.text.clone())
                .ok_or_else(|| {
                    ClaudeError::InvalidResponseFormat("No text content in response".to_string()).into()
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
            (100000, 4000)  // default for older models
        };

        AiClientMetadata {
            provider: "Anthropic".to_string(),
            model: self.model.clone(),
            max_context_length,
            max_response_length,
        }
    }
}