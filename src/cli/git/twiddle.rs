//! Twiddle command — AI-powered commit message improvement.

use anyhow::{Context, Result};
use clap::Parser;
use tracing::debug;

use super::parse_beta_header;

/// Twiddle command options.
#[derive(Parser)]
pub struct TwiddleCommand {
    /// Commit range to analyze and improve (e.g., HEAD~3..HEAD, abc123..def456).
    #[arg(value_name = "COMMIT_RANGE")]
    pub commit_range: Option<String>,

    /// Claude API model to use (if not specified, uses settings or default).
    #[arg(long)]
    pub model: Option<String>,

    /// Beta header to send with API requests (format: key:value).
    /// Only sent if the model supports it in the registry.
    #[arg(long, value_name = "KEY:VALUE")]
    pub beta_header: Option<String>,

    /// Skips confirmation prompt and applies amendments automatically.
    #[arg(long)]
    pub auto_apply: bool,

    /// Saves generated amendments to file without applying.
    #[arg(long, value_name = "FILE")]
    pub save_only: Option<String>,

    /// Uses additional project context for better suggestions (Phase 3).
    #[arg(long, default_value = "true")]
    pub use_context: bool,

    /// Path to custom context directory (defaults to .omni-dev/).
    #[arg(long)]
    pub context_dir: Option<std::path::PathBuf>,

    /// Specifies work context (e.g., "feature: user authentication").
    #[arg(long)]
    pub work_context: Option<String>,

    /// Overrides detected branch context.
    #[arg(long)]
    pub branch_context: Option<String>,

    /// Disables contextual analysis (uses basic prompting only).
    #[arg(long)]
    pub no_context: bool,

    /// Deprecated: use --concurrency instead.
    #[arg(long, default_value = "4", hide = true)]
    pub batch_size: usize,

    /// Maximum number of concurrent AI requests (default: 4).
    #[arg(long, default_value = "4")]
    pub concurrency: usize,

    /// Disables the cross-commit coherence pass.
    #[arg(long)]
    pub no_coherence: bool,

    /// Skips AI processing and only outputs repository YAML.
    #[arg(long)]
    pub no_ai: bool,

    /// Ignores existing commit messages and generates fresh ones based solely on diffs.
    #[arg(long)]
    pub fresh: bool,

    /// Runs commit message validation after applying amendments.
    #[arg(long)]
    pub check: bool,
}

impl TwiddleCommand {
    /// Executes the twiddle command with contextual intelligence.
    pub async fn execute(self) -> Result<()> {
        // If --no-ai flag is set, skip AI processing and output YAML directly
        if self.no_ai {
            return self.execute_no_ai().await;
        }

        // Preflight check: validate AI credentials before any processing
        let ai_info = crate::utils::check_ai_command_prerequisites(self.model.as_deref())?;
        println!(
            "✓ {} credentials verified (model: {})",
            ai_info.provider, ai_info.model
        );

        // Preflight check: ensure working directory is clean before expensive operations
        crate::utils::preflight::check_working_directory_clean()?;
        println!("✓ Working directory is clean");

        // Determine if contextual analysis should be used
        let use_contextual = self.use_context && !self.no_context;

        if use_contextual {
            println!(
                "🪄 Starting AI-powered commit message improvement with contextual intelligence..."
            );
        } else {
            println!("🪄 Starting AI-powered commit message improvement...");
        }

        // 1. Generate repository view to get all commits
        let mut full_repo_view = self.generate_repository_view().await?;

        // 2. Use parallel map-reduce for multiple commits
        if full_repo_view.commits.len() > 1 {
            return self
                .execute_with_map_reduce(use_contextual, full_repo_view)
                .await;
        }

        // 3. Collect contextual information (Phase 3)
        let context = if use_contextual {
            Some(self.collect_context(&full_repo_view).await?)
        } else {
            None
        };

        // Refine detected scopes using file_patterns from scope definitions
        let scope_defs = match &context {
            Some(ctx) => ctx.project.valid_scopes.clone(),
            None => self.load_check_scopes(),
        };
        for commit in &mut full_repo_view.commits {
            commit.analysis.refine_scope(&scope_defs);
        }

        // 4. Show context summary if available
        if let Some(ref ctx) = context {
            self.show_context_summary(ctx)?;
        }

        // 5. Initialize Claude client
        let beta = self
            .beta_header
            .as_deref()
            .map(parse_beta_header)
            .transpose()?;
        let claude_client = crate::claude::create_default_claude_client(self.model.clone(), beta)?;

        // Show model information
        self.show_model_info_from_client(&claude_client)?;

        // 6. Generate amendments via Claude API with context
        if self.fresh {
            println!("🔄 Fresh mode: ignoring existing commit messages...");
        }
        if use_contextual && context.is_some() {
            println!("🤖 Analyzing commits with enhanced contextual intelligence...");
        } else {
            println!("🤖 Analyzing commits with Claude AI...");
        }

        let amendments = if let Some(ctx) = context {
            claude_client
                .generate_contextual_amendments_with_options(&full_repo_view, &ctx, self.fresh)
                .await?
        } else {
            claude_client
                .generate_amendments_with_options(&full_repo_view, self.fresh)
                .await?
        };

        // 6. Handle different output modes
        if let Some(save_path) = self.save_only {
            amendments.save_to_file(save_path)?;
            println!("💾 Amendments saved to file");
            return Ok(());
        }

        // 7. Handle amendments
        if !amendments.amendments.is_empty() {
            // Create temporary file for amendments
            let temp_dir = tempfile::tempdir()?;
            let amendments_file = temp_dir.path().join("twiddle_amendments.yaml");
            amendments.save_to_file(&amendments_file)?;

            // Show file path and get user choice
            if !self.auto_apply && !self.handle_amendments_file(&amendments_file, &amendments)? {
                println!("❌ Amendment cancelled by user");
                return Ok(());
            }

            // 8. Apply amendments (re-read from file to capture any user edits)
            self.apply_amendments_from_file(&amendments_file).await?;
            println!("✅ Commit messages improved successfully!");

            // 9. Run post-twiddle check if --check flag is set
            if self.check {
                self.run_post_twiddle_check().await?;
            }
        } else {
            println!("✨ No commits found to process!");
        }

        Ok(())
    }

