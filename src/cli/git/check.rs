//! Check command — validates commit messages against guidelines.

use anyhow::{Context, Result};
use clap::Parser;

use super::parse_beta_header;

/// Check command options - validates commit messages against guidelines.
#[derive(Parser)]
pub struct CheckCommand {
    /// Commit range to check (e.g., HEAD~3..HEAD, abc123..def456).
    /// Defaults to commits ahead of main branch.
    #[arg(value_name = "COMMIT_RANGE")]
    pub commit_range: Option<String>,

    /// Claude API model to use (if not specified, uses settings or default).
    #[arg(long)]
    pub model: Option<String>,

    /// Beta header to send with API requests (format: key:value).
    /// Only sent if the model supports it in the registry.
    #[arg(long, value_name = "KEY:VALUE")]
    pub beta_header: Option<String>,

    /// Path to custom context directory (defaults to .omni-dev/).
    #[arg(long)]
    pub context_dir: Option<std::path::PathBuf>,

    /// Explicit path to guidelines file.
    #[arg(long)]
    pub guidelines: Option<std::path::PathBuf>,

    /// Output format: text (default), json, yaml.
    #[arg(long, default_value = "text")]
    pub format: String,

    /// Exits with error code if any issues found (including warnings).
    #[arg(long)]
    pub strict: bool,

    /// Only shows errors/warnings, suppresses info-level output.
    #[arg(long)]
    pub quiet: bool,

    /// Shows detailed analysis including passing commits.
    #[arg(long)]
    pub verbose: bool,

    /// Includes passing commits in output (hidden by default).
    #[arg(long)]
    pub show_passing: bool,

    /// Deprecated: use --concurrency instead.
    #[arg(long, default_value = "4", hide = true)]
    pub batch_size: usize,

    /// Maximum number of concurrent AI requests (default: 4).
    #[arg(long, default_value = "4")]
    pub concurrency: usize,

    /// Disables the cross-commit coherence pass.
    #[arg(long)]
    pub no_coherence: bool,

    /// Skips generating corrected message suggestions.
    #[arg(long)]
    pub no_suggestions: bool,

    /// Offers to apply suggested messages when issues are found.
    #[arg(long)]
    pub twiddle: bool,
}

impl CheckCommand {
    /// Executes the check command, validating commit messages against guidelines.
    pub async fn execute(self) -> Result<()> {
        use crate::data::check::OutputFormat;

        // Parse output format
        let output_format: OutputFormat = self.format.parse().unwrap_or(OutputFormat::Text);

        // Preflight check: validate AI credentials before any processing
        let ai_info = crate::utils::check_ai_command_prerequisites(self.model.as_deref())?;
        if !self.quiet && output_format == OutputFormat::Text {
            println!(
                "✓ {} credentials verified (model: {})",
                ai_info.provider, ai_info.model
            );
        }

        if !self.quiet && output_format == OutputFormat::Text {
            println!("🔍 Checking commit messages against guidelines...");
        }

        // 1. Generate repository view to get all commits
        let mut repo_view = self.generate_repository_view().await?;

        // 2. Check for empty commit range (exit code 3)
        if repo_view.commits.is_empty() {
            eprintln!("error: no commits found in range");
            std::process::exit(3);
        }

        if !self.quiet && output_format == OutputFormat::Text {
            println!("📊 Found {} commits to check", repo_view.commits.len());
        }

        // 3. Load commit guidelines and scopes
        let guidelines = self.load_guidelines().await?;
        let valid_scopes = self.load_scopes();

        // Refine detected scopes using file_patterns from scope definitions
        for commit in &mut repo_view.commits {
            commit.analysis.refine_scope(&valid_scopes);
        }

        if !self.quiet && output_format == OutputFormat::Text {
            self.show_guidance_files_status(&guidelines, &valid_scopes);
        }

        // 4. Initialize Claude client
        let beta = self
            .beta_header
            .as_deref()
            .map(parse_beta_header)
            .transpose()?;
        let claude_client = crate::claude::create_default_claude_client(self.model.clone(), beta)?;

        if self.verbose && output_format == OutputFormat::Text {
            self.show_model_info(&claude_client)?;
        }

        // 5. Use parallel map-reduce for multiple commits, direct call for single
        let report = if repo_view.commits.len() > 1 {
            if !self.quiet && output_format == OutputFormat::Text {
                println!(
                    "🔄 Processing {} commits in parallel (concurrency: {})...",
                    repo_view.commits.len(),
                    self.concurrency
                );
            }
            self.check_with_map_reduce(
                &claude_client,
                &repo_view,
                guidelines.as_deref(),
                &valid_scopes,
            )
            .await?
        } else {
            // Single commit — direct call
            if !self.quiet && output_format == OutputFormat::Text {
                println!("🤖 Analyzing commits with AI...");
            }
            claude_client
                .check_commits_with_scopes(
                    &repo_view,
                    guidelines.as_deref(),
                    &valid_scopes,
                    !self.no_suggestions,
                )
                .await?
        };

        // 7. Output results
        self.output_report(&report, output_format)?;

        // 8. If --twiddle and there are errors with suggestions, offer to apply them
        if should_offer_twiddle(self.twiddle, report.has_errors(), output_format) {
            let amendments = self.build_amendments_from_suggestions(&report, &repo_view);
            if !amendments.is_empty() && self.prompt_and_apply_suggestions(amendments).await? {
                // Amendments applied — exit successfully
                return Ok(());
            }
        }

        // 9. Determine exit code
        let exit_code = report.exit_code(self.strict);
        if exit_code != 0 {
            std::process::exit(exit_code);
        }

        Ok(())
    }

