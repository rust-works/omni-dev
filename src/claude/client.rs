//! Claude client for commit message improvement.

use anyhow::{Context, Result};
use tracing::debug;

use crate::claude::token_budget::{self, TokenBudget};
use crate::claude::{ai::bedrock::BedrockAiClient, ai::claude::ClaudeAiClient};
use crate::claude::{ai::AiClient, error::ClaudeError, prompts};
use crate::data::{
    amendments::AmendmentFile, context::CommitContext, DiffDetail, RepositoryView,
    RepositoryViewForAI,
};

/// Multiplier for YAML re-serialization overhead when calculating excess chars.
///
/// Accounts for indentation changes, literal block markers, and other
/// formatting differences when YAML is re-serialized after diff truncation.
const YAML_OVERHEAD_FACTOR: f64 = 1.10;

/// Result of fitting a prompt within the model's token budget.
struct PromptWithBudget {
    /// The user prompt (serialized from a possibly-reduced view).
    user_prompt: String,
    /// The level of diff detail that was used.
    #[allow(dead_code)] // Retained for future diagnostics; set by all budget-fitting levels
    diff_detail: DiffDetail,
}

/// Claude client for commit message improvement.
pub struct ClaudeClient {
    /// AI client implementation.
    ai_client: Box<dyn AiClient>,
}

impl ClaudeClient {
    /// Creates a new Claude client with the provided AI client implementation.
    pub fn new(ai_client: Box<dyn AiClient>) -> Self {
        Self { ai_client }
    }

    /// Returns metadata about the AI client.
    pub fn get_ai_client_metadata(&self) -> crate::claude::ai::AiClientMetadata {
        self.ai_client.get_metadata()
    }

    /// Validates that the prompt fits within the model's token budget.
    ///
    /// Estimates token counts and logs utilization before each AI request.
    /// Returns an error if the prompt exceeds available input tokens.
    fn validate_prompt_budget(&self, system_prompt: &str, user_prompt: &str) -> Result<()> {
        let metadata = self.ai_client.get_metadata();
        let budget = TokenBudget::from_metadata(&metadata);
        let estimate = budget.validate_prompt(system_prompt, user_prompt)?;

        debug!(
            model = %metadata.model,
            estimated_tokens = estimate.estimated_tokens,
            available_tokens = estimate.available_tokens,
            utilization_pct = format!("{:.1}%", estimate.utilization_pct),
            "Token budget check passed"
        );

        Ok(())
    }

