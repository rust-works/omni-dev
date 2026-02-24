//! Claude client for commit message improvement.

use anyhow::{Context, Result};
use tracing::debug;

use crate::claude::token_budget::{self, TokenBudget};
use crate::claude::{ai::bedrock::BedrockAiClient, ai::claude::ClaudeAiClient};
use crate::claude::{ai::AiClient, error::ClaudeError, prompts};
use crate::data::{
    amendments::{Amendment, AmendmentFile},
    context::CommitContext,
    DiffDetail, RepositoryView, RepositoryViewForAI,
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

/// Returned when the full diff does not fit the token budget.
///
/// Carries the data needed for split dispatch so the caller can size
/// diff chunks appropriately.
struct BudgetExceeded {
    /// Available input tokens for this model (context window minus output reserve).
    available_input_tokens: usize,
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

    /// Tests whether the full diff fits the token budget.
    ///
    /// Returns `Ok(Ok(PromptWithBudget))` when the full diff fits,
    /// `Ok(Err(BudgetExceeded))` when it does not, or a top-level error
    /// on serialization failure.
    fn try_full_diff_budget(
        &self,
        ai_view: &RepositoryViewForAI,
        system_prompt: &str,
        build_user_prompt: &impl Fn(&str) -> String,
    ) -> Result<std::result::Result<PromptWithBudget, BudgetExceeded>> {
        let metadata = self.ai_client.get_metadata();
        let budget = TokenBudget::from_metadata(&metadata);

        let yaml =
            crate::data::to_yaml(ai_view).context("Failed to serialize repository view to YAML")?;
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
            return Ok(Ok(PromptWithBudget {
                user_prompt,
                diff_detail: DiffDetail::Full,
            }));
        }

        Ok(Err(BudgetExceeded {
            available_input_tokens: budget.available_input_tokens(),
        }))
    }

    /// Generates an amendment for a single commit whose diff exceeds the
    /// token budget by splitting it into file-level chunks.
    ///
    /// Uses [`pack_file_diffs`](crate::claude::diff_pack::pack_file_diffs) to
    /// create chunks, sends one AI request per chunk, then runs a merge pass
    /// to synthesize a single [`Amendment`].
    async fn generate_amendment_split(
        &self,
        commit: &crate::git::CommitInfo,
        repo_view_for_ai: &RepositoryViewForAI,
        system_prompt: &str,
        build_user_prompt: &(dyn Fn(&str) -> String + Sync),
        available_input_tokens: usize,
        fresh: bool,
    ) -> Result<Amendment> {
        use crate::claude::diff_pack::pack_file_diffs;
        use crate::git::commit::CommitInfoForAI;

        let plan = pack_file_diffs(
            &commit.hash,
            &commit.analysis.file_diffs,
            available_input_tokens,
        )
        .with_context(|| {
            format!(
                "Failed to plan diff chunks for commit {}",
                &commit.hash[..8]
            )
        })?;

        let total_chunks = plan.chunks.len();
        debug!(
            commit = %&commit.hash[..8],
            chunks = total_chunks,
            "Split dispatch: processing commit in chunks"
        );

        let mut chunk_amendments = Vec::with_capacity(total_chunks);
        for (i, chunk) in plan.chunks.iter().enumerate() {
            let mut partial =
                CommitInfoForAI::from_commit_info_partial(commit.clone(), &chunk.file_paths)
                    .with_context(|| {
                        format!(
                            "Failed to build partial view for chunk {}/{} of commit {}",
                            i + 1,
                            total_chunks,
                            &commit.hash[..8]
                        )
                    })?;

            if fresh {
                partial.base.original_message =
                    "(Original message hidden - generate fresh message from diff)".to_string();
            }

            let partial_view = repo_view_for_ai.single_commit_view_for_ai(&partial);

            let fitted =
                self.build_prompt_fitting_budget(partial_view, system_prompt, build_user_prompt)?;

            let content = self
                .ai_client
                .send_request(system_prompt, &fitted.user_prompt)
                .await
                .with_context(|| {
                    format!(
                        "Chunk {}/{} failed for commit {}",
                        i + 1,
                        total_chunks,
                        &commit.hash[..8]
                    )
                })?;

            let amendment_file = self.parse_amendment_response(&content).with_context(|| {
                format!(
                    "Failed to parse chunk {}/{} response for commit {}",
                    i + 1,
                    total_chunks,
                    &commit.hash[..8]
                )
            })?;

            if let Some(amendment) = amendment_file.amendments.into_iter().next() {
                chunk_amendments.push(amendment);
            }
        }

        self.merge_amendment_chunks(
            &commit.hash,
            &commit.original_message,
            &commit.analysis.diff_summary,
            &chunk_amendments,
        )
        .await
    }

    /// Runs an AI reduce pass to synthesize a single amendment from partial
    /// chunk amendments for the same commit.
    ///
    /// Follows the same pattern as
    /// [`refine_amendments_coherence`](Self::refine_amendments_coherence).
    async fn merge_amendment_chunks(
        &self,
        commit_hash: &str,
        original_message: &str,
        diff_summary: &str,
        chunk_amendments: &[Amendment],
    ) -> Result<Amendment> {
        let system_prompt = prompts::AMENDMENT_CHUNK_MERGE_SYSTEM_PROMPT;
        let user_prompt = prompts::generate_chunk_merge_user_prompt(
            commit_hash,
            original_message,
            diff_summary,
            chunk_amendments,
        );

        self.validate_prompt_budget(system_prompt, &user_prompt)?;

        let content = self
            .ai_client
            .send_request(system_prompt, &user_prompt)
            .await
            .context("Merge pass failed for chunk amendments")?;

        let amendment_file = self
            .parse_amendment_response(&content)
            .context("Failed to parse merge pass response")?;

        amendment_file
            .amendments
            .into_iter()
            .next()
            .context("Merge pass returned no amendments")
    }

    /// Checks a single commit whose diff exceeds the token budget by
    /// splitting it into file-level chunks.
    ///
    /// Uses [`pack_file_diffs`](crate::claude::diff_pack::pack_file_diffs) to
    /// create chunks, sends one check request per chunk, then merges results
    /// deterministically (issue union + dedup). Runs an AI reduce pass only
    /// when at least one chunk returns a suggestion.
    async fn check_commit_split(
        &self,
        commit: &crate::git::CommitInfo,
        repo_view: &RepositoryView,
        system_prompt: &str,
        valid_scopes: &[crate::data::context::ScopeDefinition],
        include_suggestions: bool,
        available_input_tokens: usize,
    ) -> Result<crate::data::check::CheckReport> {
        use crate::claude::diff_pack::pack_file_diffs;
        use crate::data::check::{CommitCheckResult, CommitIssue, IssueSeverity};
        use crate::git::commit::CommitInfoForAI;

        let plan = pack_file_diffs(
            &commit.hash,
            &commit.analysis.file_diffs,
            available_input_tokens,
        )
        .with_context(|| {
            format!(
                "Failed to plan diff chunks for commit {}",
                &commit.hash[..8]
            )
        })?;

        let total_chunks = plan.chunks.len();
        debug!(
            commit = %&commit.hash[..8],
            chunks = total_chunks,
            "Check split dispatch: processing commit in chunks"
        );

        let build_user_prompt =
            |yaml: &str| prompts::generate_check_user_prompt(yaml, include_suggestions);

        let mut chunk_results = Vec::with_capacity(total_chunks);
        for (i, chunk) in plan.chunks.iter().enumerate() {
            let mut partial =
                CommitInfoForAI::from_commit_info_partial(commit.clone(), &chunk.file_paths)
                    .with_context(|| {
                        format!(
                            "Failed to build partial view for chunk {}/{} of commit {}",
                            i + 1,
                            total_chunks,
                            &commit.hash[..8]
                        )
                    })?;

            partial.run_pre_validation_checks(valid_scopes);

            let partial_view = RepositoryViewForAI::from_repository_view(repo_view.clone())
                .context("Failed to enhance repository view with diff content")?
                .single_commit_view_for_ai(&partial);

            let fitted =
                self.build_prompt_fitting_budget(partial_view, system_prompt, build_user_prompt)?;

            let content = self
                .ai_client
                .send_request(system_prompt, &fitted.user_prompt)
                .await
                .with_context(|| {
                    format!(
                        "Check chunk {}/{} failed for commit {}",
                        i + 1,
                        total_chunks,
                        &commit.hash[..8]
                    )
                })?;

            let report = self
                .parse_check_response(&content, repo_view)
                .with_context(|| {
                    format!(
                        "Failed to parse check chunk {}/{} response for commit {}",
                        i + 1,
                        total_chunks,
                        &commit.hash[..8]
                    )
                })?;

            if let Some(result) = report.commits.into_iter().next() {
                chunk_results.push(result);
            }
        }

        // Deterministic merge: union issues, dedup by (rule, severity, section)
        let mut seen = std::collections::HashSet::new();
        let mut merged_issues: Vec<CommitIssue> = Vec::new();
        for result in &chunk_results {
            for issue in &result.issues {
                let key: (String, IssueSeverity, String) =
                    (issue.rule.clone(), issue.severity, issue.section.clone());
                if seen.insert(key) {
                    merged_issues.push(issue.clone());
                }
            }
        }

        let passes = chunk_results.iter().all(|r| r.passes);

        // AI reduce pass for suggestion/summary only when needed
        let has_suggestions = chunk_results.iter().any(|r| r.suggestion.is_some());

        let (merged_suggestion, merged_summary) = if has_suggestions {
            self.merge_check_chunks(
                &commit.hash,
                &commit.original_message,
                &commit.analysis.diff_summary,
                passes,
                &chunk_results,
                repo_view,
            )
            .await?
        } else {
            // Take first non-None summary
            let summary = chunk_results.iter().find_map(|r| r.summary.clone());
            (None, summary)
        };

        let original_message = commit
            .original_message
            .lines()
            .next()
            .unwrap_or("")
            .to_string();

        let merged_result = CommitCheckResult {
            hash: commit.hash.clone(),
            message: original_message,
            issues: merged_issues,
            suggestion: merged_suggestion,
            passes,
            summary: merged_summary,
        };

        Ok(crate::data::check::CheckReport::new(vec![merged_result]))
    }

    /// Runs an AI reduce pass to synthesize a single suggestion and summary
    /// from partial chunk check results for the same commit.
    ///
    /// Only called when at least one chunk returned a suggestion.
    async fn merge_check_chunks(
        &self,
        commit_hash: &str,
        original_message: &str,
        diff_summary: &str,
        passes: bool,
        chunk_results: &[crate::data::check::CommitCheckResult],
        repo_view: &RepositoryView,
    ) -> Result<(Option<crate::data::check::CommitSuggestion>, Option<String>)> {
        let suggestions: Vec<&crate::data::check::CommitSuggestion> = chunk_results
            .iter()
            .filter_map(|r| r.suggestion.as_ref())
            .collect();

        let summaries: Vec<Option<&str>> =
            chunk_results.iter().map(|r| r.summary.as_deref()).collect();

        let system_prompt = prompts::CHECK_CHUNK_MERGE_SYSTEM_PROMPT;
        let user_prompt = prompts::generate_check_chunk_merge_user_prompt(
            commit_hash,
            original_message,
            diff_summary,
            passes,
            &suggestions,
            &summaries,
        );

        self.validate_prompt_budget(system_prompt, &user_prompt)?;

        let content = self
            .ai_client
            .send_request(system_prompt, &user_prompt)
            .await
            .context("Merge pass failed for check chunk suggestions")?;

        let report = self
            .parse_check_response(&content, repo_view)
            .context("Failed to parse check merge pass response")?;

        let result = report.commits.into_iter().next();
        Ok(match result {
            Some(r) => (r.suggestion, r.summary),
            None => (None, None),
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

        let ai_client = ClaudeAiClient::new(model, api_key, None)?;
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
    ///
    /// For single-commit views whose full diff exceeds the token budget,
    /// splits the diff into file-level chunks and dispatches multiple AI
    /// requests, then merges results. Multi-commit views fall back to
    /// progressive diff reduction (the caller retries individually on
    /// failure).
    pub async fn generate_amendments_with_options(
        &self,
        repo_view: &RepositoryView,
        fresh: bool,
    ) -> Result<AmendmentFile> {
        // Convert to AI-enhanced view with diff content
        let ai_repo_view =
            RepositoryViewForAI::from_repository_view_with_options(repo_view.clone(), fresh)
                .context("Failed to enhance repository view with diff content")?;

        let system_prompt = prompts::SYSTEM_PROMPT;
        let build_user_prompt = |yaml: &str| prompts::generate_user_prompt(yaml);

        // Single-commit views: try split dispatch when full diff exceeds budget
        if repo_view.commits.len() == 1 && !repo_view.commits[0].analysis.file_diffs.is_empty() {
            match self.try_full_diff_budget(&ai_repo_view, system_prompt, &build_user_prompt)? {
                Ok(fitted) => {
                    let content = self
                        .ai_client
                        .send_request(system_prompt, &fitted.user_prompt)
                        .await?;
                    return self.parse_amendment_response(&content);
                }
                Err(exceeded) => {
                    let amendment = self
                        .generate_amendment_split(
                            &repo_view.commits[0],
                            &ai_repo_view,
                            system_prompt,
                            &build_user_prompt,
                            exceeded.available_input_tokens,
                            fresh,
                        )
                        .await?;
                    return Ok(AmendmentFile {
                        amendments: vec![amendment],
                    });
                }
            }
        }

        // Multi-commit or no file_diffs: use progressive diff reduction
        let fitted =
            self.build_prompt_fitting_budget(ai_repo_view, system_prompt, build_user_prompt)?;

        let content = self
            .ai_client
            .send_request(system_prompt, &fitted.user_prompt)
            .await?;

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
    ///
    /// For single-commit views whose full diff exceeds the token budget,
    /// splits the diff into file-level chunks and dispatches multiple AI
    /// requests, then merges results. Multi-commit views fall back to
    /// progressive diff reduction.
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

        let build_user_prompt =
            |yaml: &str| prompts::generate_contextual_user_prompt(yaml, context);

        // Single-commit views: try split dispatch when full diff exceeds budget
        if repo_view.commits.len() == 1 && !repo_view.commits[0].analysis.file_diffs.is_empty() {
            match self.try_full_diff_budget(&ai_repo_view, &system_prompt, &build_user_prompt)? {
                Ok(fitted) => {
                    let content = self
                        .ai_client
                        .send_request(&system_prompt, &fitted.user_prompt)
                        .await?;
                    return self.parse_amendment_response(&content);
                }
                Err(exceeded) => {
                    let amendment = self
                        .generate_amendment_split(
                            &repo_view.commits[0],
                            &ai_repo_view,
                            &system_prompt,
                            &build_user_prompt,
                            exceeded.available_input_tokens,
                            fresh,
                        )
                        .await?;
                    return Ok(AmendmentFile {
                        amendments: vec![amendment],
                    });
                }
            }
        }

        // Multi-commit or no file_diffs: use progressive diff reduction
        let fitted =
            self.build_prompt_fitting_budget(ai_repo_view, &system_prompt, build_user_prompt)?;

        let content = self
            .ai_client
            .send_request(&system_prompt, &fitted.user_prompt)
            .await?;

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
    ///
    /// For single-commit views whose full diff exceeds the token budget,
    /// splits the diff into file-level chunks and dispatches multiple AI
    /// requests, then merges results. Multi-commit views fall back to
    /// progressive diff reduction (the caller retries individually on
    /// failure).
    async fn check_commits_with_retry(
        &self,
        repo_view: &RepositoryView,
        guidelines: Option<&str>,
        valid_scopes: &[crate::data::context::ScopeDefinition],
        include_suggestions: bool,
        max_retries: u32,
    ) -> Result<crate::data::check::CheckReport> {
        // Generate system prompt with scopes
        let system_prompt =
            prompts::generate_check_system_prompt_with_scopes(guidelines, valid_scopes);

        let build_user_prompt =
            |yaml: &str| prompts::generate_check_user_prompt(yaml, include_suggestions);

        // Single-commit views: try split dispatch when full diff exceeds budget
        if repo_view.commits.len() == 1 && !repo_view.commits[0].analysis.file_diffs.is_empty() {
            let mut ai_repo_view = RepositoryViewForAI::from_repository_view(repo_view.clone())
                .context("Failed to enhance repository view with diff content")?;
            for commit in &mut ai_repo_view.commits {
                commit.run_pre_validation_checks(valid_scopes);
            }

            match self.try_full_diff_budget(&ai_repo_view, &system_prompt, &build_user_prompt)? {
                Ok(fitted) => {
                    let content = self
                        .ai_client
                        .send_request(&system_prompt, &fitted.user_prompt)
                        .await?;
                    return self.parse_check_response(&content, repo_view);
                }
                Err(exceeded) => {
                    return self
                        .check_commit_split(
                            &repo_view.commits[0],
                            repo_view,
                            &system_prompt,
                            valid_scopes,
                            include_suggestions,
                            exceeded.available_input_tokens,
                        )
                        .await;
                }
            }
        }

        // Multi-commit or no file_diffs: use progressive diff reduction
        let mut ai_repo_view = RepositoryViewForAI::from_repository_view(repo_view.clone())
            .context("Failed to enhance repository view with diff content")?;

        // Run deterministic pre-validation checks before sending to AI
        for commit in &mut ai_repo_view.commits {
            commit.run_pre_validation_checks(valid_scopes);
        }

        // Build prompt with progressive diff reduction if needed
        let fitted =
            self.build_prompt_fitting_budget(ai_repo_view, &system_prompt, build_user_prompt)?;

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
                anyhow::bail!("Model '{model}' does not support any beta headers");
            }
            anyhow::bail!(
                "Beta header '{key}:{value}' is not supported for model '{model}'. Supported: {}",
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

    let registry = crate::claude::model_config::get_model_registry();

    // Handle Ollama configuration
    if use_ollama {
        let ollama_model = model
            .or_else(|| get_env_var("OLLAMA_MODEL").ok())
            .unwrap_or_else(|| "llama2".to_string());
        validate_beta_header(&ollama_model, &beta_header)?;
        let base_url = get_env_var("OLLAMA_BASE_URL").ok();
        let ai_client = OpenAiAiClient::new_ollama(ollama_model, base_url, beta_header)?;
        return Ok(ClaudeClient::new(Box::new(ai_client)));
    }

    // Handle OpenAI configuration
    if use_openai {
        debug!("Creating OpenAI client");
        let openai_model = model
            .or_else(|| get_env_var("OPENAI_MODEL").ok())
            .unwrap_or_else(|| {
                registry
                    .get_default_model("openai")
                    .unwrap_or("gpt-5")
                    .to_string()
            });
        debug!(openai_model = %openai_model, "Selected OpenAI model");
        validate_beta_header(&openai_model, &beta_header)?;

        let api_key = get_env_vars(&["OPENAI_API_KEY", "OPENAI_AUTH_TOKEN"]).map_err(|e| {
            debug!(error = ?e, "Failed to get OpenAI API key");
            ClaudeError::ApiKeyNotFound
        })?;
        debug!("OpenAI API key found");

        let ai_client = OpenAiAiClient::new_openai(openai_model, api_key, beta_header)?;
        debug!("OpenAI client created successfully");
        return Ok(ClaudeClient::new(Box::new(ai_client)));
    }

    // For Claude clients, try to get model from env vars or use default
    let claude_model = model
        .or_else(|| get_env_var("ANTHROPIC_MODEL").ok())
        .unwrap_or_else(|| {
            registry
                .get_default_model("claude")
                .unwrap_or("claude-sonnet-4-6")
                .to_string()
        });
    validate_beta_header(&claude_model, &beta_header)?;

    if use_bedrock {
        // Use Bedrock AI client
        let auth_token =
            get_env_var("ANTHROPIC_AUTH_TOKEN").map_err(|_| ClaudeError::ApiKeyNotFound)?;

        let base_url =
            get_env_var("ANTHROPIC_BEDROCK_BASE_URL").map_err(|_| ClaudeError::ApiKeyNotFound)?;

        let ai_client = BedrockAiClient::new(claude_model, auth_token, base_url, beta_header)?;
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

    let ai_client = ClaudeAiClient::new(claude_model, api_key, beta_header)?;
    debug!("Claude client created successfully");
    Ok(ClaudeClient::new(Box::new(ai_client)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::claude::ai::{AiClient, AiClientMetadata};
    use std::future::Future;
    use std::pin::Pin;

    /// Mock AI client for testing — never makes real HTTP requests.
    struct MockAiClient;

    impl AiClient for MockAiClient {
        fn send_request<'a>(
            &'a self,
            _system_prompt: &'a str,
            _user_prompt: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
            Box::pin(async { Ok(String::new()) })
        }

        fn get_metadata(&self) -> AiClientMetadata {
            AiClientMetadata {
                provider: "Mock".to_string(),
                model: "mock-model".to_string(),
                max_context_length: 200_000,
                max_response_length: 8_192,
                active_beta: None,
            }
        }
    }

    fn make_client() -> ClaudeClient {
        ClaudeClient::new(Box::new(MockAiClient))
    }

    // ── extract_yaml_from_response ─────────────────────────────────

    #[test]
    fn extract_yaml_pure_amendments() {
        let client = make_client();
        let content = "amendments:\n  - commit: abc123\n    message: test";
        let result = client.extract_yaml_from_response(content);
        assert!(result.starts_with("amendments:"));
    }

    #[test]
    fn extract_yaml_with_markdown_yaml_block() {
        let client = make_client();
        let content = "Here is the result:\n```yaml\namendments:\n  - commit: abc\n```\n";
        let result = client.extract_yaml_from_response(content);
        assert!(result.starts_with("amendments:"));
    }

    #[test]
    fn extract_yaml_with_generic_code_block() {
        let client = make_client();
        let content = "```\namendments:\n  - commit: abc\n```";
        let result = client.extract_yaml_from_response(content);
        assert!(result.starts_with("amendments:"));
    }

    #[test]
    fn extract_yaml_with_whitespace() {
        let client = make_client();
        let content = "  \n  amendments:\n  - commit: abc\n  ";
        let result = client.extract_yaml_from_response(content);
        assert!(result.starts_with("amendments:"));
    }

    #[test]
    fn extract_yaml_fallback_returns_trimmed() {
        let client = make_client();
        let content = "  some random text  ";
        let result = client.extract_yaml_from_response(content);
        assert_eq!(result, "some random text");
    }

    // ── extract_yaml_from_check_response ───────────────────────────

    #[test]
    fn extract_check_yaml_pure() {
        let client = make_client();
        let content = "checks:\n  - commit: abc123";
        let result = client.extract_yaml_from_check_response(content);
        assert!(result.starts_with("checks:"));
    }

    #[test]
    fn extract_check_yaml_markdown_block() {
        let client = make_client();
        let content = "```yaml\nchecks:\n  - commit: abc\n```";
        let result = client.extract_yaml_from_check_response(content);
        assert!(result.starts_with("checks:"));
    }

    #[test]
    fn extract_check_yaml_generic_block() {
        let client = make_client();
        let content = "```\nchecks:\n  - commit: abc\n```";
        let result = client.extract_yaml_from_check_response(content);
        assert!(result.starts_with("checks:"));
    }

    #[test]
    fn extract_check_yaml_fallback() {
        let client = make_client();
        let content = "  unexpected content  ";
        let result = client.extract_yaml_from_check_response(content);
        assert_eq!(result, "unexpected content");
    }

    // ── parse_amendment_response ────────────────────────────────────

    #[test]
    fn parse_amendment_response_valid() {
        let client = make_client();
        let yaml = format!(
            "amendments:\n  - commit: \"{}\"\n    message: \"test message\"",
            "a".repeat(40)
        );
        let result = client.parse_amendment_response(&yaml);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().amendments.len(), 1);
    }

    #[test]
    fn parse_amendment_response_invalid_yaml() {
        let client = make_client();
        let result = client.parse_amendment_response("not: valid: yaml: [{{");
        assert!(result.is_err());
    }

    #[test]
    fn parse_amendment_response_invalid_hash() {
        let client = make_client();
        let yaml = "amendments:\n  - commit: \"short\"\n    message: \"test\"";
        let result = client.parse_amendment_response(yaml);
        assert!(result.is_err());
    }

    // ── validate_beta_header ───────────────────────────────────────

    #[test]
    fn validate_beta_header_none_passes() {
        let result = validate_beta_header("claude-opus-4-1-20250805", &None);
        assert!(result.is_ok());
    }

    #[test]
    fn validate_beta_header_unsupported_fails() {
        let header = Some(("fake-key".to_string(), "fake-value".to_string()));
        let result = validate_beta_header("claude-opus-4-1-20250805", &header);
        assert!(result.is_err());
    }

    // ── ClaudeClient::new / get_ai_client_metadata ─────────────────

    #[test]
    fn client_metadata() {
        let client = make_client();
        let metadata = client.get_ai_client_metadata();
        assert_eq!(metadata.provider, "Mock");
        assert_eq!(metadata.model, "mock-model");
    }

    // ── property tests ────────────────────────────────────────────

    mod prop {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn yaml_response_output_trimmed(s in ".*") {
                let client = make_client();
                let result = client.extract_yaml_from_response(&s);
                prop_assert_eq!(&result, result.trim());
            }

            #[test]
            fn yaml_response_amendments_prefix_preserved(tail in ".*") {
                let client = make_client();
                let input = format!("amendments:{tail}");
                let result = client.extract_yaml_from_response(&input);
                prop_assert!(result.starts_with("amendments:"));
            }

            #[test]
            fn check_response_checks_prefix_preserved(tail in ".*") {
                let client = make_client();
                let input = format!("checks:{tail}");
                let result = client.extract_yaml_from_check_response(&input);
                prop_assert!(result.starts_with("checks:"));
            }

            #[test]
            fn yaml_fenced_block_strips_fences(
                content in "[a-zA-Z0-9: _\\-\n]{1,100}",
            ) {
                let client = make_client();
                let input = format!("```yaml\n{content}\n```");
                let result = client.extract_yaml_from_response(&input);
                prop_assert!(!result.contains("```"));
            }
        }
    }

    // ── ConfigurableMockAiClient tests ──────────────────────────────

    fn make_configurable_client(responses: Vec<Result<String>>) -> ClaudeClient {
        ClaudeClient::new(Box::new(
            crate::claude::test_utils::ConfigurableMockAiClient::new(responses),
        ))
    }

    fn make_test_repo_view(dir: &tempfile::TempDir) -> crate::data::RepositoryView {
        use crate::data::{AiInfo, FieldExplanation, WorkingDirectoryInfo};
        use crate::git::commit::FileChanges;
        use crate::git::{CommitAnalysis, CommitInfo};

        let diff_path = dir.path().join("0.diff");
        std::fs::write(&diff_path, "+added line\n").unwrap();

        crate::data::RepositoryView {
            versions: None,
            explanation: FieldExplanation::default(),
            working_directory: WorkingDirectoryInfo {
                clean: true,
                untracked_changes: Vec::new(),
            },
            remotes: Vec::new(),
            ai: AiInfo {
                scratch: String::new(),
            },
            branch_info: None,
            pr_template: None,
            pr_template_location: None,
            branch_prs: None,
            commits: vec![CommitInfo {
                hash: format!("{:0>40}", 0),
                author: "Test <test@test.com>".to_string(),
                date: chrono::Utc::now().fixed_offset(),
                original_message: "feat(test): add something".to_string(),
                in_main_branches: Vec::new(),
                analysis: CommitAnalysis {
                    detected_type: "feat".to_string(),
                    detected_scope: "test".to_string(),
                    proposed_message: "feat(test): add something".to_string(),
                    file_changes: FileChanges {
                        total_files: 1,
                        files_added: 1,
                        files_deleted: 0,
                        file_list: Vec::new(),
                    },
                    diff_summary: "file.rs | 1 +".to_string(),
                    diff_file: diff_path.to_string_lossy().to_string(),
                    file_diffs: Vec::new(),
                },
            }],
        }
    }

    fn valid_check_yaml() -> String {
        format!(
            "checks:\n  - commit: \"{hash}\"\n    passes: true\n    issues: []\n",
            hash = format!("{:0>40}", 0)
        )
    }

    #[tokio::test]
    async fn send_message_propagates_ai_error() {
        let client = make_configurable_client(vec![Err(anyhow::anyhow!("mock error"))]);
        let result = client.send_message("sys", "usr").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("mock error"));
    }

    #[tokio::test]
    async fn check_commits_succeeds_after_request_error() {
        let dir = tempfile::tempdir().unwrap();
        let repo_view = make_test_repo_view(&dir);
        // First attempt: request error; retries return valid response.
        let client = make_configurable_client(vec![
            Err(anyhow::anyhow!("rate limit")),
            Ok(valid_check_yaml()),
            Ok(valid_check_yaml()),
        ]);
        let result = client
            .check_commits_with_scopes(&repo_view, None, &[], false)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn check_commits_succeeds_after_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let repo_view = make_test_repo_view(&dir);
        // First attempt: AI returns malformed YAML; retry succeeds.
        let client = make_configurable_client(vec![
            Ok("not: valid: yaml: [[".to_string()),
            Ok(valid_check_yaml()),
            Ok(valid_check_yaml()),
        ]);
        let result = client
            .check_commits_with_scopes(&repo_view, None, &[], false)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn check_commits_fails_after_all_retries_exhausted() {
        let dir = tempfile::tempdir().unwrap();
        let repo_view = make_test_repo_view(&dir);
        let client = make_configurable_client(vec![
            Err(anyhow::anyhow!("first failure")),
            Err(anyhow::anyhow!("second failure")),
            Err(anyhow::anyhow!("final failure")),
        ]);
        let result = client
            .check_commits_with_scopes(&repo_view, None, &[], false)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn check_commits_fails_when_all_parses_fail() {
        let dir = tempfile::tempdir().unwrap();
        let repo_view = make_test_repo_view(&dir);
        let client = make_configurable_client(vec![
            Ok("bad yaml [[".to_string()),
            Ok("bad yaml [[".to_string()),
            Ok("bad yaml [[".to_string()),
        ]);
        let result = client
            .check_commits_with_scopes(&repo_view, None, &[], false)
            .await;
        assert!(result.is_err());
    }

    // ── split dispatch tests ─────────────────────────────────────

    /// Creates a mock client with a constrained context window.
    ///
    /// The window is large enough that a single-file chunk fits, but too
    /// small for both files together (including system prompt overhead).
    fn make_small_context_client(responses: Vec<Result<String>>) -> ClaudeClient {
        ClaudeClient::new(Box::new(
            crate::claude::test_utils::ConfigurableMockAiClient::new(responses)
                .with_context_length(12_000),
        ))
    }

    /// Creates a repo view with per-file diffs large enough to exceed the
    /// constrained context window, ensuring the split dispatch path triggers.
    fn make_large_diff_repo_view(dir: &tempfile::TempDir) -> crate::data::RepositoryView {
        use crate::data::{AiInfo, FieldExplanation, WorkingDirectoryInfo};
        use crate::git::commit::{FileChange, FileChanges, FileDiffRef};
        use crate::git::{CommitAnalysis, CommitInfo};

        let hash = "a".repeat(40);

        // Write a full (flat) diff file (large enough to bust the budget)
        let full_diff = "x".repeat(15_000);
        let flat_diff_path = dir.path().join("full.diff");
        std::fs::write(&flat_diff_path, &full_diff).unwrap();

        // Write two large per-file diff files (~8K chars each ≈ 2500 tokens)
        let diff_a = format!("diff --git a/src/a.rs b/src/a.rs\n{}\n", "a".repeat(8_000));
        let diff_b = format!("diff --git a/src/b.rs b/src/b.rs\n{}\n", "b".repeat(8_000));

        let path_a = dir.path().join("0000.diff");
        let path_b = dir.path().join("0001.diff");
        std::fs::write(&path_a, &diff_a).unwrap();
        std::fs::write(&path_b, &diff_b).unwrap();

        crate::data::RepositoryView {
            versions: None,
            explanation: FieldExplanation::default(),
            working_directory: WorkingDirectoryInfo {
                clean: true,
                untracked_changes: Vec::new(),
            },
            remotes: Vec::new(),
            ai: AiInfo {
                scratch: String::new(),
            },
            branch_info: None,
            pr_template: None,
            pr_template_location: None,
            branch_prs: None,
            commits: vec![CommitInfo {
                hash,
                author: "Test <test@test.com>".to_string(),
                date: chrono::Utc::now().fixed_offset(),
                original_message: "feat(test): large commit".to_string(),
                in_main_branches: Vec::new(),
                analysis: CommitAnalysis {
                    detected_type: "feat".to_string(),
                    detected_scope: "test".to_string(),
                    proposed_message: "feat(test): large commit".to_string(),
                    file_changes: FileChanges {
                        total_files: 2,
                        files_added: 2,
                        files_deleted: 0,
                        file_list: vec![
                            FileChange {
                                status: "A".to_string(),
                                file: "src/a.rs".to_string(),
                            },
                            FileChange {
                                status: "A".to_string(),
                                file: "src/b.rs".to_string(),
                            },
                        ],
                    },
                    diff_summary: " src/a.rs | 100 ++++\n src/b.rs | 100 ++++\n".to_string(),
                    diff_file: flat_diff_path.to_string_lossy().to_string(),
                    file_diffs: vec![
                        FileDiffRef {
                            path: "src/a.rs".to_string(),
                            diff_file: path_a.to_string_lossy().to_string(),
                            byte_len: diff_a.len(),
                        },
                        FileDiffRef {
                            path: "src/b.rs".to_string(),
                            diff_file: path_b.to_string_lossy().to_string(),
                            byte_len: diff_b.len(),
                        },
                    ],
                },
            }],
        }
    }

    fn valid_amendment_yaml(hash: &str, message: &str) -> String {
        format!("amendments:\n  - commit: \"{hash}\"\n    message: \"{message}\"")
    }

    #[tokio::test]
    async fn generate_amendments_split_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let repo_view = make_large_diff_repo_view(&dir);
        let hash = "a".repeat(40);

        // Responses: chunk 1 + chunk 2 + merge pass
        let client = make_small_context_client(vec![
            Ok(valid_amendment_yaml(&hash, "feat(a): add a.rs")),
            Ok(valid_amendment_yaml(&hash, "feat(b): add b.rs")),
            Ok(valid_amendment_yaml(&hash, "feat(test): add a.rs and b.rs")),
        ]);

        let result = client
            .generate_amendments_with_options(&repo_view, false)
            .await;

        assert!(result.is_ok(), "split dispatch failed: {:?}", result.err());
        let amendments = result.unwrap();
        assert_eq!(amendments.amendments.len(), 1);
        assert_eq!(amendments.amendments[0].commit, hash);
        assert!(amendments.amendments[0]
            .message
            .contains("add a.rs and b.rs"));
    }

    #[tokio::test]
    async fn generate_amendments_split_chunk_failure() {
        let dir = tempfile::tempdir().unwrap();
        let repo_view = make_large_diff_repo_view(&dir);
        let hash = "a".repeat(40);

        // First chunk succeeds, second chunk fails
        let client = make_small_context_client(vec![
            Ok(valid_amendment_yaml(&hash, "feat(a): add a.rs")),
            Err(anyhow::anyhow!("rate limit exceeded")),
        ]);

        let result = client
            .generate_amendments_with_options(&repo_view, false)
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn generate_amendments_no_split_when_fits() {
        let dir = tempfile::tempdir().unwrap();
        let repo_view = make_test_repo_view(&dir); // Small diff, no file_diffs
        let hash = format!("{:0>40}", 0);

        // Only one response needed — no split dispatch
        let client = make_configurable_client(vec![Ok(valid_amendment_yaml(
            &hash,
            "feat(test): improved message",
        ))]);

        let result = client
            .generate_amendments_with_options(&repo_view, false)
            .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap().amendments.len(), 1);
    }

    // ── check split dispatch tests ──────────────────────────────

    fn valid_check_yaml_for(hash: &str, passes: bool) -> String {
        format!(
            "checks:\n  - commit: \"{hash}\"\n    passes: {passes}\n    issues: []\n    summary: \"test summary\"\n"
        )
    }

    fn valid_check_yaml_with_issues(hash: &str) -> String {
        format!(
            concat!(
                "checks:\n",
                "  - commit: \"{hash}\"\n",
                "    passes: false\n",
                "    issues:\n",
                "      - severity: error\n",
                "        section: \"Subject Line\"\n",
                "        rule: \"subject-too-long\"\n",
                "        explanation: \"Subject exceeds 72 characters\"\n",
                "    suggestion:\n",
                "      message: \"feat(test): shorter subject\"\n",
                "      explanation: \"Shortened subject line\"\n",
                "    summary: \"Large commit with issues\"\n",
            ),
            hash = hash,
        )
    }

    fn valid_check_yaml_chunk_no_suggestion(hash: &str) -> String {
        format!(
            concat!(
                "checks:\n",
                "  - commit: \"{hash}\"\n",
                "    passes: true\n",
                "    issues: []\n",
                "    summary: \"chunk summary\"\n",
            ),
            hash = hash,
        )
    }

    #[tokio::test]
    async fn check_commits_split_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let repo_view = make_large_diff_repo_view(&dir);
        let hash = "a".repeat(40);

        // Responses: chunk 1 (issues + suggestion) + chunk 2 (issues + suggestion) + merge pass
        let client = make_small_context_client(vec![
            Ok(valid_check_yaml_with_issues(&hash)),
            Ok(valid_check_yaml_with_issues(&hash)),
            Ok(valid_check_yaml_with_issues(&hash)), // merge pass response
        ]);

        let result = client
            .check_commits_with_scopes(&repo_view, None, &[], true)
            .await;

        assert!(result.is_ok(), "split dispatch failed: {:?}", result.err());
        let report = result.unwrap();
        assert_eq!(report.commits.len(), 1);
        assert!(!report.commits[0].passes);
        // Dedup: both chunks report the same (rule, severity, section), so only 1 unique issue
        assert_eq!(report.commits[0].issues.len(), 1);
        assert_eq!(report.commits[0].issues[0].rule, "subject-too-long");
    }

    #[tokio::test]
    async fn check_commits_split_dispatch_no_merge_when_no_suggestions() {
        let dir = tempfile::tempdir().unwrap();
        let repo_view = make_large_diff_repo_view(&dir);
        let hash = "a".repeat(40);

        // Responses: chunk 1 + chunk 2, both passing with no suggestions
        // No merge pass needed — only 2 responses
        let client = make_small_context_client(vec![
            Ok(valid_check_yaml_chunk_no_suggestion(&hash)),
            Ok(valid_check_yaml_chunk_no_suggestion(&hash)),
        ]);

        let result = client
            .check_commits_with_scopes(&repo_view, None, &[], false)
            .await;

        assert!(result.is_ok(), "split dispatch failed: {:?}", result.err());
        let report = result.unwrap();
        assert_eq!(report.commits.len(), 1);
        assert!(report.commits[0].passes);
        assert!(report.commits[0].issues.is_empty());
        assert!(report.commits[0].suggestion.is_none());
        // First non-None summary from chunks
        assert_eq!(report.commits[0].summary.as_deref(), Some("chunk summary"));
    }

    #[tokio::test]
    async fn check_commits_split_chunk_failure() {
        let dir = tempfile::tempdir().unwrap();
        let repo_view = make_large_diff_repo_view(&dir);
        let hash = "a".repeat(40);

        // First chunk succeeds, second chunk fails
        let client = make_small_context_client(vec![
            Ok(valid_check_yaml_for(&hash, true)),
            Err(anyhow::anyhow!("rate limit exceeded")),
        ]);

        let result = client
            .check_commits_with_scopes(&repo_view, None, &[], false)
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn check_commits_no_split_when_fits() {
        let dir = tempfile::tempdir().unwrap();
        let repo_view = make_test_repo_view(&dir); // Small diff, no file_diffs
        let hash = format!("{:0>40}", 0);

        // Only one response needed — no split dispatch
        let client = make_configurable_client(vec![Ok(valid_check_yaml_for(&hash, true))]);

        let result = client
            .check_commits_with_scopes(&repo_view, None, &[], false)
            .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap().commits.len(), 1);
    }

    #[tokio::test]
    async fn check_commits_split_dedup_across_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let repo_view = make_large_diff_repo_view(&dir);
        let hash = "a".repeat(40);

        // Chunk 1: two issues (error + warning)
        let chunk1 = format!(
            concat!(
                "checks:\n",
                "  - commit: \"{hash}\"\n",
                "    passes: false\n",
                "    issues:\n",
                "      - severity: error\n",
                "        section: \"Subject Line\"\n",
                "        rule: \"subject-too-long\"\n",
                "        explanation: \"Subject exceeds 72 characters\"\n",
                "      - severity: warning\n",
                "        section: \"Content\"\n",
                "        rule: \"body-required\"\n",
                "        explanation: \"Large change needs body\"\n",
            ),
            hash = hash,
        );

        // Chunk 2: same error (different wording) + new info issue
        let chunk2 = format!(
            concat!(
                "checks:\n",
                "  - commit: \"{hash}\"\n",
                "    passes: false\n",
                "    issues:\n",
                "      - severity: error\n",
                "        section: \"Subject Line\"\n",
                "        rule: \"subject-too-long\"\n",
                "        explanation: \"Subject line is too long\"\n",
                "      - severity: info\n",
                "        section: \"Style\"\n",
                "        rule: \"scope-suggestion\"\n",
                "        explanation: \"Consider more specific scope\"\n",
            ),
            hash = hash,
        );

        // No suggestions → no merge pass needed
        let client = make_small_context_client(vec![Ok(chunk1), Ok(chunk2)]);

        let result = client
            .check_commits_with_scopes(&repo_view, None, &[], false)
            .await;

        assert!(result.is_ok(), "split dispatch failed: {:?}", result.err());
        let report = result.unwrap();
        assert_eq!(report.commits.len(), 1);
        assert!(!report.commits[0].passes);
        // 3 unique issues: subject-too-long, body-required, scope-suggestion
        // (subject-too-long appears in both chunks but deduped)
        assert_eq!(report.commits[0].issues.len(), 3);
    }

    #[tokio::test]
    async fn check_commits_split_passes_only_when_all_chunks_pass() {
        let dir = tempfile::tempdir().unwrap();
        let repo_view = make_large_diff_repo_view(&dir);
        let hash = "a".repeat(40);

        // Chunk 1 passes, chunk 2 fails
        let client = make_small_context_client(vec![
            Ok(valid_check_yaml_for(&hash, true)),
            Ok(valid_check_yaml_for(&hash, false)),
        ]);

        let result = client
            .check_commits_with_scopes(&repo_view, None, &[], false)
            .await;

        assert!(result.is_ok(), "split dispatch failed: {:?}", result.err());
        let report = result.unwrap();
        assert!(
            !report.commits[0].passes,
            "should fail when any chunk fails"
        );
    }
}