    /// Generates the repository view (reuses logic from TwiddleCommand).
    async fn generate_repository_view(&self) -> Result<crate::data::RepositoryView> {
        use crate::data::{
            AiInfo, BranchInfo, FieldExplanation, FileStatusInfo, RepositoryView, VersionInfo,
            WorkingDirectoryInfo,
        };
        use crate::git::{GitRepository, RemoteInfo};
        use crate::utils::ai_scratch;

        // Open git repository
        let repo = GitRepository::open()
            .context("Failed to open git repository. Make sure you're in a git repository.")?;

        // Get current branch name
        let current_branch = repo
            .get_current_branch()
            .unwrap_or_else(|_| "HEAD".to_string());

        // Determine commit range
        let commit_range = if let Some(range) = &self.commit_range {
            range.clone()
        } else {
            // Default to commits ahead of main branch
            let base = if repo.branch_exists("main")? {
                "main"
            } else if repo.branch_exists("master")? {
                "master"
            } else {
                "HEAD~5"
            };
            format!("{base}..HEAD")
        };

        // Get working directory status
        let wd_status = repo.get_working_directory_status()?;
        let working_directory = WorkingDirectoryInfo {
            clean: wd_status.clean,
            untracked_changes: wd_status
                .untracked_changes
                .into_iter()
                .map(|fs| FileStatusInfo {
                    status: fs.status,
                    file: fs.file,
                })
                .collect(),
        };

        // Get remote information
        let remotes = RemoteInfo::get_all_remotes(repo.repository())?;

        // Parse commit range and get commits
        let commits = repo.get_commits_in_range(&commit_range)?;

        // Create version information
        let versions = Some(VersionInfo {
            omni_dev: env!("CARGO_PKG_VERSION").to_string(),
        });

        // Get AI scratch directory
        let ai_scratch_path =
            ai_scratch::get_ai_scratch_dir().context("Failed to determine AI scratch directory")?;
        let ai_info = AiInfo {
            scratch: ai_scratch_path.to_string_lossy().to_string(),
        };

        // Build repository view with branch info
        let mut repo_view = RepositoryView {
            versions,
            explanation: FieldExplanation::default(),
            working_directory,
            remotes,
            ai: ai_info,
            branch_info: Some(BranchInfo {
                branch: current_branch,
            }),
            pr_template: None,
            pr_template_location: None,
            branch_prs: None,
            commits,
        };

        // Update field presence based on actual data
        repo_view.update_field_presence();

        Ok(repo_view)
    }

    /// Loads commit guidelines from file or context directory.
    async fn load_guidelines(&self) -> Result<Option<String>> {
        // If explicit guidelines path is provided, use it
        if let Some(guidelines_path) = &self.guidelines {
            let content = std::fs::read_to_string(guidelines_path).with_context(|| {
                format!(
                    "Failed to read guidelines file: {}",
                    guidelines_path.display()
                )
            })?;
            return Ok(Some(content));
        }

        // Otherwise, use standard resolution chain
        let context_dir = crate::claude::context::resolve_context_dir(self.context_dir.as_deref());
        crate::claude::context::load_config_content(&context_dir, "commit-guidelines.md")
    }

    /// Loads valid scopes from context directory with ecosystem defaults.
    fn load_scopes(&self) -> Vec<crate::data::context::ScopeDefinition> {
        let context_dir = crate::claude::context::resolve_context_dir(self.context_dir.as_deref());
        crate::claude::context::load_project_scopes(&context_dir, &std::path::PathBuf::from("."))
    }

