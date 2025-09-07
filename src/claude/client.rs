//! Claude API client implementation

use crate::claude::{error::ClaudeError, prompts};
use crate::data::{amendments::AmendmentFile, RepositoryView};
use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

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

/// Claude client for commit message improvement
pub struct ClaudeClient {
    client: Client,
    api_key: String,
    model: String,
}

impl ClaudeClient {
    /// Create new Claude client from environment variable
    pub fn new(model: String) -> Result<Self> {
        let api_key = std::env::var("CLAUDE_API_KEY")
            .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
            .map_err(|_| ClaudeError::ApiKeyNotFound)?;

        let client = Client::new();
        Ok(Self {
            client,
            api_key,
            model,
        })
    }

    /// Generate commit message amendments from repository view
    pub async fn generate_amendments(&self, repo_view: &RepositoryView) -> Result<AmendmentFile> {
        // Convert repository view to YAML
        let repo_yaml = crate::data::to_yaml(repo_view)
            .context("Failed to serialize repository view to YAML")?;

        // Generate user prompt
        let user_prompt = prompts::generate_user_prompt(&repo_yaml);

        // Build the request
        let request = ClaudeRequest {
            model: self.model.clone(),
            max_tokens: 4000,
            system: prompts::SYSTEM_PROMPT.to_string(),
            messages: vec![Message {
                role: "user".to_string(),
                content: user_prompt,
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
        let content = claude_response
            .content
            .first()
            .filter(|c| c.content_type == "text")
            .map(|c| c.text.as_str())
            .ok_or_else(|| {
                ClaudeError::InvalidResponseFormat("No text content in response".to_string())
            })?;

        // Parse YAML response to AmendmentFile
        self.parse_amendment_response(content)
    }

    /// Parse Claude's YAML response into AmendmentFile
    fn parse_amendment_response(&self, content: &str) -> Result<AmendmentFile> {
        // Extract YAML block from markdown if present
        let yaml_content = if content.contains("```yaml") {
            content
                .split("```yaml")
                .nth(1)
                .and_then(|s| s.split("```").next())
                .unwrap_or(content)
                .trim()
        } else if content.contains("```") {
            // Handle generic code blocks
            content
                .split("```")
                .nth(1)
                .and_then(|s| s.split("```").next())
                .unwrap_or(content)
                .trim()
        } else {
            content.trim()
        };

        // Parse YAML
        let amendment_file: AmendmentFile = serde_yaml::from_str(yaml_content).map_err(|e| {
            ClaudeError::AmendmentParsingFailed(format!("YAML parsing error: {}", e))
        })?;

        // Validate the parsed amendments
        amendment_file
            .validate()
            .map_err(|e| ClaudeError::AmendmentParsingFailed(format!("Validation error: {}", e)))?;

        Ok(amendment_file)
    }
}
