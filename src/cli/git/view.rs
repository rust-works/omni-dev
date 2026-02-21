//! View command — outputs repository information in YAML format.

use anyhow::{Context, Result};
use clap::Parser;

/// View command options.
#[derive(Parser)]
pub struct ViewCommand {
    /// Commit range to analyze (e.g., HEAD~3..HEAD, abc123..def456).
    #[arg(value_name = "COMMIT_RANGE")]
    pub commit_range: Option<String>,
}

impl ViewCommand {
    /// Executes the view command.
    pub fn execute(self) -> Result<()> {
        use crate::data::{
            AiInfo, FieldExplanation, FileStatusInfo, RepositoryView, VersionInfo,
            WorkingDirectoryInfo,
        };
        use crate::git::{GitRepository, RemoteInfo};
        use crate::utils::ai_scratch;

        // Preflight check: validate git repository before any processing
        crate::utils::check_git_repository()?;

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
        println!("{yaml_output}");

        Ok(())
    }
}
