//! OpenAI-compatible API client implementation (works with OpenAI, Ollama, etc.)

use super::{AiClient, AiClientMetadata};
use crate::claude::{error::ClaudeError, model_config::get_model_registry};
use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use tracing::{debug, info};

/// OpenAI API request message
#[derive(Serialize, Debug)]
struct Message {
    role: String,
    content: String,
}

/// OpenAI API request body
#[derive(Serialize, Debug)]
struct OpenAiRequest {
    model: String,
    messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    stream: bool,
}

/// OpenAI API response choice
#[derive(Deserialize, Debug)]
struct Choice {
    message: ResponseMessage,
    #[allow(dead_code)]
    finish_reason: Option<String>,
}

/// OpenAI API response message
#[derive(Deserialize, Debug)]
struct ResponseMessage {
    #[allow(dead_code)]
    role: String,
    content: String,
}

/// OpenAI API response
#[derive(Deserialize, Debug)]
struct OpenAiResponse {
    choices: Vec<Choice>,
    model: Option<String>,
    usage: Option<Usage>,
}

/// OpenAI API usage statistics
#[derive(Deserialize, Debug)]
#[allow(dead_code)]
struct Usage {
    prompt_tokens: Option<i32>,
    completion_tokens: Option<i32>,
    total_tokens: Option<i32>,
}

/// OpenAI-compatible API client (works with OpenAI, Ollama, etc.)
pub struct OpenAiAiClient {
    /// HTTP client for API requests
    client: Client,
    /// API key for authentication (optional for Ollama)
    api_key: Option<String>,
    /// Model identifier
    model: String,
    /// Base URL for the API (e.g., "https://api.openai.com" or "http://localhost:11434")
    base_url: String,
    /// Maximum tokens for responses
    max_tokens: Option<i32>,
    /// Temperature for response generation
    temperature: Option<f32>,
    /// Active beta header (key, value) if enabled
    active_beta: Option<(String, String)>,
}

impl OpenAiAiClient {
    /// Create a new OpenAI-compatible API client
    pub fn new(
        model: String,
        api_key: Option<String>,
        base_url: String,
        max_tokens: Option<i32>,
        temperature: Option<f32>,
        active_beta: Option<(String, String)>,
    ) -> Self {
        let client = Client::new();

        Self {
            client,
            api_key,
            model,
            base_url,
            max_tokens,
            temperature,
            active_beta,
        }
    }

    /// Create a new client for Ollama with sensible defaults
    pub fn new_ollama(
        model: String,
        base_url: Option<String>,
        active_beta: Option<(String, String)>,
    ) -> Self {
        Self::new(
            model,
            None, // No API key needed for Ollama
            base_url.unwrap_or_else(|| "http://localhost:11434".to_string()),
            Some(4096), // Reasonable default
            Some(0.1),  // Low temperature for consistent output
            active_beta,
        )
    }

    /// Create a new client for OpenAI with sensible defaults
    pub fn new_openai(
        model: String,
        api_key: String,
        active_beta: Option<(String, String)>,
    ) -> Self {
        Self::new(
            model,
            Some(api_key),
            "https://api.openai.com".to_string(),
            None,      // Use model registry for max tokens
            Some(0.1), // Low temperature for consistent output
            active_beta,
        )
    }

    /// Get max tokens from model registry or fallback to configured value
    fn get_max_tokens(&self) -> i32 {
        if let Some(configured_max) = self.max_tokens {
            return configured_max;
        }

        let registry = get_model_registry();
        if let Some((_, ref value)) = self.active_beta {
            registry.get_max_output_tokens_with_beta(&self.model, value) as i32
        } else {
            registry.get_max_output_tokens(&self.model) as i32
        }
    }

    /// Build the full API URL
    fn get_api_url(&self) -> Result<String> {
        let mut base = self.base_url.clone();

        // Ensure base URL doesn't end with a slash
        if base.ends_with('/') {
            base.pop();
        }

        // Add the chat completions endpoint
        let url = format!("{}/v1/chat/completions", base);

        debug!(base_url = %self.base_url, full_url = %url, "Constructed OpenAI-compatible API URL");

        Ok(url)
    }

    /// Determine if this is likely an Ollama instance
    fn is_ollama(&self) -> bool {
        self.base_url.contains("localhost")
            || self.base_url.contains("127.0.0.1")
            || self.api_key.is_none()
    }

    /// Determine if this model is GPT-5 series (uses max_completion_tokens instead of max_tokens)
    fn is_gpt5_series(&self) -> bool {
        self.model.starts_with("gpt-5") || self.model.starts_with("o1")
    }
}

