//! Git-related CLI commands

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing::{debug, error};

/// Parse a `--beta-header key:value` string into a `(key, value)` tuple.
fn parse_beta_header(s: &str) -> Result<(String, String)> {
    let (k, v) = s.split_once(':').ok_or_else(|| {
        anyhow::anyhow!("Invalid --beta-header format '{}'. Expected key:value", s)
    })?;
    Ok((k.to_string(), v.to_string()))
}

/// Git operations
#[derive(Parser)]
pub struct GitCommand {
    /// Git subcommand to execute
    #[command(subcommand)]
    pub command: GitSubcommands,
}

/// Git subcommands
#[derive(Subcommand)]
pub enum GitSubcommands {
    /// Commit-related operations
    Commit(CommitCommand),
    /// Branch-related operations
    Branch(BranchCommand),
}

/// Commit operations
#[derive(Parser)]
pub struct CommitCommand {
    /// Commit subcommand to execute
    #[command(subcommand)]
    pub command: CommitSubcommands,
}

/// Commit subcommands
#[derive(Subcommand)]
pub enum CommitSubcommands {
    /// Commit message operations
    Message(MessageCommand),
}

/// Message operations
#[derive(Parser)]
pub struct MessageCommand {
    /// Message subcommand to execute
    #[command(subcommand)]
    pub command: MessageSubcommands,
}

/// Message subcommands
#[derive(Subcommand)]
pub enum MessageSubcommands {
    /// Analyze commits and output repository information in YAML format
    View(ViewCommand),
    /// Amend commit messages based on a YAML configuration file
    Amend(AmendCommand),
    /// AI-powered commit message improvement using Claude
    Twiddle(TwiddleCommand),
    /// Check commit messages against guidelines without modifying them
    Check(CheckCommand),
}

/// View command options
#[derive(Parser)]
pub struct ViewCommand {
    /// Commit range to analyze (e.g., HEAD~3..HEAD, abc123..def456)
    #[arg(value_name = "COMMIT_RANGE")]
    pub commit_range: Option<String>,
}

/// Amend command options  
#[derive(Parser)]
pub struct AmendCommand {
    /// YAML file containing commit amendments
    #[arg(value_name = "YAML_FILE")]
    pub yaml_file: String,
}

/// Twiddle command options
#[derive(Parser)]
pub struct TwiddleCommand {
    /// Commit range to analyze and improve (e.g., HEAD~3..HEAD, abc123..def456)
    #[arg(value_name = "COMMIT_RANGE")]
    pub commit_range: Option<String>,

    /// Claude API model to use (if not specified, uses settings or default)
    #[arg(long)]
    pub model: Option<String>,

    /// Beta header to send with API requests (format: key:value)
    /// Only sent if the model supports it in the registry
    #[arg(long, value_name = "KEY:VALUE")]
    pub beta_header: Option<String>,

    /// Skip confirmation prompt and apply amendments automatically
    #[arg(long)]
    pub auto_apply: bool,

    /// Save generated amendments to file without applying
    #[arg(long, value_name = "FILE")]
    pub save_only: Option<String>,

    /// Use additional project context for better suggestions (Phase 3)
    #[arg(long, default_value = "true")]
    pub use_context: bool,

    /// Path to custom context directory (defaults to .omni-dev/)
    #[arg(long)]
    pub context_dir: Option<std::path::PathBuf>,

    /// Specify work context (e.g., "feature: user authentication")
    #[arg(long)]
    pub work_context: Option<String>,

    /// Override detected branch context
    #[arg(long)]
    pub branch_context: Option<String>,

    /// Disable contextual analysis (use basic prompting only)
    #[arg(long)]
    pub no_context: bool,

    /// Maximum number of commits to process in a single batch (default: 4)
    #[arg(long, default_value = "4")]
    pub batch_size: usize,

    /// Skip AI processing and only output repository YAML
    #[arg(long)]
    pub no_ai: bool,

    /// Ignore existing commit messages and generate fresh ones based solely on diffs
    #[arg(long)]
    pub fresh: bool,

    /// Run commit message validation after applying amendments
    #[arg(long)]
    pub check: bool,
}

/// Check command options - validates commit messages against guidelines
#[derive(Parser)]
pub struct CheckCommand {
    /// Commit range to check (e.g., HEAD~3..HEAD, abc123..def456)
    /// Defaults to commits ahead of main branch
    #[arg(value_name = "COMMIT_RANGE")]
    pub commit_range: Option<String>,

    /// Claude API model to use (if not specified, uses settings or default)
    #[arg(long)]
    pub model: Option<String>,

    /// Beta header to send with API requests (format: key:value)
    /// Only sent if the model supports it in the registry
    #[arg(long, value_name = "KEY:VALUE")]
    pub beta_header: Option<String>,

    /// Path to custom context directory (defaults to .omni-dev/)
    #[arg(long)]
    pub context_dir: Option<std::path::PathBuf>,

    /// Explicit path to guidelines file
    #[arg(long)]
    pub guidelines: Option<std::path::PathBuf>,

    /// Output format: text (default), json, yaml
    #[arg(long, default_value = "text")]
    pub format: String,

    /// Exit with error code if any issues found (including warnings)
    #[arg(long)]
    pub strict: bool,

    /// Only show errors/warnings, suppress info-level output
    #[arg(long)]
    pub quiet: bool,

    /// Show detailed analysis including passing commits
    #[arg(long)]
    pub verbose: bool,

    /// Include passing commits in output (hidden by default)
    #[arg(long)]
    pub show_passing: bool,

    /// Number of commits to process per AI request (default: 4)
    #[arg(long, default_value = "4")]
    pub batch_size: usize,

    /// Skip generating corrected message suggestions
    #[arg(long)]
    pub no_suggestions: bool,

    /// Offer to apply suggested messages when issues are found
    #[arg(long)]
    pub twiddle: bool,
}

/// Branch operations
#[derive(Parser)]
pub struct BranchCommand {
    /// Branch subcommand to execute
    #[command(subcommand)]
    pub command: BranchSubcommands,
}

/// Branch subcommands
#[derive(Subcommand)]
pub enum BranchSubcommands {
    /// Analyze branch commits and output repository information in YAML format
    Info(InfoCommand),
    /// Create operations
    Create(CreateCommand),
}

/// Info command options
#[derive(Parser)]
pub struct InfoCommand {
    /// Base branch to compare against (defaults to main/master)
    #[arg(value_name = "BASE_BRANCH")]
    pub base_branch: Option<String>,
}

/// Create operations
#[derive(Parser)]
pub struct CreateCommand {
    /// Create subcommand to execute
    #[command(subcommand)]
    pub command: CreateSubcommands,
}

/// Create subcommands
#[derive(Subcommand)]
pub enum CreateSubcommands {
    /// Create a pull request with AI-generated description
    Pr(CreatePrCommand),
}

/// Create PR command options
#[derive(Parser)]
pub struct CreatePrCommand {
    /// Base branch for the PR to be merged into (defaults to main/master)
    #[arg(long, value_name = "BRANCH")]
    pub base: Option<String>,

    /// Claude API model to use (if not specified, uses settings or default)
    #[arg(long)]
    pub model: Option<String>,

    /// Skip confirmation prompt and create PR automatically
    #[arg(long)]
    pub auto_apply: bool,

    /// Save generated PR details to file without creating PR
    #[arg(long, value_name = "FILE")]
    pub save_only: Option<String>,

    /// Create PR as ready for review (overrides default)
    #[arg(long, conflicts_with = "draft")]
    pub ready: bool,

    /// Create PR as draft (overrides default)
    #[arg(long, conflicts_with = "ready")]
    pub draft: bool,
}