    /// Executes the twiddle command with batched parallel map-reduce for multiple commits.
    ///
    /// Commits are grouped into token-budget-aware batches (map phase),
    /// then an optional coherence pass refines results across all commits
    /// (reduce phase). Coherence is skipped when all commits fit in a
    /// single batch since the AI already saw them together.
    async fn execute_with_map_reduce(
        &self,
        use_contextual: bool,
        mut full_repo_view: crate::data::RepositoryView,
    ) -> Result<()> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        use crate::claude::batch;
        use crate::claude::token_budget;
        use crate::data::amendments::AmendmentFile;

        let concurrency = self.concurrency;

        // Initialize Claude client
        let beta = self
            .beta_header
            .as_deref()
            .map(parse_beta_header)
            .transpose()?;
        let claude_client = crate::claude::create_default_claude_client(self.model.clone(), beta)?;

        // Show model information
        self.show_model_info_from_client(&claude_client)?;

        if self.fresh {
            println!("🔄 Fresh mode: ignoring existing commit messages...");
        }

        let total_commits = full_repo_view.commits.len();
        println!(
            "🔄 Processing {} commits in parallel (concurrency: {})...",
            total_commits, concurrency
        );

        // Collect context once (shared across all commits)
        let context = if use_contextual {
            Some(self.collect_context(&full_repo_view).await?)
        } else {
            None
        };

        if let Some(ref ctx) = context {
            self.show_context_summary(ctx)?;
        }

        // Refine scopes on all commits upfront
        let scope_defs = match &context {
            Some(ctx) => ctx.project.valid_scopes.clone(),
            None => self.load_check_scopes(),
        };
        for commit in &mut full_repo_view.commits {
            commit.analysis.refine_scope(&scope_defs);
        }

        // Plan batches based on token budget
        let metadata = claude_client.get_ai_client_metadata();
        let system_prompt_tokens = if let Some(ref ctx) = context {
            let prompt_style = metadata.prompt_style();
            let system_prompt =
                crate::claude::prompts::generate_contextual_system_prompt_for_provider(
                    ctx,
                    prompt_style,
                );
            token_budget::estimate_tokens(&system_prompt)
        } else {
            token_budget::estimate_tokens(crate::claude::prompts::SYSTEM_PROMPT)
        };
        let batch_plan =
            batch::plan_batches(&full_repo_view.commits, &metadata, system_prompt_tokens);

        if batch_plan.batches.len() < total_commits {
            println!(
                "   📦 Grouped {} commits into {} batches by token budget",
                total_commits,
                batch_plan.batches.len()
            );
        }

