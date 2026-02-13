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
        if self.twiddle && report.has_errors() && output_format == OutputFormat::Text {
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
        use std::fs;

        // If explicit guidelines path is provided, use it
        if let Some(guidelines_path) = &self.guidelines {
            let content = fs::read_to_string(guidelines_path).with_context(|| {
                format!(
                    "Failed to read guidelines file: {}",
                    guidelines_path.display()
                )
            })?;
            return Ok(Some(content));
        }

        // Otherwise, use project discovery to find guidelines
        let context_dir = self
            .context_dir
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from(".omni-dev"));

        // Try local override first
        let local_path = context_dir.join("local").join("commit-guidelines.md");
        if local_path.exists() {
            let content = fs::read_to_string(&local_path)
                .with_context(|| format!("Failed to read guidelines: {}", local_path.display()))?;
            return Ok(Some(content));
        }

        // Try project-level guidelines
        let project_path = context_dir.join("commit-guidelines.md");
        if project_path.exists() {
            let content = fs::read_to_string(&project_path).with_context(|| {
                format!("Failed to read guidelines: {}", project_path.display())
            })?;
            return Ok(Some(content));
        }

        // Try global guidelines
        if let Some(home) = dirs::home_dir() {
            let home_path = home.join(".omni-dev").join("commit-guidelines.md");
            if home_path.exists() {
                let content = fs::read_to_string(&home_path).with_context(|| {
                    format!("Failed to read guidelines: {}", home_path.display())
                })?;
                return Ok(Some(content));
            }
        }

        // No custom guidelines found, will use defaults
        Ok(None)
    }

    /// Loads valid scopes from context directory with ecosystem defaults.
    fn load_scopes(&self) -> Vec<crate::data::context::ScopeDefinition> {
        let context_dir = self
            .context_dir
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from(".omni-dev"));
        crate::claude::context::load_project_scopes(&context_dir, &std::path::PathBuf::from("."))
    }

    /// Shows diagnostic information about loaded guidance files.
    fn show_guidance_files_status(
        &self,
        guidelines: &Option<String>,
        valid_scopes: &[crate::data::context::ScopeDefinition],
    ) {
        let context_dir = self
            .context_dir
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from(".omni-dev"));

        println!("📋 Project guidance files status:");

        // Check commit guidelines
        let guidelines_found = guidelines.is_some();
        let guidelines_source = if guidelines_found {
            let local_path = context_dir.join("local").join("commit-guidelines.md");
            let project_path = context_dir.join("commit-guidelines.md");
            let home_path = dirs::home_dir()
                .map(|h| h.join(".omni-dev").join("commit-guidelines.md"))
                .unwrap_or_default();

            if local_path.exists() {
                format!("✅ Local override: {}", local_path.display())
            } else if project_path.exists() {
                format!("✅ Project: {}", project_path.display())
            } else if home_path.exists() {
                format!("✅ Global: {}", home_path.display())
            } else {
                "✅ (source unknown)".to_string()
            }
        } else {
            "⚪ Using defaults".to_string()
        };
        println!("   📝 Commit guidelines: {guidelines_source}");

        // Check scopes
        let scopes_count = valid_scopes.len();
        let scopes_source = if scopes_count > 0 {
            let local_path = context_dir.join("local").join("scopes.yaml");
            let project_path = context_dir.join("scopes.yaml");
            let home_path = dirs::home_dir()
                .map(|h| h.join(".omni-dev").join("scopes.yaml"))
                .unwrap_or_default();

            let source = if local_path.exists() {
                format!("Local override: {}", local_path.display())
            } else if project_path.exists() {
                format!("Project: {}", project_path.display())
            } else if home_path.exists() {
                format!("Global: {}", home_path.display())
            } else {
                "(source unknown)".to_string()
            };
            format!("✅ {source} ({scopes_count} scopes)")
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
                            Ok(items)
                        }
                        Err(e) if batch_size > 1 => {
                            // Split-and-retry: fall back to individual commits
                            eprintln!(
                                "warning: batch of {batch_size} failed, retrying individually: {e}"
                            );
                            let mut items = Vec::new();
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
                                match single_result {
                                    Ok(report) => {
                                        if let Some(r) = report.commits.into_iter().next() {
                                            let summary = r.summary.clone().unwrap_or_default();
                                            items.push((r, summary));
                                        }
                                    }
                                    Err(e) => {
                                        eprintln!("warning: failed to check commit: {e}");
                                    }
                                }
                                let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
                                if !self.quiet {
                                    println!("   ✅ {done}/{total_commits} commits checked");
                                }
                            }
                            Ok(items)
                        }
                        Err(e) => Err(e),
                    }
                }
            })
            .collect();

        let results = futures::future::join_all(futs).await;

        // Flatten batch results
        let mut successes: Vec<(CommitCheckResult, String)> = Vec::new();
        let mut failure_count = 0;

        for result in results {
            match result {
                Ok(items) => successes.extend(items),
                Err(e) => {
                    eprintln!("warning: failed to check commit: {e}");
                    failure_count += 1;
                }
            }
        }

        if failure_count > 0 {
            eprintln!("warning: {failure_count} commit(s) failed to check");
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
            // Skip passing commits unless --show-passing is set
            if result.passes && !self.show_passing {
                continue;
            }

            // Skip info-only commits in quiet mode
            if self.quiet {
                let has_errors_or_warnings = result
                    .issues
                    .iter()
                    .any(|i| matches!(i.severity, IssueSeverity::Error | IssueSeverity::Warning));
                if !has_errors_or_warnings {
                    continue;
                }
            }

            // Determine icon
            let icon = if result.passes {
                "✅"
            } else if result
                .issues
                .iter()
                .any(|i| i.severity == IssueSeverity::Error)
            {
                "❌"
            } else {
                "⚠️ "
            };

            // Short hash
            let short_hash = if result.hash.len() > 7 {
                &result.hash[..7]
            } else {
                &result.hash
            };

            println!("{} {} - \"{}\"", icon, short_hash, result.message);

            // Print issues
            for issue in &result.issues {
                // Skip info issues in quiet mode
                if self.quiet && issue.severity == IssueSeverity::Info {
                    continue;
                }

                let severity_str = match issue.severity {
                    IssueSeverity::Error => "\x1b[31mERROR\x1b[0m  ",
                    IssueSeverity::Warning => "\x1b[33mWARNING\x1b[0m",
                    IssueSeverity::Info => "\x1b[36mINFO\x1b[0m   ",
                };

                println!(
                    "   {} [{}] {}",
                    severity_str, issue.section, issue.explanation
                );
            }

            // Print suggestion if available and not in quiet mode
            if !self.quiet {
                if let Some(suggestion) = &result.suggestion {
                    println!();
                    println!("   Suggested message:");
                    for line in suggestion.message.lines() {
                        println!("      {line}");
                    }
                    if self.verbose {
                        println!();
                        println!("   Why this is better:");
                        for line in suggestion.explanation.lines() {
                            println!("   {line}");
                        }
                    }
                }
            }

            println!();
        }

        // Print summary
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!("Summary: {} commits checked", report.summary.total_commits);
        println!(
            "  {} errors, {} warnings",
            report.summary.error_count, report.summary.warning_count
        );
        println!(
            "  {} passed, {} with issues",
            report.summary.passing_commits, report.summary.failing_commits
        );

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

        report
            .commits
            .iter()
            .filter(|r| !r.passes)
            .filter_map(|r| {
                let suggestion = r.suggestion.as_ref()?;
                let full_hash = repo_view.commits.iter().find_map(|c| {
                    if c.hash.starts_with(&r.hash) || r.hash.starts_with(&c.hash) {
                        Some(c.hash.clone())
                    } else {
                        None
                    }
                });
                full_hash.map(|hash| Amendment::new(hash, suggestion.message.clone()))
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
