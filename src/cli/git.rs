//! Git-related CLI commands

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing::debug;

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

    /// Claude API model to use (defaults to claude-3-5-sonnet-20241022)
    #[arg(long, default_value = "claude-3-5-sonnet-20241022")]
    pub model: String,

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
}

/// Info command options
#[derive(Parser)]
pub struct InfoCommand {
    /// Base branch to compare against (defaults to main/master)
    #[arg(value_name = "BASE_BRANCH")]
    pub base_branch: Option<String>,
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
        let claude_client = crate::claude::create_default_claude_client(Some(self.model.clone()))?;

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

            // 8. Apply amendments
            self.apply_amendments(amendments).await?;
            println!("✅ Commit messages improved successfully!");
        } else {
            println!("✨ All commit messages are already well-formatted!");
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
        let claude_client = crate::claude::create_default_claude_client(Some(self.model.clone()))?;

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

            // Apply all amendments
            self.apply_amendments(all_amendments).await?;
            println!("✅ Commit messages improved successfully!");
        } else {
            println!("✨ All commit messages are already well-formatted!");
        }

        Ok(())
    }

    /// Generate repository view (reuse ViewCommand logic)
    async fn generate_repository_view(&self) -> Result<crate::data::RepositoryView> {
        use crate::data::{
            AiInfo, FieldExplanation, FileStatusInfo, RepositoryView, VersionInfo,
            WorkingDirectoryInfo,
        };
        use crate::git::{GitRepository, RemoteInfo};
        use crate::utils::ai_scratch;

        let commit_range = self.commit_range.as_deref().unwrap_or("HEAD~5..HEAD");

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
            print!("❓ [A]pply amendments, [S]how file, or [Q]uit? [A/s/q] ");
            io::stdout().flush()?;

            let mut input = String::new();
            io::stdin().read_line(&mut input)?;

            match input.trim().to_lowercase().as_str() {
                "a" | "apply" | "" => return Ok(true),
                "s" | "show" => {
                    self.show_amendments_file(amendments_file)?;
                    println!();
                }
                "q" | "quit" => return Ok(false),
                _ => {
                    println!(
                        "Invalid choice. Please enter 'a' to apply, 's' to show, or 'q' to quit."
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

    /// Apply amendments using existing AmendmentHandler logic
    async fn apply_amendments(
        &self,
        amendments: crate::data::amendments::AmendmentFile,
    ) -> Result<()> {
        use crate::git::AmendmentHandler;

        // Create temporary file for amendments
        let temp_dir = tempfile::tempdir()?;
        let temp_file = temp_dir.path().join("twiddle_amendments.yaml");
        amendments.save_to_file(&temp_file)?;

        // Use AmendmentHandler to apply amendments
        let handler = AmendmentHandler::new().context("Failed to initialize amendment handler")?;
        handler
            .apply_amendments(&temp_file.to_string_lossy())
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
        use crate::git::GitRepository;

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
                context.project = project_context;
            }
            Err(e) => {
                debug!(error = %e, "Discovery failed");
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
}

impl BranchCommand {
    /// Execute branch command
    pub fn execute(self) -> Result<()> {
        match self.command {
            BranchSubcommands::Info(info_cmd) => info_cmd.execute(),
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
        let pr_template = Self::read_pr_template().ok();

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

    /// Read PR template file if it exists
    fn read_pr_template() -> Result<String> {
        use std::fs;
        use std::path::Path;

        let template_path = Path::new(".github/pull_request_template.md");
        if template_path.exists() {
            fs::read_to_string(template_path)
                .context("Failed to read .github/pull_request_template.md")
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
