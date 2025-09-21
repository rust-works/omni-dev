//! Claude client for commit message improvement

use crate::claude::{ai::bedrock::BedrockAiClient, ai::claude::ClaudeAiClient};
use crate::claude::{ai::AiClient, error::ClaudeError, prompts};
use crate::data::{
    amendments::AmendmentFile, context::CommitContext, RepositoryView, RepositoryViewForAI,
};
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
        let content = self
            .ai_client
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
        let content = self
            .ai_client
            .send_request(&system_prompt, &user_prompt)
            .await?;

        // Parse YAML response to AmendmentFile
        self.parse_amendment_response(&content)
    }

    /// Parse Claude's YAML response into AmendmentFile
    fn parse_amendment_response(&self, content: &str) -> Result<AmendmentFile> {
        // Extract YAML from potential markdown wrapper
        let yaml_content = self.extract_yaml_from_response(content);

        // Try to parse YAML using our hybrid YAML parser
        let amendment_file: AmendmentFile = crate::data::from_yaml(&yaml_content).map_err(|e| {
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

    /// Generate AI-powered PR content (title + description) from repository view and template
    pub async fn generate_pr_content(
        &self,
        repo_view: &RepositoryView,
        pr_template: &str,
    ) -> Result<crate::cli::git::PrContent> {
        // Convert to AI-enhanced view with diff content
        let ai_repo_view = RepositoryViewForAI::from_repository_view(repo_view.clone())
            .context("Failed to enhance repository view with diff content")?;

        // Convert repository view to YAML
        let repo_yaml = crate::data::to_yaml(&ai_repo_view)
            .context("Failed to serialize repository view to YAML")?;

        // Generate prompts for PR description
        let user_prompt = prompts::generate_pr_description_prompt(&repo_yaml, pr_template);

        // Send request using AI client
        let content = self
            .ai_client
            .send_request(prompts::PR_GENERATION_SYSTEM_PROMPT, &user_prompt)
            .await?;

        // The AI response should be treated as YAML directly
        let yaml_content = content.trim();

        // Parse the YAML response using our hybrid YAML parser
        let pr_content: crate::cli::git::PrContent = crate::data::from_yaml(yaml_content).context(
            "Failed to parse AI response as YAML. AI may have returned malformed output.",
        )?;

        Ok(pr_content)
    }

    /// Generate AI-powered PR content with project context (title + description)
    pub async fn generate_pr_content_with_context(
        &self,
        repo_view: &RepositoryView,
        pr_template: &str,
        context: &crate::data::context::CommitContext,
    ) -> Result<crate::cli::git::PrContent> {
        // Convert to AI-enhanced view with diff content
        let ai_repo_view = RepositoryViewForAI::from_repository_view(repo_view.clone())
            .context("Failed to enhance repository view with diff content")?;

        // Convert repository view to YAML
        let repo_yaml = crate::data::to_yaml(&ai_repo_view)
            .context("Failed to serialize repository view to YAML")?;

        // Generate contextual prompts for PR description
        let system_prompt = prompts::generate_pr_system_prompt_with_context(context);
        let user_prompt =
            prompts::generate_pr_description_prompt_with_context(&repo_yaml, pr_template, context);

        // Send request using AI client
        let content = self
            .ai_client
            .send_request(&system_prompt, &user_prompt)
            .await?;

        // The AI response should be treated as YAML directly
        let yaml_content = content.trim();

        debug!(
            content_length = content.len(),
            yaml_content_length = yaml_content.len(),
            yaml_content = %yaml_content,
            "Extracted YAML content from AI response"
        );

        // Parse the YAML response using our hybrid YAML parser
        let pr_content: crate::cli::git::PrContent = crate::data::from_yaml(yaml_content).context(
            "Failed to parse AI response as YAML. AI may have returned malformed output.",
        )?;

        debug!(
            parsed_title = %pr_content.title,
            parsed_description_length = pr_content.description.len(),
            parsed_description_preview = %pr_content.description.lines().take(3).collect::<Vec<_>>().join("\\n"),
            "Successfully parsed PR content from YAML"
        );

        Ok(pr_content)
    }

    /// Extract YAML content from Claude response, handling markdown wrappers
    fn extract_yaml_from_response(&self, content: &str) -> String {
        let content = content.trim();

        // If content already starts with "amendments:", it's pure YAML - return as-is
        if content.starts_with("amendments:") {
            return content.to_string();
        }

        // Try to extract from ```yaml blocks first
        if let Some(yaml_start) = content.find("```yaml") {
            if let Some(yaml_content) = content[yaml_start + 7..].split("```").next() {
                return yaml_content.trim().to_string();
            }
        }

        // Try to extract from generic ``` blocks
        if let Some(code_start) = content.find("```") {
            if let Some(code_content) = content[code_start + 3..].split("```").next() {
                let potential_yaml = code_content.trim();
                // Check if it looks like YAML (starts with expected structure)
                if potential_yaml.starts_with("amendments:") {
                    return potential_yaml.to_string();
                }
            }
        }

        // If no markdown blocks found or extraction failed, return trimmed content
        content.to_string()
    }
}

/// Create a default Claude client using environment variables and settings
pub fn create_default_claude_client(model: Option<String>) -> Result<ClaudeClient> {
    use crate::utils::settings::{get_env_var, get_env_vars};

    // Check if we should use Bedrock
    let use_bedrock = get_env_var("CLAUDE_CODE_USE_BEDROCK")
        .map(|val| val == "true")
        .unwrap_or(false);

    // Try to get model from env var ANTHROPIC_MODEL or use default
    let model = model
        .or_else(|| get_env_var("ANTHROPIC_MODEL").ok())
        .unwrap_or_else(|| "claude-3-haiku-20240307".to_string());

    if use_bedrock {
        // Check if we should skip Bedrock auth
        let skip_bedrock_auth = get_env_var("CLAUDE_CODE_SKIP_BEDROCK_AUTH")
            .map(|val| val == "true")
            .unwrap_or(false);

        if skip_bedrock_auth {
            // Use Bedrock AI client
            let auth_token =
                get_env_var("ANTHROPIC_AUTH_TOKEN").map_err(|_| ClaudeError::ApiKeyNotFound)?;

            let base_url = get_env_var("ANTHROPIC_BEDROCK_BASE_URL")
                .map_err(|_| ClaudeError::ApiKeyNotFound)?;

            let ai_client = BedrockAiClient::new(model, auth_token, base_url);
            return Ok(ClaudeClient::new(Box::new(ai_client)));
        }
    }

    // Default: use standard Claude AI client
    let api_key = get_env_vars(&[
        "CLAUDE_API_KEY",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
    ])
    .map_err(|_| ClaudeError::ApiKeyNotFound)?;

    let ai_client = ClaudeAiClient::new(model, api_key);
    Ok(ClaudeClient::new(Box::new(ai_client)))
}
