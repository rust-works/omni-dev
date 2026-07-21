//! Create PR command — AI-powered pull request creation.

use anyhow::{bail, Context, Result};
use clap::Parser;
use tracing::{debug, error, warn};

use super::info::InfoCommand;
use crate::claude::error::is_transient_ai_error as ai_error_is_transient;

/// Create PR command options.
#[derive(Parser)]
pub struct CreatePrCommand {
    /// Base branch for the PR to be merged into (defaults to main/master).
    #[arg(long, value_name = "BRANCH")]
    pub base: Option<String>,

    /// Skips confirmation prompt and creates PR automatically.
    #[arg(long)]
    pub auto_apply: bool,

    /// Saves generated PR details to file without creating PR.
    #[arg(long, value_name = "FILE")]
    pub save_only: Option<String>,

    /// Creates PR as ready for review (overrides default).
    #[arg(long, conflicts_with = "draft")]
    pub ready: bool,

    /// Creates PR as draft (overrides default).
    #[arg(long, conflicts_with = "ready")]
    pub draft: bool,

    /// Path to custom context directory (defaults to .omni-dev/).
    #[arg(long)]
    pub context_dir: Option<std::path::PathBuf>,

    /// Skip pushing the branch to remote before creating the PR.
    #[arg(long)]
    pub no_push: bool,

    /// Use commit messages (not the diff) as the primary input for PR generation.
    #[arg(long)]
    pub from_commits: bool,
}

/// PR action choices.
#[derive(Debug, PartialEq)]
enum PrAction {
    CreateNew,
    UpdateExisting,
    Cancel,
}

/// AI-generated PR content with structured fields.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PrContent {
    /// Concise PR title (ideally 50-80 characters).
    pub title: String,
    /// Full PR description in markdown format.
    pub description: String,
}

/// PR content plus how it was produced.
///
/// `used_fallback` is deliberately kept out of [`PrContent`], which is
/// serialized to `pr-details.yaml` and must keep its two-field schema.
struct GeneratedPr {
    /// The title and description to apply.
    content: PrContent,
    /// True when the AI call failed and `content` is template-derived.
    used_fallback: bool,
}

impl GeneratedPr {
    /// Wraps content the AI produced successfully.
    fn from_ai(content: PrContent) -> Self {
        Self {
            content,
            used_fallback: false,
        }
    }

    /// Wraps template-derived content produced after an AI failure.
    fn from_fallback(content: PrContent) -> Self {
        Self {
            content,
            used_fallback: true,
        }
    }
}