    /// Shows diagnostic information about loaded guidance files.
    fn show_guidance_files_status(
        &self,
        guidelines: &Option<String>,
        valid_scopes: &[crate::data::context::ScopeDefinition],
    ) {
        use crate::claude::context::{
            config_source_label, resolve_context_dir_with_source, ConfigSourceLabel,
        };

        let (context_dir, dir_source) =
            resolve_context_dir_with_source(self.context_dir.as_deref());

        println!("📋 Project guidance files status:");
        println!("   📂 Config dir: {} ({dir_source})", context_dir.display());

        // Check commit guidelines
        let guidelines_source = if guidelines.is_some() {
            match config_source_label(&context_dir, "commit-guidelines.md") {
                ConfigSourceLabel::NotFound => "✅ (source unknown)".to_string(),
                label => format!("✅ {label}"),
            }
        } else {
            "⚪ Using defaults".to_string()
        };
        println!("   📝 Commit guidelines: {guidelines_source}");

        // Check scopes
        let scopes_count = valid_scopes.len();
        let scopes_source = if scopes_count > 0 {
            match config_source_label(&context_dir, "scopes.yaml") {
                ConfigSourceLabel::NotFound => {
                    format!("✅ (source unknown) ({scopes_count} scopes)")
                }
                label => format!("✅ {label} ({scopes_count} scopes)"),
            }
        } else {
            "⚪ None found (any scope accepted)".to_string()
        };
        println!("   🎯 Valid scopes: {scopes_source}");

        println!();
    }

    /// Checks commits in parallel using batched map-reduce pattern.
    ///
    /// Groups commits into token-budget-aware batches, processes batches
    /// in parallel, then runs an optional coherence pass (skipped when
    /// all commits fit in a single batch).
    async fn check_with_map_reduce(
        &self,
        claude_client: &crate::claude::client::ClaudeClient,
        full_repo_view: &crate::data::RepositoryView,
        guidelines: Option<&str>,
        valid_scopes: &[crate::data::context::ScopeDefinition],
    ) -> Result<crate::data::check::CheckReport> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        use crate::claude::batch;
        use crate::claude::token_budget;
        use crate::data::check::{CheckReport, CommitCheckResult};

        let total_commits = full_repo_view.commits.len();

        // Plan batches based on token budget
        let metadata = claude_client.get_ai_client_metadata();
        let system_prompt = crate::claude::prompts::generate_check_system_prompt_with_scopes(
            guidelines,
            valid_scopes,
        );
        let system_prompt_tokens = token_budget::estimate_tokens(&system_prompt);
        let batch_plan =
            batch::plan_batches(&full_repo_view.commits, &metadata, system_prompt_tokens);

        if !self.quiet && batch_plan.batches.len() < total_commits {
            println!(
                "   📦 Grouped {} commits into {} batches by token budget",
                total_commits,
                batch_plan.batches.len()
            );
        }

        let semaphore = Arc::new(tokio::sync::Semaphore::new(self.concurrency));
        let completed = Arc::new(AtomicUsize::new(0));

        // Map phase: check batches in parallel
        let futs: Vec<_> = batch_plan
            .batches
            .iter()
            .map(|batch| {
                let sem = semaphore.clone();
                let completed = completed.clone();
                let batch_indices = &batch.commit_indices;

                async move {
                    let _permit = sem
                        .acquire()
                        .await
                        .map_err(|e| anyhow::anyhow!("semaphore closed: {e}"))?;

                    let batch_size = batch_indices.len();

                    // Create view for this batch
                    let batch_view = if batch_size == 1 {
                        full_repo_view.single_commit_view(&full_repo_view.commits[batch_indices[0]])
                    } else {
                        let commits: Vec<_> = batch_indices
                            .iter()
                            .map(|&i| &full_repo_view.commits[i])
                            .collect();
                        full_repo_view.multi_commit_view(&commits)
                    };

                    let result = claude_client
                        .check_commits_with_scopes(
                            &batch_view,
                            guidelines,
                            valid_scopes,
                            !self.no_suggestions,
                        )
                        .await;

                    match result {
                        Ok(report) => {
                            let done =
                                completed.fetch_add(batch_size, Ordering::Relaxed) + batch_size;
                            if !self.quiet {
                                println!("   ✅ {done}/{total_commits} commits checked");
                            }

                            let items: Vec<_> = report
                                .commits
                                .into_iter()
                                .map(|r| {
                                    let summary = r.summary.clone().unwrap_or_default();
                                    (r, summary)
                                })
                                .collect();
                            Ok::<_, anyhow::Error>((items, vec![]))
                        }
                        Err(e) if batch_size > 1 => {
                            // Split-and-retry: fall back to individual commits
                            eprintln!(
                                "warning: batch of {batch_size} failed, retrying individually: {e}"
                            );
                            let mut items = Vec::new();
                            let mut failed_indices = Vec::new();
                            for &idx in batch_indices {
                                let single_view =
                                    full_repo_view.single_commit_view(&full_repo_view.commits[idx]);
                                let single_result = claude_client
                                    .check_commits_with_scopes(
                                        &single_view,
                                        guidelines,
                                        valid_scopes,
                                        !self.no_suggestions,
                                    )
                                    .await;
                                let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
                                match single_result {
                                    Ok(report) => {
                                        if let Some(r) = report.commits.into_iter().next() {
                                            let summary = r.summary.clone().unwrap_or_default();
                                            items.push((r, summary));
                                        }
                                        if !self.quiet {
                                            println!("   ✅ {done}/{total_commits} commits checked");
                                        }
                                    }
                                    Err(e) => {
                                        eprintln!("warning: failed to check commit: {e}");
                                        failed_indices.push(idx);
                                        if !self.quiet {
                                            println!(
                                                "   ❌ {done}/{total_commits} commits checked (failed)"
                                            );
                                        }
                                    }
                                }
                            }
                            Ok((items, failed_indices))
                        }
                        Err(e) => {
                            // Single-commit batch failed; record the index so the user can retry
                            let idx = batch_indices[0];
                            eprintln!("warning: failed to check commit: {e}");
                            let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
                            if !self.quiet {
                                println!(
                                    "   ❌ {done}/{total_commits} commits checked (failed)"
                                );
                            }
                            Ok((vec![], vec![idx]))
                        }
                    }
                }
            })
            .collect();