    /// Builds a user prompt that fits within the model's token budget,
    /// progressively reducing diff detail if necessary.
    ///
    /// Tries levels in order: Full → Truncated → StatOnly → FileListOnly.
    /// Logs a warning at each reduction level so the user knows the AI
    /// received less context.
    fn build_prompt_fitting_budget(
        &self,
        ai_view: RepositoryViewForAI,
        system_prompt: &str,
        build_user_prompt: impl Fn(&str) -> String,
    ) -> Result<PromptWithBudget> {
        let metadata = self.ai_client.get_metadata();
        let budget = TokenBudget::from_metadata(&metadata);

        // Level 1: Full diff
        let yaml = crate::data::to_yaml(&ai_view)
            .context("Failed to serialize repository view to YAML")?;
        let user_prompt = build_user_prompt(&yaml);

        if let Ok(estimate) = budget.validate_prompt(system_prompt, &user_prompt) {
            debug!(
                model = %metadata.model,
                estimated_tokens = estimate.estimated_tokens,
                available_tokens = estimate.available_tokens,
                utilization_pct = format!("{:.1}%", estimate.utilization_pct),
                diff_detail = %DiffDetail::Full,
                "Token budget check passed"
            );
            return Ok(PromptWithBudget {
                user_prompt,
                diff_detail: DiffDetail::Full,
            });
        }

        // Level 2: Truncated diff — calculate excess and trim
        let system_tokens = token_budget::estimate_tokens(system_prompt);
        let user_tokens = token_budget::estimate_tokens(&user_prompt);
        let excess_tokens =
            (system_tokens + user_tokens).saturating_sub(budget.available_input_tokens());
        let excess_chars = (token_budget::tokens_to_chars(excess_tokens) as f64
            * YAML_OVERHEAD_FACTOR)
            .ceil() as usize;

        let mut truncated_view = ai_view.clone();
        truncated_view.truncate_diffs(excess_chars);

        let yaml = crate::data::to_yaml(&truncated_view)
            .context("Failed to serialize truncated view to YAML")?;
        let user_prompt = build_user_prompt(&yaml);

        if let Ok(estimate) = budget.validate_prompt(system_prompt, &user_prompt) {
            debug!(
                model = %metadata.model,
                estimated_tokens = estimate.estimated_tokens,
                available_tokens = estimate.available_tokens,
                utilization_pct = format!("{:.1}%", estimate.utilization_pct),
                diff_detail = %DiffDetail::Truncated,
                "Token budget check passed after diff truncation"
            );
            tracing::warn!(
                "Diff content truncated to fit model context window ({})",
                metadata.model
            );
            return Ok(PromptWithBudget {
                user_prompt,
                diff_detail: DiffDetail::Truncated,
            });
        }

        // Level 3: Stat-only — replace diff content with stat summary
        let mut stat_view = ai_view.clone();
        stat_view.replace_diffs_with_stat();

        let yaml = crate::data::to_yaml(&stat_view)
            .context("Failed to serialize stat-only view to YAML")?;
        let user_prompt = build_user_prompt(&yaml);

        if let Ok(estimate) = budget.validate_prompt(system_prompt, &user_prompt) {
            debug!(
                model = %metadata.model,
                estimated_tokens = estimate.estimated_tokens,
                available_tokens = estimate.available_tokens,
                utilization_pct = format!("{:.1}%", estimate.utilization_pct),
                diff_detail = %DiffDetail::StatOnly,
                "Token budget check passed with stat-only diff"
            );
            tracing::warn!(
                "Full diff replaced with stat summary to fit model context window ({})",
                metadata.model
            );
            return Ok(PromptWithBudget {
                user_prompt,
                diff_detail: DiffDetail::StatOnly,
            });
        }

        // Level 4: File-list-only — remove all diff content
        let mut minimal_view = ai_view;
        minimal_view.remove_diffs();

        let yaml = crate::data::to_yaml(&minimal_view)
            .context("Failed to serialize minimal view to YAML")?;
        let user_prompt = build_user_prompt(&yaml);

        let estimate = budget.validate_prompt(system_prompt, &user_prompt)?;
        debug!(
            model = %metadata.model,
            estimated_tokens = estimate.estimated_tokens,
            available_tokens = estimate.available_tokens,
            utilization_pct = format!("{:.1}%", estimate.utilization_pct),
            diff_detail = %DiffDetail::FileListOnly,
            "Token budget check passed with file-list-only"
        );
        tracing::warn!(
            "All diff content removed to fit model context window — only file list available ({})",
            metadata.model
        );
        Ok(PromptWithBudget {
            user_prompt,
            diff_detail: DiffDetail::FileListOnly,
        })
    }

    /// Sends a raw prompt to the AI client and returns the text response.
    pub async fn send_message(&self, system_prompt: &str, user_prompt: &str) -> Result<String> {
        self.validate_prompt_budget(system_prompt, user_prompt)?;
        self.ai_client
            .send_request(system_prompt, user_prompt)
            .await
    }

    /// Creates a new Claude client with API key from environment variables.
    pub fn from_env(model: String) -> Result<Self> {
        // Try to get API key from environment variables
        let api_key = std::env::var("CLAUDE_API_KEY")
            .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
            .map_err(|_| ClaudeError::ApiKeyNotFound)?;

        let ai_client = ClaudeAiClient::new(model, api_key, None);
        Ok(Self::new(Box::new(ai_client)))
    }

    /// Generates commit message amendments from repository view.
    pub async fn generate_amendments(&self, repo_view: &RepositoryView) -> Result<AmendmentFile> {
        self.generate_amendments_with_options(repo_view, false)
            .await
    }

