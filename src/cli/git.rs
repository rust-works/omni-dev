//! Git-related CLI commands

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing::{debug, error};

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
    /// Base branch to compare against (defaults to main/master)
    #[arg(value_name = "BASE_BRANCH")]
    pub base_branch: Option<String>,

    /// Skip confirmation prompt and create PR automatically
    #[arg(long)]
    pub auto_apply: bool,

    /// Save generated PR details to file without creating PR
    #[arg(long, value_name = "FILE")]
    pub save_only: Option<String>,
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
        let full_repo_view = self.generate_repository_view().await?;

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

        // 4. Show context summary if available
        if let Some(ref ctx) = context {
            self.show_context_summary(ctx)?;
        }

        // 5. Initialize Claude client
        let claude_client = crate::claude::create_default_claude_client(self.model.clone())?;

        // Show model information
        self.show_model_info_from_client(&claude_client)?;

        // 6. Generate amendments via Claude API with context
        if use_contextual && context.is_some() {
            println!("🤖 Analyzing commits with enhanced contextual intelligence...");
        } else {
            println!("🤖 Analyzing commits with Claude AI...");
        }

        let amendments = if let Some(ctx) = context {
            claude_client
                .generate_contextual_amendments(&full_repo_view, &ctx)
                .await?
        } else {
            claude_client.generate_amendments(&full_repo_view).await?
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
        let claude_client = crate::claude::create_default_claude_client(self.model.clone())?;

        // Show model information
        self.show_model_info_from_client(&claude_client)?;

        // Split commits into batches
        let commit_batches: Vec<_> = full_repo_view.commits.chunks(self.batch_size).collect();

        let total_batches = commit_batches.len();
        let mut all_amendments = AmendmentFile {
            amendments: Vec::new(),
        };

        println!("📊 Processing {} batches...", total_batches);

        for (batch_num, commit_batch) in commit_batches.into_iter().enumerate() {
            println!(
                "🔄 Processing batch {}/{} ({} commits)...",
                batch_num + 1,
                total_batches,
                commit_batch.len()
            );

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

            // Collect context for this batch if needed
            let batch_context = if use_contextual {
                Some(self.collect_context(&batch_repo_view).await?)
            } else {
                None
            };

            // Generate amendments for this batch
            let batch_amendments = if let Some(ctx) = batch_context {
                claude_client
                    .generate_contextual_amendments(&batch_repo_view, &ctx)
                    .await?
            } else {
                claude_client.generate_amendments(&batch_repo_view).await?
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
            println!("   📤 Max output tokens: {}", spec.max_output_tokens);
            println!("   📥 Input context: {}", spec.input_context);

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
                "number,title,state,url,body",
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
                    prs.push(crate::data::PullRequest {
                        number,
                        title: title.to_string(),
                        state: state.to_string(),
                        url: url.to_string(),
                        body: body.to_string(),
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
    /// Execute create PR command
    pub async fn execute(self) -> Result<()> {
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
        let claude_client = crate::claude::create_default_claude_client(None)?;
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

        // 8. Validate environment only when we're about to create the PR
        self.validate_environment()?;

        // 9. Create or update PR (re-read from file to capture any user edits)
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

        match pr_action {
            PrAction::CreateNew => {
                self.create_github_pr(
                    &repo_view,
                    &final_pr_content.title,
                    &final_pr_content.description,
                )?;
                println!("✅ Pull request created successfully!");
            }
            PrAction::UpdateExisting => {
                self.update_github_pr(
                    &repo_view,
                    &final_pr_content.title,
                    &final_pr_content.description,
                )?;
                println!("✅ Pull request updated successfully!");
            }
            PrAction::Cancel => unreachable!(), // Already handled above
        }

        Ok(())
    }

    /// Validate environment and dependencies
    fn validate_environment(&self) -> Result<()> {
        // Check if gh CLI is available
        let gh_check = std::process::Command::new("gh")
            .args(["--version"])
            .output();

        match gh_check {
            Ok(output) if output.status.success() => {
                // Test if gh can access the current repo (this validates both auth and repo access)
                let repo_check = std::process::Command::new("gh")
                    .args(["repo", "view", "--json", "name"])
                    .output();

                match repo_check {
                    Ok(repo_output) if repo_output.status.success() => Ok(()),
                    Ok(repo_output) => {
                        // Get more specific error from stderr
                        let error_details = String::from_utf8_lossy(&repo_output.stderr);
                        if error_details.contains("authentication") || error_details.contains("login") {
                            anyhow::bail!("GitHub CLI (gh) authentication failed. Please run 'gh auth login' or check your GITHUB_TOKEN environment variable.")
                        } else {
                            anyhow::bail!("GitHub CLI (gh) cannot access this repository. Error: {}", error_details.trim())
                        }
                    }
                    Err(e) => anyhow::bail!("Failed to test GitHub CLI access: {}", e),
                }
            }
            _ => anyhow::bail!("GitHub CLI (gh) is not installed or not available in PATH. Please install it from https://cli.github.com/"),
        }
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
        let base_branch = match self.base_branch.as_ref() {
            Some(branch) => {
                // User specified base branch - need to determine if it's local or remote format
                let remote_branch = if branch.contains('/') {
                    // Already in remote format (e.g., "origin/main")
                    branch.clone()
                } else {
                    // Local branch name - convert to remote format
                    format!("{}/{}", primary_remote.name, branch)
                };

                // Validate that the remote branch exists
                let remote_ref = format!("refs/remotes/{}", remote_branch);
                if repo.repository().find_reference(&remote_ref).is_err() {
                    anyhow::bail!("Remote branch '{}' does not exist", remote_branch);
                }
                remote_branch
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
        let base_branch = match self.base_branch.as_ref() {
            Some(branch) => {
                // User specified base branch
                if branch.contains('/') {
                    // Already in remote format
                    branch.clone()
                } else {
                    // Get the primary remote name from repo_view
                    let primary_remote_name = repo_view
                        .remotes
                        .iter()
                        .find(|r| r.name == "origin")
                        .or_else(|| repo_view.remotes.first())
                        .map(|r| r.name.as_str())
                        .unwrap_or("origin");
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
    ) -> Result<()> {
        use std::process::Command;

        // Get branch name
        let branch_name = repo_view
            .branch_info
            .as_ref()
            .map(|bi| &bi.branch)
            .context("Branch info not available")?;

        println!("🚀 Creating pull request...");
        println!("   📋 Title: {}", title);
        println!("   🌿 Branch: {}", branch_name);

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

        // Create PR using gh CLI
        debug!("Creating PR with gh CLI - title: '{}'", title);
        debug!("PR description length: {} characters", description.len());

        let pr_result = Command::new("gh")
            .args(["pr", "create", "--title", title, "--body", description])
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
    ) -> Result<()> {
        use std::process::Command;

        // Get the first existing PR number (assuming we're updating the most recent one)
        let pr_number = repo_view
            .branch_prs
            .as_ref()
            .and_then(|prs| prs.first())
            .map(|pr| pr.number)
            .context("No existing PR found to update")?;

        println!("🚀 Updating pull request #{}...", pr_number);
        println!("   📋 Title: {}", title);

        debug!(
            pr_number = pr_number,
            title = %title,
            description_length = description.len(),
            description_preview = %description.lines().take(3).collect::<Vec<_>>().join("\\n"),
            "Updating GitHub PR with title and description"
        );

        // Update PR using gh CLI
        let gh_args = [
            "pr",
            "edit",
            &pr_number.to_string(),
            "--title",
            title,
            "--body",
            description,
        ];

        debug!(
            args = ?gh_args,
            "Executing gh command to update PR"
        );

        let pr_result = Command::new("gh")
            .args(gh_args)
            .output()
            .context("Failed to update pull request")?;

        if pr_result.status.success() {
            // Get the PR URL using the existing PR data
            if let Some(existing_pr) = repo_view.branch_prs.as_ref().and_then(|prs| prs.first()) {
                println!("🎉 Pull request updated: {}", existing_pr.url);
            } else {
                println!("🎉 Pull request #{} updated successfully!", pr_number);
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
            println!("   📤 Max output tokens: {}", spec.max_output_tokens);
            println!("   📥 Input context: {}", spec.input_context);

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
