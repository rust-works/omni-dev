//! View command — outputs repository information in YAML format.

use std::path::Path;

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
        let commit_range = self.commit_range.as_deref().unwrap_or("HEAD");
        let yaml_output = run_view(commit_range, None::<&str>)?;
        println!("{yaml_output}");
        Ok(())
    }
}

/// Runs the view logic and returns the YAML output as a `String`.
///
/// When `repo_path` is `Some`, opens the repository at that path; otherwise
/// opens the repository at the current working directory. Callers that print
/// to stdout (the CLI) and callers that return the string (the MCP server)
/// share this implementation.
pub fn run_view<P: AsRef<Path>>(commit_range: &str, repo_path: Option<P>) -> Result<String> {
    use crate::data::{
        AiInfo, FieldExplanation, FileStatusInfo, RepositoryView, VersionInfo, WorkingDirectoryInfo,
    };
    use crate::git::{GitRepository, RemoteInfo};
    use crate::utils::ai_scratch;

    let repo = if let Some(path) = repo_path {
        GitRepository::open_at(path).context("Failed to open git repository at the given path")?
    } else {
        crate::utils::check_git_repository()?;
        GitRepository::open()
            .context("Failed to open git repository. Make sure you're in a git repository.")?
    };

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

    let remotes = RemoteInfo::get_all_remotes(repo.repository())?;
    let commits = repo.get_commits_in_range(commit_range)?;

    let versions = Some(VersionInfo {
        omni_dev: env!("CARGO_PKG_VERSION").to_string(),
    });

    let ai_scratch_path =
        ai_scratch::get_ai_scratch_dir().context("Failed to determine AI scratch directory")?;
    let ai_info = AiInfo {
        scratch: ai_scratch_path.to_string_lossy().to_string(),
    };

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

    repo_view.to_yaml_output()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use git2::{Repository, Signature};
    use tempfile::TempDir;

    fn init_repo_with_commits() -> (TempDir, Vec<git2::Oid>) {
        let tmp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
        std::fs::create_dir_all(&tmp_root).unwrap();
        let temp_dir = tempfile::tempdir_in(&tmp_root).unwrap();
        let repo_path = temp_dir.path();
        let repo = Repository::init(repo_path).unwrap();
        {
            let mut config = repo.config().unwrap();
            config.set_str("user.name", "Test").unwrap();
            config.set_str("user.email", "test@example.com").unwrap();
        }

        let signature = Signature::now("Test", "test@example.com").unwrap();
        let mut commits = Vec::new();
        for (i, msg) in ["feat: one", "fix: two"].iter().enumerate() {
            std::fs::write(repo_path.join("f.txt"), format!("c{i}")).unwrap();
            let mut idx = repo.index().unwrap();
            idx.add_path(std::path::Path::new("f.txt")).unwrap();
            idx.write().unwrap();
            let tree_id = idx.write_tree().unwrap();
            let tree = repo.find_tree(tree_id).unwrap();
            let parents: Vec<git2::Commit<'_>> = match commits.last() {
                Some(id) => vec![repo.find_commit(*id).unwrap()],
                None => vec![],
            };
            let parent_refs: Vec<&git2::Commit<'_>> = parents.iter().collect();
            let oid = repo
                .commit(
                    Some("HEAD"),
                    &signature,
                    &signature,
                    msg,
                    &tree,
                    &parent_refs,
                )
                .unwrap();
            commits.push(oid);
        }
        (temp_dir, commits)
    }

    #[test]
    fn run_view_with_explicit_path_returns_yaml_with_commits() {
        let (temp_dir, _commits) = init_repo_with_commits();
        let yaml = run_view("HEAD~1..HEAD", Some(temp_dir.path())).unwrap();
        assert!(yaml.contains("commits:"), "yaml lacks commits: {yaml}");
        assert!(yaml.contains("fix: two"), "yaml missing latest: {yaml}");
    }

    #[test]
    fn run_view_default_head_returns_latest_commit() {
        let (temp_dir, _commits) = init_repo_with_commits();
        let yaml = run_view("HEAD", Some(temp_dir.path())).unwrap();
        assert!(yaml.contains("fix: two"));
    }

    #[test]
    fn run_view_with_invalid_path_returns_error() {
        let err = run_view("HEAD", Some("/no/such/path/exists")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.to_lowercase().contains("git") || msg.to_lowercase().contains("repo"),
            "expected git/repo error, got: {msg}"
        );
    }
}