    /// Generates commit message amendments from repository view with options.
    ///
    /// If `fresh` is true, ignores existing commit messages and generates new ones
    /// based solely on the diff content.
    pub async fn generate_amendments_with_options(
        &self,
        repo_view: &RepositoryView,
        fresh: bool,
    ) -> Result<AmendmentFile> {
        // Convert to AI-enhanced view with diff content
        let ai_repo_view =
            RepositoryViewForAI::from_repository_view_with_options(repo_view.clone(), fresh)
                .context("Failed to enhance repository view with diff content")?;

        // Build prompt with progressive diff reduction if needed
        let fitted =
            self.build_prompt_fitting_budget(ai_repo_view, prompts::SYSTEM_PROMPT, |yaml| {
                prompts::generate_user_prompt(yaml)
            })?;

        // Send request using AI client
        let content = self
            .ai_client
            .send_request(prompts::SYSTEM_PROMPT, &fitted.user_prompt)
            .await?;

        // Parse YAML response to AmendmentFile
        self.parse_amendment_response(&content)
    }

    /// Generates contextual commit message amendments with enhanced intelligence.
    pub async fn generate_contextual_amendments(
        &self,
        repo_view: &RepositoryView,
        context: &CommitContext,
    ) -> Result<AmendmentFile> {
        self.generate_contextual_amendments_with_options(repo_view, context, false)
            .await
    }

    /// Generates contextual commit message amendments with options.
    ///
    /// If `fresh` is true, ignores existing commit messages and generates new ones
    /// based solely on the diff content.
    pub async fn generate_contextual_amendments_with_options(
        &self,
        repo_view: &RepositoryView,
        context: &CommitContext,
        fresh: bool,
    ) -> Result<AmendmentFile> {
        // Convert to AI-enhanced view with diff content
        let ai_repo_view =
            RepositoryViewForAI::from_repository_view_with_options(repo_view.clone(), fresh)
                .context("Failed to enhance repository view with diff content")?;

        // Generate contextual prompts using intelligence
        let prompt_style = self.ai_client.get_metadata().prompt_style();
        let system_prompt =
            prompts::generate_contextual_system_prompt_for_provider(context, prompt_style);

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

        // Build prompt with progressive diff reduction if needed
        let fitted = self.build_prompt_fitting_budget(ai_repo_view, &system_prompt, |yaml| {
            prompts::generate_contextual_user_prompt(yaml, context)
        })?;

        // Send request using AI client
        let content = self
            .ai_client
            .send_request(&system_prompt, &fitted.user_prompt)
            .await?;

        // Parse YAML response to AmendmentFile
        self.parse_amendment_response(&content)
    }

