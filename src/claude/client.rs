//! Claude API client implementation

use crate::claude::{error::ClaudeError, prompts};
use crate::data::{amendments::AmendmentFile, RepositoryView, RepositoryViewForAI};
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
        // Convert to AI-enhanced view with diff content
        let ai_repo_view = RepositoryViewForAI::from_repository_view(repo_view.clone())
            .context("Failed to enhance repository view with diff content")?;

        // Convert repository view to YAML
        let repo_yaml = crate::data::to_yaml(&ai_repo_view)
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

        // Request debugging can be enabled if needed for troubleshooting

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

        // Response debugging can be enabled if needed for troubleshooting

        // Parse YAML response to AmendmentFile
        self.parse_amendment_response(content)
    }

    /// Generate contextual commit message amendments with enhanced intelligence
    pub async fn generate_contextual_amendments(
        &self,
        repo_view: &RepositoryView,
        context: &crate::data::context::CommitContext,
    ) -> Result<AmendmentFile> {
        // Convert to AI-enhanced view with diff content
        let ai_repo_view = RepositoryViewForAI::from_repository_view(repo_view.clone())
            .context("Failed to enhance repository view with diff content")?;

        // Convert repository view to YAML
        let repo_yaml = crate::data::to_yaml(&ai_repo_view)
            .context("Failed to serialize repository view to YAML")?;

        // Generate contextual prompts using Phase 3 intelligence
        let system_prompt = prompts::generate_contextual_system_prompt(context);
        let user_prompt = prompts::generate_contextual_user_prompt(&repo_yaml, context);

        // Build the request with contextual prompts
        let request = ClaudeRequest {
            model: self.model.clone(),
            max_tokens: if context.is_significant_change() {
                6000
            } else {
                4000
            },
            system: system_prompt,
            messages: vec![Message {
                role: "user".to_string(),
                content: user_prompt,
            }],
        };

        // Contextual request debugging can be enabled if needed for troubleshooting

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

        // Contextual response debugging can be enabled if needed for troubleshooting

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

        // Try to parse YAML
        let amendment_file: AmendmentFile = serde_yaml::from_str(yaml_content).map_err(|e| {
            eprintln!("DEBUG: YAML parsing failed. Raw content:");
            eprintln!("=== RAW CLAUDE RESPONSE ===");
            eprintln!("{}", content);
            eprintln!("=== EXTRACTED YAML ===");
            eprintln!("{}", yaml_content);
            eprintln!("=== YAML ERROR ===");
            eprintln!("{}", e);
            eprintln!("=== END DEBUG ===");

            // Try to provide more helpful error messages for common issues
            if yaml_content.lines().any(|line| line.contains('\t')) {
                ClaudeError::AmendmentParsingFailed("YAML parsing error: Found tab characters. YAML requires spaces for indentation.".to_string())
            } else if yaml_content.lines().any(|line| line.trim().starts_with('-') && !line.trim().starts_with("- ")) {
                ClaudeError::AmendmentParsingFailed("YAML parsing error: List items must have a space after the dash (- item).".to_string())
            } else {
                ClaudeError::AmendmentParsingFailed(format!("YAML parsing error: {}", e))
            }
        })?;

        // Validate the parsed amendments
        amendment_file
            .validate()
            .map_err(|e| ClaudeError::AmendmentParsingFailed(format!("Validation error: {}", e)))?;

        Ok(amendment_file)
    }
}