        let results = futures::future::join_all(futs).await;

        // Flatten batch results
        let mut successes: Vec<(CommitCheckResult, String)> = Vec::new();
        let mut failed_indices: Vec<usize> = Vec::new();

        for result in results {
            match result {
                Ok((items, failed)) => {
                    successes.extend(items);
                    failed_indices.extend(failed);
                }
                Err(e) => {
                    // Semaphore errors: can't identify which commits were affected
                    eprintln!("warning: batch processing error: {e}");
                }
            }
        }

        // Offer interactive retry for commits that failed
        if !failed_indices.is_empty() && !self.quiet {
            use std::io::Write as _;
            println!("\n⚠️  {} commit(s) failed to check:", failed_indices.len());
            for &idx in &failed_indices {
                let commit = &full_repo_view.commits[idx];
                let subject = commit
                    .original_message
                    .lines()
                    .next()
                    .unwrap_or("(no message)");
                println!("  - {}: {}", &commit.hash[..8], subject);
            }
            loop {
                print!("\n❓ [R]etry failed commits, or [S]kip? [R/s] ");
                std::io::stdout().flush()?;
                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;
                match input.trim().to_lowercase().as_str() {
                    "r" | "retry" | "" => {
                        let mut still_failed = Vec::new();
                        for &idx in &failed_indices {
                            let single_view =
                                full_repo_view.single_commit_view(&full_repo_view.commits[idx]);
                            match claude_client
                                .check_commits_with_scopes(
                                    &single_view,
                                    guidelines,
                                    valid_scopes,
                                    !self.no_suggestions,
                                )
                                .await
                            {
                                Ok(report) => {
                                    if let Some(r) = report.commits.into_iter().next() {
                                        let summary = r.summary.clone().unwrap_or_default();
                                        successes.push((r, summary));
                                    }
                                }
                                Err(e) => {
                                    eprintln!("warning: still failed: {e}");
                                    still_failed.push(idx);
                                }
                            }
                        }
                        failed_indices = still_failed;
                        if failed_indices.is_empty() {
                            println!("✅ All retried commits succeeded.");
                            break;
                        }
                        println!("\n⚠️  {} commit(s) still failed:", failed_indices.len());
                        for &idx in &failed_indices {
                            let commit = &full_repo_view.commits[idx];
                            let subject = commit
                                .original_message
                                .lines()
                                .next()
                                .unwrap_or("(no message)");
                            println!("  - {}: {}", &commit.hash[..8], subject);
                        }
                    }
                    "s" | "skip" => {
                        println!("Skipping {} failed commit(s).", failed_indices.len());
                        break;
                    }
                    _ => println!("Please enter 'r' to retry or 's' to skip."),
                }
            }
        } else if !failed_indices.is_empty() {
            eprintln!(
                "warning: {} commit(s) failed to check",
                failed_indices.len()
            );
        }

        if !failed_indices.is_empty() {
            eprintln!(
                "warning: {} commit(s) ultimately failed to check",
                failed_indices.len()
            );
        }

        if successes.is_empty() {
            anyhow::bail!("All commits failed to check");
        }