    /// Parses Claude's YAML response into an AmendmentFile.
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
                ClaudeError::AmendmentParsingFailed(format!("YAML parsing error: {e}"))
            }
        })?;

        // Validate the parsed amendments
        amendment_file
            .validate()
            .map_err(|e| ClaudeError::AmendmentParsingFailed(format!("Validation error: {e}")))?;

        Ok(amendment_file)
    }

    /// Generates AI-powered PR content (title + description) from repository view and template.
    pub async fn generate_pr_content(
        &self,
        repo_view: &RepositoryView,
        pr_template: &str,
    ) -> Result<crate::cli::git::PrContent> {
        // Convert to AI-enhanced view with diff content
        let ai_repo_view = RepositoryViewForAI::from_repository_view(repo_view.clone())
            .context("Failed to enhance repository view with diff content")?;

        // Build prompt with progressive diff reduction if needed
        let fitted = self.build_prompt_fitting_budget(
            ai_repo_view,
            prompts::PR_GENERATION_SYSTEM_PROMPT,
            |yaml| prompts::generate_pr_description_prompt(yaml, pr_template),
        )?;

        // Send request using AI client
        let content = self
            .ai_client
            .send_request(prompts::PR_GENERATION_SYSTEM_PROMPT, &fitted.user_prompt)
            .await?;

        // The AI response should be treated as YAML directly
        let yaml_content = content.trim();

        // Parse the YAML response using our hybrid YAML parser
        let pr_content: crate::cli::git::PrContent = crate::data::from_yaml(yaml_content).context(
            "Failed to parse AI response as YAML. AI may have returned malformed output.",
        )?;

        Ok(pr_content)
    }

    /// Generates AI-powered PR content with project context (title + description).
    pub async fn generate_pr_content_with_context(
        &self,
        repo_view: &RepositoryView,
        pr_template: &str,
        context: &crate::data::context::CommitContext,
    ) -> Result<crate::cli::git::PrContent> {
        // Convert to AI-enhanced view with diff content
        let ai_repo_view = RepositoryViewForAI::from_repository_view(repo_view.clone())
            .context("Failed to enhance repository view with diff content")?;

        // Generate contextual prompts for PR description with provider-specific handling
        let prompt_style = self.ai_client.get_metadata().prompt_style();
        let system_prompt =
            prompts::generate_pr_system_prompt_with_context_for_provider(context, prompt_style);

        // Build prompt with progressive diff reduction if needed
        let fitted = self.build_prompt_fitting_budget(ai_repo_view, &system_prompt, |yaml| {
            prompts::generate_pr_description_prompt_with_context(yaml, pr_template, context)
        })?;

        // Send request using AI client
        let content = self
            .ai_client
            .send_request(&system_prompt, &fitted.user_prompt)
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

    /// Checks commit messages against guidelines and returns a report.
    ///
    /// Validates commit messages against project guidelines or defaults,
    /// returning a structured report with issues and suggestions.
    pub async fn check_commits(
        &self,
        repo_view: &RepositoryView,
        guidelines: Option<&str>,
        include_suggestions: bool,
    ) -> Result<crate::data::check::CheckReport> {
        self.check_commits_with_scopes(repo_view, guidelines, &[], include_suggestions)
            .await
    }

    /// Checks commit messages against guidelines with valid scopes and returns a report.
    ///
    /// Validates commit messages against project guidelines or defaults,
    /// using the provided valid scopes for scope validation.
    pub async fn check_commits_with_scopes(
        &self,
        repo_view: &RepositoryView,
        guidelines: Option<&str>,
        valid_scopes: &[crate::data::context::ScopeDefinition],
        include_suggestions: bool,
    ) -> Result<crate::data::check::CheckReport> {
        self.check_commits_with_retry(repo_view, guidelines, valid_scopes, include_suggestions, 2)
            .await
    }

    /// Checks commit messages with retry logic for parse failures.
    async fn check_commits_with_retry(
        &self,
        repo_view: &RepositoryView,
        guidelines: Option<&str>,
        valid_scopes: &[crate::data::context::ScopeDefinition],
        include_suggestions: bool,
        max_retries: u32,
    ) -> Result<crate::data::check::CheckReport> {
        // Convert to AI-enhanced view with diff content
        let mut ai_repo_view = RepositoryViewForAI::from_repository_view(repo_view.clone())
            .context("Failed to enhance repository view with diff content")?;

        // Run deterministic pre-validation checks before sending to AI
        for commit in &mut ai_repo_view.commits {
            commit.run_pre_validation_checks(valid_scopes);
        }

        // Generate system prompt with scopes
        let system_prompt =
            prompts::generate_check_system_prompt_with_scopes(guidelines, valid_scopes);

        // Build prompt with progressive diff reduction if needed
        let fitted = self.build_prompt_fitting_budget(ai_repo_view, &system_prompt, |yaml| {
            prompts::generate_check_user_prompt(yaml, include_suggestions)
        })?;

        let mut last_error = None;

        for attempt in 0..=max_retries {
            // Send request using AI client
            match self
                .ai_client
                .send_request(&system_prompt, &fitted.user_prompt)
                .await
            {
                Ok(content) => match self.parse_check_response(&content, repo_view) {
                    Ok(report) => return Ok(report),
                    Err(e) => {
                        if attempt < max_retries {
                            eprintln!(
                                "warning: failed to parse AI response (attempt {}), retrying...",
                                attempt + 1
                            );
                            debug!(error = %e, attempt = attempt + 1, "Check response parse failed, retrying");
                        }
                        last_error = Some(e);
                    }
                },
                Err(e) => {
                    if attempt < max_retries {
                        eprintln!(
                            "warning: AI request failed (attempt {}), retrying...",
                            attempt + 1
                        );
                        debug!(error = %e, attempt = attempt + 1, "AI request failed, retrying");
                    }
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("Check failed after retries")))
    }

    /// Parses the check response from AI.
    fn parse_check_response(
        &self,
        content: &str,
        repo_view: &RepositoryView,
    ) -> Result<crate::data::check::CheckReport> {
        use crate::data::check::{
            AiCheckResponse, CheckReport, CommitCheckResult as CheckResultType,
        };

        // Extract YAML from potential markdown wrapper
        let yaml_content = self.extract_yaml_from_check_response(content);

        // Parse YAML response
        let ai_response: AiCheckResponse = crate::data::from_yaml(&yaml_content).map_err(|e| {
            debug!(
                error = %e,
                content_length = content.len(),
                yaml_length = yaml_content.len(),
                "Check YAML parsing failed"
            );
            debug!(content = %content, "Raw AI response");
            debug!(yaml = %yaml_content, "Extracted YAML content");
            ClaudeError::AmendmentParsingFailed(format!("Check response parsing error: {e}"))
        })?;

        // Create a map of commit hashes to original messages for lookup
        let commit_messages: std::collections::HashMap<&str, &str> = repo_view
            .commits
            .iter()
            .map(|c| (c.hash.as_str(), c.original_message.as_str()))
            .collect();

        // Convert AI response to CheckReport
        let results: Vec<CheckResultType> = ai_response
            .checks
            .into_iter()
            .map(|check| {
                let mut result: CheckResultType = check.into();
                // Fill in the original message from repo_view
                if let Some(msg) = commit_messages.get(result.hash.as_str()) {
                    result.message = msg.lines().next().unwrap_or("").to_string();
                } else {
                    // Try to find by prefix
                    for (hash, msg) in &commit_messages {
                        if hash.starts_with(&result.hash) || result.hash.starts_with(*hash) {
                            result.message = msg.lines().next().unwrap_or("").to_string();
                            break;
                        }
                    }
                }
                result
            })
            .collect();

        Ok(CheckReport::new(results))
    }

    /// Extracts YAML content from check response, handling markdown wrappers.
    fn extract_yaml_from_check_response(&self, content: &str) -> String {
        let content = content.trim();

        // If content already starts with "checks:", it's pure YAML - return as-is
        if content.starts_with("checks:") {
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
                if potential_yaml.starts_with("checks:") {
                    return potential_yaml.to_string();
                }
            }
        }

        // If no markdown blocks found or extraction failed, return trimmed content
        content.to_string()
    }

    /// Refines individually-generated amendments for cross-commit coherence.
    ///
    /// Sends commit summaries and proposed messages to the AI for a second pass
    /// that normalizes scopes, detects rename chains, and removes redundancy.
    pub async fn refine_amendments_coherence(
        &self,
        items: &[(crate::data::amendments::Amendment, String)],
    ) -> Result<AmendmentFile> {
        let system_prompt = prompts::AMENDMENT_COHERENCE_SYSTEM_PROMPT;
        let user_prompt = prompts::generate_amendment_coherence_user_prompt(items);

        self.validate_prompt_budget(system_prompt, &user_prompt)?;

        let content = self
            .ai_client
            .send_request(system_prompt, &user_prompt)
            .await?;

        self.parse_amendment_response(&content)
    }

    /// Refines individually-generated check results for cross-commit coherence.
    ///
    /// Sends commit summaries and check outcomes to the AI for a second pass
    /// that ensures consistent severity, detects cross-commit issues, and
    /// normalizes scope validation.
    pub async fn refine_checks_coherence(
        &self,
        items: &[(crate::data::check::CommitCheckResult, String)],
        repo_view: &RepositoryView,
    ) -> Result<crate::data::check::CheckReport> {
        let system_prompt = prompts::CHECK_COHERENCE_SYSTEM_PROMPT;
        let user_prompt = prompts::generate_check_coherence_user_prompt(items);

        self.validate_prompt_budget(system_prompt, &user_prompt)?;

        let content = self
            .ai_client
            .send_request(system_prompt, &user_prompt)
            .await?;

        self.parse_check_response(&content, repo_view)
    }

    /// Extracts YAML content from Claude response, handling markdown wrappers.
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

/// Validates a beta header against the model registry.
fn validate_beta_header(model: &str, beta_header: &Option<(String, String)>) -> Result<()> {
    if let Some((ref key, ref value)) = beta_header {
        let registry = crate::claude::model_config::get_model_registry();
        let supported = registry.get_beta_headers(model);
        if !supported
            .iter()
            .any(|bh| bh.key == *key && bh.value == *value)
        {
            let available: Vec<String> = supported
                .iter()
                .map(|bh| format!("{}:{}", bh.key, bh.value))
                .collect();
            if available.is_empty() {
                anyhow::bail!("Model '{}' does not support any beta headers", model);
            }
            anyhow::bail!(
                "Beta header '{}:{}' is not supported for model '{}'. Supported: {}",
                key,
                value,
                model,
                available.join(", ")
            );
        }
    }
    Ok(())
}

/// Creates a default Claude client using environment variables and settings.
pub fn create_default_claude_client(
    model: Option<String>,
    beta_header: Option<(String, String)>,
) -> Result<ClaudeClient> {
    use crate::claude::ai::openai::OpenAiAiClient;
    use crate::utils::settings::{get_env_var, get_env_vars};

    // Check if we should use OpenAI-compatible API (OpenAI or Ollama)
    let use_openai = get_env_var("USE_OPENAI")
        .map(|val| val == "true")
        .unwrap_or(false);

    let use_ollama = get_env_var("USE_OLLAMA")
        .map(|val| val == "true")
        .unwrap_or(false);

    // Check if we should use Bedrock
    let use_bedrock = get_env_var("CLAUDE_CODE_USE_BEDROCK")
        .map(|val| val == "true")
        .unwrap_or(false);

    debug!(
        use_openai = use_openai,
        use_ollama = use_ollama,
        use_bedrock = use_bedrock,
        "Client selection flags"
    );

    // Handle Ollama configuration
    if use_ollama {
        let ollama_model = model
            .or_else(|| get_env_var("OLLAMA_MODEL").ok())
            .unwrap_or_else(|| "llama2".to_string());
        validate_beta_header(&ollama_model, &beta_header)?;
        let base_url = get_env_var("OLLAMA_BASE_URL").ok();
        let ai_client = OpenAiAiClient::new_ollama(ollama_model, base_url, beta_header);
        return Ok(ClaudeClient::new(Box::new(ai_client)));
    }

    // Handle OpenAI configuration
    if use_openai {
        debug!("Creating OpenAI client");
        let openai_model = model
            .or_else(|| get_env_var("OPENAI_MODEL").ok())
            .unwrap_or_else(|| "gpt-5".to_string());
        debug!(openai_model = %openai_model, "Selected OpenAI model");
        validate_beta_header(&openai_model, &beta_header)?;

        let api_key = get_env_vars(&["OPENAI_API_KEY", "OPENAI_AUTH_TOKEN"]).map_err(|e| {
            debug!(error = ?e, "Failed to get OpenAI API key");
            ClaudeError::ApiKeyNotFound
        })?;
        debug!("OpenAI API key found");

        let ai_client = OpenAiAiClient::new_openai(openai_model, api_key, beta_header);
        debug!("OpenAI client created successfully");
        return Ok(ClaudeClient::new(Box::new(ai_client)));
    }

    // For Claude clients, try to get model from env vars or use default
    let claude_model = model
        .or_else(|| get_env_var("ANTHROPIC_MODEL").ok())
        .unwrap_or_else(|| "claude-opus-4-1-20250805".to_string());
    validate_beta_header(&claude_model, &beta_header)?;

    if use_bedrock {
        // Use Bedrock AI client
        let auth_token =
            get_env_var("ANTHROPIC_AUTH_TOKEN").map_err(|_| ClaudeError::ApiKeyNotFound)?;

        let base_url =
            get_env_var("ANTHROPIC_BEDROCK_BASE_URL").map_err(|_| ClaudeError::ApiKeyNotFound)?;

        let ai_client = BedrockAiClient::new(claude_model, auth_token, base_url, beta_header);
        return Ok(ClaudeClient::new(Box::new(ai_client)));
    }

    // Default: use standard Claude AI client
    debug!("Falling back to Claude client");
    let api_key = get_env_vars(&[
        "CLAUDE_API_KEY",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
    ])
    .map_err(|_| ClaudeError::ApiKeyNotFound)?;

    let ai_client = ClaudeAiClient::new(claude_model, api_key, beta_header);
    debug!("Claude client created successfully");
    Ok(ClaudeClient::new(Box::new(ai_client)))
}
