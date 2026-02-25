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

    /// Maximum number of concurrent AI requests (default: 4).
    #[arg(long, default_value = "4")]
    pub concurrency: usize,

    /// Deprecated: use --concurrency instead.
    #[arg(long, hide = true)]
    pub batch_size: Option<usize>,

    /// Disables the cross-commit coherence pass.
    #[arg(long)]
    pub no_coherence: bool,

    /// Skips AI processing and only outputs repository YAML.
    #[arg(long)]
    pub no_ai: bool,

    /// Ignores existing commit messages and generates fresh ones based solely on diffs.
    /// This is the default behavior.
    #[arg(long, conflicts_with = "refine")]
    pub fresh: bool,

    /// Uses existing commit messages as a starting point for AI refinement
    /// instead of generating fresh messages from scratch.
    #[arg(long, conflicts_with = "fresh")]
    pub refine: bool,

    /// Runs commit message validation after applying amendments.
    #[arg(long)]
    pub check: bool,
}

impl TwiddleCommand {
    /// Returns true when existing messages should be hidden from the AI.
    /// Fresh is the default; `--refine` overrides it.
    fn is_fresh(&self) -> bool {
        !self.refine
    }

    /// Executes the twiddle command with contextual intelligence.
    pub async fn execute(mut self) -> Result<()> {
        // Resolve deprecated --batch-size into --concurrency
        if let Some(bs) = self.batch_size {
            eprintln!("warning: --batch-size is deprecated; use --concurrency instead");
            self.concurrency = bs;
        }

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
        if self.refine {
            println!("🔄 Refine mode: using existing commit messages as starting point...");
        }
        if use_contextual && context.is_some() {
            println!("🤖 Analyzing commits with enhanced contextual intelligence...");
        } else {
            println!("🤖 Analyzing commits with Claude AI...");
        }

        let amendments = if let Some(ctx) = context {
            claude_client
                .generate_contextual_amendments_with_options(&full_repo_view, &ctx, self.is_fresh())
                .await?
        } else {
            claude_client
                .generate_amendments_with_options(&full_repo_view, self.is_fresh())
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
            {
                use std::io::IsTerminal;
                if !self.auto_apply
                    && !self.handle_amendments_file(
                        &amendments_file,
                        &amendments,
                        std::io::stdin().is_terminal(),
                        &mut std::io::BufReader::new(std::io::stdin()),
                    )?
                {
                    println!("❌ Amendment cancelled by user");
                    return Ok(());
                }
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

        if self.refine {
            println!("🔄 Refine mode: using existing commit messages as starting point...");
        }

        let total_commits = full_repo_view.commits.len();
        println!(
            "🔄 Processing {total_commits} commits in parallel (concurrency: {concurrency})..."
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
        let fresh = self.is_fresh();

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
                            println!("   ✅ {done}/{total_commits} commits processed");

                            let items: Vec<_> = amendment_file
                                .amendments
                                .into_iter()
                                .map(|a| {
                                    let summary = a.summary.clone().unwrap_or_default();
                                    (a, summary)
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
                                        let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
                                        println!("   ✅ {done}/{total_commits} commits processed");
                                    }
                                    Err(e) => {
                                        eprintln!("warning: failed to process commit: {e}");
                                        // Print the full error chain for debugging using anyhow's chain()
                                        for (i, cause) in e.chain().skip(1).enumerate() {
                                            eprintln!("  caused by [{i}]: {cause}");
                                        }
                                        failed_indices.push(idx);
                                        println!("   ❌ commit processing failed");
                                    }
                                }
                            }
                            Ok((items, failed_indices))
                        }
                        Err(e) => {
                            // Single-commit batch failed; record the index so the user can retry
                            let idx = batch_indices[0];
                            eprintln!("warning: failed to process commit: {e}");
                            // Print the full error chain for debugging using anyhow's chain()
                            for (i, cause) in e.chain().skip(1).enumerate() {
                                eprintln!("  caused by [{i}]: {cause}");
                            }
                            let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
                            println!("   ❌ {done}/{total_commits} commits processed (failed)");
                            Ok((vec![], vec![idx]))
                        }
                    }
                }
            })
            .collect();

        let results = futures::future::join_all(futs).await;

        // Flatten batch results
        let mut successes: Vec<(crate::data::amendments::Amendment, String)> = Vec::new();
        let mut failed_indices: Vec<usize> = Vec::new();

        for (result, batch) in results.into_iter().zip(&batch_plan.batches) {
            match result {
                Ok((items, failed)) => {
                    successes.extend(items);
                    failed_indices.extend(failed);
                }
                Err(e) => {
                    eprintln!("warning: batch processing error: {e}");
                    failed_indices.extend(&batch.commit_indices);
                }
            }
        }

        // Offer interactive retry for commits that failed
        if !failed_indices.is_empty() {
            use std::io::IsTerminal;
            self.run_interactive_retry_generate_amendments(
                &mut failed_indices,
                &full_repo_view,
                &claude_client,
                context.as_ref(),
                fresh,
                &mut successes,
                std::io::stdin().is_terminal(),
                &mut std::io::BufReader::new(std::io::stdin()),
            )
            .await?;
        }

        if !failed_indices.is_empty() {
            eprintln!(
                "warning: {} commit(s) ultimately failed to process",
                failed_indices.len()
            );
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

            {
                use std::io::IsTerminal;
                if !self.auto_apply
                    && !self.handle_amendments_file(
                        &amendments_file,
                        &all_amendments,
                        std::io::stdin().is_terminal(),
                        &mut std::io::BufReader::new(std::io::stdin()),
                    )?
                {
                    println!("❌ Amendment cancelled by user");
                    return Ok(());
                }
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
    ///
    /// `is_terminal` and `reader` are injected so tests can drive the function
    /// without blocking on real stdin.
    fn handle_amendments_file(
        &self,
        amendments_file: &std::path::Path,
        amendments: &crate::data::amendments::AmendmentFile,
        is_terminal: bool,
        reader: &mut (dyn std::io::BufRead + Send),
    ) -> Result<bool> {
        use std::io::{self, Write};

        println!(
            "\n📝 Found {} commits that could be improved.",
            amendments.amendments.len()
        );
        println!("💾 Amendments saved to: {}", amendments_file.display());
        println!();

        if !is_terminal {
            eprintln!("warning: stdin is not interactive, cannot prompt for amendments");
            return Ok(false);
        }

        loop {
            print!("❓ [A]pply amendments, [S]how file, [E]dit file, or [Q]uit? [A/s/e/q] ");
            io::stdout().flush()?;

            let mut input = String::new();
            let bytes = reader.read_line(&mut input)?;
            if bytes == 0 {
                eprintln!("warning: stdin closed, cancelling amendments");
                return Ok(false);
            }

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

        println!("{contents}");
        println!("─────────────────────────────");

        Ok(())
    }

    /// Opens the amendments file in an external editor.
    fn edit_amendments_file(&self, amendments_file: &std::path::Path) -> Result<()> {
        use std::env;
        use std::io::{self, Write};
        use std::process::Command;

        // Try to get editor from environment variables
        let editor = if let Ok(e) = env::var("OMNI_DEV_EDITOR").or_else(|_| env::var("EDITOR")) {
            e
        } else {
            // Prompt user for editor if neither environment variable is set
            println!("🔧 Neither OMNI_DEV_EDITOR nor EDITOR environment variables are defined.");
            print!("Please enter the command to use as your editor: ");
            io::stdout().flush().context("Failed to flush stdout")?;

            let mut input = String::new();
            io::stdin()
                .read_line(&mut input)
                .context("Failed to read user input")?;
            input.trim().to_string()
        };

        if editor.is_empty() {
            println!("❌ No editor specified. Returning to menu.");
            return Ok(());
        }

        println!("📝 Opening amendments file in editor: {editor}");

        let (editor_cmd, args) = super::formatting::parse_editor_command(&editor);

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
                println!("❌ Failed to execute editor '{editor}': {e}");
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
        use crate::claude::context::{
            BranchAnalyzer, FileAnalyzer, ProjectDiscovery, WorkPatternAnalyzer,
        };
        use crate::data::context::CommitContext;

        let mut context = CommitContext::new();

        // 1. Discover project context
        let (context_dir, dir_source) =
            crate::claude::context::resolve_context_dir_with_source(self.context_dir.as_deref());

        // ProjectDiscovery takes repo root and context directory
        let repo_root = std::path::PathBuf::from(".");
        let discovery = ProjectDiscovery::new(repo_root, context_dir.clone());
        debug!(context_dir = ?context_dir, "Using context directory");
        match discovery.discover() {
            Ok(project_context) => {
                debug!("Discovery successful");

                // Show diagnostic information about loaded guidance files
                self.show_guidance_files_status(&project_context, &context_dir, &dir_source)?;

                context.project = project_context;
            }
            Err(e) => {
                debug!(error = %e, "Discovery failed");
                context.project = crate::data::context::ProjectContext::default();
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

        // 3.5. Analyze file-level context
        if !repo_view.commits.is_empty() {
            context.files = FileAnalyzer::analyze_commits(&repo_view.commits);
        }

        // 4. Apply user-provided context overrides
        if let Some(ref work_ctx) = self.work_context {
            context.user_provided = Some(work_ctx.clone());
        }

        if let Some(ref branch_ctx) = self.branch_context {
            context.branch.description.clone_from(branch_ctx);
        }

        Ok(context)
    }

    /// Shows the context summary to the user.
    fn show_context_summary(&self, context: &crate::data::context::CommitContext) -> Result<()> {
        println!("🔍 Context Analysis:");

        // Project context
        if !context.project.valid_scopes.is_empty() {
            println!(
                "   📁 Valid scopes: {}",
                format_scope_list(&context.project.valid_scopes)
            );
        }

        // Branch context
        if context.branch.is_feature_branch {
            println!(
                "   🌿 Branch: {} ({})",
                context.branch.description, context.branch.work_type
            );
            if let Some(ref ticket) = context.branch.ticket_id {
                println!("   🎫 Ticket: {ticket}");
            }
        }

        // Work pattern
        if let Some(label) = format_work_pattern(&context.range.work_pattern) {
            println!("   {label}");
        }

        // File analysis
        if let Some(label) = super::formatting::format_file_analysis(&context.files) {
            println!("   {label}");
        }

        // Verbosity level
        println!(
            "   {}",
            format_verbosity_level(context.suggested_verbosity())
        );

        // User context
        if let Some(ref user_ctx) = context.user_provided {
            println!("   👤 User context: {user_ctx}");
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
                println!("   🔬 Beta header: {key}: {value}");
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
        dir_source: &crate::claude::context::ConfigDirSource,
    ) -> Result<()> {
        use crate::claude::context::{config_source_label, ConfigSourceLabel};

        println!("📋 Project guidance files status:");
        println!("   📂 Config dir: {} ({dir_source})", context_dir.display());

        // Check commit guidelines
        let guidelines_source = if project_context.commit_guidelines.is_some() {
            match config_source_label(context_dir, "commit-guidelines.md") {
                ConfigSourceLabel::NotFound => "✅ (source unknown)".to_string(),
                label => format!("✅ {label}"),
            }
        } else {
            "❌ None found".to_string()
        };
        println!("   📝 Commit guidelines: {guidelines_source}");

        // Check scopes
        let scopes_count = project_context.valid_scopes.len();
        let scopes_source = if scopes_count > 0 {
            match config_source_label(context_dir, "scopes.yaml") {
                ConfigSourceLabel::NotFound => {
                    format!("✅ (source unknown + ecosystem defaults) ({scopes_count} scopes)")
                }
                label => format!("✅ {label} ({scopes_count} scopes)"),
            }
        } else {
            "❌ None found".to_string()
        };
        println!("   🎯 Valid scopes: {scopes_source}");

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
            {
                use std::io::IsTerminal;
                if !self.auto_apply
                    && !self.handle_amendments_file(
                        &amendments_file,
                        &amendment_file,
                        std::io::stdin().is_terminal(),
                        &mut std::io::BufReader::new(std::io::stdin()),
                    )?
                {
                    println!("❌ Amendment cancelled by user");
                    return Ok(());
                }
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
                println!("🔍 Re-checking commit messages (retry {attempt}/{MAX_CHECK_RETRIES})...");
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
                    "⚠️  Some commit messages still have issues after {MAX_CHECK_RETRIES} retries"
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

    /// Loads commit guidelines for check via the standard resolution chain.
    fn load_check_guidelines(&self) -> Result<Option<String>> {
        let context_dir = crate::claude::context::resolve_context_dir(self.context_dir.as_deref());
        crate::claude::context::load_config_content(&context_dir, "commit-guidelines.md")
    }

    /// Loads valid scopes for check with ecosystem defaults.
    fn load_check_scopes(&self) -> Vec<crate::data::context::ScopeDefinition> {
        let context_dir = crate::claude::context::resolve_context_dir(self.context_dir.as_deref());
        crate::claude::context::load_project_scopes(&context_dir, &std::path::PathBuf::from("."))
    }

    /// Shows guidance files status for check.
    fn show_check_guidance_files_status(
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
                            println!("   ✅ {done}/{total_commits} commits checked");

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
                                        true,
                                    )
                                    .await;
                                match single_result {
                                    Ok(report) => {
                                        if let Some(r) = report.commits.into_iter().next() {
                                            let summary = r.summary.clone().unwrap_or_default();
                                            items.push((r, summary));
                                        }
                                        let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
                                        println!("   ✅ {done}/{total_commits} commits checked");
                                    }
                                    Err(e) => {
                                        eprintln!("warning: failed to check commit: {e}");
                                        failed_indices.push(idx);
                                        println!("   ❌ commit check failed");
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
                            println!("   ❌ {done}/{total_commits} commits checked (failed)");
                            Ok((vec![], vec![idx]))
                        }
                    }
                }
            })
            .collect();

        let results = futures::future::join_all(futs).await;

        let mut successes: Vec<(CommitCheckResult, String)> = Vec::new();
        let mut failed_indices: Vec<usize> = Vec::new();

        for (result, batch) in results.into_iter().zip(&batch_plan.batches) {
            match result {
                Ok((items, failed)) => {
                    successes.extend(items);
                    failed_indices.extend(failed);
                }
                Err(e) => {
                    eprintln!("warning: batch processing error: {e}");
                    failed_indices.extend(&batch.commit_indices);
                }
            }
        }

        // Offer interactive retry for commits that failed
        if !failed_indices.is_empty() {
            use std::io::IsTerminal;
            if std::io::stdin().is_terminal() {
                self.run_interactive_retry_twiddle_check(
                    &mut failed_indices,
                    full_repo_view,
                    claude_client,
                    guidelines,
                    valid_scopes,
                    &mut successes,
                    &mut std::io::BufReader::new(std::io::stdin()),
                )
                .await?;
            } else {
                eprintln!(
                    "warning: stdin is not interactive, skipping retry prompt for {} failed commit(s)",
                    failed_indices.len()
                );
            }
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

    /// Prompts the user to retry or skip failed commits, updating `failed_indices` and `successes`.
    ///
    /// Accepts `reader` for stdin injection so the interactive loop can be unit-tested.
    #[allow(clippy::too_many_arguments)]
    async fn run_interactive_retry_twiddle_check(
        &self,
        failed_indices: &mut Vec<usize>,
        full_repo_view: &crate::data::RepositoryView,
        claude_client: &crate::claude::client::ClaudeClient,
        guidelines: Option<&str>,
        valid_scopes: &[crate::data::context::ScopeDefinition],
        successes: &mut Vec<(crate::data::check::CommitCheckResult, String)>,
        reader: &mut (dyn std::io::BufRead + Send),
    ) -> Result<()> {
        use std::io::Write as _;
        println!("\n⚠️  {} commit(s) failed to check:", failed_indices.len());
        for &idx in failed_indices.iter() {
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
            let bytes = reader.read_line(&mut input)?;
            if bytes == 0 {
                eprintln!("warning: stdin closed, skipping failed commit(s)");
                break;
            }
            match input.trim().to_lowercase().as_str() {
                "r" | "retry" | "" => {
                    let mut still_failed = Vec::new();
                    for &idx in failed_indices.iter() {
                        let single_view =
                            full_repo_view.single_commit_view(&full_repo_view.commits[idx]);
                        match claude_client
                            .check_commits_with_scopes(&single_view, guidelines, valid_scopes, true)
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
                    *failed_indices = still_failed;
                    if failed_indices.is_empty() {
                        println!("✅ All retried commits succeeded.");
                        break;
                    }
                    println!("\n⚠️  {} commit(s) still failed:", failed_indices.len());
                    for &idx in failed_indices.iter() {
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
        Ok(())
    }

    /// Prompts the user to retry or skip commits that failed amendment generation,
    /// updating `failed_indices` and `successes` in place.
    ///
    /// `is_terminal` and `reader` are injected so tests can drive the function
    /// without blocking on real stdin.
    #[allow(clippy::too_many_arguments)]
    async fn run_interactive_retry_generate_amendments(
        &self,
        failed_indices: &mut Vec<usize>,
        full_repo_view: &crate::data::RepositoryView,
        claude_client: &crate::claude::client::ClaudeClient,
        context: Option<&crate::data::context::CommitContext>,
        fresh: bool,
        successes: &mut Vec<(crate::data::amendments::Amendment, String)>,
        is_terminal: bool,
        reader: &mut (dyn std::io::BufRead + Send),
    ) -> Result<()> {
        use std::io::Write as _;
        println!(
            "\n⚠️  {} commit(s) failed to process:",
            failed_indices.len()
        );
        for &idx in failed_indices.iter() {
            let commit = &full_repo_view.commits[idx];
            let subject = commit
                .original_message
                .lines()
                .next()
                .unwrap_or("(no message)");
            println!("  - {}: {}", &commit.hash[..8], subject);
        }
        if !is_terminal {
            eprintln!(
                "warning: stdin is not interactive, skipping retry prompt for {} failed commit(s)",
                failed_indices.len()
            );
            return Ok(());
        }
        loop {
            print!("\n❓ [R]etry failed commits, or [S]kip? [R/s] ");
            std::io::stdout().flush()?;
            let mut input = String::new();
            let bytes = reader.read_line(&mut input)?;
            if bytes == 0 {
                eprintln!("warning: stdin closed, skipping failed commit(s)");
                break;
            }
            match input.trim().to_lowercase().as_str() {
                "r" | "retry" | "" => {
                    let mut still_failed = Vec::new();
                    for &idx in failed_indices.iter() {
                        let single_view =
                            full_repo_view.single_commit_view(&full_repo_view.commits[idx]);
                        let result = if let Some(ctx) = context {
                            claude_client
                                .generate_contextual_amendments_with_options(
                                    &single_view,
                                    ctx,
                                    fresh,
                                )
                                .await
                        } else {
                            claude_client
                                .generate_amendments_with_options(&single_view, fresh)
                                .await
                        };
                        match result {
                            Ok(af) => {
                                if let Some(a) = af.amendments.into_iter().next() {
                                    let summary = a.summary.clone().unwrap_or_default();
                                    successes.push((a, summary));
                                }
                            }
                            Err(e) => {
                                eprintln!("warning: still failed: {e}");
                                still_failed.push(idx);
                            }
                        }
                    }
                    *failed_indices = still_failed;
                    if failed_indices.is_empty() {
                        println!("✅ All retried commits succeeded.");
                        break;
                    }
                    println!("\n⚠️  {} commit(s) still failed:", failed_indices.len());
                    for &idx in failed_indices.iter() {
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
        Ok(())
    }

    /// Outputs the text format check report (mirrors `CheckCommand::output_text_report`).
    fn output_check_text_report(&self, report: &crate::data::check::CheckReport) -> Result<()> {
        println!();

        for result in &report.commits {
            // Skip passing commits
            if result.passes {
                continue;
            }

            let icon = super::formatting::determine_commit_icon(result.passes, &result.issues);
            let short_hash = super::formatting::truncate_hash(&result.hash);

            println!("{} {} - \"{}\"", icon, short_hash, result.message);

            // Print issues
            for issue in &result.issues {
                let severity_str = super::formatting::format_severity_label(issue.severity);

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
                    println!("      {line}");
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

// --- Extracted pure functions ---

/// Formats a work pattern as a display label with emoji.
///
/// Returns `None` for `WorkPattern::Unknown` since it should not be displayed.
fn format_work_pattern(pattern: &crate::data::context::WorkPattern) -> Option<&'static str> {
    use crate::data::context::WorkPattern;
    match pattern {
        WorkPattern::Sequential => Some("\u{1f504} Pattern: Sequential development"),
        WorkPattern::Refactoring => Some("\u{1f9f9} Pattern: Refactoring work"),
        WorkPattern::BugHunt => Some("\u{1f41b} Pattern: Bug investigation"),
        WorkPattern::Documentation => Some("\u{1f4d6} Pattern: Documentation updates"),
        WorkPattern::Configuration => Some("\u{2699}\u{fe0f}  Pattern: Configuration changes"),
        WorkPattern::Unknown => None,
    }
}

/// Formats a verbosity level as a display label with emoji.
fn format_verbosity_level(level: crate::data::context::VerbosityLevel) -> &'static str {
    use crate::data::context::VerbosityLevel;
    match level {
        VerbosityLevel::Comprehensive => {
            "\u{1f4dd} Detail level: Comprehensive (significant changes detected)"
        }
        VerbosityLevel::Detailed => "\u{1f4dd} Detail level: Detailed",
        VerbosityLevel::Concise => "\u{1f4dd} Detail level: Concise",
    }
}

/// Formats a list of scope definitions as a comma-separated string of names.
fn format_scope_list(scopes: &[crate::data::context::ScopeDefinition]) -> String {
    scopes
        .iter()
        .map(|s| s.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::data::context::{ScopeDefinition, VerbosityLevel, WorkPattern};

    // --- format_work_pattern ---

    #[test]
    fn work_pattern_sequential() {
        let result = format_work_pattern(&WorkPattern::Sequential);
        assert!(result.is_some());
        assert!(result.unwrap().contains("Sequential development"));
    }

    #[test]
    fn work_pattern_refactoring() {
        let result = format_work_pattern(&WorkPattern::Refactoring);
        assert!(result.is_some());
        assert!(result.unwrap().contains("Refactoring work"));
    }

    #[test]
    fn work_pattern_bug_hunt() {
        let result = format_work_pattern(&WorkPattern::BugHunt);
        assert!(result.is_some());
        assert!(result.unwrap().contains("Bug investigation"));
    }

    #[test]
    fn work_pattern_docs() {
        let result = format_work_pattern(&WorkPattern::Documentation);
        assert!(result.is_some());
        assert!(result.unwrap().contains("Documentation updates"));
    }

    #[test]
    fn work_pattern_config() {
        let result = format_work_pattern(&WorkPattern::Configuration);
        assert!(result.is_some());
        assert!(result.unwrap().contains("Configuration changes"));
    }

    #[test]
    fn work_pattern_unknown() {
        assert!(format_work_pattern(&WorkPattern::Unknown).is_none());
    }

    // --- format_verbosity_level ---

    #[test]
    fn verbosity_comprehensive() {
        let label = format_verbosity_level(VerbosityLevel::Comprehensive);
        assert!(label.contains("Comprehensive"));
        assert!(label.contains("significant changes"));
    }

    #[test]
    fn verbosity_detailed() {
        let label = format_verbosity_level(VerbosityLevel::Detailed);
        assert!(label.contains("Detailed"));
    }

    #[test]
    fn verbosity_concise() {
        let label = format_verbosity_level(VerbosityLevel::Concise);
        assert!(label.contains("Concise"));
    }

    // --- format_scope_list ---

    #[test]
    fn scope_list_single() {
        let scopes = vec![ScopeDefinition {
            name: "cli".to_string(),
            description: String::new(),
            examples: vec![],
            file_patterns: vec![],
        }];
        assert_eq!(format_scope_list(&scopes), "cli");
    }

    #[test]
    fn scope_list_multiple() {
        let scopes = vec![
            ScopeDefinition {
                name: "cli".to_string(),
                description: String::new(),
                examples: vec![],
                file_patterns: vec![],
            },
            ScopeDefinition {
                name: "git".to_string(),
                description: String::new(),
                examples: vec![],
                file_patterns: vec![],
            },
            ScopeDefinition {
                name: "docs".to_string(),
                description: String::new(),
                examples: vec![],
                file_patterns: vec![],
            },
        ];
        assert_eq!(format_scope_list(&scopes), "cli, git, docs");
    }

    // --- resolve_context_dir ---

    #[test]
    fn context_dir_default() {
        let result = crate::claude::context::resolve_context_dir(None);
        // Walk-up may find .omni-dev in the real repo, or fall back to ".omni-dev"
        assert!(
            result.ends_with(".omni-dev"),
            "expected path ending in .omni-dev, got {result:?}"
        );
    }

    #[test]
    fn context_dir_override() {
        let custom = std::path::PathBuf::from("custom-dir");
        let result = crate::claude::context::resolve_context_dir(Some(&custom));
        assert_eq!(result, custom);
    }

    // --- is_fresh ---

    fn parse_twiddle(args: &[&str]) -> TwiddleCommand {
        let mut full_args = vec!["twiddle"];
        full_args.extend_from_slice(args);
        TwiddleCommand::try_parse_from(full_args).unwrap()
    }

    #[test]
    fn default_is_fresh() {
        let cmd = parse_twiddle(&[]);
        assert!(cmd.is_fresh(), "default should be fresh mode");
    }

    #[test]
    fn refine_disables_fresh() {
        let cmd = parse_twiddle(&["--refine"]);
        assert!(!cmd.is_fresh(), "--refine should disable fresh mode");
    }

    #[test]
    fn explicit_fresh_is_fresh() {
        let cmd = parse_twiddle(&["--fresh"]);
        assert!(cmd.is_fresh(), "--fresh should be fresh mode");
    }

    #[test]
    fn fresh_and_refine_conflict() {
        let result = TwiddleCommand::try_parse_from(["twiddle", "--fresh", "--refine"]);
        assert!(result.is_err(), "--fresh and --refine should conflict");
    }

    // --- check_commits_map_reduce (success paths via mock client) ---

    fn make_twiddle_cmd() -> TwiddleCommand {
        TwiddleCommand {
            commit_range: None,
            model: None,
            beta_header: None,
            auto_apply: false,
            save_only: None,
            use_context: false,
            context_dir: None,
            work_context: None,
            branch_context: None,
            no_context: true,
            concurrency: 4,
            batch_size: None,
            no_coherence: true,
            no_ai: false,
            fresh: false,
            refine: false,
            check: false,
        }
    }

    fn make_twiddle_commit(hash: &str) -> (crate::git::CommitInfo, tempfile::NamedTempFile) {
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
                file_diffs: Vec::new(),
            },
        };
        (commit, tmp)
    }

    fn make_twiddle_repo_view(commits: Vec<crate::git::CommitInfo>) -> crate::data::RepositoryView {
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

    fn twiddle_check_yaml(hash: &str) -> String {
        format!("checks:\n  - commit: {hash}\n    passes: true\n    issues: []\n")
    }

    fn make_mock_client(
        responses: Vec<anyhow::Result<String>>,
    ) -> crate::claude::client::ClaudeClient {
        crate::claude::client::ClaudeClient::new(Box::new(
            crate::claude::test_utils::ConfigurableMockAiClient::new(responses),
        ))
    }

    #[tokio::test]
    async fn check_commits_map_reduce_single_commit_succeeds() {
        // Happy path: one commit, batch succeeds on first attempt.
        let (commit, _tmp) = make_twiddle_commit("abc00000");
        let cmd = make_twiddle_cmd();
        let repo_view = make_twiddle_repo_view(vec![commit]);
        let client = make_mock_client(vec![Ok(twiddle_check_yaml("abc00000"))]);
        let result = cmd
            .check_commits_map_reduce(&client, &repo_view, None, &[])
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().commits.len(), 1);
    }

    #[tokio::test]
    async fn check_commits_map_reduce_batch_fails_split_retry_both_succeed() {
        // Two commits in one batch. Batch fails (3 retries), then each commit
        // succeeds individually via split-and-retry. No stdin interaction since
        // failed_indices stays empty after both retries succeed.
        let (c1, _t1) = make_twiddle_commit("abc00000");
        let (c2, _t2) = make_twiddle_commit("def00000");
        let cmd = make_twiddle_cmd();
        let repo_view = make_twiddle_repo_view(vec![c1, c2]);
        let mut responses: Vec<anyhow::Result<String>> =
            (0..3).map(|_| Err(anyhow::anyhow!("batch fail"))).collect();
        responses.push(Ok(twiddle_check_yaml("abc00000")));
        responses.push(Ok(twiddle_check_yaml("def00000")));
        let client = make_mock_client(responses);
        let result = cmd
            .check_commits_map_reduce(&client, &repo_view, None, &[])
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().commits.len(), 2);
    }

    // --- run_interactive_retry_twiddle_check ---

    #[tokio::test]
    async fn interactive_retry_twiddle_skip_immediately() {
        // "s" input → loop exits without calling the AI client at all.
        let (commit, _tmp) = make_twiddle_commit("abc00000");
        let cmd = make_twiddle_cmd();
        let repo_view = make_twiddle_repo_view(vec![commit]);
        let client = make_mock_client(vec![]);
        let mut failed = vec![0usize];
        let mut successes = vec![];
        let mut stdin = std::io::Cursor::new(b"s\n" as &[u8]);
        cmd.run_interactive_retry_twiddle_check(
            &mut failed,
            &repo_view,
            &client,
            None,
            &[],
            &mut successes,
            &mut stdin,
        )
        .await
        .unwrap();
        assert_eq!(
            failed,
            vec![0],
            "skip should leave failed_indices unchanged"
        );
        assert!(successes.is_empty());
    }

    #[tokio::test]
    async fn interactive_retry_twiddle_retry_succeeds() {
        // "r" input → retries the failed commit, which succeeds.
        let (commit, _tmp) = make_twiddle_commit("abc00000");
        let cmd = make_twiddle_cmd();
        let repo_view = make_twiddle_repo_view(vec![commit]);
        let client = make_mock_client(vec![Ok(twiddle_check_yaml("abc00000"))]);
        let mut failed = vec![0usize];
        let mut successes = vec![];
        let mut stdin = std::io::Cursor::new(b"r\n" as &[u8]);
        cmd.run_interactive_retry_twiddle_check(
            &mut failed,
            &repo_view,
            &client,
            None,
            &[],
            &mut successes,
            &mut stdin,
        )
        .await
        .unwrap();
        assert!(
            failed.is_empty(),
            "retry succeeded → failed_indices cleared"
        );
        assert_eq!(successes.len(), 1);
    }

    #[tokio::test]
    async fn interactive_retry_twiddle_default_input_retries() {
        // Empty input (just Enter) is treated as "r" (retry).
        let (commit, _tmp) = make_twiddle_commit("abc00000");
        let cmd = make_twiddle_cmd();
        let repo_view = make_twiddle_repo_view(vec![commit]);
        let client = make_mock_client(vec![Ok(twiddle_check_yaml("abc00000"))]);
        let mut failed = vec![0usize];
        let mut successes = vec![];
        let mut stdin = std::io::Cursor::new(b"\n" as &[u8]);
        cmd.run_interactive_retry_twiddle_check(
            &mut failed,
            &repo_view,
            &client,
            None,
            &[],
            &mut successes,
            &mut stdin,
        )
        .await
        .unwrap();
        assert!(failed.is_empty());
        assert_eq!(successes.len(), 1);
    }

    #[tokio::test]
    async fn interactive_retry_twiddle_still_fails_then_skip() {
        // "r" → retry fails → still in failed_indices → "s" → skip.
        let (commit, _tmp) = make_twiddle_commit("abc00000");
        let cmd = make_twiddle_cmd();
        let repo_view = make_twiddle_repo_view(vec![commit]);
        // Retry attempt hits max_retries=2 (3 total attempts).
        let responses = (0..3).map(|_| Err(anyhow::anyhow!("mock fail"))).collect();
        let client = make_mock_client(responses);
        let mut failed = vec![0usize];
        let mut successes = vec![];
        let mut stdin = std::io::Cursor::new(b"r\ns\n" as &[u8]);
        cmd.run_interactive_retry_twiddle_check(
            &mut failed,
            &repo_view,
            &client,
            None,
            &[],
            &mut successes,
            &mut stdin,
        )
        .await
        .unwrap();
        assert_eq!(failed, vec![0], "commit still failed after retry");
        assert!(successes.is_empty());
    }

    #[tokio::test]
    async fn interactive_retry_twiddle_invalid_input_then_skip() {
        // Unrecognised input → "please enter r or s" message → "s" exits.
        let (commit, _tmp) = make_twiddle_commit("abc00000");
        let cmd = make_twiddle_cmd();
        let repo_view = make_twiddle_repo_view(vec![commit]);
        let client = make_mock_client(vec![]);
        let mut failed = vec![0usize];
        let mut successes = vec![];
        let mut stdin = std::io::Cursor::new(b"x\ns\n" as &[u8]);
        cmd.run_interactive_retry_twiddle_check(
            &mut failed,
            &repo_view,
            &client,
            None,
            &[],
            &mut successes,
            &mut stdin,
        )
        .await
        .unwrap();
        assert_eq!(failed, vec![0]);
        assert!(successes.is_empty());
    }

    #[tokio::test]
    async fn interactive_retry_twiddle_eof_breaks_immediately() {
        // EOF (empty reader) → read_line returns Ok(0) → loop breaks without
        // calling the AI client. failed_indices stays unchanged.
        let (commit, _tmp) = make_twiddle_commit("abc00000");
        let cmd = make_twiddle_cmd();
        let repo_view = make_twiddle_repo_view(vec![commit]);
        let client = make_mock_client(vec![]); // no responses consumed
        let mut failed = vec![0usize];
        let mut successes = vec![];
        let mut stdin = std::io::Cursor::new(b"" as &[u8]);
        cmd.run_interactive_retry_twiddle_check(
            &mut failed,
            &repo_view,
            &client,
            None,
            &[],
            &mut successes,
            &mut stdin,
        )
        .await
        .unwrap();
        assert_eq!(failed, vec![0], "EOF should leave failed_indices unchanged");
        assert!(successes.is_empty());
    }

    // --- handle_amendments_file ---

    fn make_amendment_file() -> crate::data::amendments::AmendmentFile {
        crate::data::amendments::AmendmentFile {
            amendments: vec![crate::data::amendments::Amendment {
                commit: "abc0000000000000000000000000000000000001".to_string(),
                message: "feat: improved commit message".to_string(),
                summary: None,
            }],
        }
    }

    #[test]
    fn handle_amendments_file_non_terminal_returns_false() {
        // is_terminal=false → non-interactive warning, returns Ok(false) immediately.
        let cmd = make_twiddle_cmd();
        let amendments = make_amendment_file();
        let dummy_path = std::path::Path::new("/tmp/dummy_amendments.yaml");
        let mut reader = std::io::Cursor::new(b"" as &[u8]);
        let result = cmd
            .handle_amendments_file(dummy_path, &amendments, false, &mut reader)
            .unwrap();
        assert!(!result, "non-terminal should return false");
    }

    #[test]
    fn handle_amendments_file_eof_returns_false() {
        // is_terminal=true, EOF reader → read_line returns 0, returns Ok(false).
        let cmd = make_twiddle_cmd();
        let amendments = make_amendment_file();
        let dummy_path = std::path::Path::new("/tmp/dummy_amendments.yaml");
        let mut reader = std::io::Cursor::new(b"" as &[u8]);
        let result = cmd
            .handle_amendments_file(dummy_path, &amendments, true, &mut reader)
            .unwrap();
        assert!(!result, "EOF should return false");
    }

    #[test]
    fn handle_amendments_file_quit_returns_false() {
        // is_terminal=true, "q\n" → user quits, returns Ok(false).
        let cmd = make_twiddle_cmd();
        let amendments = make_amendment_file();
        let dummy_path = std::path::Path::new("/tmp/dummy_amendments.yaml");
        let mut reader = std::io::Cursor::new(b"q\n" as &[u8]);
        let result = cmd
            .handle_amendments_file(dummy_path, &amendments, true, &mut reader)
            .unwrap();
        assert!(!result, "quit should return false");
    }

    #[test]
    fn handle_amendments_file_apply_returns_true() {
        // is_terminal=true, "a\n" → user applies, returns Ok(true).
        let cmd = make_twiddle_cmd();
        let amendments = make_amendment_file();
        let dummy_path = std::path::Path::new("/tmp/dummy_amendments.yaml");
        let mut reader = std::io::Cursor::new(b"a\n" as &[u8]);
        let result = cmd
            .handle_amendments_file(dummy_path, &amendments, true, &mut reader)
            .unwrap();
        assert!(result, "apply should return true");
    }

    #[test]
    fn handle_amendments_file_invalid_then_quit_returns_false() {
        // is_terminal=true, invalid input then "q\n" → prints error, then user quits.
        let cmd = make_twiddle_cmd();
        let amendments = make_amendment_file();
        let dummy_path = std::path::Path::new("/tmp/dummy_amendments.yaml");
        let mut reader = std::io::Cursor::new(b"x\nq\n" as &[u8]);
        let result = cmd
            .handle_amendments_file(dummy_path, &amendments, true, &mut reader)
            .unwrap();
        assert!(!result, "invalid then quit should return false");
    }

    // --- run_interactive_retry_generate_amendments ---

    /// Full 40-char hex hash used for amendment retry tests (validation requires ≥40 chars).
    const HASH_40: &str = "abc0000000000000000000000000000000000000";

    fn twiddle_amendment_yaml(hash: &str) -> String {
        format!("amendments:\n  - commit: \"{hash}\"\n    message: \"feat: improved message\"\n")
    }

    #[tokio::test]
    async fn retry_generate_amendments_non_terminal_returns_immediately() {
        // is_terminal=false → warning printed, returns Ok(()) without prompting.
        let (commit, _tmp) = make_twiddle_commit("abc00000");
        let cmd = make_twiddle_cmd();
        let repo_view = make_twiddle_repo_view(vec![commit]);
        let client = make_mock_client(vec![]); // no calls expected
        let mut failed = vec![0usize];
        let mut successes = vec![];
        let mut reader = std::io::Cursor::new(b"" as &[u8]);
        cmd.run_interactive_retry_generate_amendments(
            &mut failed,
            &repo_view,
            &client,
            None,
            false,
            &mut successes,
            false, // is_terminal
            &mut reader,
        )
        .await
        .unwrap();
        assert_eq!(
            failed,
            vec![0],
            "non-terminal should leave failed unchanged"
        );
        assert!(successes.is_empty());
    }

    #[tokio::test]
    async fn retry_generate_amendments_eof_breaks_immediately() {
        // is_terminal=true, EOF → read_line returns 0 → breaks without AI calls.
        let (commit, _tmp) = make_twiddle_commit("abc00000");
        let cmd = make_twiddle_cmd();
        let repo_view = make_twiddle_repo_view(vec![commit]);
        let client = make_mock_client(vec![]); // no calls expected
        let mut failed = vec![0usize];
        let mut successes = vec![];
        let mut reader = std::io::Cursor::new(b"" as &[u8]);
        cmd.run_interactive_retry_generate_amendments(
            &mut failed,
            &repo_view,
            &client,
            None,
            false,
            &mut successes,
            true, // is_terminal
            &mut reader,
        )
        .await
        .unwrap();
        assert_eq!(failed, vec![0], "EOF should leave failed unchanged");
        assert!(successes.is_empty());
    }

    #[tokio::test]
    async fn retry_generate_amendments_skip_breaks_immediately() {
        // is_terminal=true, "s\n" → user skips, failed stays unchanged.
        let (commit, _tmp) = make_twiddle_commit("abc00000");
        let cmd = make_twiddle_cmd();
        let repo_view = make_twiddle_repo_view(vec![commit]);
        let client = make_mock_client(vec![]); // no calls expected
        let mut failed = vec![0usize];
        let mut successes = vec![];
        let mut reader = std::io::Cursor::new(b"s\n" as &[u8]);
        cmd.run_interactive_retry_generate_amendments(
            &mut failed,
            &repo_view,
            &client,
            None,
            false,
            &mut successes,
            true,
            &mut reader,
        )
        .await
        .unwrap();
        assert_eq!(failed, vec![0], "skip should leave failed unchanged");
        assert!(successes.is_empty());
    }

    #[tokio::test]
    async fn retry_generate_amendments_invalid_then_skip() {
        // Unrecognised input → "please enter r or s" message → "s" exits.
        let (commit, _tmp) = make_twiddle_commit("abc00000");
        let cmd = make_twiddle_cmd();
        let repo_view = make_twiddle_repo_view(vec![commit]);
        let client = make_mock_client(vec![]);
        let mut failed = vec![0usize];
        let mut successes = vec![];
        let mut reader = std::io::Cursor::new(b"x\ns\n" as &[u8]);
        cmd.run_interactive_retry_generate_amendments(
            &mut failed,
            &repo_view,
            &client,
            None,
            false,
            &mut successes,
            true,
            &mut reader,
        )
        .await
        .unwrap();
        assert_eq!(failed, vec![0]);
        assert!(successes.is_empty());
    }

    #[tokio::test]
    async fn retry_generate_amendments_retry_fails_then_skip() {
        // "r" → AI call fails → still in failed → "s" → skips.
        let (commit, _tmp) = make_twiddle_commit("abc00000");
        let cmd = make_twiddle_cmd();
        let repo_view = make_twiddle_repo_view(vec![commit]);
        let client = make_mock_client(vec![Err(anyhow::anyhow!("mock fail"))]);
        let mut failed = vec![0usize];
        let mut successes = vec![];
        let mut reader = std::io::Cursor::new(b"r\ns\n" as &[u8]);
        cmd.run_interactive_retry_generate_amendments(
            &mut failed,
            &repo_view,
            &client,
            None,
            false,
            &mut successes,
            true,
            &mut reader,
        )
        .await
        .unwrap();
        assert_eq!(failed, vec![0], "commit still failed after retry");
        assert!(successes.is_empty());
    }

    #[tokio::test]
    async fn retry_generate_amendments_retry_succeeds() {
        // "r" → AI returns valid amendment → failed cleared, success recorded.
        let (commit, _tmp) = make_twiddle_commit(HASH_40);
        let cmd = make_twiddle_cmd();
        let repo_view = make_twiddle_repo_view(vec![commit]);
        let client = make_mock_client(vec![Ok(twiddle_amendment_yaml(HASH_40))]);
        let mut failed = vec![0usize];
        let mut successes = vec![];
        let mut reader = std::io::Cursor::new(b"r\n" as &[u8]);
        cmd.run_interactive_retry_generate_amendments(
            &mut failed,
            &repo_view,
            &client,
            None,
            false,
            &mut successes,
            true,
            &mut reader,
        )
        .await
        .unwrap();
        assert!(failed.is_empty(), "retry succeeded → failed cleared");
        assert_eq!(successes.len(), 1);
    }
}
