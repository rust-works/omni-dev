//! Info command — analyzes branch commits and outputs repository information.

use std::path::Path;

use anyhow::{Context, Result};
use clap::Parser;

/// Info command options.
#[derive(Parser)]
pub struct InfoCommand {
    /// Base branch to compare against (defaults to main/master).
    #[arg(value_name = "BASE_BRANCH")]
    pub base_branch: Option<String>,
}

impl InfoCommand {
    /// Executes the info command.
    pub fn execute(self) -> Result<()> {
        let yaml_output = run_info(self.base_branch.as_deref(), None::<&str>)?;
        println!("{yaml_output}");
        Ok(())
    }

    /// Reads the PR template file if it exists, returning both content and location.
    pub(crate) fn read_pr_template() -> Result<(String, String)> {
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

    /// Returns pull requests for the current branch using gh CLI.
    pub(crate) fn get_branch_prs(branch_name: &str) -> Result<Vec<crate::data::PullRequest>> {
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
                    pr_json.get("number").and_then(serde_json::Value::as_u64),
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

/// Runs the info logic and returns the repository YAML as a `String`.
///
/// Shared by the CLI (which prints the result) and the MCP server (which
/// returns it as tool content). When `repo_path` is `Some`, opens the
/// repository at that path; otherwise opens at the current working directory.
/// `base_branch` defaults to `main` or `master` when omitted.
pub fn run_info<P: AsRef<Path>>(base_branch: Option<&str>, repo_path: Option<P>) -> Result<String> {
    use crate::data::{
        AiInfo, BranchInfo, FieldExplanation, FileStatusInfo, RepositoryView, VersionInfo,
        WorkingDirectoryInfo,
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

    let current_branch = repo
        .get_current_branch()
        .context("Failed to get current branch. Make sure you're not in detached HEAD state.")?;

    let resolved_base = match base_branch {
        Some(branch) => {
            if !repo.branch_exists(branch)? {
                anyhow::bail!("Base branch '{branch}' does not exist");
            }
            branch.to_string()
        }
        None => {
            if repo.branch_exists("main")? {
                "main".to_string()
            } else if repo.branch_exists("master")? {
                "master".to_string()
            } else {
                anyhow::bail!("No default base branch found (main or master)");
            }
        }
    };

    let commit_range = format!("{resolved_base}..HEAD");

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
    let commits = repo.get_commits_in_range(&commit_range)?;

    let (pr_template, pr_template_location) = match InfoCommand::read_pr_template().ok() {
        Some((content, location)) => (Some(content), Some(location)),
        None => (None, None),
    };

    let branch_prs = InfoCommand::get_branch_prs(&current_branch)
        .ok()
        .filter(|prs| !prs.is_empty());

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
        branch_info: Some(BranchInfo {
            branch: current_branch,
        }),
        pr_template,
        pr_template_location,
        branch_prs,
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
            config.set_str("init.defaultBranch", "main").unwrap();
        }

        // Re-point HEAD at refs/heads/main so the first commit lands on "main"
        repo.set_head("refs/heads/main").unwrap();

        let signature = Signature::now("Test", "test@example.com").unwrap();
        let mut commits = Vec::new();
        for (i, msg) in ["base: init", "feat: work"].iter().enumerate() {
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
    fn run_info_default_branch_uses_main() {
        let (temp_dir, _commits) = init_repo_with_commits();
        // With only a `main` branch, HEAD==main → main..HEAD is empty, so the
        // output lacks commits but still returns YAML with branch_info.
        let yaml = run_info(None, Some(temp_dir.path())).unwrap();
        assert!(
            yaml.contains("branch:"),
            "yaml should include branch_info: {yaml}"
        );
    }

    #[test]
    fn run_info_with_explicit_missing_base_errors() {
        let (temp_dir, _commits) = init_repo_with_commits();
        let err = run_info(Some("no-such-branch"), Some(temp_dir.path())).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no-such-branch"),
            "expected missing-branch error: {msg}"
        );
    }

    #[test]
    fn run_info_no_default_base_branch_errors() {
        // Init an empty repo with only a non-main branch.
        let tmp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
        std::fs::create_dir_all(&tmp_root).unwrap();
        let temp_dir = tempfile::tempdir_in(&tmp_root).unwrap();
        let repo = Repository::init(temp_dir.path()).unwrap();
        {
            let mut config = repo.config().unwrap();
            config.set_str("user.name", "Test").unwrap();
            config.set_str("user.email", "test@example.com").unwrap();
        }
        let signature = Signature::now("Test", "test@example.com").unwrap();
        // Create on a non-default branch "dev" only.
        repo.set_head("refs/heads/dev").unwrap();
        std::fs::write(temp_dir.path().join("f.txt"), "c").unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(std::path::Path::new("f.txt")).unwrap();
        idx.write().unwrap();
        let tree_id = idx.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &signature, &signature, "first", &tree, &[])
            .unwrap();

        let err = run_info(None, Some(temp_dir.path())).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("main or master"), "got: {msg}");
    }

    #[test]
    fn run_info_with_invalid_path_returns_error() {
        let err = run_info(None, Some("/no/such/path/exists")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.to_lowercase().contains("git") || msg.to_lowercase().contains("repo"),
            "expected git/repo error, got: {msg}"
        );
    }

    /// Exercises the `None` branch of `repo_path` (the CLI default), which
    /// goes through `crate::utils::check_git_repository` and `GitRepository::open`.
    /// We enter a fresh repo via `CwdGuard` so the test is hermetic.
    #[tokio::test]
    async fn run_info_opens_cwd_repo_when_no_path_given() {
        let (temp_dir, _commits) = init_repo_with_commits();
        let _guard = super::super::CwdGuard::enter(temp_dir.path())
            .await
            .unwrap();
        let yaml = run_info(None, None::<&str>).unwrap();
        assert!(yaml.contains("branch:"));
    }

    #[test]
    fn run_info_with_explicit_existing_base_succeeds() {
        let (temp_dir, _commits) = init_repo_with_commits();
        // Explicitly pass "main" as base — branch exists, validation succeeds.
        let yaml = run_info(Some("main"), Some(temp_dir.path())).unwrap();
        assert!(yaml.contains("branch:"));
    }

    #[test]
    fn run_info_falls_back_to_master_when_main_missing() {
        // Init a repo with a `master` branch (no `main`) — exercises the
        // master fallback in the default-base resolution.
        let tmp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
        std::fs::create_dir_all(&tmp_root).unwrap();
        let temp_dir = tempfile::tempdir_in(&tmp_root).unwrap();
        let repo = Repository::init(temp_dir.path()).unwrap();
        {
            let mut cfg = repo.config().unwrap();
            cfg.set_str("user.name", "Test").unwrap();
            cfg.set_str("user.email", "test@example.com").unwrap();
        }
        repo.set_head("refs/heads/master").unwrap();
        let signature = Signature::now("Test", "test@example.com").unwrap();
        std::fs::write(temp_dir.path().join("f.txt"), "x").unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(std::path::Path::new("f.txt")).unwrap();
        idx.write().unwrap();
        let tree_id = idx.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &signature, &signature, "init", &tree, &[])
            .unwrap();

        let yaml = run_info(None, Some(temp_dir.path())).unwrap();
        assert!(yaml.contains("branch:"));
    }

    /// Exercises the `read_pr_template()` Some arm by placing a PR template
    /// in the expected location.
    #[tokio::test]
    async fn run_info_picks_up_pr_template_from_cwd() {
        let (temp_dir, _commits) = init_repo_with_commits();
        let github_dir = temp_dir.path().join(".github");
        std::fs::create_dir_all(&github_dir).unwrap();
        std::fs::write(
            github_dir.join("pull_request_template.md"),
            "## Sample Template",
        )
        .unwrap();

        let _guard = super::super::CwdGuard::enter(temp_dir.path())
            .await
            .unwrap();
        let yaml = run_info(None, None::<&str>).unwrap();
        assert!(
            yaml.contains("pr_template:") || yaml.contains("Sample Template"),
            "expected PR template info in yaml: {yaml}"
        );
    }
}