        // Map phase: process batches in parallel
        let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency));
        let completed = Arc::new(AtomicUsize::new(0));

        let repo_ref = &full_repo_view;
        let client_ref = &claude_client;
        let context_ref = &context;
        let fresh = self.fresh;

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
                        repo_ref.single_commit_view(&repo_ref.commits[batch_indices[0]])
                    } else {
                        let commits: Vec<_> = batch_indices
                            .iter()
                            .map(|&i| &repo_ref.commits[i])
                            .collect();
                        repo_ref.multi_commit_view(&commits)
                    };

                    // Generate amendments for the batch
                    let result = if let Some(ref ctx) = context_ref {
                        client_ref
                            .generate_contextual_amendments_with_options(&batch_view, ctx, fresh)
                            .await
                    } else {
                        client_ref
                            .generate_amendments_with_options(&batch_view, fresh)
                            .await
                    };

                    match result {
                        Ok(amendment_file) => {
                            let done =
                                completed.fetch_add(batch_size, Ordering::Relaxed) + batch_size;
                            println!("   ✅ {}/{} commits processed", done, total_commits);

                            let items: Vec<_> = amendment_file
                                .amendments
                                .into_iter()
                                .map(|a| {
                                    let summary = a.summary.clone().unwrap_or_default();
                                    (a, summary)
                                })
                                .collect();
                            Ok(items)
                        }
                        Err(e) if batch_size > 1 => {
                            // Split-and-retry: fall back to individual commits
                            eprintln!(
                                "warning: batch of {} failed, retrying individually: {e}",
                                batch_size
                            );
                            let mut items = Vec::new();
                            for &idx in batch_indices {
                                let single_view =
                                    repo_ref.single_commit_view(&repo_ref.commits[idx]);
                                let single_result = if let Some(ref ctx) = context_ref {
                                    client_ref
                                        .generate_contextual_amendments_with_options(
                                            &single_view,
                                            ctx,
                                            fresh,
                                        )
                                        .await
                                } else {
                                    client_ref
                                        .generate_amendments_with_options(&single_view, fresh)
                                        .await
                                };
                                match single_result {
                                    Ok(af) => {
                                        if let Some(a) = af.amendments.into_iter().next() {
                                            let summary = a.summary.clone().unwrap_or_default();
                                            items.push((a, summary));
                                        }
                                    }
                                    Err(e) => {
                                        eprintln!("warning: failed to process commit: {e}");
                                    }
                                }
                                let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
                                println!("   ✅ {}/{} commits processed", done, total_commits);
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
        let mut successes: Vec<(crate::data::amendments::Amendment, String)> = Vec::new();
        let mut failure_count = 0;

        for result in results {
            match result {
                Ok(items) => successes.extend(items),
                Err(e) => {
                    eprintln!("warning: failed to process commit: {e}");
                    failure_count += 1;
                }
            }
        }

        if failure_count > 0 {
            eprintln!("warning: {failure_count} commit(s) failed to process");
        }

        if successes.is_empty() {
            anyhow::bail!("All commits failed to process");
        }

        // Reduce phase: optional coherence pass
        // Skip when all commits were in a single batch (AI already saw them together)
        let single_batch = batch_plan.batches.len() <= 1;
        let all_amendments = if !self.no_coherence && !single_batch && successes.len() >= 2 {
            println!("🔗 Running cross-commit coherence pass...");
            match claude_client.refine_amendments_coherence(&successes).await {
                Ok(refined) => refined,
                Err(e) => {
                    eprintln!("warning: coherence pass failed, using individual results: {e}");
                    AmendmentFile {
                        amendments: successes.into_iter().map(|(a, _)| a).collect(),
                    }
                }
            }
        } else {
            AmendmentFile {
                amendments: successes.into_iter().map(|(a, _)| a).collect(),
            }
        };

        println!(
            "✅ All commits processed! Found {} amendments.",
            all_amendments.amendments.len()
        );

        // Handle different output modes
        if let Some(save_path) = &self.save_only {
            all_amendments.save_to_file(save_path)?;
            println!("💾 Amendments saved to file");
            return Ok(());
        }

        // Handle amendments
        if !all_amendments.amendments.is_empty() {
            let temp_dir = tempfile::tempdir()?;
            let amendments_file = temp_dir.path().join("twiddle_amendments.yaml");
            all_amendments.save_to_file(&amendments_file)?;

            if !self.auto_apply
                && !self.handle_amendments_file(&amendments_file, &all_amendments)?
            {
                println!("❌ Amendment cancelled by user");
                return Ok(());
            }

            self.apply_amendments_from_file(&amendments_file).await?;
            println!("✅ Commit messages improved successfully!");

            if self.check {
                self.run_post_twiddle_check().await?;
            }
        } else {
            println!("✨ No commits found to process!");
        }

        Ok(())
    }

    /// Generates the repository view (reuses ViewCommand logic).
    async fn generate_repository_view(&self) -> Result<crate::data::RepositoryView> {
        use crate::data::{
            AiInfo, BranchInfo, FieldExplanation, FileStatusInfo, RepositoryView, VersionInfo,
            WorkingDirectoryInfo,
        };
        use crate::git::{GitRepository, RemoteInfo};
        use crate::utils::ai_scratch;

        let commit_range = self.commit_range.as_deref().unwrap_or("HEAD~5..HEAD");

        // Open git repository
        let repo = GitRepository::open()
            .context("Failed to open git repository. Make sure you're in a git repository.")?;

        // Get current branch name
        let current_branch = repo
            .get_current_branch()
            .unwrap_or_else(|_| "HEAD".to_string());

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
        let commits = repo.get_commits_in_range(commit_range)?;

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

    /// Handles the amendments file by showing the path and getting the user choice.
    fn handle_amendments_file(
        &self,
        amendments_file: &std::path::Path,
        amendments: &crate::data::amendments::AmendmentFile,
    ) -> Result<bool> {
        use std::io::{self, Write};

        println!(
            "\n📝 Found {} commits that could be improved.",
            amendments.amendments.len()
        );
        println!("💾 Amendments saved to: {}", amendments_file.display());
        println!();

        loop {
            print!("❓ [A]pply amendments, [S]how file, [E]dit file, or [Q]uit? [A/s/e/q] ");
            io::stdout().flush()?;

            let mut input = String::new();
            io::stdin().read_line(&mut input)?;

            match input.trim().to_lowercase().as_str() {
                "a" | "apply" | "" => return Ok(true),
                "s" | "show" => {
                    self.show_amendments_file(amendments_file)?;
                    println!();
                }
                "e" | "edit" => {
                    self.edit_amendments_file(amendments_file)?;
                    println!();
                }
                "q" | "quit" => return Ok(false),
                _ => {
                    println!(
                        "Invalid choice. Please enter 'a' to apply, 's' to show, 'e' to edit, or 'q' to quit."
                    );
                }
            }
        }
    }

    /// Shows the contents of the amendments file.
    fn show_amendments_file(&self, amendments_file: &std::path::Path) -> Result<()> {
        use std::fs;

        println!("\n📄 Amendments file contents:");
        println!("─────────────────────────────");

        let contents =
            fs::read_to_string(amendments_file).context("Failed to read amendments file")?;

        println!("{}", contents);
        println!("─────────────────────────────");

        Ok(())
    }

    /// Opens the amendments file in an external editor.
    fn edit_amendments_file(&self, amendments_file: &std::path::Path) -> Result<()> {
        use std::env;
        use std::io::{self, Write};
        use std::process::Command;

        // Try to get editor from environment variables
        let editor = env::var("OMNI_DEV_EDITOR")
            .or_else(|_| env::var("EDITOR"))
            .unwrap_or_else(|_| {
                // Prompt user for editor if neither environment variable is set
                println!(
                    "🔧 Neither OMNI_DEV_EDITOR nor EDITOR environment variables are defined."
                );
                print!("Please enter the command to use as your editor: ");
                io::stdout().flush().expect("Failed to flush stdout");

                let mut input = String::new();
                io::stdin()
                    .read_line(&mut input)
                    .expect("Failed to read user input");
                input.trim().to_string()
            });

        if editor.is_empty() {
            println!("❌ No editor specified. Returning to menu.");
            return Ok(());
        }

        println!("📝 Opening amendments file in editor: {}", editor);

        // Split editor command to handle arguments
        let mut cmd_parts = editor.split_whitespace();
        let editor_cmd = cmd_parts.next().unwrap_or(&editor);
        let args: Vec<&str> = cmd_parts.collect();

        let mut command = Command::new(editor_cmd);
        command.args(args);
        command.arg(amendments_file.to_string_lossy().as_ref());

        match command.status() {
            Ok(status) => {
                if status.success() {
                    println!("✅ Editor session completed.");
                } else {
                    println!(
                        "⚠️  Editor exited with non-zero status: {:?}",
                        status.code()
                    );
                }
            }
            Err(e) => {
                println!("❌ Failed to execute editor '{}': {}", editor, e);
                println!("   Please check that the editor command is correct and available in your PATH.");
            }
        }

        Ok(())
    }

    /// Applies amendments from a file path (re-reads from disk to capture user edits).
    async fn apply_amendments_from_file(&self, amendments_file: &std::path::Path) -> Result<()> {
        use crate::git::AmendmentHandler;

        // Use AmendmentHandler to apply amendments directly from file
        let handler = AmendmentHandler::new().context("Failed to initialize amendment handler")?;
        handler
            .apply_amendments(&amendments_file.to_string_lossy())
            .context("Failed to apply amendments")?;

        Ok(())
    }

    /// Collects contextual information for enhanced commit message generation.
    async fn collect_context(
        &self,
        repo_view: &crate::data::RepositoryView,
    ) -> Result<crate::data::context::CommitContext> {
        use crate::claude::context::{BranchAnalyzer, ProjectDiscovery, WorkPatternAnalyzer};
        use crate::data::context::CommitContext;

        let mut context = CommitContext::new();

        // 1. Discover project context
        let context_dir = self
            .context_dir
            .as_ref()
            .cloned()
            .unwrap_or_else(|| std::path::PathBuf::from(".omni-dev"));

        // ProjectDiscovery takes repo root and context directory
        let repo_root = std::path::PathBuf::from(".");
        let discovery = ProjectDiscovery::new(repo_root, context_dir.clone());
        debug!(context_dir = ?context_dir, "Using context directory");
        match discovery.discover() {
            Ok(project_context) => {
                debug!("Discovery successful");

                // Show diagnostic information about loaded guidance files
                self.show_guidance_files_status(&project_context, &context_dir)?;

                context.project = project_context;
            }
            Err(e) => {
                debug!(error = %e, "Discovery failed");
                context.project = Default::default();
            }
        }

        // 2. Analyze current branch from repository view
        if let Some(branch_info) = &repo_view.branch_info {
            context.branch = BranchAnalyzer::analyze(&branch_info.branch).unwrap_or_default();
        } else {
            // Fallback to getting current branch directly if not in repo view
            use crate::git::GitRepository;
            let repo = GitRepository::open()?;
            let current_branch = repo
                .get_current_branch()
                .unwrap_or_else(|_| "HEAD".to_string());
            context.branch = BranchAnalyzer::analyze(&current_branch).unwrap_or_default();
        }

        // 3. Analyze commit range patterns
        if !repo_view.commits.is_empty() {
            context.range = WorkPatternAnalyzer::analyze_commit_range(&repo_view.commits);
        }

        // 4. Apply user-provided context overrides
        if let Some(ref work_ctx) = self.work_context {
            context.user_provided = Some(work_ctx.clone());
        }

        if let Some(ref branch_ctx) = self.branch_context {
            context.branch.description = branch_ctx.clone();
        }

        Ok(context)
    }

    /// Shows the context summary to the user.
    fn show_context_summary(&self, context: &crate::data::context::CommitContext) -> Result<()> {
        use crate::data::context::{VerbosityLevel, WorkPattern};

        println!("🔍 Context Analysis:");

        // Project context
        if !context.project.valid_scopes.is_empty() {
            let scope_names: Vec<&str> = context
                .project
                .valid_scopes
                .iter()
                .map(|s| s.name.as_str())
                .collect();
            println!("   📁 Valid scopes: {}", scope_names.join(", "));
        }

        // Branch context
        if context.branch.is_feature_branch {
            println!(
                "   🌿 Branch: {} ({})",
                context.branch.description, context.branch.work_type
            );
            if let Some(ref ticket) = context.branch.ticket_id {
                println!("   🎫 Ticket: {}", ticket);
            }
        }

        // Work pattern
        match context.range.work_pattern {
            WorkPattern::Sequential => println!("   🔄 Pattern: Sequential development"),
            WorkPattern::Refactoring => println!("   🧹 Pattern: Refactoring work"),
            WorkPattern::BugHunt => println!("   🐛 Pattern: Bug investigation"),
            WorkPattern::Documentation => println!("   📖 Pattern: Documentation updates"),
            WorkPattern::Configuration => println!("   ⚙️  Pattern: Configuration changes"),
            WorkPattern::Unknown => {}
        }

        // Verbosity level
        match context.suggested_verbosity() {
            VerbosityLevel::Comprehensive => {
                println!("   📝 Detail level: Comprehensive (significant changes detected)")
            }
            VerbosityLevel::Detailed => println!("   📝 Detail level: Detailed"),
            VerbosityLevel::Concise => println!("   📝 Detail level: Concise"),
        }

        // User context
        if let Some(ref user_ctx) = context.user_provided {
            println!("   👤 User context: {}", user_ctx);
        }

        println!();
        Ok(())
    }

    /// Shows model information from the actual AI client.
    fn show_model_info_from_client(
        &self,
        client: &crate::claude::client::ClaudeClient,
    ) -> Result<()> {
        use crate::claude::model_config::get_model_registry;

        println!("🤖 AI Model Configuration:");

        // Get actual metadata from the client
        let metadata = client.get_ai_client_metadata();
        let registry = get_model_registry();

        if let Some(spec) = registry.get_model_spec(&metadata.model) {
            // Highlight the API identifier portion in yellow
            if metadata.model != spec.api_identifier {
                println!(
                    "   📡 Model: {} → \x1b[33m{}\x1b[0m",
                    metadata.model, spec.api_identifier
                );
            } else {
                println!("   📡 Model: \x1b[33m{}\x1b[0m", metadata.model);
            }

            println!("   🏷️  Provider: {}", spec.provider);
            println!("   📊 Generation: {}", spec.generation);
            println!("   ⭐ Tier: {} ({})", spec.tier, {
                if let Some(tier_info) = registry.get_tier_info(&spec.provider, &spec.tier) {
                    &tier_info.description
                } else {
                    "No description available"
                }
            });
            println!("   📤 Max output tokens: {}", metadata.max_response_length);
            println!("   📥 Input context: {}", metadata.max_context_length);

            if let Some((ref key, ref value)) = metadata.active_beta {
                println!("   🔬 Beta header: {}: {}", key, value);
            }

            if spec.legacy {
                println!("   ⚠️  Legacy model (consider upgrading to newer version)");
            }
        } else {
            // Fallback to client metadata if not in registry
            println!("   📡 Model: \x1b[33m{}\x1b[0m", metadata.model);
            println!("   🏷️  Provider: {}", metadata.provider);
            println!("   ⚠️  Model not found in registry, using client metadata:");
            println!("   📤 Max output tokens: {}", metadata.max_response_length);
            println!("   📥 Input context: {}", metadata.max_context_length);
        }

        println!();
        Ok(())
    }

    /// Shows diagnostic information about loaded guidance files.
    fn show_guidance_files_status(
        &self,
        project_context: &crate::data::context::ProjectContext,
        context_dir: &std::path::Path,
    ) -> Result<()> {
        println!("📋 Project guidance files status:");

        // Check commit guidelines
        let guidelines_found = project_context.commit_guidelines.is_some();
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
            "❌ None found".to_string()
        };
        println!("   📝 Commit guidelines: {}", guidelines_source);

        // Check scopes
        let scopes_count = project_context.valid_scopes.len();
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
                "(source unknown + ecosystem defaults)".to_string()
            };
            format!("✅ {} ({} scopes)", source, scopes_count)
        } else {
            "❌ None found".to_string()
        };
        println!("   🎯 Valid scopes: {}", scopes_source);

        println!();
        Ok(())
    }

    /// Executes the twiddle command without AI, creating amendments with original messages.
    async fn execute_no_ai(&self) -> Result<()> {
        use crate::data::amendments::{Amendment, AmendmentFile};

        println!("📋 Generating amendments YAML without AI processing...");

        // Generate repository view to get all commits
        let repo_view = self.generate_repository_view().await?;

        // Create amendments with original commit messages (no AI improvements)
        let amendments: Vec<Amendment> = repo_view
            .commits
            .iter()
            .map(|commit| Amendment {
                commit: commit.hash.clone(),
                message: commit.original_message.clone(),
                summary: None,
            })
            .collect();

        let amendment_file = AmendmentFile { amendments };

        // Handle different output modes
        if let Some(save_path) = &self.save_only {
            amendment_file.save_to_file(save_path)?;
            println!("💾 Amendments saved to file");
            return Ok(());
        }

        // Handle amendments using the same flow as the AI-powered version
        if !amendment_file.amendments.is_empty() {
            // Create temporary file for amendments
            let temp_dir = tempfile::tempdir()?;
            let amendments_file = temp_dir.path().join("twiddle_amendments.yaml");
            amendment_file.save_to_file(&amendments_file)?;

            // Show file path and get user choice
            if !self.auto_apply
                && !self.handle_amendments_file(&amendments_file, &amendment_file)?
            {
                println!("❌ Amendment cancelled by user");
                return Ok(());
            }

            // Apply amendments (re-read from file to capture any user edits)
            self.apply_amendments_from_file(&amendments_file).await?;
            println!("✅ Commit messages applied successfully!");

            // Run post-twiddle check if --check flag is set
            if self.check {
                self.run_post_twiddle_check().await?;
            }
        } else {
            println!("✨ No commits found to process!");
        }

        Ok(())
    }

    /// Runs commit message validation after twiddle amendments are applied.
    /// If the check finds errors with suggestions, automatically applies the
    /// suggestions and re-checks, up to 3 retries.
    async fn run_post_twiddle_check(&self) -> Result<()> {
        use crate::data::amendments::AmendmentFile;

        const MAX_CHECK_RETRIES: u32 = 3;

        // Load guidelines, scopes, and Claude client once (they don't change between retries)
        let guidelines = self.load_check_guidelines()?;
        let valid_scopes = self.load_check_scopes();
        let beta = self
            .beta_header
            .as_deref()
            .map(parse_beta_header)
            .transpose()?;
        let claude_client = crate::claude::create_default_claude_client(self.model.clone(), beta)?;

        for attempt in 0..=MAX_CHECK_RETRIES {
            println!();
            if attempt == 0 {
                println!("🔍 Running commit message validation...");
            } else {
                println!(
                    "🔍 Re-checking commit messages (retry {}/{})...",
                    attempt, MAX_CHECK_RETRIES
                );
            }

            // Generate fresh repository view to get updated commit messages
            let mut repo_view = self.generate_repository_view().await?;

            if repo_view.commits.is_empty() {
                println!("⚠️  No commits to check");
                return Ok(());
            }

            println!("📊 Checking {} commits", repo_view.commits.len());

            // Refine detected scopes using file_patterns from scope definitions
            for commit in &mut repo_view.commits {
                commit.analysis.refine_scope(&valid_scopes);
            }

            if attempt == 0 {
                self.show_check_guidance_files_status(&guidelines, &valid_scopes);
            }

            // Run check
            let report = if repo_view.commits.len() > 1 {
                println!(
                    "🔄 Checking {} commits in parallel...",
                    repo_view.commits.len()
                );
                self.check_commits_map_reduce(
                    &claude_client,
                    &repo_view,
                    guidelines.as_deref(),
                    &valid_scopes,
                )
                .await?
            } else {
                println!("🤖 Analyzing commits with AI...");
                claude_client
                    .check_commits_with_scopes(
                        &repo_view,
                        guidelines.as_deref(),
                        &valid_scopes,
                        true,
                    )
                    .await?
            };

            // Output text report
            self.output_check_text_report(&report)?;

            // If no errors, we're done
            if !report.has_errors() {
                if report.has_warnings() {
                    println!("ℹ️  Some commit messages have minor warnings");
                } else {
                    println!("✅ All commit messages pass validation");
                }
                return Ok(());
            }

            // If we've exhausted retries, report and stop
            if attempt == MAX_CHECK_RETRIES {
                println!(
                    "⚠️  Some commit messages still have issues after {} retries",
                    MAX_CHECK_RETRIES
                );
                return Ok(());
            }

            // Build amendments from suggestions for failing commits
            let amendments = self.build_amendments_from_suggestions(&report, &repo_view);

            if amendments.is_empty() {
                println!(
                    "⚠️  Some commit messages have issues but no suggestions available to retry"
                );
                return Ok(());
            }

            // Apply the suggested amendments
            println!(
                "🔄 Applying {} suggested fix(es) and re-checking...",
                amendments.len()
            );
            let amendment_file = AmendmentFile { amendments };
            let temp_file = tempfile::NamedTempFile::new()
                .context("Failed to create temp file for retry amendments")?;
            amendment_file
                .save_to_file(temp_file.path())
                .context("Failed to save retry amendments")?;
            self.apply_amendments_from_file(temp_file.path()).await?;
        }

        Ok(())
    }

    /// Builds amendments from check report suggestions for failing commits.
    /// Resolves short hashes from the AI response to full 40-char hashes
    /// from the repository view.
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
                // Resolve short hash to full 40-char hash
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

    /// Loads commit guidelines for check (mirrors `CheckCommand::load_guidelines`).
    fn load_check_guidelines(&self) -> Result<Option<String>> {
        use std::fs;

        let context_dir = self
            .context_dir
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from(".omni-dev"));

        // Try local override first
        let local_path = context_dir.join("local").join("commit-guidelines.md");
        if local_path.exists() {
            let content = fs::read_to_string(&local_path)
                .with_context(|| format!("Failed to read guidelines: {:?}", local_path))?;
            return Ok(Some(content));
        }

        // Try project-level guidelines
        let project_path = context_dir.join("commit-guidelines.md");
        if project_path.exists() {
            let content = fs::read_to_string(&project_path)
                .with_context(|| format!("Failed to read guidelines: {:?}", project_path))?;
            return Ok(Some(content));
        }

        // Try global guidelines
        if let Some(home) = dirs::home_dir() {
            let home_path = home.join(".omni-dev").join("commit-guidelines.md");
            if home_path.exists() {
                let content = fs::read_to_string(&home_path)
                    .with_context(|| format!("Failed to read guidelines: {:?}", home_path))?;
                return Ok(Some(content));
            }
        }

        Ok(None)
    }

    /// Loads valid scopes for check with ecosystem defaults.
    fn load_check_scopes(&self) -> Vec<crate::data::context::ScopeDefinition> {
        let context_dir = self
            .context_dir
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from(".omni-dev"));
        crate::claude::context::load_project_scopes(&context_dir, &std::path::PathBuf::from("."))
    }

    /// Shows guidance files status for check.
    fn show_check_guidance_files_status(
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
        println!("   📝 Commit guidelines: {}", guidelines_source);

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
            format!("✅ {} ({} scopes)", source, scopes_count)
        } else {
            "⚪ None found (any scope accepted)".to_string()
        };
        println!("   🎯 Valid scopes: {}", scopes_source);

        println!();
    }

    /// Checks commits using batched parallel map-reduce.
    async fn check_commits_map_reduce(
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

        if batch_plan.batches.len() < total_commits {
            println!(
                "   📦 Grouped {} commits into {} batches by token budget",
                total_commits,
                batch_plan.batches.len()
            );
        }

        let semaphore = Arc::new(tokio::sync::Semaphore::new(self.concurrency));
        let completed = Arc::new(AtomicUsize::new(0));

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
                        .check_commits_with_scopes(&batch_view, guidelines, valid_scopes, true)
                        .await;

                    match result {
                        Ok(report) => {
                            let done =
                                completed.fetch_add(batch_size, Ordering::Relaxed) + batch_size;
                            println!("   ✅ {}/{} commits checked", done, total_commits);

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
                            eprintln!(
                                "warning: batch of {} failed, retrying individually: {e}",
                                batch_size
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
                                        true,
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
                                println!("   ✅ {}/{} commits checked", done, total_commits);
                            }
                            Ok(items)
                        }
                        Err(e) => Err(e),
                    }
                }
            })
            .collect();

        let results = futures::future::join_all(futs).await;

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

        // Coherence pass: skip when all commits were in a single batch
        let single_batch = batch_plan.batches.len() <= 1;
        if !self.no_coherence && !single_batch && successes.len() >= 2 {
            println!("🔗 Running cross-commit coherence pass...");
            match claude_client
                .refine_checks_coherence(&successes, full_repo_view)
                .await
            {
                Ok(refined) => return Ok(refined),
                Err(e) => {
                    eprintln!("warning: coherence pass failed, using individual results: {e}");
                }
            }
        }

        Ok(CheckReport::new(
            successes.into_iter().map(|(r, _)| r).collect(),
        ))
    }

    /// Outputs the text format check report (mirrors `CheckCommand::output_text_report`).
    fn output_check_text_report(&self, report: &crate::data::check::CheckReport) -> Result<()> {
        use crate::data::check::IssueSeverity;

        println!();

        for result in &report.commits {
            // Skip passing commits
            if result.passes {
                continue;
            }

            // Determine icon
            let icon = if result
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

            // Print suggestion if available
            if let Some(suggestion) = &result.suggestion {
                println!();
                println!("   Suggested message:");
                for line in suggestion.message.lines() {
                    println!("      {}", line);
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
}