impl GitCommand {
    /// Execute git command
    pub fn execute(self) -> Result<()> {
        match self.command {
            GitSubcommands::Commit(commit_cmd) => commit_cmd.execute(),
            GitSubcommands::Branch(branch_cmd) => branch_cmd.execute(),
        }
    }
}

impl CommitCommand {
    /// Execute commit command
    pub fn execute(self) -> Result<()> {
        match self.command {
            CommitSubcommands::Message(message_cmd) => message_cmd.execute(),
        }
    }
}

impl MessageCommand {
    /// Execute message command
    pub fn execute(self) -> Result<()> {
        match self.command {
            MessageSubcommands::View(view_cmd) => view_cmd.execute(),
            MessageSubcommands::Amend(amend_cmd) => amend_cmd.execute(),
            MessageSubcommands::Twiddle(twiddle_cmd) => {
                // Use tokio runtime for async execution
                let rt =
                    tokio::runtime::Runtime::new().context("Failed to create tokio runtime")?;
                rt.block_on(twiddle_cmd.execute())
            }
            MessageSubcommands::Check(check_cmd) => {
                // Use tokio runtime for async execution
                let rt =
                    tokio::runtime::Runtime::new().context("Failed to create tokio runtime")?;
                rt.block_on(check_cmd.execute())
            }
        }
    }
}

impl ViewCommand {
    /// Execute view command
    pub fn execute(self) -> Result<()> {
        use crate::data::{
            AiInfo, FieldExplanation, FileStatusInfo, RepositoryView, VersionInfo,
            WorkingDirectoryInfo,
        };
        use crate::git::{GitRepository, RemoteInfo};
        use crate::utils::ai_scratch;

        let commit_range = self.commit_range.as_deref().unwrap_or("HEAD");

        // Open git repository
        let repo = GitRepository::open()
            .context("Failed to open git repository. Make sure you're in a git repository.")?;

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

        // Build repository view
        let mut repo_view = RepositoryView {
            versions,
            explanation: FieldExplanation::default(),
            working_directory,
            remotes,
            ai: ai_info,
            branch_info: None,
            pr_template: None,
            pr_template_location: None,
            branch_prs: None,
            commits,
        };

        // Update field presence based on actual data
        repo_view.update_field_presence();

        // Output as YAML
        let yaml_output = crate::data::to_yaml(&repo_view)?;
        println!("{}", yaml_output);

        Ok(())
    }
}

impl AmendCommand {
    /// Execute amend command
    pub fn execute(self) -> Result<()> {
        use crate::git::AmendmentHandler;

        println!("🔄 Starting commit amendment process...");
        println!("📄 Loading amendments from: {}", self.yaml_file);

        // Create amendment handler and apply amendments
        let handler = AmendmentHandler::new().context("Failed to initialize amendment handler")?;

        handler
            .apply_amendments(&self.yaml_file)
            .context("Failed to apply amendments")?;

        Ok(())
    }
}

impl TwiddleCommand {
    /// Execute twiddle command with contextual intelligence
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