        // Reduce phase: optional coherence pass
        // Skip when all commits were in a single batch (AI already saw them together)
        let single_batch = batch_plan.batches.len() <= 1;
        if !self.no_coherence && !single_batch && successes.len() >= 2 {
            if !self.quiet {
                println!("🔗 Running cross-commit coherence pass...");
            }
            match claude_client
                .refine_checks_coherence(&successes, full_repo_view)
                .await
            {
                Ok(refined) => {
                    if !self.quiet {
                        println!("✅ All commits checked!");
                    }
                    return Ok(refined);
                }
                Err(e) => {
                    eprintln!("warning: coherence pass failed, using individual results: {e}");
                }
            }
        }

        if !self.quiet {
            println!("✅ All commits checked!");
        }

        let all_results: Vec<CommitCheckResult> = successes.into_iter().map(|(r, _)| r).collect();

        Ok(CheckReport::new(all_results))
    }

    /// Outputs the check report in the specified format.
    fn output_report(
        &self,
        report: &crate::data::check::CheckReport,
        format: crate::data::check::OutputFormat,
    ) -> Result<()> {
        use crate::data::check::OutputFormat;

        match format {
            OutputFormat::Text => self.output_text_report(report),
            OutputFormat::Json => {
                let json = serde_json::to_string_pretty(report)
                    .context("Failed to serialize report to JSON")?;
                println!("{json}");
                Ok(())
            }
            OutputFormat::Yaml => {
                let yaml =
                    crate::data::to_yaml(report).context("Failed to serialize report to YAML")?;
                println!("{yaml}");
                Ok(())
            }
        }
    }

    /// Outputs the text format report.
    fn output_text_report(&self, report: &crate::data::check::CheckReport) -> Result<()> {
        use crate::data::check::IssueSeverity;

        println!();

        for result in &report.commits {
            if !should_display_commit(result.passes, self.show_passing) {
                continue;
            }

            // Skip info-only commits in quiet mode
            if self.quiet && !has_errors_or_warnings(&result.issues) {
                continue;
            }

            let icon = super::formatting::determine_commit_icon(result.passes, &result.issues);
            let short_hash = super::formatting::truncate_hash(&result.hash);
            println!("{}", format_commit_line(icon, short_hash, &result.message));

            // Print issues
            for issue in &result.issues {
                // Skip info issues in quiet mode
                if self.quiet && issue.severity == IssueSeverity::Info {
                    continue;
                }

                let severity_str = super::formatting::format_severity_label(issue.severity);
                println!(
                    "   {} [{}] {}",
                    severity_str, issue.section, issue.explanation
                );
            }

            // Print suggestion if available and not in quiet mode
            if !self.quiet {
                if let Some(suggestion) = &result.suggestion {
                    println!();
                    print!("{}", format_suggestion_text(suggestion, self.verbose));
                }
            }

            println!();
        }

        // Print summary
        println!("{}", format_summary_text(&report.summary));

        Ok(())
    }

    /// Shows model information.
    fn show_model_info(&self, client: &crate::claude::client::ClaudeClient) -> Result<()> {
        use crate::claude::model_config::get_model_registry;

        println!("🤖 AI Model Configuration:");

        let metadata = client.get_ai_client_metadata();
        let registry = get_model_registry();

        if let Some(spec) = registry.get_model_spec(&metadata.model) {
            if metadata.model != spec.api_identifier {
                println!(
                    "   📡 Model: {} → \x1b[33m{}\x1b[0m",
                    metadata.model, spec.api_identifier
                );
            } else {
                println!("   📡 Model: \x1b[33m{}\x1b[0m", metadata.model);
            }
            println!("   🏷️  Provider: {}", spec.provider);
        } else {
            println!("   📡 Model: \x1b[33m{}\x1b[0m", metadata.model);
            println!("   🏷️  Provider: {}", metadata.provider);
        }

        println!();
        Ok(())
    }

    /// Builds amendments from check report suggestions for failing commits.
    fn build_amendments_from_suggestions(
        &self,
        report: &crate::data::check::CheckReport,
        repo_view: &crate::data::RepositoryView,
    ) -> Vec<crate::data::amendments::Amendment> {
        use crate::data::amendments::Amendment;

        let candidate_hashes: Vec<String> =
            repo_view.commits.iter().map(|c| c.hash.clone()).collect();

        report
            .commits
            .iter()
            .filter(|r| !r.passes)
            .filter_map(|r| {
                let suggestion = r.suggestion.as_ref()?;
                let full_hash = super::formatting::resolve_short_hash(&r.hash, &candidate_hashes)?;
                Some(Amendment::new(
                    full_hash.to_string(),
                    suggestion.message.clone(),
                ))
            })
            .collect()
    }

    /// Prompts the user to apply suggested amendments and applies them if accepted.
    /// Returns true if amendments were applied, false if user declined.
    async fn prompt_and_apply_suggestions(
        &self,
        amendments: Vec<crate::data::amendments::Amendment>,
    ) -> Result<bool> {
        use crate::data::amendments::AmendmentFile;
        use crate::git::AmendmentHandler;
        use std::io::{self, Write};

        println!();
        println!(
            "🔧 {} commit(s) have issues with suggested fixes available.",
            amendments.len()
        );

        loop {
            print!("❓ [A]pply suggested fixes, or [Q]uit? [A/q] ");
            io::stdout().flush()?;

            let mut input = String::new();
            io::stdin().read_line(&mut input)?;

            match input.trim().to_lowercase().as_str() {
                "a" | "apply" | "" => {
                    let amendment_file = AmendmentFile { amendments };
                    let temp_file = tempfile::NamedTempFile::new()
                        .context("Failed to create temp file for amendments")?;
                    amendment_file
                        .save_to_file(temp_file.path())
                        .context("Failed to save amendments")?;

                    let handler = AmendmentHandler::new()
                        .context("Failed to initialize amendment handler")?;
                    handler
                        .apply_amendments(&temp_file.path().to_string_lossy())
                        .context("Failed to apply amendments")?;

                    println!("✅ Suggested fixes applied successfully!");
                    return Ok(true);
                }
                "q" | "quit" => return Ok(false),
                _ => {
                    println!("Invalid choice. Please enter 'a' to apply or 'q' to quit.");
                }
            }
        }
    }
}

