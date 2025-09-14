//! Claude client for commit message improvement

use crate::claude::{ai_client::AiClient, error::ClaudeError, prompts};
use crate::claude::claude_ai_client::ClaudeAiClient;
use crate::data::{amendments::AmendmentFile, context::CommitContext, RepositoryView, RepositoryViewForAI};
use anyhow::{Context, Result};
use tracing::debug;

/// Claude client for commit message improvement
pub struct ClaudeClient {
    /// AI client implementation
    ai_client: Box<dyn AiClient>,
}

impl ClaudeClient {
    /// Create new Claude client with provided AI client implementation
    pub fn new(ai_client: Box<dyn AiClient>) -> Self {
        Self { ai_client }
    }

    /// Create new Claude client with API key from environment variables
    pub fn from_env(model: String) -> Result<Self> {
        // Try to get API key from environment variables
        let api_key = std::env::var("CLAUDE_API_KEY")
            .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
            .map_err(|_| ClaudeError::ApiKeyNotFound)?;

        let ai_client = ClaudeAiClient::new(model, api_key);
        Ok(Self::new(Box::new(ai_client)))
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

        // Send request using AI client
        let content = self.ai_client
            .send_request(prompts::SYSTEM_PROMPT, &user_prompt)
            .await?;

        // Parse YAML response to AmendmentFile
        self.parse_amendment_response(&content)
    }

    /// Generate contextual commit message amendments with enhanced intelligence
    pub async fn generate_contextual_amendments(
        &self,
        repo_view: &RepositoryView,
        context: &CommitContext,
    ) -> Result<AmendmentFile> {
        // Convert to AI-enhanced view with diff content
        let ai_repo_view = RepositoryViewForAI::from_repository_view(repo_view.clone())
            .context("Failed to enhance repository view with diff content")?;

        // Convert repository view to YAML
        let repo_yaml = crate::data::to_yaml(&ai_repo_view)
            .context("Failed to serialize repository view to YAML")?;

        // Generate contextual prompts using intelligence
        let system_prompt = prompts::generate_contextual_system_prompt(context);
        let user_prompt = prompts::generate_contextual_user_prompt(&repo_yaml, context);

        // Debug logging to troubleshoot custom commit type issue
        match &context.project.commit_guidelines {
            Some(guidelines) => {
                debug!(length = guidelines.len(), "Project commit guidelines found");
                debug!(guidelines = %guidelines, "Commit guidelines content");
            }
            None => {
                debug!("No project commit guidelines found");
            }
        }

        // Send request using AI client
        let content = self.ai_client
            .send_request(&system_prompt, &user_prompt)
            .await?;

        // Parse YAML response to AmendmentFile
        self.parse_amendment_response(&content)
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
            debug!(
                error = %e,
                content_length = content.len(),
                yaml_length = yaml_content.len(),
                "YAML parsing failed"
            );
            debug!(content = %content, "Raw Claude response");
            debug!(yaml = %yaml_content, "Extracted YAML content");

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

/// Create a default Claude client using environment variables
pub fn create_default_claude_client(model: Option<String>) -> Result<ClaudeClient> {
    let model = model.unwrap_or_else(|| "claude-3-haiku-20240307".to_string());

    // Try to get API key from environment variables
    let api_key = std::env::var("CLAUDE_API_KEY")
        .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
        .map_err(|_| ClaudeError::ApiKeyNotFound)?;

    let ai_client = ClaudeAiClient::new(model, api_key);
    Ok(ClaudeClient::new(Box::new(ai_client)))
}