impl AiClient for OpenAiAiClient {
    fn send_request<'a>(
        &'a self,
        system_prompt: &'a str,
        user_prompt: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
        Box::pin(async move {
            debug!(
                system_prompt_len = system_prompt.len(),
                user_prompt_len = user_prompt.len(),
                model = %self.model,
                base_url = %self.base_url,
                is_ollama = self.is_ollama(),
                "Preparing OpenAI-compatible API request"
            );

            // Build messages array with system prompt first, then user prompt
            let mut messages = Vec::new();

            if !system_prompt.is_empty() {
                messages.push(Message {
                    role: "system".to_string(),
                    content: system_prompt.to_string(),
                });
            }

            messages.push(Message {
                role: "user".to_string(),
                content: user_prompt.to_string(),
            });

            let max_tokens = self.get_max_tokens();
            let request = if self.is_gpt5_series() {
                OpenAiRequest {
                    model: self.model.clone(),
                    messages,
                    max_tokens: None,
                    max_completion_tokens: Some(max_tokens),
                    temperature: None, // GPT-5 only supports default temperature (1.0)
                    stream: false,
                }
            } else {
                OpenAiRequest {
                    model: self.model.clone(),
                    messages,
                    max_tokens: Some(max_tokens),
                    max_completion_tokens: None,
                    temperature: self.temperature,
                    stream: false,
                }
            };

            debug!(
                max_tokens = max_tokens,
                configured_temperature = ?self.temperature,
                effective_temperature = ?request.temperature,
                message_count = request.messages.len(),
                is_gpt5_series = self.is_gpt5_series(),
                uses_max_completion_tokens = self.is_gpt5_series(),
                "Built OpenAI-compatible request payload"
            );

            let api_url = self.get_api_url()?;
            info!(url = %api_url, model = %self.model, "Sending request to OpenAI-compatible API");

            // Build the request
            let mut req_builder = self
                .client
                .post(&api_url)
                .header("Content-Type", "application/json")
                .json(&request);

            // Add authorization header if API key is provided
            if let Some(ref api_key) = self.api_key {
                req_builder = req_builder.header("Authorization", format!("Bearer {}", api_key));
            }

            let response = req_builder
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

            let openai_response: OpenAiResponse = response
                .json()
                .await
                .map_err(|e| ClaudeError::InvalidResponseFormat(e.to_string()))?;

            debug!(
                choice_count = openai_response.choices.len(),
                model = ?openai_response.model,
                usage = ?openai_response.usage,
                "Received OpenAI-compatible API response"
            );

            // Extract text content from the first choice
            let result = openai_response
                .choices
                .first()
                .map(|choice| choice.message.content.clone())
                .ok_or_else(|| {
                    ClaudeError::InvalidResponseFormat("No choices in response".to_string()).into()
                });

            if let Ok(ref text) = result {
                debug!(
                    response_len = text.len(),
                    "Successfully extracted text content from OpenAI-compatible API response"
                );
                debug!(
                    response_content = %text,
                    "OpenAI-compatible API response content"
                );
            }

            result
        })
    }

    fn get_metadata(&self) -> AiClientMetadata {
        let registry = get_model_registry();

        // For unknown models, use reasonable defaults
        let max_context_length = if registry.get_input_context(&self.model) > 0 {
            registry.get_input_context(&self.model)
        } else {
            32768 // Reasonable default for modern models
        };

        let max_response_length = if registry.get_max_output_tokens(&self.model) > 0 {
            registry.get_max_output_tokens(&self.model)
        } else {
            4096 // Reasonable default
        };

        let provider = if self.is_ollama() {
            "Ollama".to_string()
        } else {
            "OpenAI".to_string()
        };

        AiClientMetadata {
            provider,
            model: self.model.clone(),
            max_context_length,
            max_response_length,
            active_beta: self.active_beta.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_ollama() {
        let client = OpenAiAiClient::new_ollama("llama2".to_string(), None, None);
        assert_eq!(client.model, "llama2");
        assert_eq!(client.base_url, "http://localhost:11434");
        assert!(client.api_key.is_none());
        assert!(client.is_ollama());
    }

    #[test]
    fn test_new_ollama_custom_url() {
        let client = OpenAiAiClient::new_ollama(
            "codellama".to_string(),
            Some("http://192.168.1.100:11434".to_string()),
            None,
        );
        assert_eq!(client.base_url, "http://192.168.1.100:11434");
        assert!(client.is_ollama());
    }

    #[test]
    fn test_new_openai() {
        let client =
            OpenAiAiClient::new_openai("gpt-4".to_string(), "sk-test123".to_string(), None);
        assert_eq!(client.model, "gpt-4");
        assert_eq!(client.base_url, "https://api.openai.com");
        assert_eq!(client.api_key, Some("sk-test123".to_string()));
        assert!(!client.is_ollama());
    }

    #[test]
    fn test_get_api_url() {
        let client = OpenAiAiClient::new_ollama("llama2".to_string(), None, None);
        let url = client.get_api_url().unwrap();
        assert_eq!(url, "http://localhost:11434/v1/chat/completions");
    }

    #[test]
    fn test_get_api_url_trailing_slash() {
        let client = OpenAiAiClient::new(
            "test-model".to_string(),
            None,
            "http://localhost:11434/".to_string(),
            None,
            None,
            None,
        );
        let url = client.get_api_url().unwrap();
        assert_eq!(url, "http://localhost:11434/v1/chat/completions");
    }

    #[test]
    fn test_is_ollama_detection() {
        // Test localhost detection
        let ollama_client = OpenAiAiClient::new(
            "llama2".to_string(),
            None,
            "http://localhost:11434".to_string(),
            None,
            None,
            None,
        );
        assert!(ollama_client.is_ollama());

        // Test 127.0.0.1 detection
        let local_client = OpenAiAiClient::new(
            "llama2".to_string(),
            Some("fake-key".to_string()),
            "http://127.0.0.1:11434".to_string(),
            None,
            None,
            None,
        );
        assert!(local_client.is_ollama());

        // Test no API key detection
        let no_key_client = OpenAiAiClient::new(
            "llama2".to_string(),
            None,
            "http://remote-server.com".to_string(),
            None,
            None,
            None,
        );
        assert!(no_key_client.is_ollama());

        // Test OpenAI detection
        let openai_client = OpenAiAiClient::new(
            "gpt-4".to_string(),
            Some("sk-real-key".to_string()),
            "https://api.openai.com".to_string(),
            None,
            None,
            None,
        );
        assert!(!openai_client.is_ollama());
    }
}