// --- Extracted pure functions ---

/// Returns whether a commit should be displayed based on its pass status.
fn should_display_commit(passes: bool, show_passing: bool) -> bool {
    !passes || show_passing
}

/// Returns whether any issues have Error or Warning severity.
fn has_errors_or_warnings(issues: &[crate::data::check::CommitIssue]) -> bool {
    use crate::data::check::IssueSeverity;
    issues
        .iter()
        .any(|i| matches!(i.severity, IssueSeverity::Error | IssueSeverity::Warning))
}

/// Returns whether the twiddle (auto-fix) flow should be offered.
fn should_offer_twiddle(
    twiddle_flag: bool,
    has_errors: bool,
    format: crate::data::check::OutputFormat,
) -> bool {
    twiddle_flag && has_errors && format == crate::data::check::OutputFormat::Text
}

/// Formats a commit suggestion as indented text.
fn format_suggestion_text(
    suggestion: &crate::data::check::CommitSuggestion,
    verbose: bool,
) -> String {
    let mut output = String::new();
    output.push_str("   Suggested message:\n");
    for line in suggestion.message.lines() {
        output.push_str(&format!("      {line}\n"));
    }
    if verbose {
        output.push('\n');
        output.push_str("   Why this is better:\n");
        for line in suggestion.explanation.lines() {
            output.push_str(&format!("   {line}\n"));
        }
    }
    output
}

/// Formats the summary section of a check report.
fn format_summary_text(summary: &crate::data::check::CheckSummary) -> String {
    format!(
        "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n\
         Summary: {} commits checked\n\
         \x20 {} errors, {} warnings\n\
         \x20 {} passed, {} with issues",
        summary.total_commits,
        summary.error_count,
        summary.warning_count,
        summary.passing_commits,
        summary.failing_commits,
    )
}

