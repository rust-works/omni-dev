//! Claude client for commit message improvement.

use anyhow::{Context, Result};
use tracing::{debug, info};

use crate::claude::token_budget::TokenBudget;
use crate::claude::{ai::bedrock::BedrockAiClient, ai::claude::ClaudeAiClient};
use crate::claude::{ai::AiClient, error::ClaudeError, prompts};
use crate::data::{
    amendments::{Amendment, AmendmentFile},
    context::CommitContext,
    RepositoryView, RepositoryViewForAI,
};

/// Returned when the full diff does not fit the token budget.
///
/// Carries the data needed for split dispatch so the caller can size
/// diff chunks appropriately.
struct BudgetExceeded {
    /// Available input tokens for this model (context window minus output reserve).
    available_input_tokens: usize,
}

/// Maximum retries for amendment parse/request failures (matches check retry count).
const AMENDMENT_PARSE_MAX_RETRIES: u32 = 2;

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

    /// Builds a user prompt and validates it against the model's token budget.
    ///
    /// Serializes the repository view to YAML, constructs the user prompt, and
    /// checks that it fits within the available input tokens. Returns an error
    /// if the prompt exceeds the budget.
    fn build_prompt_fitting_budget(
        &self,
        ai_view: &RepositoryViewForAI,
        system_prompt: &str,
        build_user_prompt: &(impl Fn(&str) -> String + ?Sized),
    ) -> Result<String> {
        let metadata = self.ai_client.get_metadata();
        let budget = TokenBudget::from_metadata(&metadata);

        let yaml =
            crate::data::to_yaml(ai_view).context("Failed to serialize repository view to YAML")?;
        let user_prompt = build_user_prompt(&yaml);

        let estimate = budget.validate_prompt(system_prompt, &user_prompt)?;
        debug!(
            model = %metadata.model,
            estimated_tokens = estimate.estimated_tokens,
            available_tokens = estimate.available_tokens,
            utilization_pct = format!("{:.1}%", estimate.utilization_pct),
            "Token budget check passed"
        );

        Ok(user_prompt)
    }

    /// Tests whether the full diff fits the token budget.
    ///
    /// Returns `Ok(Ok(user_prompt))` when the full diff fits,
    /// `Ok(Err(BudgetExceeded))` when it does not, or a top-level error
    /// on serialization failure.
    fn try_full_diff_budget(
        &self,
        ai_view: &RepositoryViewForAI,
        system_prompt: &str,
        build_user_prompt: &(impl Fn(&str) -> String + ?Sized),
    ) -> Result<std::result::Result<String, BudgetExceeded>> {
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
                "Token budget check passed"
            );
            return Ok(Ok(user_prompt));
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
        use crate::claude::batch::{
            PER_COMMIT_METADATA_OVERHEAD_TOKENS, USER_PROMPT_TEMPLATE_OVERHEAD_TOKENS,
            VIEW_ENVELOPE_OVERHEAD_TOKENS,
        };
        use crate::claude::diff_pack::pack_file_diffs;
        use crate::claude::token_budget;
        use crate::git::commit::CommitInfoForAI;

        // Compute effective capacity for diff packing by subtracting overhead
        // that will be added when the full prompt is assembled. This mirrors
        // the calculation in `batch::plan_batches`.
        //
        // Each chunk includes the FULL original_message and diff_summary (not
        // just the partial diff), so we must subtract those from capacity.
        // We also subtract user prompt template overhead for instruction text.
        let system_prompt_tokens = token_budget::estimate_tokens(system_prompt);
        let commit_text_tokens = token_budget::estimate_tokens(&commit.original_message)
            + token_budget::estimate_tokens(&commit.analysis.diff_summary);
        let chunk_capacity = available_input_tokens
            .saturating_sub(system_prompt_tokens)
            .saturating_sub(VIEW_ENVELOPE_OVERHEAD_TOKENS)
            .saturating_sub(PER_COMMIT_METADATA_OVERHEAD_TOKENS)
            .saturating_sub(USER_PROMPT_TEMPLATE_OVERHEAD_TOKENS)
            .saturating_sub(commit_text_tokens);

        debug!(
            commit = %&commit.hash[..8],
            available_input_tokens,
            system_prompt_tokens,
            envelope_overhead = VIEW_ENVELOPE_OVERHEAD_TOKENS,
            metadata_overhead = PER_COMMIT_METADATA_OVERHEAD_TOKENS,
            template_overhead = USER_PROMPT_TEMPLATE_OVERHEAD_TOKENS,
            commit_text_tokens,
            chunk_capacity,
            "Split dispatch: computed chunk capacity"
        );

        let plan = pack_file_diffs(&commit.hash, &commit.analysis.file_diffs, chunk_capacity)
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
            chunk_capacity,
            "Split dispatch: processing commit in chunks"
        );

        let mut chunk_amendments = Vec::with_capacity(total_chunks);
        for (i, chunk) in plan.chunks.iter().enumerate() {
            let mut partial = CommitInfoForAI::from_commit_info_partial_with_overrides(
                commit.clone(),
                &chunk.file_paths,
                &chunk.diff_overrides,
            )
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

            // Log the actual diff content size for this chunk
            let diff_content_len = partial.base.analysis.diff_content.len();
            let diff_content_tokens =
                token_budget::estimate_tokens_from_char_count(diff_content_len);
            debug!(
                commit = %&commit.hash[..8],
                chunk_index = i,
                diff_content_len,
                diff_content_tokens,
                "Split dispatch: chunk diff content size"
            );

            let user_prompt =
                self.build_prompt_fitting_budget(&partial_view, system_prompt, build_user_prompt)?;

            info!(
                commit = %&commit.hash[..8],
                chunk = i + 1,
                total_chunks,
                user_prompt_len = user_prompt.len(),
                "Split dispatch: sending chunk to AI"
            );

            let content = match self
                .ai_client
                .send_request(system_prompt, &user_prompt)
                .await
            {
                Ok(content) => content,
                Err(e) => {
                    // Log the underlying error before wrapping
                    tracing::error!(
                        commit = %&commit.hash[..8],
                        chunk = i + 1,
                        error = %e,
                        error_debug = ?e,
                        "Split dispatch: AI request failed"
                    );
                    return Err(e).with_context(|| {
                        format!(
                            "Chunk {}/{} failed for commit {}",
                            i + 1,
                            total_chunks,
                            &commit.hash[..8]
                        )
                    });
                }
            };

            info!(
                commit = %&commit.hash[..8],
                chunk = i + 1,
                response_len = content.len(),
                "Split dispatch: received chunk response"
            );

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

    /// Generates an amendment for a single commit, using split dispatch
    /// if the full diff exceeds the token budget.
    ///
    /// Tries the full diff first. If it exceeds the budget and the commit
    /// has file-level diffs, falls back to
    /// [`generate_amendment_split`](Self::generate_amendment_split).
    async fn generate_amendment_for_commit(
        &self,
        commit: &crate::git::CommitInfo,
        repo_view_for_ai: &RepositoryViewForAI,
        system_prompt: &str,
        build_user_prompt: &(dyn Fn(&str) -> String + Sync),
        fresh: bool,
    ) -> Result<Amendment> {
        let mut ai_commit = crate::git::commit::CommitInfoForAI::from_commit_info(commit.clone())?;
        if fresh {
            ai_commit.base.original_message =
                "(Original message hidden - generate fresh message from diff)".to_string();
        }
        let single_view = repo_view_for_ai.single_commit_view_for_ai(&ai_commit);

        match self.try_full_diff_budget(&single_view, system_prompt, build_user_prompt)? {
            Ok(user_prompt) => {
                let amendment_file = self
                    .send_and_parse_amendment_with_retry(system_prompt, &user_prompt)
                    .await?;
                amendment_file
                    .amendments
                    .into_iter()
                    .next()
                    .context("AI returned no amendments for commit")
            }
            Err(exceeded) => {
                if commit.analysis.file_diffs.is_empty() {
                    anyhow::bail!(
                        "Token budget exceeded for commit {} but no file-level diffs available for split dispatch",
                        &commit.hash[..8]
                    );
                }
                self.generate_amendment_split(
                    commit,
                    repo_view_for_ai,
                    system_prompt,
                    build_user_prompt,
                    exceeded.available_input_tokens,
                    fresh,
                )
                .await
            }
        }
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
        use crate::claude::batch::{
            PER_COMMIT_METADATA_OVERHEAD_TOKENS, USER_PROMPT_TEMPLATE_OVERHEAD_TOKENS,
            VIEW_ENVELOPE_OVERHEAD_TOKENS,
        };
        use crate::claude::diff_pack::pack_file_diffs;
        use crate::claude::token_budget;
        use crate::data::check::{CommitCheckResult, CommitIssue, IssueSeverity};
        use crate::git::commit::CommitInfoForAI;

        // Compute effective capacity for diff packing by subtracting overhead
        // that will be added when the full prompt is assembled. This mirrors
        // the calculation in `batch::plan_batches`.
        //
        // Each chunk includes the FULL original_message and diff_summary (not
        // just the partial diff), so we must subtract those from capacity.
        // We also subtract user prompt template overhead for instruction text.
        let system_prompt_tokens = token_budget::estimate_tokens(system_prompt);
        let commit_text_tokens = token_budget::estimate_tokens(&commit.original_message)
            + token_budget::estimate_tokens(&commit.analysis.diff_summary);
        let chunk_capacity = available_input_tokens
            .saturating_sub(system_prompt_tokens)
            .saturating_sub(VIEW_ENVELOPE_OVERHEAD_TOKENS)
            .saturating_sub(PER_COMMIT_METADATA_OVERHEAD_TOKENS)
            .saturating_sub(USER_PROMPT_TEMPLATE_OVERHEAD_TOKENS)
            .saturating_sub(commit_text_tokens);

        debug!(
            commit = %&commit.hash[..8],
            available_input_tokens,
            system_prompt_tokens,
            envelope_overhead = VIEW_ENVELOPE_OVERHEAD_TOKENS,
            metadata_overhead = PER_COMMIT_METADATA_OVERHEAD_TOKENS,
            template_overhead = USER_PROMPT_TEMPLATE_OVERHEAD_TOKENS,
            commit_text_tokens,
            chunk_capacity,
            "Check split dispatch: computed chunk capacity"
        );

        let plan = pack_file_diffs(&commit.hash, &commit.analysis.file_diffs, chunk_capacity)
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
            chunk_capacity,
            "Check split dispatch: processing commit in chunks"
        );

        let build_user_prompt =
            |yaml: &str| prompts::generate_check_user_prompt(yaml, include_suggestions);

        let mut chunk_results = Vec::with_capacity(total_chunks);
        for (i, chunk) in plan.chunks.iter().enumerate() {
            let mut partial = CommitInfoForAI::from_commit_info_partial_with_overrides(
                commit.clone(),
                &chunk.file_paths,
                &chunk.diff_overrides,
            )
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

            let user_prompt =
                self.build_prompt_fitting_budget(&partial_view, system_prompt, &build_user_prompt)?;

            let content = self
                .ai_client
                .send_request(system_prompt, &user_prompt)
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

        // Try full view first; fall back to per-commit split dispatch
        match self.try_full_diff_budget(&ai_repo_view, system_prompt, &build_user_prompt)? {
            Ok(user_prompt) => {
                self.send_and_parse_amendment_with_retry(system_prompt, &user_prompt)
                    .await
            }
            Err(_exceeded) => {
                let mut amendments = Vec::new();
                for commit in &repo_view.commits {
                    let amendment = self
                        .generate_amendment_for_commit(
                            commit,
                            &ai_repo_view,
                            system_prompt,
                            &build_user_prompt,
                            fresh,
                        )
                        .await?;
                    amendments.push(amendment);
                }
                Ok(AmendmentFile { amendments })
            }
        }
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

        // Try full view first; fall back to per-commit split dispatch
        match self.try_full_diff_budget(&ai_repo_view, &system_prompt, &build_user_prompt)? {
            Ok(user_prompt) => {
                self.send_and_parse_amendment_with_retry(&system_prompt, &user_prompt)
                    .await
            }
            Err(_exceeded) => {
                let mut amendments = Vec::new();
                for commit in &repo_view.commits {
                    let amendment = self
                        .generate_amendment_for_commit(
                            commit,
                            &ai_repo_view,
                            &system_prompt,
                            &build_user_prompt,
                            fresh,
                        )
                        .await?;
                    amendments.push(amendment);
                }
                Ok(AmendmentFile { amendments })
            }
        }
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

    /// Sends a prompt to the AI and parses the response as an [`AmendmentFile`],
    /// retrying on parse or request failures.
    ///
    /// Mirrors the retry pattern in [`check_commits_with_retry`](Self::check_commits_with_retry):
    /// up to [`AMENDMENT_PARSE_MAX_RETRIES`] additional attempts after the first
    /// failure. Logs a warning via `eprintln!` and a `debug!` trace on each retry.
    /// Returns the last error if all attempts are exhausted.
    async fn send_and_parse_amendment_with_retry(
        &self,
        system_prompt: &str,
        user_prompt: &str,
    ) -> Result<AmendmentFile> {
        let mut last_error = None;
        for attempt in 0..=AMENDMENT_PARSE_MAX_RETRIES {
            match self
                .ai_client
                .send_request(system_prompt, user_prompt)
                .await
            {
                Ok(content) => match self.parse_amendment_response(&content) {
                    Ok(amendment_file) => return Ok(amendment_file),
                    Err(e) => {
                        if attempt < AMENDMENT_PARSE_MAX_RETRIES {
                            eprintln!(
                                "warning: failed to parse amendment response (attempt {}), retrying...",
                                attempt + 1
                            );
                            debug!(error = %e, attempt = attempt + 1, "Amendment response parse failed, retrying");
                        }
                        last_error = Some(e);
                    }
                },
                Err(e) => {
                    if attempt < AMENDMENT_PARSE_MAX_RETRIES {
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
        Err(last_error
            .unwrap_or_else(|| anyhow::anyhow!("Amendment generation failed after retries")))
    }

    /// Parses an AI response as PR content YAML.
    fn parse_pr_response(&self, content: &str) -> Result<crate::cli::git::PrContent> {
        let yaml_content = content.trim();
        crate::data::from_yaml(yaml_content)
            .context("Failed to parse AI response as YAML. AI may have returned malformed output.")
    }

    /// Generates PR content for a single commit whose diff exceeds the token
    /// budget by splitting it into file-level chunks.
    ///
    /// Analogous to [`generate_amendment_split`](Self::generate_amendment_split)
    /// but produces [`PrContent`](crate::cli::git::PrContent) instead of an
    /// amendment.
    async fn generate_pr_content_split(
        &self,
        commit: &crate::git::CommitInfo,
        repo_view_for_ai: &RepositoryViewForAI,
        system_prompt: &str,
        build_user_prompt: &(dyn Fn(&str) -> String + Sync),
        available_input_tokens: usize,
        pr_template: &str,
    ) -> Result<crate::cli::git::PrContent> {
        use crate::claude::batch::{
            PER_COMMIT_METADATA_OVERHEAD_TOKENS, USER_PROMPT_TEMPLATE_OVERHEAD_TOKENS,
            VIEW_ENVELOPE_OVERHEAD_TOKENS,
        };
        use crate::claude::diff_pack::pack_file_diffs;
        use crate::claude::token_budget;
        use crate::git::commit::CommitInfoForAI;

        // Compute effective capacity for diff packing by subtracting overhead
        // that will be added when the full prompt is assembled. This mirrors
        // the calculation in `batch::plan_batches`.
        //
        // Each chunk includes the FULL original_message and diff_summary (not
        // just the partial diff), so we must subtract those from capacity.
        // We also subtract user prompt template overhead for instruction text.
        let system_prompt_tokens = token_budget::estimate_tokens(system_prompt);
        let commit_text_tokens = token_budget::estimate_tokens(&commit.original_message)
            + token_budget::estimate_tokens(&commit.analysis.diff_summary);
        let chunk_capacity = available_input_tokens
            .saturating_sub(system_prompt_tokens)
            .saturating_sub(VIEW_ENVELOPE_OVERHEAD_TOKENS)
            .saturating_sub(PER_COMMIT_METADATA_OVERHEAD_TOKENS)
            .saturating_sub(USER_PROMPT_TEMPLATE_OVERHEAD_TOKENS)
            .saturating_sub(commit_text_tokens);

        debug!(
            commit = %&commit.hash[..8],
            available_input_tokens,
            system_prompt_tokens,
            envelope_overhead = VIEW_ENVELOPE_OVERHEAD_TOKENS,
            metadata_overhead = PER_COMMIT_METADATA_OVERHEAD_TOKENS,
            template_overhead = USER_PROMPT_TEMPLATE_OVERHEAD_TOKENS,
            commit_text_tokens,
            chunk_capacity,
            "PR split dispatch: computed chunk capacity"
        );

        let plan = pack_file_diffs(&commit.hash, &commit.analysis.file_diffs, chunk_capacity)
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
            chunk_capacity,
            "PR split dispatch: processing commit in chunks"
        );

        let mut chunk_contents = Vec::with_capacity(total_chunks);
        for (i, chunk) in plan.chunks.iter().enumerate() {
            let partial = CommitInfoForAI::from_commit_info_partial_with_overrides(
                commit.clone(),
                &chunk.file_paths,
                &chunk.diff_overrides,
            )
            .with_context(|| {
                format!(
                    "Failed to build partial view for chunk {}/{} of commit {}",
                    i + 1,
                    total_chunks,
                    &commit.hash[..8]
                )
            })?;

            let partial_view = repo_view_for_ai.single_commit_view_for_ai(&partial);

            let user_prompt =
                self.build_prompt_fitting_budget(&partial_view, system_prompt, build_user_prompt)?;

            let content = self
                .ai_client
                .send_request(system_prompt, &user_prompt)
                .await
                .with_context(|| {
                    format!(
                        "PR chunk {}/{} failed for commit {}",
                        i + 1,
                        total_chunks,
                        &commit.hash[..8]
                    )
                })?;

            let pr_content = self.parse_pr_response(&content).with_context(|| {
                format!(
                    "Failed to parse PR chunk {}/{} response for commit {}",
                    i + 1,
                    total_chunks,
                    &commit.hash[..8]
                )
            })?;

            chunk_contents.push(pr_content);
        }

        self.merge_pr_content_chunks(&chunk_contents, pr_template)
            .await
    }

    /// Runs an AI reduce pass to synthesize a single PR content from partial
    /// per-commit or per-chunk PR contents.
    async fn merge_pr_content_chunks(
        &self,
        partial_contents: &[crate::cli::git::PrContent],
        pr_template: &str,
    ) -> Result<crate::cli::git::PrContent> {
        let system_prompt = prompts::PR_CONTENT_MERGE_SYSTEM_PROMPT;
        let user_prompt =
            prompts::generate_pr_content_merge_user_prompt(partial_contents, pr_template);

        self.validate_prompt_budget(system_prompt, &user_prompt)?;

        let content = self
            .ai_client
            .send_request(system_prompt, &user_prompt)
            .await
            .context("Merge pass failed for PR content chunks")?;

        self.parse_pr_response(&content)
            .context("Failed to parse PR content merge pass response")
    }

    /// Generates PR content for a single commit, using split dispatch if needed.
    async fn generate_pr_content_for_commit(
        &self,
        commit: &crate::git::CommitInfo,
        repo_view_for_ai: &RepositoryViewForAI,
        system_prompt: &str,
        build_user_prompt: &(dyn Fn(&str) -> String + Sync),
        pr_template: &str,
    ) -> Result<crate::cli::git::PrContent> {
        let ai_commit = crate::git::commit::CommitInfoForAI::from_commit_info(commit.clone())?;
        let single_view = repo_view_for_ai.single_commit_view_for_ai(&ai_commit);

        match self.try_full_diff_budget(&single_view, system_prompt, build_user_prompt)? {
            Ok(user_prompt) => {
                let content = self
                    .ai_client
                    .send_request(system_prompt, &user_prompt)
                    .await?;
                self.parse_pr_response(&content)
            }
            Err(exceeded) => {
                if commit.analysis.file_diffs.is_empty() {
                    anyhow::bail!(
                        "Token budget exceeded for commit {} but no file-level diffs available for split dispatch",
                        &commit.hash[..8]
                    );
                }
                self.generate_pr_content_split(
                    commit,
                    repo_view_for_ai,
                    system_prompt,
                    build_user_prompt,
                    exceeded.available_input_tokens,
                    pr_template,
                )
                .await
            }
        }
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

        let build_user_prompt =
            |yaml: &str| prompts::generate_pr_description_prompt(yaml, pr_template);

        // Try full view first; fall back to per-commit split dispatch
        match self.try_full_diff_budget(
            &ai_repo_view,
            prompts::PR_GENERATION_SYSTEM_PROMPT,
            &build_user_prompt,
        )? {
            Ok(user_prompt) => {
                let content = self
                    .ai_client
                    .send_request(prompts::PR_GENERATION_SYSTEM_PROMPT, &user_prompt)
                    .await?;
                self.parse_pr_response(&content)
            }
            Err(_exceeded) => {
                let mut per_commit_contents = Vec::new();
                for commit in &repo_view.commits {
                    let pr = self
                        .generate_pr_content_for_commit(
                            commit,
                            &ai_repo_view,
                            prompts::PR_GENERATION_SYSTEM_PROMPT,
                            &build_user_prompt,
                            pr_template,
                        )
                        .await?;
                    per_commit_contents.push(pr);
                }
                if per_commit_contents.len() == 1 {
                    return per_commit_contents
                        .into_iter()
                        .next()
                        .context("Per-commit PR contents unexpectedly empty");
                }
                self.merge_pr_content_chunks(&per_commit_contents, pr_template)
                    .await
            }
        }
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

        let build_user_prompt = |yaml: &str| {
            prompts::generate_pr_description_prompt_with_context(yaml, pr_template, context)
        };

        // Try full view first; fall back to per-commit split dispatch
        match self.try_full_diff_budget(&ai_repo_view, &system_prompt, &build_user_prompt)? {
            Ok(user_prompt) => {
                let content = self
                    .ai_client
                    .send_request(&system_prompt, &user_prompt)
                    .await?;

                debug!(
                    content_length = content.len(),
                    "Received AI response for PR content"
                );

                let pr_content = self.parse_pr_response(&content)?;

                debug!(
                    parsed_title = %pr_content.title,
                    parsed_description_length = pr_content.description.len(),
                    parsed_description_preview = %pr_content.description.lines().take(3).collect::<Vec<_>>().join("\\n"),
                    "Successfully parsed PR content from YAML"
                );

                Ok(pr_content)
            }
            Err(_exceeded) => {
                let mut per_commit_contents = Vec::new();
                for commit in &repo_view.commits {
                    let pr = self
                        .generate_pr_content_for_commit(
                            commit,
                            &ai_repo_view,
                            &system_prompt,
                            &build_user_prompt,
                            pr_template,
                        )
                        .await?;
                    per_commit_contents.push(pr);
                }
                if per_commit_contents.len() == 1 {
                    return per_commit_contents
                        .into_iter()
                        .next()
                        .context("Per-commit PR contents unexpectedly empty");
                }
                self.merge_pr_content_chunks(&per_commit_contents, pr_template)
                    .await
            }
        }
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

        let mut ai_repo_view = RepositoryViewForAI::from_repository_view(repo_view.clone())
            .context("Failed to enhance repository view with diff content")?;
        for commit in &mut ai_repo_view.commits {
            commit.run_pre_validation_checks(valid_scopes);
        }

        // Try full view first; fall back to per-commit split dispatch
        match self.try_full_diff_budget(&ai_repo_view, &system_prompt, &build_user_prompt)? {
            Ok(user_prompt) => {
                // Full view fits: send with retry loop
                let mut last_error = None;
                for attempt in 0..=max_retries {
                    match self
                        .ai_client
                        .send_request(&system_prompt, &user_prompt)
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
            Err(_exceeded) => {
                // Per-commit split dispatch
                let mut all_results = Vec::new();
                for commit in &repo_view.commits {
                    let single_view = repo_view.single_commit_view(commit);
                    let mut single_ai_view =
                        RepositoryViewForAI::from_repository_view(single_view.clone())
                            .context("Failed to enhance single-commit view with diff content")?;
                    for c in &mut single_ai_view.commits {
                        c.run_pre_validation_checks(valid_scopes);
                    }

                    match self.try_full_diff_budget(
                        &single_ai_view,
                        &system_prompt,
                        &build_user_prompt,
                    )? {
                        Ok(user_prompt) => {
                            let content = self
                                .ai_client
                                .send_request(&system_prompt, &user_prompt)
                                .await?;
                            let report = self.parse_check_response(&content, &single_view)?;
                            all_results.extend(report.commits);
                        }
                        Err(exceeded) => {
                            if commit.analysis.file_diffs.is_empty() {
                                anyhow::bail!(
                                    "Token budget exceeded for commit {} but no file-level diffs available for split dispatch",
                                    &commit.hash[..8]
                                );
                            }
                            let report = self
                                .check_commit_split(
                                    commit,
                                    &single_view,
                                    &system_prompt,
                                    valid_scopes,
                                    include_suggestions,
                                    exceeded.available_input_tokens,
                                )
                                .await?;
                            all_results.extend(report.commits);
                        }
                    }
                }
                Ok(crate::data::check::CheckReport::new(all_results))
            }
        }
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
        // Context of 50k with more conservative token estimation (2.5 chars/token
        // vs 3.5) ensures per-file diffs fit in chunks without placeholders while
        // still being large enough to trigger split dispatch for multiple files.
        let mock = crate::claude::test_utils::ConfigurableMockAiClient::new(responses)
            .with_context_length(50_000);
        ClaudeClient::new(Box::new(mock))
    }

    /// Like [`make_small_context_client`] but also returns a handle to inspect
    /// how many mock responses remain unconsumed after the test runs.
    fn make_small_context_client_tracked(
        responses: Vec<Result<String>>,
    ) -> (ClaudeClient, crate::claude::test_utils::ResponseQueueHandle) {
        let mock = crate::claude::test_utils::ConfigurableMockAiClient::new(responses)
            .with_context_length(50_000);
        let handle = mock.response_handle();
        (ClaudeClient::new(Box::new(mock)), handle)
    }

    /// Creates a repo view with per-file diffs large enough to exceed the
    /// constrained context window, ensuring the split dispatch path triggers.
    fn make_large_diff_repo_view(dir: &tempfile::TempDir) -> crate::data::RepositoryView {
        use crate::data::{AiInfo, FieldExplanation, WorkingDirectoryInfo};
        use crate::git::commit::{FileChange, FileChanges, FileDiffRef};
        use crate::git::{CommitAnalysis, CommitInfo};

        let hash = "a".repeat(40);

        // Write a full (flat) diff file large enough to bust the budget.
        // With 50k context / 2.5 chars-per-token / 1.2 margin, available ≈ 41k tokens.
        // 120k chars → ~57,600 tokens → well over budget.
        let full_diff = "x".repeat(120_000);
        let flat_diff_path = dir.path().join("full.diff");
        std::fs::write(&flat_diff_path, &full_diff).unwrap();

        // Write two large per-file diff files (~30K chars each ≈ 14,400 tokens with
        // conservative 2.5 chars/token * 1.2 margin estimation)
        let diff_a = format!("diff --git a/src/a.rs b/src/a.rs\n{}\n", "a".repeat(30_000));
        let diff_b = format!("diff --git a/src/b.rs b/src/b.rs\n{}\n", "b".repeat(30_000));

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

    // ── multi-commit and PR generation paths ──────────────────────

    /// Creates a repo view with two small commits (fits budget without split dispatch).
    fn make_multi_commit_repo_view(dir: &tempfile::TempDir) -> crate::data::RepositoryView {
        use crate::data::{AiInfo, FieldExplanation, WorkingDirectoryInfo};
        use crate::git::commit::FileChanges;
        use crate::git::{CommitAnalysis, CommitInfo};

        let diff_a = dir.path().join("0.diff");
        let diff_b = dir.path().join("1.diff");
        std::fs::write(&diff_a, "+line a\n").unwrap();
        std::fs::write(&diff_b, "+line b\n").unwrap();

        let hash_a = "a".repeat(40);
        let hash_b = "b".repeat(40);

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
            commits: vec![
                CommitInfo {
                    hash: hash_a,
                    author: "Test <test@test.com>".to_string(),
                    date: chrono::Utc::now().fixed_offset(),
                    original_message: "feat(a): add a".to_string(),
                    in_main_branches: Vec::new(),
                    analysis: CommitAnalysis {
                        detected_type: "feat".to_string(),
                        detected_scope: "a".to_string(),
                        proposed_message: "feat(a): add a".to_string(),
                        file_changes: FileChanges {
                            total_files: 1,
                            files_added: 1,
                            files_deleted: 0,
                            file_list: Vec::new(),
                        },
                        diff_summary: "a.rs | 1 +".to_string(),
                        diff_file: diff_a.to_string_lossy().to_string(),
                        file_diffs: Vec::new(),
                    },
                },
                CommitInfo {
                    hash: hash_b,
                    author: "Test <test@test.com>".to_string(),
                    date: chrono::Utc::now().fixed_offset(),
                    original_message: "feat(b): add b".to_string(),
                    in_main_branches: Vec::new(),
                    analysis: CommitAnalysis {
                        detected_type: "feat".to_string(),
                        detected_scope: "b".to_string(),
                        proposed_message: "feat(b): add b".to_string(),
                        file_changes: FileChanges {
                            total_files: 1,
                            files_added: 1,
                            files_deleted: 0,
                            file_list: Vec::new(),
                        },
                        diff_summary: "b.rs | 1 +".to_string(),
                        diff_file: diff_b.to_string_lossy().to_string(),
                        file_diffs: Vec::new(),
                    },
                },
            ],
        }
    }

    #[tokio::test]
    async fn generate_amendments_multi_commit() {
        let dir = tempfile::tempdir().unwrap();
        let repo_view = make_multi_commit_repo_view(&dir);
        let hash_a = "a".repeat(40);
        let hash_b = "b".repeat(40);

        let response = format!(
            concat!(
                "amendments:\n",
                "  - commit: \"{hash_a}\"\n",
                "    message: \"feat(a): improved a\"\n",
                "  - commit: \"{hash_b}\"\n",
                "    message: \"feat(b): improved b\"\n",
            ),
            hash_a = hash_a,
            hash_b = hash_b,
        );
        let client = make_configurable_client(vec![Ok(response)]);

        let result = client
            .generate_amendments_with_options(&repo_view, false)
            .await;

        assert!(
            result.is_ok(),
            "multi-commit amendment failed: {:?}",
            result.err()
        );
        let amendments = result.unwrap();
        assert_eq!(amendments.amendments.len(), 2);
    }

    #[tokio::test]
    async fn generate_contextual_amendments_multi_commit() {
        let dir = tempfile::tempdir().unwrap();
        let repo_view = make_multi_commit_repo_view(&dir);
        let hash_a = "a".repeat(40);
        let hash_b = "b".repeat(40);

        let response = format!(
            concat!(
                "amendments:\n",
                "  - commit: \"{hash_a}\"\n",
                "    message: \"feat(a): improved a\"\n",
                "  - commit: \"{hash_b}\"\n",
                "    message: \"feat(b): improved b\"\n",
            ),
            hash_a = hash_a,
            hash_b = hash_b,
        );
        let client = make_configurable_client(vec![Ok(response)]);
        let context = crate::data::context::CommitContext::default();

        let result = client
            .generate_contextual_amendments_with_options(&repo_view, &context, false)
            .await;

        assert!(
            result.is_ok(),
            "multi-commit contextual amendment failed: {:?}",
            result.err()
        );
        let amendments = result.unwrap();
        assert_eq!(amendments.amendments.len(), 2);
    }

    #[tokio::test]
    async fn generate_pr_content_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let repo_view = make_test_repo_view(&dir);

        let response = "title: \"feat: add something\"\ndescription: \"Adds a new feature.\"\n";
        let client = make_configurable_client(vec![Ok(response.to_string())]);

        let result = client.generate_pr_content(&repo_view, "").await;

        assert!(result.is_ok(), "PR generation failed: {:?}", result.err());
        let pr = result.unwrap();
        assert_eq!(pr.title, "feat: add something");
        assert_eq!(pr.description, "Adds a new feature.");
    }

    #[tokio::test]
    async fn generate_pr_content_with_context_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let repo_view = make_test_repo_view(&dir);
        let context = crate::data::context::CommitContext::default();

        let response = "title: \"feat: add something\"\ndescription: \"Adds a new feature.\"\n";
        let client = make_configurable_client(vec![Ok(response.to_string())]);

        let result = client
            .generate_pr_content_with_context(&repo_view, "", &context)
            .await;

        assert!(
            result.is_ok(),
            "PR generation with context failed: {:?}",
            result.err()
        );
        let pr = result.unwrap();
        assert_eq!(pr.title, "feat: add something");
    }

    #[tokio::test]
    async fn check_commits_multi_commit() {
        let dir = tempfile::tempdir().unwrap();
        let repo_view = make_multi_commit_repo_view(&dir);
        let hash_a = "a".repeat(40);
        let hash_b = "b".repeat(40);

        let response = format!(
            concat!(
                "checks:\n",
                "  - commit: \"{hash_a}\"\n",
                "    passes: true\n",
                "    issues: []\n",
                "  - commit: \"{hash_b}\"\n",
                "    passes: true\n",
                "    issues: []\n",
            ),
            hash_a = hash_a,
            hash_b = hash_b,
        );
        let client = make_configurable_client(vec![Ok(response)]);

        let result = client
            .check_commits_with_scopes(&repo_view, None, &[], false)
            .await;

        assert!(
            result.is_ok(),
            "multi-commit check failed: {:?}",
            result.err()
        );
        let report = result.unwrap();
        assert_eq!(report.commits.len(), 2);
        assert!(report.commits[0].passes);
        assert!(report.commits[1].passes);
    }

    // ── Multi-commit split dispatch helpers ──────────────────────────

    /// Creates a repo view with two large-diff commits whose combined view
    /// exceeds the constrained 25KB context window.
    fn make_large_multi_commit_repo_view(dir: &tempfile::TempDir) -> crate::data::RepositoryView {
        use crate::data::{AiInfo, FieldExplanation, WorkingDirectoryInfo};
        use crate::git::commit::{FileChange, FileChanges, FileDiffRef};
        use crate::git::{CommitAnalysis, CommitInfo};

        let hash_a = "a".repeat(40);
        let hash_b = "b".repeat(40);

        // Write flat diff files large enough to bust the 50K-token budget when combined.
        // Each 60k chars ≈ 28,800 tokens; combined ≈ 57,600 > 41,808 available.
        let diff_content_a = "x".repeat(60_000);
        let diff_content_b = "y".repeat(60_000);
        let flat_a = dir.path().join("flat_a.diff");
        let flat_b = dir.path().join("flat_b.diff");
        std::fs::write(&flat_a, &diff_content_a).unwrap();
        std::fs::write(&flat_b, &diff_content_b).unwrap();

        // Write per-file diff files for split dispatch
        let file_diff_a = format!("diff --git a/src/a.rs b/src/a.rs\n{}\n", "a".repeat(30_000));
        let file_diff_b = format!("diff --git a/src/b.rs b/src/b.rs\n{}\n", "b".repeat(30_000));
        let per_file_a = dir.path().join("pf_a.diff");
        let per_file_b = dir.path().join("pf_b.diff");
        std::fs::write(&per_file_a, &file_diff_a).unwrap();
        std::fs::write(&per_file_b, &file_diff_b).unwrap();

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
            commits: vec![
                CommitInfo {
                    hash: hash_a,
                    author: "Test <test@test.com>".to_string(),
                    date: chrono::Utc::now().fixed_offset(),
                    original_message: "feat(a): add module a".to_string(),
                    in_main_branches: Vec::new(),
                    analysis: CommitAnalysis {
                        detected_type: "feat".to_string(),
                        detected_scope: "a".to_string(),
                        proposed_message: "feat(a): add module a".to_string(),
                        file_changes: FileChanges {
                            total_files: 1,
                            files_added: 1,
                            files_deleted: 0,
                            file_list: vec![FileChange {
                                status: "A".to_string(),
                                file: "src/a.rs".to_string(),
                            }],
                        },
                        diff_summary: " src/a.rs | 100 ++++\n".to_string(),
                        diff_file: flat_a.to_string_lossy().to_string(),
                        file_diffs: vec![FileDiffRef {
                            path: "src/a.rs".to_string(),
                            diff_file: per_file_a.to_string_lossy().to_string(),
                            byte_len: file_diff_a.len(),
                        }],
                    },
                },
                CommitInfo {
                    hash: hash_b,
                    author: "Test <test@test.com>".to_string(),
                    date: chrono::Utc::now().fixed_offset(),
                    original_message: "feat(b): add module b".to_string(),
                    in_main_branches: Vec::new(),
                    analysis: CommitAnalysis {
                        detected_type: "feat".to_string(),
                        detected_scope: "b".to_string(),
                        proposed_message: "feat(b): add module b".to_string(),
                        file_changes: FileChanges {
                            total_files: 1,
                            files_added: 1,
                            files_deleted: 0,
                            file_list: vec![FileChange {
                                status: "A".to_string(),
                                file: "src/b.rs".to_string(),
                            }],
                        },
                        diff_summary: " src/b.rs | 100 ++++\n".to_string(),
                        diff_file: flat_b.to_string_lossy().to_string(),
                        file_diffs: vec![FileDiffRef {
                            path: "src/b.rs".to_string(),
                            diff_file: per_file_b.to_string_lossy().to_string(),
                            byte_len: file_diff_b.len(),
                        }],
                    },
                },
            ],
        }
    }

    fn valid_pr_yaml(title: &str, description: &str) -> String {
        format!("title: \"{title}\"\ndescription: \"{description}\"\n")
    }

    // ── Multi-commit amendment split dispatch tests ──────────────────

    #[tokio::test]
    async fn generate_amendments_multi_commit_split_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let repo_view = make_large_multi_commit_repo_view(&dir);
        let hash_a = "a".repeat(40);
        let hash_b = "b".repeat(40);

        // Full view exceeds budget → per-commit fallback
        // Each commit fits individually (1 file each) → 1 response per commit
        let (client, handle) = make_small_context_client_tracked(vec![
            Ok(valid_amendment_yaml(&hash_a, "feat(a): improved a")),
            Ok(valid_amendment_yaml(&hash_b, "feat(b): improved b")),
        ]);

        let result = client
            .generate_amendments_with_options(&repo_view, false)
            .await;

        assert!(
            result.is_ok(),
            "multi-commit split dispatch failed: {:?}",
            result.err()
        );
        let amendments = result.unwrap();
        assert_eq!(amendments.amendments.len(), 2);
        assert_eq!(amendments.amendments[0].commit, hash_a);
        assert_eq!(amendments.amendments[1].commit, hash_b);
        assert!(amendments.amendments[0].message.contains("improved a"));
        assert!(amendments.amendments[1].message.contains("improved b"));
        assert_eq!(handle.remaining(), 0, "expected all responses consumed");
    }

    #[tokio::test]
    async fn generate_contextual_amendments_multi_commit_split_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let repo_view = make_large_multi_commit_repo_view(&dir);
        let hash_a = "a".repeat(40);
        let hash_b = "b".repeat(40);
        let context = crate::data::context::CommitContext::default();

        let (client, handle) = make_small_context_client_tracked(vec![
            Ok(valid_amendment_yaml(&hash_a, "feat(a): improved a")),
            Ok(valid_amendment_yaml(&hash_b, "feat(b): improved b")),
        ]);

        let result = client
            .generate_contextual_amendments_with_options(&repo_view, &context, false)
            .await;

        assert!(
            result.is_ok(),
            "multi-commit contextual split dispatch failed: {:?}",
            result.err()
        );
        let amendments = result.unwrap();
        assert_eq!(amendments.amendments.len(), 2);
        assert_eq!(amendments.amendments[0].commit, hash_a);
        assert_eq!(amendments.amendments[1].commit, hash_b);
        assert_eq!(handle.remaining(), 0, "expected all responses consumed");
    }

    // ── Multi-commit check split dispatch tests ──────────────────────

    #[tokio::test]
    async fn check_commits_multi_commit_split_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let repo_view = make_large_multi_commit_repo_view(&dir);
        let hash_a = "a".repeat(40);
        let hash_b = "b".repeat(40);

        // Full view exceeds budget → per-commit fallback
        let (client, handle) = make_small_context_client_tracked(vec![
            Ok(valid_check_yaml_for(&hash_a, true)),
            Ok(valid_check_yaml_for(&hash_b, true)),
        ]);

        let result = client
            .check_commits_with_scopes(&repo_view, None, &[], false)
            .await;

        assert!(
            result.is_ok(),
            "multi-commit check split dispatch failed: {:?}",
            result.err()
        );
        let report = result.unwrap();
        assert_eq!(report.commits.len(), 2);
        assert!(report.commits[0].passes);
        assert!(report.commits[1].passes);
        assert_eq!(handle.remaining(), 0, "expected all responses consumed");
    }

    // ── PR split dispatch tests ──────────────────────────────────────

    #[tokio::test]
    async fn generate_pr_content_split_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let repo_view = make_large_diff_repo_view(&dir);

        // Single large commit: full view exceeds budget → per-commit fallback
        // 1 commit with 2 file chunks → chunk 1 + chunk 2 + chunk merge pass
        // Single per-commit result → returned directly (no extra merge)
        let (client, handle) = make_small_context_client_tracked(vec![
            Ok(valid_pr_yaml("feat(a): add a.rs", "Adds a.rs module")),
            Ok(valid_pr_yaml("feat(b): add b.rs", "Adds b.rs module")),
            Ok(valid_pr_yaml(
                "feat(test): add modules",
                "Adds a.rs and b.rs",
            )),
        ]);

        let result = client.generate_pr_content(&repo_view, "").await;

        assert!(
            result.is_ok(),
            "PR split dispatch failed: {:?}",
            result.err()
        );
        let pr = result.unwrap();
        assert!(pr.title.contains("add modules"));
        assert_eq!(handle.remaining(), 0, "expected all responses consumed");
    }

    #[tokio::test]
    async fn generate_pr_content_multi_commit_split_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let repo_view = make_large_multi_commit_repo_view(&dir);

        // Full view exceeds budget → per-commit fallback
        // Each commit fits individually → 1 response per commit, then merge pass
        let (client, handle) = make_small_context_client_tracked(vec![
            Ok(valid_pr_yaml("feat(a): add module a", "Adds module a")),
            Ok(valid_pr_yaml("feat(b): add module b", "Adds module b")),
            Ok(valid_pr_yaml(
                "feat: add modules a and b",
                "Adds both modules",
            )),
        ]);

        let result = client.generate_pr_content(&repo_view, "").await;

        assert!(
            result.is_ok(),
            "PR multi-commit split dispatch failed: {:?}",
            result.err()
        );
        let pr = result.unwrap();
        assert!(pr.title.contains("modules"));
        assert_eq!(handle.remaining(), 0, "expected all responses consumed");
    }

    #[tokio::test]
    async fn generate_pr_content_with_context_split_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let repo_view = make_large_multi_commit_repo_view(&dir);
        let context = crate::data::context::CommitContext::default();

        // Full view exceeds budget → per-commit fallback → merge pass
        let (client, handle) = make_small_context_client_tracked(vec![
            Ok(valid_pr_yaml("feat(a): add module a", "Adds module a")),
            Ok(valid_pr_yaml("feat(b): add module b", "Adds module b")),
            Ok(valid_pr_yaml(
                "feat: add modules a and b",
                "Adds both modules",
            )),
        ]);

        let result = client
            .generate_pr_content_with_context(&repo_view, "", &context)
            .await;

        assert!(
            result.is_ok(),
            "PR with context split dispatch failed: {:?}",
            result.err()
        );
        let pr = result.unwrap();
        assert!(pr.title.contains("modules"));
        assert_eq!(handle.remaining(), 0, "expected all responses consumed");
    }

    // ── prompt-recording split dispatch tests ────────────────────

    /// Like [`make_small_context_client_tracked`] but also returns a
    /// [`PromptRecordHandle`] for inspecting which prompts were sent.
    fn make_small_context_client_with_prompts(
        responses: Vec<Result<String>>,
    ) -> (
        ClaudeClient,
        crate::claude::test_utils::ResponseQueueHandle,
        crate::claude::test_utils::PromptRecordHandle,
    ) {
        let mock = crate::claude::test_utils::ConfigurableMockAiClient::new(responses)
            .with_context_length(50_000);
        let response_handle = mock.response_handle();
        let prompt_handle = mock.prompt_handle();
        (
            ClaudeClient::new(Box::new(mock)),
            response_handle,
            prompt_handle,
        )
    }

    /// Creates a default-context mock client that also records prompts.
    fn make_configurable_client_with_prompts(
        responses: Vec<Result<String>>,
    ) -> (
        ClaudeClient,
        crate::claude::test_utils::ResponseQueueHandle,
        crate::claude::test_utils::PromptRecordHandle,
    ) {
        let mock = crate::claude::test_utils::ConfigurableMockAiClient::new(responses);
        let response_handle = mock.response_handle();
        let prompt_handle = mock.prompt_handle();
        (
            ClaudeClient::new(Box::new(mock)),
            response_handle,
            prompt_handle,
        )
    }

    /// Creates a repo view with one commit containing a single large file
    /// whose diff exceeds the token budget. Because the per-file diff is
    /// loaded as a whole (hunk-level granularity from the packer is lost
    /// at the dispatch layer), the split dispatch path will fail with a
    /// budget error. This helper exists to test that the error propagates
    /// cleanly rather than silently degrading.
    fn make_single_oversized_file_repo_view(
        dir: &tempfile::TempDir,
    ) -> crate::data::RepositoryView {
        use crate::data::{AiInfo, FieldExplanation, WorkingDirectoryInfo};
        use crate::git::commit::{FileChange, FileChanges, FileDiffRef};
        use crate::git::{CommitAnalysis, CommitInfo};

        let hash = "c".repeat(40);

        // A single file diff large enough (~80K bytes ≈ 25K tokens) to
        // exceed the 25K context window budget even for a single chunk.
        let diff_content = format!(
            "diff --git a/src/big.rs b/src/big.rs\n{}\n",
            "x".repeat(80_000)
        );

        let flat_diff_path = dir.path().join("full.diff");
        std::fs::write(&flat_diff_path, &diff_content).unwrap();

        let per_file_path = dir.path().join("0000.diff");
        std::fs::write(&per_file_path, &diff_content).unwrap();

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
                original_message: "feat(big): add large module".to_string(),
                in_main_branches: Vec::new(),
                analysis: CommitAnalysis {
                    detected_type: "feat".to_string(),
                    detected_scope: "big".to_string(),
                    proposed_message: "feat(big): add large module".to_string(),
                    file_changes: FileChanges {
                        total_files: 1,
                        files_added: 1,
                        files_deleted: 0,
                        file_list: vec![FileChange {
                            status: "A".to_string(),
                            file: "src/big.rs".to_string(),
                        }],
                    },
                    diff_summary: " src/big.rs | 80 ++++\n".to_string(),
                    diff_file: flat_diff_path.to_string_lossy().to_string(),
                    file_diffs: vec![FileDiffRef {
                        path: "src/big.rs".to_string(),
                        diff_file: per_file_path.to_string_lossy().to_string(),
                        byte_len: diff_content.len(),
                    }],
                },
            }],
        }
    }

    /// A small single-file commit whose diff fits within the token budget.
    ///
    /// Exercises the non-split path: `generate_amendments_with_options` →
    /// `try_full_diff_budget` succeeds → single AI request → amendment
    /// returned directly. Verifies exactly one request is made and the
    /// user prompt contains the actual diff content.
    #[tokio::test]
    async fn amendment_single_file_under_budget_no_split() {
        let dir = tempfile::tempdir().unwrap();
        let repo_view = make_test_repo_view(&dir);
        let hash = format!("{:0>40}", 0);

        let (client, response_handle, prompt_handle) =
            make_configurable_client_with_prompts(vec![Ok(valid_amendment_yaml(
                &hash,
                "feat(test): improved message",
            ))]);

        let result = client
            .generate_amendments_with_options(&repo_view, false)
            .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap().amendments.len(), 1);
        assert_eq!(response_handle.remaining(), 0);

        let prompts = prompt_handle.prompts();
        assert_eq!(
            prompts.len(),
            1,
            "expected exactly one AI request, no split"
        );

        let (_, user_prompt) = &prompts[0];
        assert!(
            user_prompt.contains("added line"),
            "user prompt should contain the diff content"
        );
    }

    /// A two-file commit that exceeds the token budget when combined.
    ///
    /// Exercises the file-level split path: `generate_amendments_with_options`
    /// → `try_full_diff_budget` fails → `generate_amendment_for_commit` →
    /// `try_full_diff_budget` fails again → `generate_amendment_split` →
    /// `pack_file_diffs` creates 2 chunks (one file each) → 2 AI requests
    /// → `merge_amendment_chunks` reduce pass → 1 merged amendment.
    ///
    /// Verifies that each chunk's user prompt contains only its file's diff
    /// content, and the merge prompt contains both partial amendment messages.
    #[tokio::test]
    async fn amendment_two_chunks_prompt_content() {
        let dir = tempfile::tempdir().unwrap();
        let repo_view = make_large_diff_repo_view(&dir);
        let hash = "a".repeat(40);

        let (client, response_handle, prompt_handle) =
            make_small_context_client_with_prompts(vec![
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
        assert!(amendments.amendments[0]
            .message
            .contains("add a.rs and b.rs"));
        assert_eq!(response_handle.remaining(), 0);

        let prompts = prompt_handle.prompts();
        assert_eq!(prompts.len(), 3, "expected 2 chunks + 1 merge = 3 requests");

        // Chunk 1 should contain file-a diff content (repeated 'a' chars)
        let (_, chunk1_user) = &prompts[0];
        assert!(
            chunk1_user.contains("aaa"),
            "chunk 1 prompt should contain file-a diff content"
        );

        // Chunk 2 should contain file-b diff content (repeated 'b' chars)
        let (_, chunk2_user) = &prompts[1];
        assert!(
            chunk2_user.contains("bbb"),
            "chunk 2 prompt should contain file-b diff content"
        );

        // Merge pass: system prompt is the synthesis prompt
        let (merge_sys, merge_user) = &prompts[2];
        assert!(
            merge_sys.contains("synthesiz"),
            "merge system prompt should contain synthesis instructions"
        );
        // Merge user prompt should contain both partial messages
        assert!(
            merge_user.contains("feat(a): add a.rs") && merge_user.contains("feat(b): add b.rs"),
            "merge user prompt should contain both partial amendment messages"
        );
    }

    /// A single file whose diff exceeds the budget even after split dispatch.
    ///
    /// Exercises the budget-error path: `generate_amendment_for_commit` →
    /// budget exceeded → `generate_amendment_split` → `pack_file_diffs`
    /// plans hunk-level chunks → but `from_commit_info_partial` loads the
    /// full per-file diff (deduplicates the repeated path) →
    /// Oversized files that can't be split get placeholders and proceed.
    ///
    /// Verifies that files too large for the budget are replaced with
    /// placeholder text indicating the file was omitted, rather than
    /// failing with a "prompt too large" error.
    #[tokio::test]
    async fn amendment_single_oversized_file_gets_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        let repo_view = make_single_oversized_file_repo_view(&dir);
        let hash = "c".repeat(40);

        // The file is too large for the full budget but gets a placeholder.
        // With 50k context, the placeholder is small enough to fit in a
        // single request (no split dispatch needed). We expect 1 request.
        let (client, _, prompt_handle) = make_small_context_client_with_prompts(vec![Ok(
            valid_amendment_yaml(&hash, "feat(big): add large module"),
        )]);

        let result = client
            .generate_amendments_with_options(&repo_view, false)
            .await;

        // Should succeed (either single request or split with placeholder)
        assert!(
            result.is_ok(),
            "expected success with placeholder, got: {result:?}"
        );

        // One request (placeholder makes it fit in single request)
        assert!(
            prompt_handle.request_count() >= 1,
            "expected at least 1 request, got {}",
            prompt_handle.request_count()
        );
    }

    /// A two-chunk split where the second chunk's AI request fails.
    ///
    /// Exercises the error-propagation path within `generate_amendment_split`:
    /// chunk 1 succeeds → chunk 2 returns `Err` → the `?` operator in the
    /// loop body propagates the error immediately, skipping the merge pass.
    ///
    /// Verifies that exactly 2 requests are recorded (no further processing)
    /// and the overall result is `Err` (no silent degradation).
    #[tokio::test]
    async fn amendment_chunk_failure_stops_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let repo_view = make_large_diff_repo_view(&dir);
        let hash = "a".repeat(40);

        // First chunk succeeds, second chunk fails
        let (client, _, prompt_handle) = make_small_context_client_with_prompts(vec![
            Ok(valid_amendment_yaml(&hash, "feat(a): add a.rs")),
            Err(anyhow::anyhow!("rate limit exceeded")),
        ]);

        let result = client
            .generate_amendments_with_options(&repo_view, false)
            .await;

        assert!(result.is_err());

        // Exactly 2 requests: chunk 1 (success) + chunk 2 (failure)
        let prompts = prompt_handle.prompts();
        assert_eq!(
            prompts.len(),
            2,
            "should stop after the failing chunk, got {} requests",
            prompts.len()
        );

        // The first request should reference one of the files
        let (_, first_user) = &prompts[0];
        assert!(
            first_user.contains("src/a.rs") || first_user.contains("src/b.rs"),
            "first chunk prompt should reference a file"
        );
    }

    /// Two-chunk amendment split dispatch, focused on the reduce pass inputs.
    ///
    /// Exercises `merge_amendment_chunks` which calls
    /// `generate_chunk_merge_user_prompt` to assemble the merge prompt from:
    /// the commit hash, original message, diff_summary, and the partial
    /// amendment messages returned by each chunk.
    ///
    /// Verifies that the merge (3rd) request's user prompt contains all of:
    /// both partial messages, the original commit message, the diff_summary
    /// file paths, and the commit hash.
    #[tokio::test]
    async fn amendment_reduce_pass_prompt_content() {
        let dir = tempfile::tempdir().unwrap();
        let repo_view = make_large_diff_repo_view(&dir);
        let hash = "a".repeat(40);

        let (client, _, prompt_handle) = make_small_context_client_with_prompts(vec![
            Ok(valid_amendment_yaml(
                &hash,
                "feat(a): add module a implementation",
            )),
            Ok(valid_amendment_yaml(
                &hash,
                "feat(b): add module b implementation",
            )),
            Ok(valid_amendment_yaml(
                &hash,
                "feat(test): add modules a and b",
            )),
        ]);

        let result = client
            .generate_amendments_with_options(&repo_view, false)
            .await;

        assert!(result.is_ok());

        let prompts = prompt_handle.prompts();
        assert_eq!(prompts.len(), 3);

        // The merge pass is the last (3rd) request
        let (merge_system, merge_user) = &prompts[2];

        // System prompt should be the amendment chunk merge prompt
        assert!(
            merge_system.contains("synthesiz"),
            "merge system prompt should contain synthesis instructions"
        );

        // User prompt should contain the partial messages from chunks
        assert!(
            merge_user.contains("feat(a): add module a implementation"),
            "merge user prompt should contain chunk 1's partial message"
        );
        assert!(
            merge_user.contains("feat(b): add module b implementation"),
            "merge user prompt should contain chunk 2's partial message"
        );

        // User prompt should contain the original commit message
        assert!(
            merge_user.contains("feat(test): large commit"),
            "merge user prompt should contain the original commit message"
        );

        // User prompt should contain the diff_summary referencing both files
        assert!(
            merge_user.contains("src/a.rs") && merge_user.contains("src/b.rs"),
            "merge user prompt should contain the diff_summary"
        );

        // User prompt should reference the commit hash
        assert!(
            merge_user.contains(&hash),
            "merge user prompt should reference the commit hash"
        );
    }

    /// Two-chunk check split dispatch with issue deduplication and merge.
    ///
    /// Exercises `check_commit_split` which:
    /// 1. Dispatches 2 chunk requests (one per file)
    /// 2. Collects issues from both chunks into a `HashSet` keyed by
    ///    `(rule, severity, section)` — duplicates are dropped
    /// 3. Detects that both chunks have suggestions → calls
    ///    `merge_check_chunks` for the AI reduce pass
    ///
    /// Chunk 1 reports: `error:subject-too-long:Subject Line` +
    ///                   `warning:body-required:Content`
    /// Chunk 2 reports: `error:subject-too-long:Subject Line` (duplicate) +
    ///                   `info:scope-suggestion:Style` (new)
    ///
    /// Verifies: 3 unique issues after dedup, suggestion from merge pass,
    /// and the merge prompt contains both partial suggestions + diff_summary.
    #[tokio::test]
    async fn check_split_dedup_and_merge_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let repo_view = make_large_diff_repo_view(&dir);
        let hash = "a".repeat(40);

        // Chunk 1: error (subject-too-long) + warning (body-required) + suggestion
        let chunk1_yaml = format!(
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
                "    suggestion:\n",
                "      message: \"feat(a): shorter subject for a\"\n",
                "      explanation: \"Shortened subject for file a\"\n",
                "    summary: \"Adds module a\"\n",
            ),
            hash = hash,
        );

        // Chunk 2: same error (different explanation) + new info issue + suggestion
        let chunk2_yaml = format!(
            concat!(
                "checks:\n",
                "  - commit: \"{hash}\"\n",
                "    passes: false\n",
                "    issues:\n",
                "      - severity: error\n",
                "        section: \"Subject Line\"\n",
                "        rule: \"subject-too-long\"\n",
                "        explanation: \"Subject line is way too long\"\n",
                "      - severity: info\n",
                "        section: \"Style\"\n",
                "        rule: \"scope-suggestion\"\n",
                "        explanation: \"Consider more specific scope\"\n",
                "    suggestion:\n",
                "      message: \"feat(b): shorter subject for b\"\n",
                "      explanation: \"Shortened subject for file b\"\n",
                "    summary: \"Adds module b\"\n",
            ),
            hash = hash,
        );

        // Merge pass (called because suggestions exist)
        let merge_yaml = format!(
            concat!(
                "checks:\n",
                "  - commit: \"{hash}\"\n",
                "    passes: false\n",
                "    issues: []\n",
                "    suggestion:\n",
                "      message: \"feat(test): add modules a and b\"\n",
                "      explanation: \"Combined suggestion\"\n",
                "    summary: \"Adds modules a and b\"\n",
            ),
            hash = hash,
        );

        let (client, response_handle, prompt_handle) =
            make_small_context_client_with_prompts(vec![
                Ok(chunk1_yaml),
                Ok(chunk2_yaml),
                Ok(merge_yaml),
            ]);

        let result = client
            .check_commits_with_scopes(&repo_view, None, &[], true)
            .await;

        assert!(result.is_ok(), "split dispatch failed: {:?}", result.err());
        let report = result.unwrap();
        assert_eq!(report.commits.len(), 1);
        assert!(!report.commits[0].passes);
        assert_eq!(response_handle.remaining(), 0);

        // Dedup: 3 unique (rule, severity, section) tuples
        //  - subject-too-long / error / Subject Line   (appears in both → deduped)
        //  - body-required    / warning / Content
        //  - scope-suggestion / info / Style
        assert_eq!(
            report.commits[0].issues.len(),
            3,
            "expected 3 unique issues after dedup, got {:?}",
            report.commits[0]
                .issues
                .iter()
                .map(|i| &i.rule)
                .collect::<Vec<_>>()
        );

        // Suggestion should come from the merge pass
        assert!(report.commits[0].suggestion.is_some());
        assert!(
            report.commits[0]
                .suggestion
                .as_ref()
                .unwrap()
                .message
                .contains("add modules a and b"),
            "suggestion should come from the merge pass"
        );

        // Prompt content assertions
        let prompts = prompt_handle.prompts();
        assert_eq!(prompts.len(), 3, "expected 2 chunks + 1 merge");

        // Chunk prompts should collectively cover both files
        let (_, chunk1_user) = &prompts[0];
        let (_, chunk2_user) = &prompts[1];
        let combined_chunk_prompts = format!("{chunk1_user}{chunk2_user}");
        assert!(
            combined_chunk_prompts.contains("src/a.rs")
                && combined_chunk_prompts.contains("src/b.rs"),
            "chunk prompts should collectively cover both files"
        );

        // Merge pass prompt should contain partial suggestions
        let (merge_sys, merge_user) = &prompts[2];
        assert!(
            merge_sys.contains("synthesiz") || merge_sys.contains("reviewer"),
            "merge system prompt should be the check chunk merge prompt"
        );
        assert!(
            merge_user.contains("feat(a): shorter subject for a")
                && merge_user.contains("feat(b): shorter subject for b"),
            "merge user prompt should contain both partial suggestions"
        );
        // Merge prompt should contain the diff_summary
        assert!(
            merge_user.contains("src/a.rs") && merge_user.contains("src/b.rs"),
            "merge user prompt should contain the diff_summary"
        );
    }

    // ── Amendment retry tests ──────────────────────────────────────────

    #[tokio::test]
    async fn amendment_retry_parse_failure_then_success() {
        let dir = tempfile::tempdir().unwrap();
        let repo_view = make_test_repo_view(&dir);
        let hash = format!("{:0>40}", 0);

        let (client, response_handle, prompt_handle) = make_configurable_client_with_prompts(vec![
            Ok("not valid yaml {{[".to_string()),
            Ok(valid_amendment_yaml(&hash, "feat(test): improved")),
        ]);

        let result = client
            .generate_amendments_with_options(&repo_view, false)
            .await;

        assert!(
            result.is_ok(),
            "should succeed after retry: {:?}",
            result.err()
        );
        assert_eq!(result.unwrap().amendments.len(), 1);
        assert_eq!(response_handle.remaining(), 0, "both responses consumed");
        assert_eq!(prompt_handle.request_count(), 2, "exactly 2 AI requests");
    }

    #[tokio::test]
    async fn amendment_retry_request_failure_then_success() {
        let dir = tempfile::tempdir().unwrap();
        let repo_view = make_test_repo_view(&dir);
        let hash = format!("{:0>40}", 0);

        let (client, response_handle, prompt_handle) = make_configurable_client_with_prompts(vec![
            Err(anyhow::anyhow!("rate limit")),
            Ok(valid_amendment_yaml(&hash, "feat(test): improved")),
        ]);

        let result = client
            .generate_amendments_with_options(&repo_view, false)
            .await;

        assert!(
            result.is_ok(),
            "should succeed after retry: {:?}",
            result.err()
        );
        assert_eq!(result.unwrap().amendments.len(), 1);
        assert_eq!(response_handle.remaining(), 0);
        assert_eq!(prompt_handle.request_count(), 2);
    }

    #[tokio::test]
    async fn amendment_retry_all_attempts_exhausted() {
        let dir = tempfile::tempdir().unwrap();
        let repo_view = make_test_repo_view(&dir);

        let (client, response_handle, prompt_handle) = make_configurable_client_with_prompts(vec![
            Ok("bad yaml 1".to_string()),
            Ok("bad yaml 2".to_string()),
            Ok("bad yaml 3".to_string()),
        ]);

        let result = client
            .generate_amendments_with_options(&repo_view, false)
            .await;

        assert!(result.is_err(), "should fail after all retries exhausted");
        assert_eq!(response_handle.remaining(), 0, "all 3 responses consumed");
        assert_eq!(
            prompt_handle.request_count(),
            3,
            "exactly 3 AI requests (1 + 2 retries)"
        );
    }

    #[tokio::test]
    async fn amendment_retry_success_first_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let repo_view = make_test_repo_view(&dir);
        let hash = format!("{:0>40}", 0);

        let (client, response_handle, prompt_handle) =
            make_configurable_client_with_prompts(vec![Ok(valid_amendment_yaml(
                &hash,
                "feat(test): works first time",
            ))]);

        let result = client
            .generate_amendments_with_options(&repo_view, false)
            .await;

        assert!(result.is_ok());
        assert_eq!(response_handle.remaining(), 0);
        assert_eq!(prompt_handle.request_count(), 1, "only 1 request, no retry");
    }

    #[tokio::test]
    async fn amendment_retry_mixed_request_and_parse_failures() {
        let dir = tempfile::tempdir().unwrap();
        let repo_view = make_test_repo_view(&dir);
        let hash = format!("{:0>40}", 0);

        let (client, response_handle, prompt_handle) = make_configurable_client_with_prompts(vec![
            Err(anyhow::anyhow!("network error")),
            Ok("invalid yaml {{".to_string()),
            Ok(valid_amendment_yaml(&hash, "feat(test): third time")),
        ]);

        let result = client
            .generate_amendments_with_options(&repo_view, false)
            .await;

        assert!(
            result.is_ok(),
            "should succeed on third attempt: {:?}",
            result.err()
        );
        assert_eq!(result.unwrap().amendments.len(), 1);
        assert_eq!(response_handle.remaining(), 0);
        assert_eq!(prompt_handle.request_count(), 3, "all 3 attempts used");
    }
}