        // 2. Check if batching is needed
        if full_repo_view.commits.len() > self.batch_size {
            println!(
                "📦 Processing {} commits in batches of {} to ensure reliable analysis...",
                full_repo_view.commits.len(),
                self.batch_size
            );
            return self
                .execute_with_batching(use_contextual, full_repo_view)
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

    /// Execute twiddle command with automatic batching for large commit ranges
    async fn execute_with_batching(
        &self,
        use_contextual: bool,
        full_repo_view: crate::data::RepositoryView,
    ) -> Result<()> {
        use crate::data::amendments::AmendmentFile;

        // Initialize Claude client
        let beta = self
            .beta_header
            .as_deref()
            .map(parse_beta_header)
            .transpose()?;
        let claude_client = crate::claude::create_default_claude_client(self.model.clone(), beta)?;

        // Show model information
        self.show_model_info_from_client(&claude_client)?;

        // Split commits into batches
        let commit_batches: Vec<_> = full_repo_view.commits.chunks(self.batch_size).collect();

        let total_batches = commit_batches.len();
        let mut all_amendments = AmendmentFile {
            amendments: Vec::new(),
        };

        if self.fresh {
            println!("🔄 Fresh mode: ignoring existing commit messages...");
        }
        println!("📊 Processing {} batches...", total_batches);

        for (batch_num, commit_batch) in commit_batches.into_iter().enumerate() {
            println!(
                "🔄 Processing batch {}/{} ({} commits)...",
                batch_num + 1,
                total_batches,
                commit_batch.len()
            );

            // Create a repository view for just this batch
            let mut batch_repo_view = crate::data::RepositoryView {
                versions: full_repo_view.versions.clone(),
                explanation: full_repo_view.explanation.clone(),
                working_directory: full_repo_view.working_directory.clone(),
                remotes: full_repo_view.remotes.clone(),
                ai: full_repo_view.ai.clone(),
                branch_info: full_repo_view.branch_info.clone(),
                pr_template: full_repo_view.pr_template.clone(),
                pr_template_location: full_repo_view.pr_template_location.clone(),
                branch_prs: full_repo_view.branch_prs.clone(),
                commits: commit_batch.to_vec(),
            };

            // Collect context for this batch if needed
            let batch_context = if use_contextual {
                Some(self.collect_context(&batch_repo_view).await?)
            } else {
                None
            };

            // Refine detected scopes using file_patterns from scope definitions
            let batch_scope_defs = match &batch_context {
                Some(ctx) => ctx.project.valid_scopes.clone(),
                None => self.load_check_scopes(),
            };
            for commit in &mut batch_repo_view.commits {
                commit.analysis.refine_scope(&batch_scope_defs);
            }

            // Generate amendments for this batch
            let batch_amendments = if let Some(ctx) = batch_context {
                claude_client
                    .generate_contextual_amendments_with_options(&batch_repo_view, &ctx, self.fresh)
                    .await?
            } else {
                claude_client
                    .generate_amendments_with_options(&batch_repo_view, self.fresh)
                    .await?
            };

            // Merge amendments from this batch
            all_amendments
                .amendments
                .extend(batch_amendments.amendments);

            if batch_num + 1 < total_batches {
                println!("   ✅ Batch {}/{} completed", batch_num + 1, total_batches);
                // Small delay between batches to be respectful to the API
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            }
        }

        println!(
            "✅ All batches completed! Found {} commits to improve.",
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
            // Create temporary file for amendments
            let temp_dir = tempfile::tempdir()?;
            let amendments_file = temp_dir.path().join("twiddle_amendments.yaml");
            all_amendments.save_to_file(&amendments_file)?;

            // Show file path and get user choice
            if !self.auto_apply
                && !self.handle_amendments_file(&amendments_file, &all_amendments)?
            {
                println!("❌ Amendment cancelled by user");
                return Ok(());
            }

            // Apply all amendments (re-read from file to capture any user edits)
            self.apply_amendments_from_file(&amendments_file).await?;
            println!("✅ Commit messages improved successfully!");

            // Run post-twiddle check if --check flag is set
            if self.check {
                self.run_post_twiddle_check().await?;
            }
        } else {
            println!("✨ No commits found to process!");
        }

        Ok(())
    }

    /// Generate repository view (reuse ViewCommand logic)
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

    /// Handle amendments file - show path and get user choice
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

    /// Show the contents of the amendments file
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

    /// Open the amendments file in an external editor
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

    /// Apply amendments from a file path (re-reads from disk to capture user edits)
    async fn apply_amendments_from_file(&self, amendments_file: &std::path::Path) -> Result<()> {
        use crate::git::AmendmentHandler;

        // Use AmendmentHandler to apply amendments directly from file
        let handler = AmendmentHandler::new().context("Failed to initialize amendment handler")?;
        handler
            .apply_amendments(&amendments_file.to_string_lossy())
            .context("Failed to apply amendments")?;

        Ok(())
    }

    /// Collect contextual information for enhanced commit message generation
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

    /// Show context summary to user
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

    /// Show model information from actual AI client
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

    /// Show diagnostic information about loaded guidance files
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

    /// Execute twiddle command without AI - create amendments with original messages
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

    /// Run commit message validation after twiddle amendments are applied.
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
            let report = if repo_view.commits.len() > self.batch_size {
                println!("📦 Checking commits in batches of {}...", self.batch_size);
                self.check_commits_with_batching(
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

    /// Build amendments from check report suggestions for failing commits.
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
            .filter(|r| !r.passes && r.suggestion.is_some())
            .filter_map(|r| {
                let suggestion = r.suggestion.as_ref().unwrap();
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

    /// Load commit guidelines for check (mirrors CheckCommand::load_guidelines)
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

    /// Load valid scopes for check (mirrors CheckCommand::load_scopes)
    fn load_check_scopes(&self) -> Vec<crate::data::context::ScopeDefinition> {
        use crate::data::context::ScopeDefinition;
        use std::fs;

        #[derive(serde::Deserialize)]
        struct ScopesConfig {
            scopes: Vec<ScopeDefinition>,
        }

        let context_dir = self
            .context_dir
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from(".omni-dev"));

        // Try local override first
        let local_path = context_dir.join("local").join("scopes.yaml");
        if local_path.exists() {
            if let Ok(content) = fs::read_to_string(&local_path) {
                if let Ok(config) = serde_yaml::from_str::<ScopesConfig>(&content) {
                    return config.scopes;
                }
            }
        }

        // Try project-level scopes
        let project_path = context_dir.join("scopes.yaml");
        if project_path.exists() {
            if let Ok(content) = fs::read_to_string(&project_path) {
                if let Ok(config) = serde_yaml::from_str::<ScopesConfig>(&content) {
                    return config.scopes;
                }
            }
        }

        // Try global scopes
        if let Some(home) = dirs::home_dir() {
            let home_path = home.join(".omni-dev").join("scopes.yaml");
            if home_path.exists() {
                if let Ok(content) = fs::read_to_string(&home_path) {
                    if let Ok(config) = serde_yaml::from_str::<ScopesConfig>(&content) {
                        return config.scopes;
                    }
                }
            }
        }

        Vec::new()
    }

    /// Show guidance files status for check
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

    /// Check commits with batching (mirrors CheckCommand::check_with_batching)
    async fn check_commits_with_batching(
        &self,
        claude_client: &crate::claude::client::ClaudeClient,
        full_repo_view: &crate::data::RepositoryView,
        guidelines: Option<&str>,
        valid_scopes: &[crate::data::context::ScopeDefinition],
    ) -> Result<crate::data::check::CheckReport> {
        use crate::data::check::{CheckReport, CommitCheckResult};

        let commit_batches: Vec<_> = full_repo_view.commits.chunks(self.batch_size).collect();
        let total_batches = commit_batches.len();
        let mut all_results: Vec<CommitCheckResult> = Vec::new();

        for (batch_num, commit_batch) in commit_batches.into_iter().enumerate() {
            println!(
                "🔄 Checking batch {}/{} ({} commits)...",
                batch_num + 1,
                total_batches,
                commit_batch.len()
            );

            let batch_repo_view = crate::data::RepositoryView {
                versions: full_repo_view.versions.clone(),
                explanation: full_repo_view.explanation.clone(),
                working_directory: full_repo_view.working_directory.clone(),
                remotes: full_repo_view.remotes.clone(),
                ai: full_repo_view.ai.clone(),
                branch_info: full_repo_view.branch_info.clone(),
                pr_template: full_repo_view.pr_template.clone(),
                pr_template_location: full_repo_view.pr_template_location.clone(),
                branch_prs: full_repo_view.branch_prs.clone(),
                commits: commit_batch.to_vec(),
            };

            let batch_report = claude_client
                .check_commits_with_scopes(&batch_repo_view, guidelines, valid_scopes, true)
                .await?;

            all_results.extend(batch_report.commits);

            if batch_num + 1 < total_batches {
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            }
        }

        Ok(CheckReport::new(all_results))
    }

    /// Output text format check report (mirrors CheckCommand::output_text_report)
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

impl BranchCommand {
    /// Execute branch command
    pub fn execute(self) -> Result<()> {
        match self.command {
            BranchSubcommands::Info(info_cmd) => info_cmd.execute(),
            BranchSubcommands::Create(create_cmd) => {
                // Use tokio runtime for async execution
                let rt = tokio::runtime::Runtime::new()
                    .context("Failed to create tokio runtime for PR creation")?;
                rt.block_on(create_cmd.execute())
            }
        }
    }
}

impl InfoCommand {
    /// Execute info command
    pub fn execute(self) -> Result<()> {
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
        let current_branch = repo.get_current_branch().context(
            "Failed to get current branch. Make sure you're not in detached HEAD state.",
        )?;

        // Determine base branch
        let base_branch = match self.base_branch {
            Some(branch) => {
                // Validate that the specified base branch exists
                if !repo.branch_exists(&branch)? {
                    anyhow::bail!("Base branch '{}' does not exist", branch);
                }
                branch
            }
            None => {
                // Default to main or master
                if repo.branch_exists("main")? {
                    "main".to_string()
                } else if repo.branch_exists("master")? {
                    "master".to_string()
                } else {
                    anyhow::bail!("No default base branch found (main or master)");
                }
            }
        };

        // Calculate commit range: [base_branch]..HEAD
        let commit_range = format!("{}..HEAD", base_branch);

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

        // Check for PR template
        let pr_template_result = Self::read_pr_template().ok();
        let (pr_template, pr_template_location) = match pr_template_result {
            Some((content, location)) => (Some(content), Some(location)),
            None => (None, None),
        };

        // Get PRs for current branch
        let branch_prs = Self::get_branch_prs(&current_branch)
            .ok()
            .filter(|prs| !prs.is_empty());

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
            pr_template,
            pr_template_location,
            branch_prs,
            commits,
        };

        // Update field presence based on actual data
        repo_view.update_field_presence();

        // Output as YAML
        let yaml_output = crate::data::to_yaml(&repo_view)?;
        println!("{}", yaml_output);

        Ok(())
    }

    /// Read PR template file if it exists, returning both content and location
    fn read_pr_template() -> Result<(String, String)> {
        use std::fs;
        use std::path::Path;

        let template_path = Path::new(".github/pull_request_template.md");
        if template_path.exists() {
            let content = fs::read_to_string(template_path)
                .context("Failed to read .github/pull_request_template.md")?;
            Ok((content, template_path.to_string_lossy().to_string()))
        } else {
            anyhow::bail!("PR template file does not exist")
        }
    }

    /// Get pull requests for the current branch using gh CLI
    fn get_branch_prs(branch_name: &str) -> Result<Vec<crate::data::PullRequest>> {
        use serde_json::Value;
        use std::process::Command;

        // Use gh CLI to get PRs for the branch
        let output = Command::new("gh")
            .args([
                "pr",
                "list",
                "--head",
                branch_name,
                "--json",
                "number,title,state,url,body,baseRefName",
                "--limit",
                "50",
            ])
            .output()
            .context("Failed to execute gh command")?;

        if !output.status.success() {
            anyhow::bail!(
                "gh command failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let json_str = String::from_utf8_lossy(&output.stdout);
        let prs_json: Value =
            serde_json::from_str(&json_str).context("Failed to parse PR JSON from gh")?;

        let mut prs = Vec::new();
        if let Some(prs_array) = prs_json.as_array() {
            for pr_json in prs_array {
                if let (Some(number), Some(title), Some(state), Some(url), Some(body)) = (
                    pr_json.get("number").and_then(|n| n.as_u64()),
                    pr_json.get("title").and_then(|t| t.as_str()),
                    pr_json.get("state").and_then(|s| s.as_str()),
                    pr_json.get("url").and_then(|u| u.as_str()),
                    pr_json.get("body").and_then(|b| b.as_str()),
                ) {
                    let base = pr_json
                        .get("baseRefName")
                        .and_then(|b| b.as_str())
                        .unwrap_or("")
                        .to_string();
                    prs.push(crate::data::PullRequest {
                        number,
                        title: title.to_string(),
                        state: state.to_string(),
                        url: url.to_string(),
                        body: body.to_string(),
                        base,
                    });
                }
            }
        }

        Ok(prs)
    }
}

/// PR action choices
#[derive(Debug, PartialEq)]
enum PrAction {
    CreateNew,
    UpdateExisting,
    Cancel,
}

/// AI-generated PR content with structured fields
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct PrContent {
    /// Concise PR title (ideally 50-80 characters)
    pub title: String,
    /// Full PR description in markdown format
    pub description: String,
}

impl CreateCommand {
    /// Execute create command
    pub async fn execute(self) -> Result<()> {
        match self.command {
            CreateSubcommands::Pr(pr_cmd) => pr_cmd.execute().await,
        }
    }
}

impl CreatePrCommand {
    /// Determine if PR should be created as draft
    ///
    /// Priority order:
    /// 1. --ready flag (not draft)
    /// 2. --draft flag (draft)
    /// 3. OMNI_DEV_DEFAULT_DRAFT_PR env/config setting
    /// 4. Hard-coded default (draft)
    fn should_create_as_draft(&self) -> bool {
        use crate::utils::settings::get_env_var;

        // Explicit flags take precedence
        if self.ready {
            return false;
        }
        if self.draft {
            return true;
        }

        // Check configuration setting
        get_env_var("OMNI_DEV_DEFAULT_DRAFT_PR")
            .ok()
            .and_then(|val| match val.to_lowercase().as_str() {
                "true" | "1" | "yes" => Some(true),
                "false" | "0" | "no" => Some(false),
                _ => None,
            })
            .unwrap_or(true) // Default to draft if not configured
    }

    /// Execute create PR command
    pub async fn execute(self) -> Result<()> {
        // Preflight check: validate all prerequisites before any processing
        // This catches missing credentials/tools early before wasting time
        let ai_info = crate::utils::check_pr_command_prerequisites(self.model.as_deref())?;
        println!(
            "✓ {} credentials verified (model: {})",
            ai_info.provider, ai_info.model
        );
        println!("✓ GitHub CLI verified");

        println!("🔄 Starting pull request creation process...");

        // 1. Generate repository view (reuse InfoCommand logic)
        let repo_view = self.generate_repository_view()?;

        // 2. Validate branch state (always needed)
        self.validate_branch_state(&repo_view)?;

        // 3. Show guidance files status early (before AI processing)
        use crate::claude::context::ProjectDiscovery;
        let repo_root = std::path::PathBuf::from(".");
        let context_dir = std::path::PathBuf::from(".omni-dev");
        let discovery = ProjectDiscovery::new(repo_root, context_dir);
        let project_context = discovery.discover().unwrap_or_default();
        self.show_guidance_files_status(&project_context)?;

        // 4. Show AI model configuration before generation
        let claude_client = crate::claude::create_default_claude_client(self.model.clone(), None)?;
        self.show_model_info_from_client(&claude_client)?;

        // 5. Show branch analysis and commit information
        self.show_commit_range_info(&repo_view)?;

        // 6. Show context analysis (quick collection for display only)
        let context = {
            use crate::claude::context::{BranchAnalyzer, WorkPatternAnalyzer};
            use crate::data::context::CommitContext;
            let mut context = CommitContext::new();
            context.project = project_context;

            // Quick analysis for display
            if let Some(branch_info) = &repo_view.branch_info {
                context.branch = BranchAnalyzer::analyze(&branch_info.branch).unwrap_or_default();
            }

            if !repo_view.commits.is_empty() {
                context.range = WorkPatternAnalyzer::analyze_commit_range(&repo_view.commits);
            }
            context
        };
        self.show_context_summary(&context)?;

        // 7. Generate AI-powered PR content (title + description)
        debug!("About to generate PR content from AI");
        let (pr_content, _claude_client) = self
            .generate_pr_content_with_client_internal(&repo_view, claude_client)
            .await?;

        // 8. Show detailed context information (like twiddle command)
        self.show_context_information(&repo_view).await?;
        debug!(
            generated_title = %pr_content.title,
            generated_description_length = pr_content.description.len(),
            generated_description_preview = %pr_content.description.lines().take(3).collect::<Vec<_>>().join("\\n"),
            "Generated PR content from AI"
        );

        // 5. Handle different output modes
        if let Some(save_path) = self.save_only {
            let pr_yaml = crate::data::to_yaml(&pr_content)
                .context("Failed to serialize PR content to YAML")?;
            std::fs::write(&save_path, &pr_yaml).context("Failed to save PR details to file")?;
            println!("💾 PR details saved to: {}", save_path);
            return Ok(());
        }

        // 6. Create temporary file for PR details
        debug!("About to serialize PR content to YAML");
        let temp_dir = tempfile::tempdir()?;
        let pr_file = temp_dir.path().join("pr-details.yaml");

        debug!(
            pre_serialize_title = %pr_content.title,
            pre_serialize_description_length = pr_content.description.len(),
            pre_serialize_description_preview = %pr_content.description.lines().take(3).collect::<Vec<_>>().join("\\n"),
            "About to serialize PR content with to_yaml"
        );

        let pr_yaml =
            crate::data::to_yaml(&pr_content).context("Failed to serialize PR content to YAML")?;

        debug!(
            file_path = %pr_file.display(),
            yaml_content_length = pr_yaml.len(),
            yaml_content = %pr_yaml,
            original_title = %pr_content.title,
            original_description_length = pr_content.description.len(),
            "Writing PR details to temporary YAML file"
        );

        std::fs::write(&pr_file, &pr_yaml)?;

        // 7. Handle PR details file - show path and get user choice
        let pr_action = if self.auto_apply {
            // For auto-apply, default to update if PR exists, otherwise create new
            if repo_view
                .branch_prs
                .as_ref()
                .is_some_and(|prs| !prs.is_empty())
            {
                PrAction::UpdateExisting
            } else {
                PrAction::CreateNew
            }
        } else {
            self.handle_pr_file(&pr_file, &repo_view)?
        };

        if pr_action == PrAction::Cancel {
            println!("❌ PR operation cancelled by user");
            return Ok(());
        }

        // 8. Create or update PR (re-read from file to capture any user edits)
        let final_pr_yaml =
            std::fs::read_to_string(&pr_file).context("Failed to read PR details file")?;

        debug!(
            yaml_length = final_pr_yaml.len(),
            yaml_content = %final_pr_yaml,
            "Read PR details YAML from file"
        );

        let final_pr_content: PrContent = serde_yaml::from_str(&final_pr_yaml)
            .context("Failed to parse PR details YAML. Please check the file format.")?;

        debug!(
            title = %final_pr_content.title,
            description_length = final_pr_content.description.len(),
            description_preview = %final_pr_content.description.lines().take(3).collect::<Vec<_>>().join("\\n"),
            "Parsed PR content from YAML"
        );

        // Determine draft status
        let is_draft = self.should_create_as_draft();

        match pr_action {
            PrAction::CreateNew => {
                self.create_github_pr(
                    &repo_view,
                    &final_pr_content.title,
                    &final_pr_content.description,
                    is_draft,
                    self.base.as_deref(),
                )?;
                println!("✅ Pull request created successfully!");
            }
            PrAction::UpdateExisting => {
                self.update_github_pr(
                    &repo_view,
                    &final_pr_content.title,
                    &final_pr_content.description,
                    self.base.as_deref(),
                )?;
                println!("✅ Pull request updated successfully!");
            }
            PrAction::Cancel => unreachable!(), // Already handled above
        }

        Ok(())
    }

    /// Generate repository view (reuse InfoCommand logic)
    fn generate_repository_view(&self) -> Result<crate::data::RepositoryView> {
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
        let current_branch = repo.get_current_branch().context(
            "Failed to get current branch. Make sure you're not in detached HEAD state.",
        )?;

        // Get remote information to determine proper remote and main branch
        let remotes = RemoteInfo::get_all_remotes(repo.repository())?;

        // Find the primary remote (prefer origin, fallback to first available)
        let primary_remote = remotes
            .iter()
            .find(|r| r.name == "origin")
            .or_else(|| remotes.first())
            .ok_or_else(|| anyhow::anyhow!("No remotes found in repository"))?;

        // Determine base branch (with remote prefix)
        let base_branch = match self.base.as_ref() {
            Some(branch) => {
                // User specified base branch - try to resolve it
                // First, check if it's already a valid remote ref (e.g., "origin/main")
                let remote_ref = format!("refs/remotes/{}", branch);
                if repo.repository().find_reference(&remote_ref).is_ok() {
                    branch.clone()
                } else {
                    // Try prepending the primary remote name (e.g., "main" -> "origin/main")
                    let with_remote = format!("{}/{}", primary_remote.name, branch);
                    let remote_ref = format!("refs/remotes/{}", with_remote);
                    if repo.repository().find_reference(&remote_ref).is_ok() {
                        with_remote
                    } else {
                        anyhow::bail!(
                            "Remote branch '{}' does not exist (also tried '{}')",
                            branch,
                            with_remote
                        );
                    }
                }
            }
            None => {
                // Auto-detect using the primary remote's main branch
                let main_branch = &primary_remote.main_branch;
                if main_branch == "unknown" {
                    anyhow::bail!(
                        "Could not determine main branch for remote '{}'",
                        primary_remote.name
                    );
                }

                let remote_main = format!("{}/{}", primary_remote.name, main_branch);

                // Validate that the remote main branch exists
                let remote_ref = format!("refs/remotes/{}", remote_main);
                if repo.repository().find_reference(&remote_ref).is_err() {
                    anyhow::bail!(
                        "Remote main branch '{}' does not exist. Try running 'git fetch' first.",
                        remote_main
                    );
                }

                remote_main
            }
        };

        // Calculate commit range: [remote_base]..HEAD
        let commit_range = format!("{}..HEAD", base_branch);

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

        // Check for PR template
        let pr_template_result = InfoCommand::read_pr_template().ok();
        let (pr_template, pr_template_location) = match pr_template_result {
            Some((content, location)) => (Some(content), Some(location)),
            None => (None, None),
        };

        // Get PRs for current branch
        let branch_prs = InfoCommand::get_branch_prs(&current_branch)
            .ok()
            .filter(|prs| !prs.is_empty());

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
            pr_template,
            pr_template_location,
            branch_prs,
            commits,
        };

        // Update field presence based on actual data
        repo_view.update_field_presence();

        Ok(repo_view)
    }

    /// Validate branch state for PR creation
    fn validate_branch_state(&self, repo_view: &crate::data::RepositoryView) -> Result<()> {
        // Check if working directory is clean
        if !repo_view.working_directory.clean {
            anyhow::bail!(
                "Working directory has uncommitted changes. Please commit or stash your changes before creating a PR."
            );
        }

        // Check if there are any untracked changes
        if !repo_view.working_directory.untracked_changes.is_empty() {
            let file_list: Vec<&str> = repo_view
                .working_directory
                .untracked_changes
                .iter()
                .map(|f| f.file.as_str())
                .collect();
            anyhow::bail!(
                "Working directory has untracked changes: {}. Please commit or stash your changes before creating a PR.",
                file_list.join(", ")
            );
        }

        // Check if commits exist
        if repo_view.commits.is_empty() {
            anyhow::bail!("No commits found to create PR from. Make sure you have commits that are not in the base branch.");
        }

        // Check if PR already exists for this branch
        if let Some(existing_prs) = &repo_view.branch_prs {
            if !existing_prs.is_empty() {
                let pr_info: Vec<String> = existing_prs
                    .iter()
                    .map(|pr| format!("#{} ({})", pr.number, pr.state))
                    .collect();

                println!(
                    "📋 Existing PR(s) found for this branch: {}",
                    pr_info.join(", ")
                );
                // Don't bail - we'll handle this in the main flow
            }
        }

        Ok(())
    }

    /// Show detailed context information (similar to twiddle command)
    async fn show_context_information(
        &self,
        _repo_view: &crate::data::RepositoryView,
    ) -> Result<()> {
        // Note: commit range info and context summary are now shown earlier
        // This method is kept for potential future detailed information
        // that should be shown after AI generation

        Ok(())
    }

    /// Show commit range and count information
    fn show_commit_range_info(&self, repo_view: &crate::data::RepositoryView) -> Result<()> {
        // Recreate the base branch determination logic from generate_repository_view
        let base_branch = match self.base.as_ref() {
            Some(branch) => {
                // User specified base branch
                // Get the primary remote name from repo_view
                let primary_remote_name = repo_view
                    .remotes
                    .iter()
                    .find(|r| r.name == "origin")
                    .or_else(|| repo_view.remotes.first())
                    .map(|r| r.name.as_str())
                    .unwrap_or("origin");
                // Check if already has remote prefix
                if branch.starts_with(&format!("{}/", primary_remote_name)) {
                    branch.clone()
                } else {
                    format!("{}/{}", primary_remote_name, branch)
                }
            }
            None => {
                // Auto-detected base branch from remotes
                repo_view
                    .remotes
                    .iter()
                    .find(|r| r.name == "origin")
                    .or_else(|| repo_view.remotes.first())
                    .map(|r| format!("{}/{}", r.name, r.main_branch))
                    .unwrap_or_else(|| "unknown".to_string())
            }
        };

        let commit_range = format!("{}..HEAD", base_branch);
        let commit_count = repo_view.commits.len();

        // Get current branch name
        let current_branch = repo_view
            .branch_info
            .as_ref()
            .map(|bi| bi.branch.as_str())
            .unwrap_or("unknown");

        println!("📊 Branch Analysis:");
        println!("   🌿 Current branch: {}", current_branch);
        println!("   📏 Commit range: {}", commit_range);
        println!("   📝 Commits found: {} commits", commit_count);
        println!();

        Ok(())
    }

    /// Collect contextual information for enhanced PR generation (adapted from twiddle)
    async fn collect_context(
        &self,
        repo_view: &crate::data::RepositoryView,
    ) -> Result<crate::data::context::CommitContext> {
        use crate::claude::context::{BranchAnalyzer, ProjectDiscovery, WorkPatternAnalyzer};
        use crate::data::context::CommitContext;
        use crate::git::GitRepository;

        let mut context = CommitContext::new();

        // 1. Discover project context
        let context_dir = std::path::PathBuf::from(".omni-dev");

        // ProjectDiscovery takes repo root and context directory
        let repo_root = std::path::PathBuf::from(".");
        let discovery = ProjectDiscovery::new(repo_root, context_dir.clone());
        match discovery.discover() {
            Ok(project_context) => {
                context.project = project_context;
            }
            Err(_e) => {
                context.project = Default::default();
            }
        }

        // 2. Analyze current branch
        let repo = GitRepository::open()?;
        let current_branch = repo
            .get_current_branch()
            .unwrap_or_else(|_| "HEAD".to_string());
        context.branch = BranchAnalyzer::analyze(&current_branch).unwrap_or_default();

        // 3. Analyze commit range patterns
        if !repo_view.commits.is_empty() {
            context.range = WorkPatternAnalyzer::analyze_commit_range(&repo_view.commits);
        }

        Ok(context)
    }

    /// Show guidance files status (adapted from twiddle)
    fn show_guidance_files_status(
        &self,
        project_context: &crate::data::context::ProjectContext,
    ) -> Result<()> {
        let context_dir = std::path::PathBuf::from(".omni-dev");

        println!("📋 Project guidance files status:");

        // Check PR guidelines (for PR commands)
        let pr_guidelines_found = project_context.pr_guidelines.is_some();
        let pr_guidelines_source = if pr_guidelines_found {
            let local_path = context_dir.join("local").join("pr-guidelines.md");
            let project_path = context_dir.join("pr-guidelines.md");
            let home_path = dirs::home_dir()
                .map(|h| h.join(".omni-dev").join("pr-guidelines.md"))
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
        println!("   🔀 PR guidelines: {}", pr_guidelines_source);

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

        // Check PR template
        let pr_template_path = std::path::Path::new(".github/pull_request_template.md");
        let pr_template_status = if pr_template_path.exists() {
            format!("✅ Project: {}", pr_template_path.display())
        } else {
            "❌ None found".to_string()
        };
        println!("   📋 PR template: {}", pr_template_status);

        println!();
        Ok(())
    }

    /// Show context summary (adapted from twiddle)
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

        println!();
        Ok(())
    }

    /// Generate PR content with pre-created client (internal method that doesn't show model info)
    async fn generate_pr_content_with_client_internal(
        &self,
        repo_view: &crate::data::RepositoryView,
        claude_client: crate::claude::client::ClaudeClient,
    ) -> Result<(PrContent, crate::claude::client::ClaudeClient)> {
        use tracing::debug;

        // Get PR template (either from repo or default)
        let pr_template = match &repo_view.pr_template {
            Some(template) => template.clone(),
            None => self.get_default_pr_template(),
        };

        debug!(
            pr_template_length = pr_template.len(),
            pr_template_preview = %pr_template.lines().take(5).collect::<Vec<_>>().join("\\n"),
            "Using PR template for generation"
        );

        println!("🤖 Generating AI-powered PR description...");

        // Collect project context for PR guidelines
        debug!("Collecting context for PR generation");
        let context = self.collect_context(repo_view).await?;
        debug!("Context collection completed");

        // Generate AI-powered PR content with context
        debug!("About to call Claude AI for PR content generation");
        match claude_client
            .generate_pr_content_with_context(repo_view, &pr_template, &context)
            .await
        {
            Ok(pr_content) => {
                debug!(
                    ai_generated_title = %pr_content.title,
                    ai_generated_description_length = pr_content.description.len(),
                    ai_generated_description_preview = %pr_content.description.lines().take(3).collect::<Vec<_>>().join("\\n"),
                    "AI successfully generated PR content"
                );
                Ok((pr_content, claude_client))
            }
            Err(e) => {
                debug!(error = %e, "AI PR generation failed, falling back to basic description");
                // Fallback to basic description with commit analysis (silently)
                let mut description = pr_template;
                self.enhance_description_with_commits(&mut description, repo_view)?;

                // Generate fallback title from commits
                let title = self.generate_title_from_commits(repo_view);

                debug!(
                    fallback_title = %title,
                    fallback_description_length = description.len(),
                    "Created fallback PR content"
                );

                Ok((PrContent { title, description }, claude_client))
            }
        }
    }

    /// Get default PR template when none exists in the repository
    fn get_default_pr_template(&self) -> String {
        r#"# Pull Request

## Description
<!-- Provide a brief description of what this PR does -->

## Type of Change
<!-- Mark the relevant option with an "x" -->
- [ ] Bug fix (non-breaking change which fixes an issue)
- [ ] New feature (non-breaking change which adds functionality)
- [ ] Breaking change (fix or feature that would cause existing functionality to not work as expected)
- [ ] Documentation update
- [ ] Refactoring (no functional changes)
- [ ] Performance improvement
- [ ] Test coverage improvement

## Changes Made
<!-- List the specific changes made in this PR -->
- 
- 
- 

## Testing
- [ ] All existing tests pass
- [ ] New tests added for new functionality
- [ ] Manual testing performed

## Additional Notes
<!-- Add any additional notes for reviewers -->
"#.to_string()
    }

    /// Enhance PR description with commit analysis
    fn enhance_description_with_commits(
        &self,
        description: &mut String,
        repo_view: &crate::data::RepositoryView,
    ) -> Result<()> {
        if repo_view.commits.is_empty() {
            return Ok(());
        }

        // Add commit summary section
        description.push_str("\n---\n");
        description.push_str("## 📝 Commit Summary\n");
        description
            .push_str("*This section was automatically generated based on commit analysis*\n\n");

        // Analyze commit types and scopes
        let mut types_found = std::collections::HashSet::new();
        let mut scopes_found = std::collections::HashSet::new();
        let mut has_breaking_changes = false;

        for commit in &repo_view.commits {
            let detected_type = &commit.analysis.detected_type;
            types_found.insert(detected_type.clone());
            if detected_type.contains("BREAKING")
                || commit.original_message.contains("BREAKING CHANGE")
            {
                has_breaking_changes = true;
            }

            let detected_scope = &commit.analysis.detected_scope;
            if !detected_scope.is_empty() {
                scopes_found.insert(detected_scope.clone());
            }
        }

        // Update type checkboxes based on detected types
        if let Some(feat_pos) = description.find("- [ ] New feature") {
            if types_found.contains("feat") {
                description.replace_range(feat_pos..feat_pos + 5, "- [x]");
            }
        }
        if let Some(fix_pos) = description.find("- [ ] Bug fix") {
            if types_found.contains("fix") {
                description.replace_range(fix_pos..fix_pos + 5, "- [x]");
            }
        }
        if let Some(docs_pos) = description.find("- [ ] Documentation update") {
            if types_found.contains("docs") {
                description.replace_range(docs_pos..docs_pos + 5, "- [x]");
            }
        }
        if let Some(refactor_pos) = description.find("- [ ] Refactoring") {
            if types_found.contains("refactor") {
                description.replace_range(refactor_pos..refactor_pos + 5, "- [x]");
            }
        }
        if let Some(breaking_pos) = description.find("- [ ] Breaking change") {
            if has_breaking_changes {
                description.replace_range(breaking_pos..breaking_pos + 5, "- [x]");
            }
        }

        // Add detected scopes
        if !scopes_found.is_empty() {
            let scopes_list: Vec<_> = scopes_found.into_iter().collect();
            description.push_str(&format!(
                "**Affected areas:** {}\n\n",
                scopes_list.join(", ")
            ));
        }

        // Add commit list
        description.push_str("### Commits in this PR:\n");
        for commit in &repo_view.commits {
            let short_hash = &commit.hash[..8];
            let first_line = commit.original_message.lines().next().unwrap_or("").trim();
            description.push_str(&format!("- `{}` {}\n", short_hash, first_line));
        }

        // Add file change summary
        let total_files: usize = repo_view
            .commits
            .iter()
            .map(|c| c.analysis.file_changes.total_files)
            .sum();

        if total_files > 0 {
            description.push_str(&format!("\n**Files changed:** {} files\n", total_files));
        }

        Ok(())
    }

    /// Handle PR description file - show path and get user choice
    fn handle_pr_file(
        &self,
        pr_file: &std::path::Path,
        repo_view: &crate::data::RepositoryView,
    ) -> Result<PrAction> {
        use std::io::{self, Write};

        println!("\n📝 PR details generated.");
        println!("💾 Details saved to: {}", pr_file.display());

        // Show draft status
        let is_draft = self.should_create_as_draft();
        let status_icon = if is_draft { "📋" } else { "✅" };
        let status_text = if is_draft {
            "draft"
        } else {
            "ready for review"
        };
        println!("{} PR will be created as: {}", status_icon, status_text);
        println!();

        // Check if there are existing PRs and show different options
        let has_existing_prs = repo_view
            .branch_prs
            .as_ref()
            .is_some_and(|prs| !prs.is_empty());

        loop {
            if has_existing_prs {
                print!("❓ [U]pdate existing PR, [N]ew PR anyway, [S]how file, [E]dit file, or [Q]uit? [U/n/s/e/q] ");
            } else {
                print!(
                    "❓ [A]ccept and create PR, [S]how file, [E]dit file, or [Q]uit? [A/s/e/q] "
                );
            }
            io::stdout().flush()?;

            let mut input = String::new();
            io::stdin().read_line(&mut input)?;

            match input.trim().to_lowercase().as_str() {
                "u" | "update" if has_existing_prs => return Ok(PrAction::UpdateExisting),
                "n" | "new" if has_existing_prs => return Ok(PrAction::CreateNew),
                "a" | "accept" | "" if !has_existing_prs => return Ok(PrAction::CreateNew),
                "s" | "show" => {
                    self.show_pr_file(pr_file)?;
                    println!();
                }
                "e" | "edit" => {
                    self.edit_pr_file(pr_file)?;
                    println!();
                }
                "q" | "quit" => return Ok(PrAction::Cancel),
                _ => {
                    if has_existing_prs {
                        println!("Invalid choice. Please enter 'u' to update existing PR, 'n' for new PR, 's' to show, 'e' to edit, or 'q' to quit.");
                    } else {
                        println!("Invalid choice. Please enter 'a' to accept, 's' to show, 'e' to edit, or 'q' to quit.");
                    }
                }
            }
        }
    }

    /// Show the contents of the PR details file
    fn show_pr_file(&self, pr_file: &std::path::Path) -> Result<()> {
        use std::fs;

        println!("\n📄 PR details file contents:");
        println!("─────────────────────────────");

        let contents = fs::read_to_string(pr_file).context("Failed to read PR details file")?;
        println!("{}", contents);
        println!("─────────────────────────────");

        Ok(())
    }

    /// Open the PR details file in an external editor
    fn edit_pr_file(&self, pr_file: &std::path::Path) -> Result<()> {
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

        println!("📝 Opening PR details file in editor: {}", editor);

        // Split editor command to handle arguments
        let mut cmd_parts = editor.split_whitespace();
        let editor_cmd = cmd_parts.next().unwrap_or(&editor);
        let args: Vec<&str> = cmd_parts.collect();

        let mut command = Command::new(editor_cmd);
        command.args(args);
        command.arg(pr_file.to_string_lossy().as_ref());

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

    /// Generate a concise title from commit analysis (fallback)
    fn generate_title_from_commits(&self, repo_view: &crate::data::RepositoryView) -> String {
        if repo_view.commits.is_empty() {
            return "Pull Request".to_string();
        }

        // For single commit, use its first line
        if repo_view.commits.len() == 1 {
            return repo_view.commits[0]
                .original_message
                .lines()
                .next()
                .unwrap_or("Pull Request")
                .trim()
                .to_string();
        }

        // For multiple commits, generate from branch name
        let branch_name = repo_view
            .branch_info
            .as_ref()
            .map(|bi| bi.branch.as_str())
            .unwrap_or("feature");

        let cleaned_branch = branch_name.replace(['/', '-', '_'], " ");

        format!("feat: {}", cleaned_branch)
    }

    /// Create new GitHub PR using gh CLI
    fn create_github_pr(
        &self,
        repo_view: &crate::data::RepositoryView,
        title: &str,
        description: &str,
        is_draft: bool,
        new_base: Option<&str>,
    ) -> Result<()> {
        use std::process::Command;

        // Get branch name
        let branch_name = repo_view
            .branch_info
            .as_ref()
            .map(|bi| &bi.branch)
            .context("Branch info not available")?;

        let pr_status = if is_draft {
            "draft"
        } else {
            "ready for review"
        };
        println!("🚀 Creating pull request ({})...", pr_status);
        println!("   📋 Title: {}", title);
        println!("   🌿 Branch: {}", branch_name);
        if let Some(base) = new_base {
            println!("   🎯 Base: {}", base);
        }

        // Check if branch is pushed to remote and push if needed
        debug!("Opening git repository to check branch status");
        let git_repo =
            crate::git::GitRepository::open().context("Failed to open git repository")?;

        debug!(
            "Checking if branch '{}' exists on remote 'origin'",
            branch_name
        );
        if !git_repo.branch_exists_on_remote(branch_name, "origin")? {
            println!("📤 Pushing branch to remote...");
            debug!(
                "Branch '{}' not found on remote, attempting to push",
                branch_name
            );
            git_repo
                .push_branch(branch_name, "origin")
                .context("Failed to push branch to remote")?;
        } else {
            debug!("Branch '{}' already exists on remote 'origin'", branch_name);
        }

        // Create PR using gh CLI with explicit head branch
        debug!("Creating PR with gh CLI - title: '{}'", title);
        debug!("PR description length: {} characters", description.len());
        debug!("PR draft status: {}", is_draft);
        if let Some(base) = new_base {
            debug!("PR base branch: {}", base);
        }

        let mut args = vec![
            "pr",
            "create",
            "--head",
            branch_name,
            "--title",
            title,
            "--body",
            description,
        ];

        if let Some(base) = new_base {
            args.push("--base");
            args.push(base);
        }

        if is_draft {
            args.push("--draft");
        }

        let pr_result = Command::new("gh")
            .args(&args)
            .output()
            .context("Failed to create pull request")?;

        if pr_result.status.success() {
            let pr_url = String::from_utf8_lossy(&pr_result.stdout);
            let pr_url = pr_url.trim();
            debug!("PR created successfully with URL: {}", pr_url);
            println!("🎉 Pull request created: {}", pr_url);
        } else {
            let error_msg = String::from_utf8_lossy(&pr_result.stderr);
            error!("gh CLI failed to create PR: {}", error_msg);
            anyhow::bail!("Failed to create pull request: {}", error_msg);
        }

        Ok(())
    }

    /// Update existing GitHub PR using gh CLI
    fn update_github_pr(
        &self,
        repo_view: &crate::data::RepositoryView,
        title: &str,
        description: &str,
        new_base: Option<&str>,
    ) -> Result<()> {
        use std::io::{self, Write};
        use std::process::Command;

        // Get the first existing PR (assuming we're updating the most recent one)
        let existing_pr = repo_view
            .branch_prs
            .as_ref()
            .and_then(|prs| prs.first())
            .context("No existing PR found to update")?;

        let pr_number = existing_pr.number;
        let current_base = &existing_pr.base;

        println!("🚀 Updating pull request #{}...", pr_number);
        println!("   📋 Title: {}", title);

        // Check if base branch should be changed
        let change_base = if let Some(base) = new_base {
            if !current_base.is_empty() && current_base != base {
                print!(
                    "   🎯 Current base: {} → New base: {}. Change? [y/N]: ",
                    current_base, base
                );
                io::stdout().flush()?;

                let mut input = String::new();
                io::stdin().read_line(&mut input)?;
                let response = input.trim().to_lowercase();
                response == "y" || response == "yes"
            } else {
                false
            }
        } else {
            false
        };

        debug!(
            pr_number = pr_number,
            title = %title,
            description_length = description.len(),
            description_preview = %description.lines().take(3).collect::<Vec<_>>().join("\\n"),
            change_base = change_base,
            "Updating GitHub PR with title and description"
        );

        // Update PR using gh CLI
        let pr_number_str = pr_number.to_string();
        let mut gh_args = vec![
            "pr",
            "edit",
            &pr_number_str,
            "--title",
            title,
            "--body",
            description,
        ];

        if change_base {
            if let Some(base) = new_base {
                gh_args.push("--base");
                gh_args.push(base);
            }
        }

        debug!(
            args = ?gh_args,
            "Executing gh command to update PR"
        );

        let pr_result = Command::new("gh")
            .args(&gh_args)
            .output()
            .context("Failed to update pull request")?;

        if pr_result.status.success() {
            // Get the PR URL using the existing PR data
            println!("🎉 Pull request updated: {}", existing_pr.url);
            if change_base {
                if let Some(base) = new_base {
                    println!("   🎯 Base branch changed to: {}", base);
                }
            }
        } else {
            let error_msg = String::from_utf8_lossy(&pr_result.stderr);
            anyhow::bail!("Failed to update pull request: {}", error_msg);
        }

        Ok(())
    }

    /// Show model information from actual AI client
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
}

impl CheckCommand {
    /// Execute check command - validates commit messages against guidelines
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

        // 5. Check if batching is needed
        let report = if repo_view.commits.len() > self.batch_size {
            if !self.quiet && output_format == OutputFormat::Text {
                println!(
                    "📦 Processing {} commits in batches of {}...",
                    repo_view.commits.len(),
                    self.batch_size
                );
            }
            self.check_with_batching(
                &claude_client,
                &repo_view,
                guidelines.as_deref(),
                &valid_scopes,
            )
            .await?
        } else {
            // 6. Single batch check
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

    /// Generate repository view (reuse logic from TwiddleCommand)
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
        let commit_range = match &self.commit_range {
            Some(range) => range.clone(),
            None => {
                // Default to commits ahead of main branch
                let base = if repo.branch_exists("main")? {
                    "main"
                } else if repo.branch_exists("master")? {
                    "master"
                } else {
                    "HEAD~5"
                };
                format!("{}..HEAD", base)
            }
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

    /// Load commit guidelines from file or context directory
    async fn load_guidelines(&self) -> Result<Option<String>> {
        use std::fs;

        // If explicit guidelines path is provided, use it
        if let Some(guidelines_path) = &self.guidelines {
            let content = fs::read_to_string(guidelines_path).with_context(|| {
                format!("Failed to read guidelines file: {:?}", guidelines_path)
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

        // No custom guidelines found, will use defaults
        Ok(None)
    }

    /// Load valid scopes from context directory
    ///
    /// This ensures the check command uses the same scopes as the twiddle command,
    /// preventing false positives when validating commit messages.
    fn load_scopes(&self) -> Vec<crate::data::context::ScopeDefinition> {
        use crate::data::context::ScopeDefinition;
        use std::fs;

        // Local config struct matching the YAML format
        #[derive(serde::Deserialize)]
        struct ScopesConfig {
            scopes: Vec<ScopeDefinition>,
        }

        let context_dir = self
            .context_dir
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from(".omni-dev"));

        // Try local override first
        let local_path = context_dir.join("local").join("scopes.yaml");
        if local_path.exists() {
            if let Ok(content) = fs::read_to_string(&local_path) {
                if let Ok(config) = serde_yaml::from_str::<ScopesConfig>(&content) {
                    return config.scopes;
                }
            }
        }

        // Try project-level scopes
        let project_path = context_dir.join("scopes.yaml");
        if project_path.exists() {
            if let Ok(content) = fs::read_to_string(&project_path) {
                if let Ok(config) = serde_yaml::from_str::<ScopesConfig>(&content) {
                    return config.scopes;
                }
            }
        }

        // Try global scopes
        if let Some(home) = dirs::home_dir() {
            let home_path = home.join(".omni-dev").join("scopes.yaml");
            if home_path.exists() {
                if let Ok(content) = fs::read_to_string(&home_path) {
                    if let Ok(config) = serde_yaml::from_str::<ScopesConfig>(&content) {
                        return config.scopes;
                    }
                }
            }
        }

        // No scopes found
        Vec::new()
    }

    /// Show diagnostic information about loaded guidance files
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

    /// Check commits with batching for large commit ranges
    async fn check_with_batching(
        &self,
        claude_client: &crate::claude::client::ClaudeClient,
        full_repo_view: &crate::data::RepositoryView,
        guidelines: Option<&str>,
        valid_scopes: &[crate::data::context::ScopeDefinition],
    ) -> Result<crate::data::check::CheckReport> {
        use crate::data::check::{CheckReport, CommitCheckResult};

        let commit_batches: Vec<_> = full_repo_view.commits.chunks(self.batch_size).collect();
        let total_batches = commit_batches.len();
        let mut all_results: Vec<CommitCheckResult> = Vec::new();

        for (batch_num, commit_batch) in commit_batches.into_iter().enumerate() {
            if !self.quiet {
                println!(
                    "🔄 Processing batch {}/{} ({} commits)...",
                    batch_num + 1,
                    total_batches,
                    commit_batch.len()
                );
            }

            // Create a repository view for just this batch
            let batch_repo_view = crate::data::RepositoryView {
                versions: full_repo_view.versions.clone(),
                explanation: full_repo_view.explanation.clone(),
                working_directory: full_repo_view.working_directory.clone(),
                remotes: full_repo_view.remotes.clone(),
                ai: full_repo_view.ai.clone(),
                branch_info: full_repo_view.branch_info.clone(),
                pr_template: full_repo_view.pr_template.clone(),
                pr_template_location: full_repo_view.pr_template_location.clone(),
                branch_prs: full_repo_view.branch_prs.clone(),
                commits: commit_batch.to_vec(),
            };

            // Check this batch with scopes
            let batch_report = claude_client
                .check_commits_with_scopes(
                    &batch_repo_view,
                    guidelines,
                    valid_scopes,
                    !self.no_suggestions,
                )
                .await?;

            // Merge results
            all_results.extend(batch_report.commits);

            if batch_num + 1 < total_batches {
                // Small delay between batches
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            }
        }

        Ok(CheckReport::new(all_results))
    }

    /// Output the check report in the specified format
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
                println!("{}", json);
                Ok(())
            }
            OutputFormat::Yaml => {
                let yaml =
                    crate::data::to_yaml(report).context("Failed to serialize report to YAML")?;
                println!("{}", yaml);
                Ok(())
            }
        }
    }

    /// Output text format report
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
                        println!("      {}", line);
                    }
                    if self.verbose {
                        println!();
                        println!("   Why this is better:");
                        for line in suggestion.explanation.lines() {
                            println!("   {}", line);
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

    /// Show model information
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

    /// Build amendments from check report suggestions for failing commits.
    fn build_amendments_from_suggestions(
        &self,
        report: &crate::data::check::CheckReport,
        repo_view: &crate::data::RepositoryView,
    ) -> Vec<crate::data::amendments::Amendment> {
        use crate::data::amendments::Amendment;

        report
            .commits
            .iter()
            .filter(|r| !r.passes && r.suggestion.is_some())
            .filter_map(|r| {
                let suggestion = r.suggestion.as_ref().unwrap();
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

    /// Prompt user to apply suggested amendments and apply them if accepted.
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