/// Formats a single commit line for text output.
fn format_commit_line(icon: &str, short_hash: &str, message: &str) -> String {
    format!("{icon} {short_hash} - \"{message}\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::check::{
        CheckSummary, CommitIssue, CommitSuggestion, IssueSeverity, OutputFormat,
    };

    // --- should_display_commit ---

    #[test]
    fn display_commit_passing_hidden() {
        assert!(!should_display_commit(true, false));
    }

    #[test]
    fn display_commit_passing_shown() {
        assert!(should_display_commit(true, true));
    }

    #[test]
    fn display_commit_failing() {
        assert!(should_display_commit(false, false));
        assert!(should_display_commit(false, true));
    }

    // --- has_errors_or_warnings ---

    #[test]
    fn errors_or_warnings_with_error() {
        let issues = vec![CommitIssue {
            severity: IssueSeverity::Error,
            section: "subject".to_string(),
            rule: "length".to_string(),
            explanation: "too long".to_string(),
        }];
        assert!(has_errors_or_warnings(&issues));
    }

    #[test]
    fn errors_or_warnings_with_warning() {
        let issues = vec![CommitIssue {
            severity: IssueSeverity::Warning,
            section: "body".to_string(),
            rule: "style".to_string(),
            explanation: "minor issue".to_string(),
        }];
        assert!(has_errors_or_warnings(&issues));
    }

    #[test]
    fn errors_or_warnings_info_only() {
        let issues = vec![CommitIssue {
            severity: IssueSeverity::Info,
            section: "body".to_string(),
            rule: "suggestion".to_string(),
            explanation: "consider adding more detail".to_string(),
        }];
        assert!(!has_errors_or_warnings(&issues));
    }

    #[test]
    fn errors_or_warnings_empty() {
        assert!(!has_errors_or_warnings(&[]));
    }

    // --- should_offer_twiddle ---

    #[test]
    fn offer_twiddle_all_conditions_met() {
        assert!(should_offer_twiddle(true, true, OutputFormat::Text));
    }

    #[test]
    fn offer_twiddle_flag_off() {
        assert!(!should_offer_twiddle(false, true, OutputFormat::Text));
    }

    #[test]
    fn offer_twiddle_no_errors() {
        assert!(!should_offer_twiddle(true, false, OutputFormat::Text));
    }

    #[test]
    fn offer_twiddle_json_format() {
        assert!(!should_offer_twiddle(true, true, OutputFormat::Json));
    }

    // --- format_suggestion_text ---

    #[test]
    fn suggestion_text_basic() {
        let suggestion = CommitSuggestion {
            message: "feat(cli): add new flag".to_string(),
            explanation: "uses conventional format".to_string(),
        };
        let result = format_suggestion_text(&suggestion, false);
        assert!(result.contains("Suggested message:"));
        assert!(result.contains("feat(cli): add new flag"));
        assert!(!result.contains("Why this is better"));
    }

    #[test]
    fn suggestion_text_verbose() {
        let suggestion = CommitSuggestion {
            message: "fix: resolve crash".to_string(),
            explanation: "clear description of fix".to_string(),
        };
        let result = format_suggestion_text(&suggestion, true);
        assert!(result.contains("Suggested message:"));
        assert!(result.contains("fix: resolve crash"));
        assert!(result.contains("Why this is better:"));
        assert!(result.contains("clear description of fix"));
    }

    // --- format_summary_text ---

    #[test]
    fn summary_text_formatting() {
        let summary = CheckSummary {
            total_commits: 5,
            passing_commits: 3,
            failing_commits: 2,
            error_count: 1,
            warning_count: 4,
            info_count: 0,
        };
        let result = format_summary_text(&summary);
        assert!(result.contains("5 commits checked"));
        assert!(result.contains("1 errors, 4 warnings"));
        assert!(result.contains("3 passed, 2 with issues"));
    }

    // --- format_commit_line ---

    #[test]
    fn commit_line_formatting() {
        let line = format_commit_line("✅", "abc1234", "feat: add feature");
        assert_eq!(line, "✅ abc1234 - \"feat: add feature\"");
    }

    // --- check_with_map_reduce (error path coverage) ---

    fn make_check_cmd(quiet: bool) -> CheckCommand {
        CheckCommand {
            commit_range: None,
            model: None,
            beta_header: None,
            context_dir: None,
            guidelines: None,
            format: "text".to_string(),
            strict: false,
            quiet,
            verbose: false,
            show_passing: false,
            batch_size: 4,
            concurrency: 4,
            no_coherence: true,
            no_suggestions: false,
            twiddle: false,
        }
    }

    fn make_check_commit(hash: &str) -> (crate::git::CommitInfo, tempfile::NamedTempFile) {
        use crate::git::commit::FileChanges;
        use crate::git::{CommitAnalysis, CommitInfo};
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let commit = CommitInfo {
            hash: hash.to_string(),
            author: "Test <test@test.com>".to_string(),
            date: chrono::Utc::now().fixed_offset(),
            original_message: format!("feat: commit {hash}"),
            in_main_branches: vec![],
            analysis: CommitAnalysis {
                detected_type: "feat".to_string(),
                detected_scope: String::new(),
                proposed_message: format!("feat: commit {hash}"),
                file_changes: FileChanges {
                    total_files: 0,
                    files_added: 0,
                    files_deleted: 0,
                    file_list: vec![],
                },
                diff_summary: String::new(),
                diff_file: tmp.path().to_string_lossy().to_string(),
            },
        };
        (commit, tmp)
    }

    fn make_check_repo_view(commits: Vec<crate::git::CommitInfo>) -> crate::data::RepositoryView {
        use crate::data::{AiInfo, FieldExplanation, RepositoryView, WorkingDirectoryInfo};
        RepositoryView {
            versions: None,
            explanation: FieldExplanation::default(),
            working_directory: WorkingDirectoryInfo {
                clean: true,
                untracked_changes: vec![],
            },
            remotes: vec![],
            ai: AiInfo {
                scratch: String::new(),
            },
            branch_info: None,
            pr_template: None,
            pr_template_location: None,
            branch_prs: None,
            commits,
        }
    }

    fn check_yaml(hash: &str) -> String {
        format!("checks:\n  - commit: {hash}\n    passes: true\n    issues: []\n")
    }

    fn make_client(responses: Vec<anyhow::Result<String>>) -> crate::claude::client::ClaudeClient {
        crate::claude::client::ClaudeClient::new(Box::new(
            crate::claude::test_utils::ConfigurableMockAiClient::new(responses),
        ))
    }

    // check_commits_with_retry uses max_retries=2 (3 total attempts), so a
    // batch or individual commit needs 3 consecutive Err responses to fail.
    fn errs(n: usize) -> Vec<anyhow::Result<String>> {
        (0..n)
            .map(|_| Err(anyhow::anyhow!("mock failure")))
            .collect()
    }

    #[tokio::test]
    async fn check_with_map_reduce_single_commit_fails_returns_err() {
        // A single-commit batch that exhausts all retries records the index in
        // failed_indices and returns Ok(([], [idx])). With successes empty the
        // method bails, so the overall result is Err.
        let (commit, _tmp) = make_check_commit("abc00000");
        let cmd = make_check_cmd(true);
        let repo_view = make_check_repo_view(vec![commit]);
        let client = make_client(errs(3));
        let result = cmd
            .check_with_map_reduce(&client, &repo_view, None, &[])
            .await;
        assert!(result.is_err(), "empty successes should bail");
    }

    #[tokio::test]
    async fn check_with_map_reduce_single_commit_succeeds() {
        // Happy path: one commit, one successful batch response.
        let (commit, _tmp) = make_check_commit("abc00000");
        let cmd = make_check_cmd(true);
        let repo_view = make_check_repo_view(vec![commit]);
        let client = make_client(vec![Ok(check_yaml("abc00000"))]);
        let result = cmd
            .check_with_map_reduce(&client, &repo_view, None, &[])
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().commits.len(), 1);
    }

    #[tokio::test]
    async fn check_with_map_reduce_batch_fails_split_retry_both_succeed() {
        // Two commits fit into one batch. The batch fails (3 retries exhausted),
        // triggering split-and-retry. Both individual commits then succeed.
        let (c1, _t1) = make_check_commit("abc00000");
        let (c2, _t2) = make_check_commit("def00000");
        let cmd = make_check_cmd(true);
        let repo_view = make_check_repo_view(vec![c1, c2]);
        let mut responses = errs(3); // batch failure
        responses.push(Ok(check_yaml("abc00000"))); // abc individual
        responses.push(Ok(check_yaml("def00000"))); // def individual
        let client = make_client(responses);
        let result = cmd
            .check_with_map_reduce(&client, &repo_view, None, &[])
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().commits.len(), 2);
    }

    #[tokio::test]
    async fn check_with_map_reduce_batch_fails_split_one_individual_fails_quiet() {
        // Batch fails → split-and-retry. abc succeeds; def exhausts its retries
        // and is recorded in failed_indices. In quiet mode the method returns
        // Ok with partial results rather than bailing (successes is non-empty).
        let (c1, _t1) = make_check_commit("abc00000");
        let (c2, _t2) = make_check_commit("def00000");
        let cmd = make_check_cmd(true);
        let repo_view = make_check_repo_view(vec![c1, c2]);
        let mut responses = errs(3); // batch failure
        responses.push(Ok(check_yaml("abc00000"))); // abc individual succeeds
        responses.extend(errs(3)); // def individual exhausts retries
        let client = make_client(responses);
        let result = cmd
            .check_with_map_reduce(&client, &repo_view, None, &[])
            .await;
        // abc succeeded, so successes is non-empty and the method returns Ok
        assert!(result.is_ok());
        assert_eq!(result.unwrap().commits.len(), 1);
    }

    #[tokio::test]
    async fn check_with_map_reduce_all_fail_in_split_retry_returns_err() {
        // Batch fails → split-and-retry. Both individual commits also fail.
        // successes stays empty so the method bails.
        let (c1, _t1) = make_check_commit("abc00000");
        let (c2, _t2) = make_check_commit("def00000");
        let cmd = make_check_cmd(true);
        let repo_view = make_check_repo_view(vec![c1, c2]);
        let mut responses = errs(3); // batch failure
        responses.extend(errs(3)); // abc individual exhausts retries
        responses.extend(errs(3)); // def individual exhausts retries
        let client = make_client(responses);
        let result = cmd
            .check_with_map_reduce(&client, &repo_view, None, &[])
            .await;
        assert!(result.is_err(), "no successes should bail");
    }
}