impl CreatePrCommand {
    /// Determines if the PR should be created as draft.
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
            .and_then(|val| parse_bool_string(&val))
            .unwrap_or(true) // Default to draft if not configured
    }

    /// Executes the create PR command.
    pub async fn execute(self, repo: Option<&std::path::Path>) -> Result<()> {
        // Resolve the repo root once; every git, config, scratch, PR-template,
        // and `gh` read below anchors to it (the CWD is the default when no
        // path is injected).
        let repo_root = match repo {
            Some(p) => p.to_path_buf(),
            None => std::env::current_dir().context("Failed to determine current directory")?,
        };
        let repo_root = repo_root.as_path();

        // Preflight check: validate all prerequisites before any processing
        // This catches missing credentials/tools early before wasting time
        // Model selection uses the global `--model` flag (propagated as
        // OMNI_DEV_MODEL) and the per-backend env chain.
        let ai_info = crate::utils::check_pr_command_prerequisites(None, repo_root)?;
        println!(
            "✓ {} credentials verified (model: {})",
            ai_info.provider, ai_info.model
        );
        println!("✓ GitHub CLI verified");

        println!("🔄 Starting pull request creation process...");

        // 1. Generate repository view (reuse InfoCommand logic)
        let repo_view = self.generate_repository_view(repo_root)?;

        // 2. Validate branch state (always needed)
        self.validate_branch_state(&repo_view)?;

        // 3. Show guidance files status early (before AI processing)
        use crate::claude::context::ProjectDiscovery;
        let context_dir =
            crate::claude::context::resolve_context_dir_at(self.context_dir.as_deref(), repo_root);
        let discovery = ProjectDiscovery::new(repo_root.to_path_buf(), context_dir);
        let project_context = discovery.discover().unwrap_or_default();
        self.show_guidance_files_status(repo_root, &project_context)?;

        // 4. Show AI model configuration before generation
        let claude_client = crate::claude::create_default_claude_client(None, None).await?;
        self.show_model_info_from_client(&claude_client)?;

        // 5. Show branch analysis and commit information
        self.show_commit_range_info(&repo_view)?;

        // 6. Show context analysis (quick collection for display only)
        let context = {
            use crate::claude::context::{BranchAnalyzer, FileAnalyzer, WorkPatternAnalyzer};
            use crate::data::context::CommitContext;
            let mut context = CommitContext::new();
            context.project = project_context;

            // Quick analysis for display
            if let Some(branch_info) = &repo_view.branch_info {
                context.branch = BranchAnalyzer::analyze(&branch_info.branch).unwrap_or_default();
            }

            if !repo_view.commits.is_empty() {
                context.range = WorkPatternAnalyzer::analyze_commit_range(&repo_view.commits);
                context.files = FileAnalyzer::analyze_commits(&repo_view.commits);
            }
            context
        };
        self.show_context_summary(&context)?;

        // 7. Generate AI-powered PR content (title + description)
        debug!("About to generate PR content from AI");
        let (generated, _claude_client) = self
            .generate_pr_content_with_client_internal(repo_root, &repo_view, claude_client)
            .await?;
        let GeneratedPr {
            content: pr_content,
            used_fallback,
        } = generated;

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
            println!("💾 PR details saved to: {save_path}");
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

        if used_fallback && self.auto_apply && pr_action == PrAction::UpdateExisting {
            self.refuse_template_clobber(&repo_view)?;
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
                    repo_root,
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
                    repo_root,
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

    /// Generates the repository view (reuses InfoCommand logic).
    fn generate_repository_view(
        &self,
        repo_root: &std::path::Path,
    ) -> Result<crate::data::RepositoryView> {
        use crate::data::{
            AiInfo, BranchInfo, FieldExplanation, FileStatusInfo, RepositoryView, VersionInfo,
            WorkingDirectoryInfo,
        };
        use crate::git::{GitRepository, RemoteInfo};
        use crate::utils::ai_scratch;

        // Open git repository at the injected root
        let repo = GitRepository::open_at(repo_root)
            .context("Failed to open git repository at the given path")?;

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
        let base_branch = if let Some(branch) = self.base.as_ref() {
            // User specified base branch - try to resolve it
            // First, check if it's already a valid remote ref (e.g., "origin/main")
            let remote_ref = format!("refs/remotes/{branch}");
            if repo.repository().find_reference(&remote_ref).is_ok() {
                branch.clone()
            } else {
                // Try prepending the primary remote name (e.g., "main" -> "origin/main")
                let with_remote = format!("{}/{}", primary_remote.name, branch);
                let remote_ref = format!("refs/remotes/{with_remote}");
                if repo.repository().find_reference(&remote_ref).is_ok() {
                    with_remote
                } else {
                    anyhow::bail!(
                        "Remote branch '{branch}' does not exist (also tried '{with_remote}')"
                    );
                }
            }
        } else {
            // Auto-detect using the primary remote's main branch
            let main_branch = &primary_remote.main_branch;
            if main_branch == "unknown" {
                let remote_name = &primary_remote.name;
                anyhow::bail!("Could not determine main branch for remote '{remote_name}'");
            }

            let remote_main = format!("{}/{}", primary_remote.name, main_branch);

            // Validate that the remote main branch exists
            let remote_ref = format!("refs/remotes/{remote_main}");
            if repo.repository().find_reference(&remote_ref).is_err() {
                anyhow::bail!(
                    "Remote main branch '{remote_main}' does not exist. Try running 'git fetch' first."
                );
            }

            remote_main
        };

        // Calculate commit range: [remote_base]..HEAD
        let commit_range = format!("{base_branch}..HEAD");

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
        let pr_template_result = InfoCommand::read_pr_template(repo_root).ok();
        let (pr_template, pr_template_location) = match pr_template_result {
            Some((content, location)) => (Some(content), Some(location)),
            None => (None, None),
        };

        // Get PRs for current branch
        let branch_prs = InfoCommand::get_branch_prs(&current_branch, repo_root)
            .ok()
            .filter(|prs| !prs.is_empty());

        // Create version information
        let versions = Some(VersionInfo {
            omni_dev: env!("CARGO_PKG_VERSION").to_string(),
        });

        // Get AI scratch directory
        let ai_scratch_path = ai_scratch::get_ai_scratch_dir_at(repo_root)
            .context("Failed to determine AI scratch directory")?;
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

    /// Validates the branch state for PR creation.
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

    /// Shows detailed context information (similar to twiddle command).
    async fn show_context_information(
        &self,
        _repo_view: &crate::data::RepositoryView,
    ) -> Result<()> {
        // Note: commit range info and context summary are now shown earlier
        // This method is kept for potential future detailed information
        // that should be shown after AI generation

        Ok(())
    }

    /// Shows commit range and count information.
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
                    .map_or("origin", |r| r.name.as_str());
                // Check if already has remote prefix
                if branch.starts_with(&format!("{primary_remote_name}/")) {
                    branch.clone()
                } else {
                    format!("{primary_remote_name}/{branch}")
                }
            }
            None => {
                // Auto-detected base branch from remotes
                repo_view
                    .remotes
                    .iter()
                    .find(|r| r.name == "origin")
                    .or_else(|| repo_view.remotes.first())
                    .map_or_else(
                        || "unknown".to_string(),
                        |r| format!("{}/{}", r.name, r.main_branch),
                    )
            }
        };

        let commit_range = format!("{base_branch}..HEAD");
        let commit_count = repo_view.commits.len();

        // Get current branch name
        let current_branch = repo_view
            .branch_info
            .as_ref()
            .map_or("unknown", |bi| bi.branch.as_str());

        println!("📊 Branch Analysis:");
        println!("   🌿 Current branch: {current_branch}");
        println!("   📏 Commit range: {commit_range}");
        println!("   📝 Commits found: {commit_count} commits");
        println!();

        Ok(())
    }

    /// Collects contextual information for enhanced PR generation (adapted from twiddle).
    async fn collect_context(
        &self,
        repo_root: &std::path::Path,
        repo_view: &crate::data::RepositoryView,
    ) -> Result<crate::data::context::CommitContext> {
        use crate::claude::context::{
            BranchAnalyzer, FileAnalyzer, ProjectDiscovery, WorkPatternAnalyzer,
        };
        use crate::data::context::{CommitContext, ProjectContext};
        use crate::git::GitRepository;

        let mut context = CommitContext::new();

        // 1. Discover project context
        let context_dir =
            crate::claude::context::resolve_context_dir_at(self.context_dir.as_deref(), repo_root);

        // ProjectDiscovery takes repo root and context directory
        let discovery = ProjectDiscovery::new(repo_root.to_path_buf(), context_dir);
        match discovery.discover() {
            Ok(project_context) => {
                context.project = project_context;
            }
            Err(_e) => {
                context.project = ProjectContext::default();
            }
        }

        // 2. Analyze current branch
        let repo = GitRepository::open_at(repo_root)?;
        let current_branch = repo
            .get_current_branch()
            .unwrap_or_else(|_| "HEAD".to_string());
        context.branch = BranchAnalyzer::analyze(&current_branch).unwrap_or_default();

        // 3. Analyze commit range patterns
        if !repo_view.commits.is_empty() {
            context.range = WorkPatternAnalyzer::analyze_commit_range(&repo_view.commits);
        }

        // 3.5. Analyze file-level context
        if !repo_view.commits.is_empty() {
            context.files = FileAnalyzer::analyze_commits(&repo_view.commits);
        }

        Ok(context)
    }

    /// Shows guidance files status (adapted from twiddle).
    fn show_guidance_files_status(
        &self,
        repo_root: &std::path::Path,
        project_context: &crate::data::context::ProjectContext,
    ) -> Result<()> {
        use crate::claude::context::{
            config_source_label, resolve_context_dir_with_source_at, ConfigSourceLabel,
        };

        let (context_dir, dir_source) =
            resolve_context_dir_with_source_at(self.context_dir.as_deref(), repo_root);

        println!("📋 Project guidance files status:");
        println!("   📂 Config dir: {} ({dir_source})", context_dir.display());

        // Check PR guidelines (for PR commands)
        let pr_guidelines_source = if project_context.pr_guidelines.is_some() {
            match config_source_label(&context_dir, "pr-guidelines.md") {
                ConfigSourceLabel::NotFound => "✅ (source unknown)".to_string(),
                label => format!("✅ {label}"),
            }
        } else {
            "❌ None found".to_string()
        };
        println!("   🔀 PR guidelines: {pr_guidelines_source}");

        // Check scopes
        let scopes_count = project_context.valid_scopes.len();
        let scopes_source = if scopes_count > 0 {
            match config_source_label(&context_dir, "scopes.yaml") {
                ConfigSourceLabel::NotFound => {
                    format!("✅ (source unknown + ecosystem defaults) ({scopes_count} scopes)")
                }
                label => format!("✅ {label} ({scopes_count} scopes)"),
            }
        } else {
            "❌ None found".to_string()
        };
        println!("   🎯 Valid scopes: {scopes_source}");

        // Check PR template
        let pr_template_path = repo_root.join(".github/pull_request_template.md");
        let pr_template_status = if pr_template_path.exists() {
            format!("✅ Project: {}", pr_template_path.display())
        } else {
            "❌ None found".to_string()
        };
        println!("   📋 PR template: {pr_template_status}");

        println!();
        Ok(())
    }

    /// Shows the context summary (adapted from twiddle).
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
                println!("   🎫 Ticket: {ticket}");
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

        // File analysis
        if let Some(label) = super::formatting::format_file_analysis(&context.files) {
            println!("   {label}");
        }

        // Verbosity level
        match context.suggested_verbosity() {
            VerbosityLevel::Comprehensive => {
                println!("   📝 Detail level: Comprehensive (significant changes detected)");
            }
            VerbosityLevel::Detailed => println!("   📝 Detail level: Detailed"),
            VerbosityLevel::Concise => println!("   📝 Detail level: Concise"),
        }

        println!();
        Ok(())
    }

    /// Generates PR content with a pre-created client (internal method that does not show model info).
    ///
    /// The returned [`GeneratedPr::used_fallback`] reports whether the content
    /// came from the AI or from the template fallback, so callers can refuse to
    /// overwrite a populated PR body with template text (issue #1333).
    async fn generate_pr_content_with_client_internal(
        &self,
        repo_root: &std::path::Path,
        repo_view: &crate::data::RepositoryView,
        claude_client: crate::claude::client::ClaudeClient,
    ) -> Result<(GeneratedPr, crate::claude::client::ClaudeClient)> {
        use tracing::debug;

        let pr_template = self.resolve_pr_template(repo_view);

        debug!(
            pr_template_length = pr_template.len(),
            pr_template_preview = %pr_template.lines().take(5).collect::<Vec<_>>().join("\\n"),
            "Using PR template for generation"
        );

        println!("🤖 Generating AI-powered PR description...");

        // Collect project context for PR guidelines
        debug!("Collecting context for PR generation");
        let context = self.collect_context(repo_root, repo_view).await?;
        debug!("Context collection completed");

        // Generate AI-powered PR content with context
        debug!(
            from_commits = self.from_commits,
            "About to call Claude AI for PR content generation"
        );
        let ai_result = if self.from_commits {
            claude_client
                .generate_pr_content_with_context_from_commits(repo_view, &pr_template, &context)
                .await
        } else {
            claude_client
                .generate_pr_content_with_context(repo_view, &pr_template, &context)
                .await
        };
        match ai_result {
            Ok(pr_content) => {
                debug!(
                    ai_generated_title = %pr_content.title,
                    ai_generated_description_length = pr_content.description.len(),
                    ai_generated_description_preview = %pr_content.description.lines().take(3).collect::<Vec<_>>().join("\\n"),
                    "AI successfully generated PR content"
                );
                Ok((GeneratedPr::from_ai(pr_content), claude_client))
            }
            // A permanent failure can never succeed on a retry, so degrading to
            // a template would report success for work that did not happen.
            Err(e) if !ai_error_is_transient(&e) => {
                Err(e).context("AI PR generation failed with a non-retryable error")
            }
            Err(e) => {
                let content = self.fallback_pr_content(&e, pr_template, repo_view)?;
                Ok((GeneratedPr::from_fallback(content), claude_client))
            }
        }
    }

    /// Builds template-derived PR content after a transient AI failure, warning
    /// the user that the result is degraded.
    ///
    /// The warning goes to stderr rather than stdout: [`run_create_pr`] shares
    /// this path and the MCP server owns stdout for JSON-RPC.
    fn fallback_pr_content(
        &self,
        error: &anyhow::Error,
        pr_template: String,
        repo_view: &crate::data::RepositoryView,
    ) -> Result<PrContent> {
        warn!(error = %format!("{error:#}"), "AI PR generation failed, falling back to the PR template");
        eprintln!("warning: AI PR generation failed: {error:#}");
        eprintln!(
            "warning: falling back to the PR template — the description below is not AI-generated."
        );

        let mut description = pr_template;
        self.enhance_description_with_commits(&mut description, repo_view)?;
        let title = self.generate_title_from_commits(repo_view);

        debug!(
            fallback_title = %title,
            fallback_description_length = description.len(),
            "Created fallback PR content"
        );

        Ok(PrContent { title, description })
    }

    /// Returns the PR template for this repository, falling back to the
    /// built-in default when the repository has none.
    fn resolve_pr_template(&self, repo_view: &crate::data::RepositoryView) -> String {
        match &repo_view.pr_template {
            Some(template) => template.clone(),
            None => self.get_default_pr_template(),
        }
    }

    /// Refuses to replace a populated PR description with template content
    /// after the AI failed (issue #1333).
    ///
    /// Overwriting is irreversible from this tool's side, so a non-interactive
    /// run stops rather than destroying a real description. Only applies when
    /// the user cannot see and approve the content first — an interactive run
    /// displays it and asks.
    fn refuse_template_clobber(&self, repo_view: &crate::data::RepositoryView) -> Result<()> {
        let Some(existing) = repo_view.branch_prs.as_ref().and_then(|prs| prs.first()) else {
            return Ok(());
        };

        if Self::body_is_safe_to_replace(&existing.body, &self.resolve_pr_template(repo_view)) {
            return Ok(());
        }

        bail!(
            "Refusing to overwrite the description of PR #{} with template content \
             after AI generation failed.\n\
             The existing description would be lost and cannot be recovered by this tool.\n\
             Re-run without --auto-apply to review the content first, or resolve the AI \
             failure reported above.",
            existing.number
        );
    }

    /// Reports whether an existing PR body can be replaced without losing work.
    ///
    /// Safe only when the body is empty or is still the unfilled template.
    /// Anything else — including a previously enhanced template — is treated as
    /// worth keeping, since refusing is recoverable and overwriting is not.
    fn body_is_safe_to_replace(body: &str, template: &str) -> bool {
        let body = body.trim();
        body.is_empty() || body == template.trim()
    }

    /// Returns the default PR template when none exists in the repository.
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

    /// Enhances the PR description with commit analysis.
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
            if is_breaking_change(detected_type, &commit.original_message) {
                has_breaking_changes = true;
            }

            let detected_scope = &commit.analysis.detected_scope;
            if !detected_scope.is_empty() {
                scopes_found.insert(detected_scope.clone());
            }
        }

        // Update type checkboxes based on detected types
        if types_found.contains("feat") {
            check_checkbox(description, "- [ ] New feature");
        }
        if types_found.contains("fix") {
            check_checkbox(description, "- [ ] Bug fix");
        }
        if types_found.contains("docs") {
            check_checkbox(description, "- [ ] Documentation update");
        }
        if types_found.contains("refactor") {
            check_checkbox(description, "- [ ] Refactoring");
        }
        if has_breaking_changes {
            check_checkbox(description, "- [ ] Breaking change");
        }

        // Add detected scopes
        let scopes_list: Vec<_> = scopes_found.into_iter().collect();
        let scopes_section = format_scopes_section(&scopes_list);
        if !scopes_section.is_empty() {
            description.push_str(&scopes_section);
        }

        // Add commit list
        let commit_entries: Vec<(&str, &str)> = repo_view
            .commits
            .iter()
            .map(|c| {
                let short = &c.hash[..crate::git::SHORT_HASH_LEN];
                let first = extract_first_line(&c.original_message);
                (short, first)
            })
            .collect();
        description.push_str(&format_commit_list(&commit_entries));

        // Add file change summary
        let total_files: usize = repo_view
            .commits
            .iter()
            .map(|c| c.analysis.file_changes.total_files)
            .sum();

        if total_files > 0 {
            description.push_str(&format!("\n**Files changed:** {total_files} files\n"));
        }

        Ok(())
    }

    /// Handles the PR description file by showing the path and getting the user choice.
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
        let (status_icon, status_text) = format_draft_status(is_draft);
        println!("{status_icon} PR will be created as: {status_text}");
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

    /// Shows the contents of the PR details file.
    fn show_pr_file(&self, pr_file: &std::path::Path) -> Result<()> {
        use std::fs;

        println!("\n📄 PR details file contents:");
        println!("─────────────────────────────");

        let contents = fs::read_to_string(pr_file).context("Failed to read PR details file")?;
        println!("{contents}");
        println!("─────────────────────────────");

        Ok(())
    }

    /// Opens the PR details file in an external editor.
    fn edit_pr_file(&self, pr_file: &std::path::Path) -> Result<()> {
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

        println!("📝 Opening PR details file in editor: {editor}");

        let (editor_cmd, args) = super::formatting::parse_editor_command(&editor);

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
                println!("❌ Failed to execute editor '{editor}': {e}");
                println!("   Please check that the editor command is correct and available in your PATH.");
            }
        }

        Ok(())
    }

    /// Generates a concise title from commit analysis (fallback).
    fn generate_title_from_commits(&self, repo_view: &crate::data::RepositoryView) -> String {
        if repo_view.commits.is_empty() {
            return "Pull Request".to_string();
        }

        // For single commit, use its first line
        if repo_view.commits.len() == 1 {
            let first = extract_first_line(&repo_view.commits[0].original_message);
            let trimmed = first.trim();
            return if trimmed.is_empty() {
                "Pull Request".to_string()
            } else {
                trimmed.to_string()
            };
        }

        // For multiple commits, generate from branch name
        let branch_name = repo_view
            .branch_info
            .as_ref()
            .map_or("feature", |bi| bi.branch.as_str());

        format!("feat: {}", clean_branch_name(branch_name))
    }

    /// Creates a new GitHub PR using gh CLI.
    fn create_github_pr(
        &self,
        repo_root: &std::path::Path,
        repo_view: &crate::data::RepositoryView,
        title: &str,
        description: &str,
        is_draft: bool,
        new_base: Option<&str>,
    ) -> Result<()> {
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
        println!("🚀 Creating pull request ({pr_status})...");
        println!("   📋 Title: {title}");
        println!("   🌿 Branch: {branch_name}");
        if let Some(base) = new_base {
            println!("   🎯 Base: {base}");
        }

        // Push branch to remote unless --no-push was specified
        let push_action = if self.no_push {
            determine_push_action(true, false)
        } else {
            debug!("Opening git repository to check branch status");
            let git_repo = crate::git::GitRepository::open_at(repo_root)
                .context("Failed to open git repository at the given path")?;

            debug!(
                "Checking if branch '{}' exists on remote 'origin'",
                branch_name
            );
            let branch_on_remote = git_repo.branch_exists_on_remote(branch_name, "origin")?;
            let action = determine_push_action(false, branch_on_remote);

            debug!("Push action for branch '{}': {:?}", branch_name, action);
            println!("📤 Pushing branch to remote...");
            git_repo
                .push_branch(branch_name, "origin")
                .context("Failed to push branch to remote")?;

            action
        };

        if push_action == PushAction::Skip {
            debug!("Skipping push (--no-push flag set)");
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

        let pr_result = crate::github_metrics::run_gh(
            &crate::pr_status::resolve_gh_binary(),
            args,
            "pr create",
            Some(repo_root),
        )
        .context("Failed to create pull request")?;

        if pr_result.status.success() {
            let pr_url = String::from_utf8_lossy(&pr_result.stdout);
            let pr_url = pr_url.trim();
            debug!("PR created successfully with URL: {}", pr_url);
            println!("🎉 Pull request created: {pr_url}");
        } else {
            let error_msg = String::from_utf8_lossy(&pr_result.stderr);
            error!("gh CLI failed to create PR: {}", error_msg);
            anyhow::bail!("Failed to create pull request: {error_msg}");
        }

        Ok(())
    }

    /// Updates an existing GitHub PR using gh CLI.
    fn update_github_pr(
        &self,
        repo_root: &std::path::Path,
        repo_view: &crate::data::RepositoryView,
        title: &str,
        description: &str,
        new_base: Option<&str>,
    ) -> Result<()> {
        use std::io::{self, Write};

        // Get the first existing PR (assuming we're updating the most recent one)
        let existing_pr = repo_view
            .branch_prs
            .as_ref()
            .and_then(|prs| prs.first())
            .context("No existing PR found to update")?;

        let pr_number = existing_pr.number;
        let current_base = &existing_pr.base;

        println!("🚀 Updating pull request #{pr_number}...");
        println!("   📋 Title: {title}");

        // Check if base branch should be changed
        let change_base = if let Some(base) = new_base {
            if !current_base.is_empty() && current_base != base {
                print!("   🎯 Current base: {current_base} → New base: {base}. Change? [y/N]: ");
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

        let pr_result = crate::github_metrics::run_gh(
            &crate::pr_status::resolve_gh_binary(),
            gh_args,
            "pr edit",
            Some(repo_root),
        )
        .context("Failed to update pull request")?;

        if pr_result.status.success() {
            // Get the PR URL using the existing PR data
            println!("🎉 Pull request updated: {}", existing_pr.url);
            if change_base {
                if let Some(base) = new_base {
                    println!("   🎯 Base branch changed to: {base}");
                }
            }
        } else {
            let error_msg = String::from_utf8_lossy(&pr_result.stderr);
            anyhow::bail!("Failed to update pull request: {error_msg}");
        }

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
        // NOTE (#967): this diagnostic banner reads the process-wide model
        // catalog (`get_model_registry` → CWD-relative project models.yaml),
        // not a `--repo`-scoped catalog. It is informational only and does not
        // affect the generated PR content, so it is left CWD-scoped until the
        // repo-aware `ModelRegistry::load_at` foundation lands.
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
}

// --- Extracted pure functions ---

/// Describes what push action should be taken before PR creation.
#[derive(Debug, PartialEq)]
enum PushAction {
    /// Skip pushing entirely (user passed `--no-push`).
    Skip,
    /// Push to sync with an existing remote branch.
    SyncExisting,
    /// Push a new branch to remote.
    PushNew,
}

/// Determines what push action to take based on the `--no-push` flag and remote branch state.
fn determine_push_action(no_push: bool, branch_on_remote: bool) -> PushAction {
    if no_push {
        PushAction::Skip
    } else if branch_on_remote {
        PushAction::SyncExisting
    } else {
        PushAction::PushNew
    }
}

/// Parses a boolean-like string value.
///
/// Accepts "true"/"1"/"yes" as `true` and "false"/"0"/"no" as `false`.
/// Returns `None` for unrecognized values.
fn parse_bool_string(val: &str) -> Option<bool> {
    match val.to_lowercase().as_str() {
        "true" | "1" | "yes" => Some(true),
        "false" | "0" | "no" => Some(false),
        _ => None,
    }
}

/// Returns whether a commit represents a breaking change.
fn is_breaking_change(detected_type: &str, original_message: &str) -> bool {
    detected_type.contains("BREAKING") || original_message.contains("BREAKING CHANGE")
}

/// Checks a markdown checkbox in the description by replacing `- [ ]` with `- [x]`.
fn check_checkbox(description: &mut String, search_text: &str) {
    if let Some(pos) = description.find(search_text) {
        description.replace_range(pos..pos + 5, "- [x]");
    }
}

/// Formats a list of scopes as a markdown "Affected areas" section.
///
/// Returns an empty string if the list is empty.
fn format_scopes_section(scopes: &[String]) -> String {
    if scopes.is_empty() {
        return String::new();
    }
    format!("**Affected areas:** {}\n\n", scopes.join(", "))
}

/// Formats commit entries as a markdown list with short hashes.
fn format_commit_list(entries: &[(&str, &str)]) -> String {
    let mut output = String::from("### Commits in this PR:\n");
    for (hash, message) in entries {
        output.push_str(&format!("- `{hash}` {message}\n"));
    }
    output
}

/// Replaces path separators (`/`, `-`, `_`) in a branch name with spaces.
fn clean_branch_name(branch: &str) -> String {
    branch.replace(['/', '-', '_'], " ")
}

/// Returns the first line of a text block, trimmed.
fn extract_first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("").trim()
}

/// Returns an (icon, label) pair for a PR's draft status.
fn format_draft_status(is_draft: bool) -> (&'static str, &'static str) {
    if is_draft {
        ("\u{1f4cb}", "draft")
    } else {
        ("\u{2705}", "ready for review")
    }
}

/// Structured output from [`run_create_pr`] for programmatic consumers (MCP).
#[derive(Debug, Clone)]
pub struct CreatePrOutcome {
    /// Title as produced by the AI (or the fallback heuristic).
    pub title: String,
    /// Description body as produced by the AI (or the fallback heuristic).
    pub description: String,
    /// YAML serialisation of the [`PrContent`].
    pub pr_yaml: String,
}

/// Non-interactive core for `omni-dev git branch create pr`.
///
/// Generates PR title + description via the AI but does NOT push the branch
/// or call `gh pr create`. The MCP boundary should expose the proposed PR
/// content so the assistant can decide what to do with it; actually pushing
/// a branch or creating a PR is out of scope for a single tool call. This
/// function must produce no stdout output — the MCP server uses stdout for
/// the JSON-RPC protocol.
pub async fn run_create_pr(
    model: Option<String>,
    base_branch: Option<&str>,
    repo_path: Option<&std::path::Path>,
) -> Result<CreatePrOutcome> {
    // Resolve the repo root once; the repository view and context discovery
    // anchor to it (the CWD is the default when no path is injected), so no
    // read resolves against the process working directory.
    let repo_root = match repo_path {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir().context("Failed to determine current directory")?,
    };

    crate::utils::check_pr_command_prerequisites(model.as_deref(), &repo_root)?;

    let cmd = CreatePrCommand {
        base: base_branch.map(str::to_string),
        auto_apply: true,
        save_only: None,
        ready: false,
        draft: false,
        context_dir: None,
        no_push: true,
        from_commits: false,
    };

    let repo_view = cmd.generate_repository_view(&repo_root)?;
    let context = cmd.collect_context(&repo_root, &repo_view).await?;
    let claude_client = crate::claude::create_default_claude_client(model, None).await?;
    run_create_pr_with_client(&cmd, &repo_view, &context, &claude_client).await
}

/// Non-credential-gated inner core of [`run_create_pr`] for unit tests.
///
/// Takes an already-built [`CreatePrCommand`], [`crate::data::RepositoryView`],
/// and [`crate::data::context::CommitContext`] so tests can construct those
/// in-memory (avoiding the git-remote setup `generate_repository_view`
/// requires). Callers are responsible for preflight, CWD, and context
/// assembly.
pub(crate) async fn run_create_pr_with_client(
    cmd: &CreatePrCommand,
    repo_view: &crate::data::RepositoryView,
    context: &crate::data::context::CommitContext,
    claude_client: &crate::claude::client::ClaudeClient,
) -> Result<CreatePrOutcome> {
    let pr_template = cmd.resolve_pr_template(repo_view);

    let ai_result = if cmd.from_commits {
        claude_client
            .generate_pr_content_with_context_from_commits(repo_view, &pr_template, context)
            .await
    } else {
        claude_client
            .generate_pr_content_with_context(repo_view, &pr_template, context)
            .await
    };
    let pr_content = match ai_result {
        Ok(content) => content,
        // A permanent failure is reported rather than papered over; this call
        // does not mutate the PR, so a transient failure only needs the warning
        // `fallback_pr_content` emits (issue #1333).
        Err(e) if !ai_error_is_transient(&e) => {
            return Err(e).context("AI PR generation failed with a non-retryable error");
        }
        Err(e) => cmd.fallback_pr_content(&e, pr_template, repo_view)?,
    };

    let pr_yaml = crate::data::to_yaml(&pr_content).context("Failed to serialise PrContent")?;

    Ok(CreatePrOutcome {
        title: pr_content.title,
        description: pr_content.description,
        pr_yaml,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod run_create_pr_tests {
    use super::*;
    use crate::claude::client::ClaudeClient;
    use crate::claude::error::ClaudeError;
    use crate::claude::test_utils::ConfigurableMockAiClient;
    use crate::data::context::CommitContext;
    use crate::data::{
        AiInfo, BranchInfo, FieldExplanation, RepositoryView, VersionInfo, WorkingDirectoryInfo,
    };
    use crate::git::commit::FileChanges;
    use crate::git::{CommitAnalysis, CommitInfo};

    #[tokio::test]
    async fn run_create_pr_invalid_repo_path_errors_before_ai() {
        let err = run_create_pr(
            None,
            None,
            Some(std::path::Path::new("/no/such/path/exists")),
        )
        .await
        .unwrap_err();
        let msg = format!("{err:#}").to_lowercase();
        // Preflight may surface a credentials error first, or (with creds
        // present) `generate_repository_view` opens the injected path via
        // `open_at` and fails with a git/repository error. Either proves the
        // injected path is honored without mutating the process CWD.
        assert!(
            msg.contains("git")
                || msg.contains("repository")
                || msg.contains("credential")
                || msg.contains("api")
                || msg.contains("directory"),
            "expected git/repository or preflight error, got: {msg}"
        );
    }

    fn fresh_cmd() -> CreatePrCommand {
        CreatePrCommand {
            base: None,
            auto_apply: true,
            save_only: None,
            ready: false,
            draft: false,
            context_dir: None,
            no_push: true,
            from_commits: false,
        }
    }

    fn sample_commit(hash: &str, message: &str) -> (CommitInfo, tempfile::NamedTempFile) {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let commit = CommitInfo {
            hash: hash.to_string(),
            author: "Test <test@test.com>".to_string(),
            date: chrono::Utc::now().fixed_offset(),
            original_message: message.to_string(),
            in_main_branches: vec![],
            analysis: CommitAnalysis {
                detected_type: "feat".to_string(),
                detected_scope: String::new(),
                proposed_message: message.to_string(),
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

    /// Builds a repo view carrying one existing PR with the given body, as
    /// `gh pr list --json …,body` would populate it.
    fn repo_view_with_existing_pr(pr_template: Option<String>, body: &str) -> RepositoryView {
        let mut view = sample_repo_view(vec![], pr_template);
        view.branch_prs = Some(vec![crate::data::PullRequest {
            number: 42,
            title: "Existing PR".to_string(),
            state: "open".to_string(),
            url: "https://example.invalid/pr/42".to_string(),
            body: body.to_string(),
            base: "main".to_string(),
        }]);
        view
    }

    #[test]
    fn body_is_safe_to_replace_only_when_empty_or_template() {
        let template = "# Pull Request\n\n## Description\n";
        assert!(CreatePrCommand::body_is_safe_to_replace("", template));
        assert!(CreatePrCommand::body_is_safe_to_replace(
            "   \n  ", template
        ));
        // Whitespace differences must not make a template look like real content.
        assert!(CreatePrCommand::body_is_safe_to_replace(
            "\n# Pull Request\n\n## Description\n\n",
            template
        ));
        assert!(!CreatePrCommand::body_is_safe_to_replace(
            "A real, hand-written description.",
            template
        ));
    }

    /// Issue #1333: the original bug replaced a populated description with an
    /// unfilled template and reported success.
    #[test]
    fn refuse_template_clobber_rejects_populated_body() {
        let cmd = fresh_cmd();
        let view = repo_view_with_existing_pr(None, "A real description worth keeping.");

        let err = cmd
            .refuse_template_clobber(&view)
            .expect_err("must refuse to destroy a populated description");
        let chain = format!("{err:#}");
        assert!(chain.contains("#42"), "should name the PR: {chain}");
        assert!(
            chain.contains("Refusing to overwrite"),
            "should say what it refused: {chain}"
        );
    }

    #[test]
    fn refuse_template_clobber_allows_empty_body() {
        let cmd = fresh_cmd();
        let view = repo_view_with_existing_pr(None, "   ");
        assert!(cmd.refuse_template_clobber(&view).is_ok());
    }

    #[test]
    fn refuse_template_clobber_allows_unfilled_template_body() {
        let cmd = fresh_cmd();
        let template = "# Custom template\n";
        let view = repo_view_with_existing_pr(Some(template.to_string()), template);
        assert!(cmd.refuse_template_clobber(&view).is_ok());
    }

    #[test]
    fn refuse_template_clobber_allows_when_no_existing_pr() {
        let cmd = fresh_cmd();
        let view = sample_repo_view(vec![], None);
        assert!(cmd.refuse_template_clobber(&view).is_ok());
    }

    fn sample_repo_view(commits: Vec<CommitInfo>, pr_template: Option<String>) -> RepositoryView {
        RepositoryView {
            versions: Some(VersionInfo {
                omni_dev: "0.0.0".to_string(),
            }),
            explanation: FieldExplanation::default(),
            working_directory: WorkingDirectoryInfo {
                clean: true,
                untracked_changes: vec![],
            },
            remotes: vec![],
            ai: AiInfo {
                scratch: String::new(),
            },
            branch_info: Some(BranchInfo {
                branch: "feature/test".to_string(),
            }),
            pr_template,
            pr_template_location: None,
            branch_prs: None,
            commits,
        }
    }

    #[tokio::test]
    async fn run_create_pr_with_client_ai_success_returns_content() {
        let (c1, _tmp) = sample_commit("abcdef00", "feat: work");
        let repo_view = sample_repo_view(vec![c1], None);
        let context = CommitContext::new();
        let cmd = fresh_cmd();

        let yaml = "title: My PR\ndescription: |\n  Body text\n".to_string();
        let mock = ConfigurableMockAiClient::new(vec![Ok(yaml)]);
        let client = ClaudeClient::new(Box::new(mock));

        let outcome = run_create_pr_with_client(&cmd, &repo_view, &context, &client)
            .await
            .unwrap();
        assert_eq!(outcome.title, "My PR");
        assert!(outcome.description.contains("Body text"));
        assert!(outcome.pr_yaml.contains("title:"));
    }

    #[tokio::test]
    async fn run_create_pr_with_client_ai_failure_falls_back_to_commit_summary() {
        let (c1, _tmp) = sample_commit("abcdef00", "feat: single commit subject");
        let repo_view = sample_repo_view(vec![c1], None);
        let context = CommitContext::new();
        let cmd = fresh_cmd();

        // Empty mock → AI call exhausts retries → fallback path triggered.
        let mock = ConfigurableMockAiClient::new(vec![]);
        let client = ClaudeClient::new(Box::new(mock));

        let outcome = run_create_pr_with_client(&cmd, &repo_view, &context, &client)
            .await
            .unwrap();
        assert!(
            outcome.title.contains("feat: single commit subject")
                || outcome.title.contains("Pull Request")
                || outcome.title.contains("feature/test"),
            "fallback title unexpected: {}",
            outcome.title
        );
    }

    /// Issue #1333: a 404 (a model that does not exist) can never succeed, so
    /// it must surface as an error rather than a template-filled "success".
    #[tokio::test]
    async fn run_create_pr_with_client_permanent_ai_failure_errors() {
        let (c1, _tmp) = sample_commit("abcdef00", "feat: work");
        let repo_view = sample_repo_view(vec![c1], None);
        let context = CommitContext::new();
        let cmd = fresh_cmd();

        let mock = ConfigurableMockAiClient::new(vec![Err(ClaudeError::ApiHttpError {
            status: 404,
            body: "model: claude-sonnet-4-8 not found".to_string(),
        }
        .into())]);
        let client = ClaudeClient::new(Box::new(mock));

        let err = run_create_pr_with_client(&cmd, &repo_view, &context, &client)
            .await
            .expect_err("a 404 must not be reported as success");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("non-retryable"),
            "error should explain why it did not fall back: {chain}"
        );
        assert!(
            chain.contains("404"),
            "error should name the underlying failure: {chain}"
        );
    }

    /// A transient failure still degrades to the template, since a retry (or a
    /// later run) could plausibly succeed.
    #[tokio::test]
    async fn run_create_pr_with_client_transient_ai_failure_falls_back() {
        let (c1, _tmp) = sample_commit("abcdef00", "feat: work");
        let repo_view = sample_repo_view(vec![c1], None);
        let context = CommitContext::new();
        let cmd = fresh_cmd();

        let mock = ConfigurableMockAiClient::new(vec![Err(ClaudeError::ApiHttpError {
            status: 503,
            body: "upstream unavailable".to_string(),
        }
        .into())]);
        let client = ClaudeClient::new(Box::new(mock));

        let outcome = run_create_pr_with_client(&cmd, &repo_view, &context, &client)
            .await
            .expect("a 5xx should still fall back to the template");
        assert!(!outcome.title.is_empty());
    }

    #[tokio::test]
    async fn run_create_pr_with_client_uses_repo_template_when_present() {
        let (c1, _tmp) = sample_commit("abcdef00", "feat: x");
        let repo_view = sample_repo_view(vec![c1], Some("# Custom template\n".to_string()));
        let context = CommitContext::new();
        let cmd = fresh_cmd();

        // AI fails → fallback uses the repo template as the description base.
        let mock = ConfigurableMockAiClient::new(vec![]);
        let client = ClaudeClient::new(Box::new(mock));

        let outcome = run_create_pr_with_client(&cmd, &repo_view, &context, &client)
            .await
            .unwrap();
        assert!(
            outcome.description.contains("# Custom template"),
            "fallback description should include repo template: {}",
            outcome.description
        );
    }

    #[tokio::test]
    async fn run_create_pr_with_client_from_commits_omits_diff() {
        // Write a recognisable diff payload so we can prove it never reaches
        // the AI when from_commits is set.
        let dir = tempfile::tempdir().unwrap();
        let diff_path = dir.path().join("recognisable.diff");
        std::fs::write(
            &diff_path,
            "diff --git a/x b/x\n@@ -1 +1 @@\n-old\n+UNIQUE_DIFF_MARKER\n",
        )
        .unwrap();

        let commit = CommitInfo {
            hash: format!("{:0>40}", 0),
            author: "Test <test@test.com>".to_string(),
            date: chrono::Utc::now().fixed_offset(),
            original_message: "feat: UNIQUE_COMMIT_SUBJECT_MARKER".to_string(),
            in_main_branches: vec![],
            analysis: CommitAnalysis {
                detected_type: "feat".to_string(),
                detected_scope: String::new(),
                proposed_message: "feat: t".to_string(),
                file_changes: FileChanges {
                    total_files: 1,
                    files_added: 0,
                    files_deleted: 0,
                    file_list: vec![],
                },
                diff_summary: String::new(),
                diff_file: diff_path.to_string_lossy().to_string(),
                file_diffs: Vec::new(),
            },
        };
        let repo_view = sample_repo_view(vec![commit], None);
        let context = CommitContext::new();
        let mut cmd = fresh_cmd();
        cmd.from_commits = true;

        let yaml = "title: feat: x\ndescription: y\n".to_string();
        let mock = ConfigurableMockAiClient::new(vec![Ok(yaml)]);
        let prompt_handle = mock.prompt_handle();
        let client = ClaudeClient::new(Box::new(mock));

        run_create_pr_with_client(&cmd, &repo_view, &context, &client)
            .await
            .unwrap();

        let prompts = prompt_handle.prompts();
        assert_eq!(prompts.len(), 1, "expected one AI call");
        let (_, user_prompt) = &prompts[0];
        assert!(
            user_prompt.contains("UNIQUE_COMMIT_SUBJECT_MARKER"),
            "commit subject should be in the prompt"
        );
        assert!(
            !user_prompt.contains("UNIQUE_DIFF_MARKER"),
            "diff content must NOT appear in the prompt when from_commits is set"
        );
        assert!(
            !user_prompt.contains("diff --git"),
            "diff hunks must NOT appear in the prompt when from_commits is set"
        );
    }

    #[test]
    fn create_pr_outcome_clone_and_debug() {
        let outcome = CreatePrOutcome {
            title: "t".to_string(),
            description: "d".to_string(),
            pr_yaml: "y".to_string(),
        };
        let cloned = outcome.clone();
        assert_eq!(format!("{outcome:?}"), format!("{cloned:?}"));
    }

    /// Builds a temp repo with `origin/main`, a feature branch one commit
    /// ahead, and a distinctive `.github/pull_request_template.md`. Returns the
    /// temp dir so callers can drive `generate_repository_view` against it.
    fn init_repo_with_remote_and_template(template_marker: &str) -> tempfile::TempDir {
        use git2::{Repository, Signature};
        let tmp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
        std::fs::create_dir_all(&tmp_root).unwrap();
        let temp_dir = tempfile::tempdir_in(&tmp_root).unwrap();
        let repo_path = temp_dir.path();
        let repo = Repository::init(repo_path).unwrap();
        {
            let mut config = repo.config().unwrap();
            config.set_str("user.name", "Test").unwrap();
            config.set_str("user.email", "test@example.com").unwrap();
            config.set_str("init.defaultBranch", "main").unwrap();
        }
        repo.set_head("refs/heads/main").unwrap();

        let signature = Signature::now("Test", "test@example.com").unwrap();

        // Base commit on main.
        std::fs::write(repo_path.join("f.txt"), "base").unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(std::path::Path::new("f.txt")).unwrap();
        idx.write().unwrap();
        let tree_id = idx.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let base_oid = repo
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "base: init",
                &tree,
                &[],
            )
            .unwrap();

        // A non-github remote so detection never shells out to gh.
        repo.remote("origin", "https://example.com/test/repo.git")
            .unwrap();
        // origin/main tracking ref at the base commit.
        repo.reference(
            "refs/remotes/origin/main",
            base_oid,
            true,
            "set origin/main",
        )
        .unwrap();

        // Feature branch one commit ahead of origin/main.
        let base_commit = repo.find_commit(base_oid).unwrap();
        repo.branch("feature/test", &base_commit, true).unwrap();
        repo.set_head("refs/heads/feature/test").unwrap();
        std::fs::write(repo_path.join("f.txt"), "feature").unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(std::path::Path::new("f.txt")).unwrap();
        idx.write().unwrap();
        let tree_id = idx.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "feat: feature work",
            &tree,
            &[&base_commit],
        )
        .unwrap();

        // Distinctive PR template inside the injected repo's .github/.
        let github_dir = repo_path.join(".github");
        std::fs::create_dir_all(&github_dir).unwrap();
        std::fs::write(github_dir.join("pull_request_template.md"), template_marker).unwrap();

        temp_dir
    }

    /// "No silent mix" anchoring guard: `generate_repository_view` resolves the
    /// PR template, branch, and commits from the INJECTED repo root, not the
    /// process CWD (the omni-dev checkout, which ships its own
    /// `.github/pull_request_template.md`). We leave the process CWD untouched
    /// and assert the returned view reflects the injected repo's distinctive
    /// template and feature branch.
    #[test]
    fn generate_repository_view_anchors_to_injected_repo() {
        let marker = "## INJECTED_PR_TEMPLATE_MARKER_42";
        let temp_dir = init_repo_with_remote_and_template(marker);
        let cmd = fresh_cmd();

        let repo_view = cmd.generate_repository_view(temp_dir.path()).unwrap();

        // PR template came from the injected repo, not the ambient CWD.
        assert_eq!(
            repo_view.pr_template.as_deref(),
            Some(marker),
            "PR template must be read from the injected repo root"
        );
        // Branch + commits reflect the injected repo's feature branch.
        assert_eq!(
            repo_view.branch_info.as_ref().map(|b| b.branch.as_str()),
            Some("feature/test")
        );
        assert_eq!(
            repo_view.commits.len(),
            1,
            "exactly the one commit ahead of origin/main"
        );
        assert!(repo_view.commits[0]
            .original_message
            .contains("feature work"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_bool_string ---

    #[test]
    fn parse_bool_true_variants() {
        assert_eq!(parse_bool_string("true"), Some(true));
        assert_eq!(parse_bool_string("1"), Some(true));
        assert_eq!(parse_bool_string("yes"), Some(true));
    }

    #[test]
    fn parse_bool_false_variants() {
        assert_eq!(parse_bool_string("false"), Some(false));
        assert_eq!(parse_bool_string("0"), Some(false));
        assert_eq!(parse_bool_string("no"), Some(false));
    }

    #[test]
    fn parse_bool_invalid() {
        assert_eq!(parse_bool_string("maybe"), None);
        assert_eq!(parse_bool_string(""), None);
    }

    #[test]
    fn parse_bool_case_insensitive() {
        assert_eq!(parse_bool_string("TRUE"), Some(true));
        assert_eq!(parse_bool_string("Yes"), Some(true));
        assert_eq!(parse_bool_string("FALSE"), Some(false));
        assert_eq!(parse_bool_string("No"), Some(false));
    }

    // --- is_breaking_change ---

    #[test]
    fn breaking_change_type_contains() {
        assert!(is_breaking_change("BREAKING", "normal message"));
    }

    #[test]
    fn breaking_change_message_contains() {
        assert!(is_breaking_change("feat", "BREAKING CHANGE: removed API"));
    }

    #[test]
    fn breaking_change_none() {
        assert!(!is_breaking_change("feat", "add new feature"));
    }

    // --- check_checkbox ---

    #[test]
    fn check_checkbox_found() {
        let mut desc = "- [ ] New feature\n- [ ] Bug fix".to_string();
        check_checkbox(&mut desc, "- [ ] New feature");
        assert!(desc.contains("- [x] New feature"));
        assert!(desc.contains("- [ ] Bug fix"));
    }

    #[test]
    fn check_checkbox_not_found() {
        let mut desc = "- [ ] Bug fix".to_string();
        let original = desc.clone();
        check_checkbox(&mut desc, "- [ ] New feature");
        assert_eq!(desc, original);
    }

    // --- format_scopes_section ---

    #[test]
    fn scopes_section_single() {
        let scopes = vec!["cli".to_string()];
        assert_eq!(
            format_scopes_section(&scopes),
            "**Affected areas:** cli\n\n"
        );
    }

    #[test]
    fn scopes_section_multiple() {
        let scopes = vec!["cli".to_string(), "git".to_string()];
        let result = format_scopes_section(&scopes);
        assert!(result.contains("cli"));
        assert!(result.contains("git"));
        assert!(result.starts_with("**Affected areas:**"));
    }

    #[test]
    fn scopes_section_empty() {
        assert_eq!(format_scopes_section(&[]), "");
    }

    // --- format_commit_list ---

    #[test]
    fn commit_list_formatting() {
        let entries = vec![
            ("abc12345", "feat: add feature"),
            ("def67890", "fix: resolve bug"),
        ];
        let result = format_commit_list(&entries);
        assert!(result.contains("### Commits in this PR:"));
        assert!(result.contains("- `abc12345` feat: add feature"));
        assert!(result.contains("- `def67890` fix: resolve bug"));
    }

    // --- clean_branch_name ---

    #[test]
    fn clean_branch_simple() {
        assert_eq!(clean_branch_name("feat/add-login"), "feat add login");
    }

    #[test]
    fn clean_branch_underscores() {
        assert_eq!(clean_branch_name("user_name/fix_bug"), "user name fix bug");
    }

    // --- extract_first_line ---

    #[test]
    fn first_line_multiline() {
        assert_eq!(extract_first_line("first\nsecond\nthird"), "first");
    }

    #[test]
    fn first_line_single() {
        assert_eq!(extract_first_line("only line"), "only line");
    }

    #[test]
    fn first_line_empty() {
        assert_eq!(extract_first_line(""), "");
    }

    // --- format_draft_status ---

    #[test]
    fn draft_status_true() {
        let (icon, text) = format_draft_status(true);
        assert_eq!(text, "draft");
        assert!(!icon.is_empty());
    }

    #[test]
    fn draft_status_false() {
        let (icon, text) = format_draft_status(false);
        assert_eq!(text, "ready for review");
        assert!(!icon.is_empty());
    }

    // --- determine_push_action ---

    #[test]
    fn push_action_skip_when_no_push() {
        assert_eq!(determine_push_action(true, false), PushAction::Skip);
        assert_eq!(determine_push_action(true, true), PushAction::Skip);
    }

    #[test]
    fn push_action_sync_existing_branch() {
        assert_eq!(determine_push_action(false, true), PushAction::SyncExisting);
    }

    #[test]
    fn push_action_push_new_branch() {
        assert_eq!(determine_push_action(false, false), PushAction::PushNew);
    }
}
