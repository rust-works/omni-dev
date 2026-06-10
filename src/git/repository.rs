//! Git repository operations.

use anyhow::{Context, Result};
use git2::{Repository, Status};
use tracing::{debug, error, info};

use crate::git::CommitInfo;

/// Git repository wrapper.
pub struct GitRepository {
    repo: Repository,
}

/// Working directory status.
#[derive(Debug)]
pub struct WorkingDirectoryStatus {
    /// Whether the working directory has no changes.
    pub clean: bool,
    /// List of files with uncommitted changes.
    pub untracked_changes: Vec<FileStatus>,
}

/// File status information.
#[derive(Debug)]
pub struct FileStatus {
    /// Git status flags (e.g., "AM", "??", "M ").
    pub status: String,
    /// Path to the file relative to repository root.
    pub file: String,
}

impl GitRepository {
    /// Opens a repository at the specified path.
    pub fn open_at<P: AsRef<std::path::Path>>(path: P) -> Result<Self> {
        let repo = Repository::open(path).context("Failed to open git repository")?;

        Ok(Self { repo })
    }

    /// Returns the working directory status.
    pub fn get_working_directory_status(&self) -> Result<WorkingDirectoryStatus> {
        let statuses = self
            .repo
            .statuses(None)
            .context("Failed to get repository status")?;

        let mut untracked_changes = Vec::new();

        for entry in statuses.iter() {
            if let Ok(path) = entry.path() {
                let status_flags = entry.status();

                // Skip ignored files - they should not affect clean status
                if status_flags.contains(Status::IGNORED) {
                    continue;
                }

                let status_str = format_status_flags(status_flags);

                untracked_changes.push(FileStatus {
                    status: status_str,
                    file: path.to_string(),
                });
            }
        }

        let clean = untracked_changes.is_empty();

        Ok(WorkingDirectoryStatus {
            clean,
            untracked_changes,
        })
    }

    /// Checks if the working directory is clean.
    pub fn is_working_directory_clean(&self) -> Result<bool> {
        let status = self.get_working_directory_status()?;
        Ok(status.clean)
    }

    /// Returns the repository path.
    pub fn path(&self) -> &std::path::Path {
        self.repo.path()
    }

    /// Returns the workdir path.
    pub fn workdir(&self) -> Option<&std::path::Path> {
        self.repo.workdir()
    }

    /// Returns access to the underlying `git2::Repository`.
    pub fn repository(&self) -> &Repository {
        &self.repo
    }

    /// Returns the current branch name.
    pub fn get_current_branch(&self) -> Result<String> {
        let head = self.repo.head().context("Failed to get HEAD reference")?;

        if let Ok(name) = head.shorthand() {
            if name != "HEAD" {
                return Ok(name.to_string());
            }
        }

        anyhow::bail!("Repository is in detached HEAD state")
    }

    /// Checks if a branch exists.
    pub fn branch_exists(&self, branch_name: &str) -> Result<bool> {
        // Check if it exists as a local branch
        if self
            .repo
            .find_branch(branch_name, git2::BranchType::Local)
            .is_ok()
        {
            return Ok(true);
        }

        // Check if it exists as a remote branch
        if self
            .repo
            .find_branch(branch_name, git2::BranchType::Remote)
            .is_ok()
        {
            return Ok(true);
        }

        // Check if we can resolve it as a reference
        if self.repo.revparse_single(branch_name).is_ok() {
            return Ok(true);
        }

        Ok(false)
    }

    /// Parses a commit range and returns the commits.
    pub fn get_commits_in_range(&self, range: &str) -> Result<Vec<CommitInfo>> {
        let mut commits = Vec::new();

        if range == "HEAD" {
            // Single HEAD commit
            let head = self.repo.head().context("Failed to get HEAD")?;
            let commit = head
                .peel_to_commit()
                .context("Failed to peel HEAD to commit")?;
            commits.push(CommitInfo::from_git_commit(&self.repo, &commit)?);
        } else if range.contains("..") {
            // Range format like HEAD~3..HEAD
            let parts: Vec<&str> = range.split("..").collect();
            if parts.len() != 2 {
                anyhow::bail!("Invalid range format: {range}");
            }

            let start_spec = parts[0];
            let end_spec = parts[1];

            // Parse start and end commits
            let start_obj = self
                .repo
                .revparse_single(start_spec)
                .with_context(|| format!("Failed to parse start commit: {start_spec}"))?;
            let end_obj = self
                .repo
                .revparse_single(end_spec)
                .with_context(|| format!("Failed to parse end commit: {end_spec}"))?;

            let start_commit = start_obj
                .peel_to_commit()
                .context("Failed to peel start object to commit")?;
            let end_commit = end_obj
                .peel_to_commit()
                .context("Failed to peel end object to commit")?;

            // Walk from end_commit back to start_commit (exclusive)
            let mut walker = self.repo.revwalk().context("Failed to create revwalk")?;
            walker
                .push(end_commit.id())
                .context("Failed to push end commit")?;
            walker
                .hide(start_commit.id())
                .context("Failed to hide start commit")?;

            for oid in walker {
                let oid = oid.context("Failed to get commit OID from walker")?;
                let commit = self
                    .repo
                    .find_commit(oid)
                    .context("Failed to find commit")?;

                // Skip merge commits
                if commit.parent_count() > 1 {
                    continue;
                }

                commits.push(CommitInfo::from_git_commit(&self.repo, &commit)?);
            }

            // Reverse to get chronological order (oldest first)
            commits.reverse();
        } else {
            // Single commit by hash or reference
            let obj = self
                .repo
                .revparse_single(range)
                .with_context(|| format!("Failed to parse commit: {range}"))?;
            let commit = obj
                .peel_to_commit()
                .context("Failed to peel object to commit")?;
            commits.push(CommitInfo::from_git_commit(&self.repo, &commit)?);
        }

        Ok(commits)
    }
}

/// Formats git status flags into a string representation.
fn format_status_flags(flags: Status) -> String {
    let mut status = String::new();

    if flags.contains(Status::INDEX_NEW) {
        status.push('A');
    } else if flags.contains(Status::INDEX_MODIFIED) {
        status.push('M');
    } else if flags.contains(Status::INDEX_DELETED) {
        status.push('D');
    } else if flags.contains(Status::INDEX_RENAMED) {
        status.push('R');
    } else if flags.contains(Status::INDEX_TYPECHANGE) {
        status.push('T');
    } else {
        status.push(' ');
    }

    if flags.contains(Status::WT_NEW) {
        status.push('?');
    } else if flags.contains(Status::WT_MODIFIED) {
        status.push('M');
    } else if flags.contains(Status::WT_DELETED) {
        status.push('D');
    } else if flags.contains(Status::WT_TYPECHANGE) {
        status.push('T');
    } else if flags.contains(Status::WT_RENAMED) {
        status.push('R');
    } else {
        status.push(' ');
    }

    status
}

impl GitRepository {
    /// Runs a `git` CLI subcommand in the repository's working directory.
    ///
    /// Remote operations shell out to the user's `git` rather than using
    /// libgit2's network transport so they work across all URL schemes (SSH,
    /// HTTPS) and honour the user's existing authentication configuration
    /// (`ssh-agent`, `~/.ssh/config`, credential helpers). The vendored libgit2
    /// lacks a reliable SSH transport on some platforms. See issue #903.
    fn run_git(&self, args: &[&str]) -> Result<std::process::Output> {
        let workdir = self
            .repo
            .workdir()
            .context("Cannot run git command: repository has no working directory")?;

        std::process::Command::new("git")
            .current_dir(workdir)
            .args(args)
            .output()
            .context("Failed to execute git command")
    }

    /// Pushes the current branch to remote.
    pub fn push_branch(&self, branch_name: &str, remote_name: &str) -> Result<()> {
        info!(
            "Pushing branch '{}' to remote '{}'",
            branch_name, remote_name
        );

        // Shell out to `git push` so the push works across all URL schemes and
        // uses the user's configured authentication. `--set-upstream` records
        // the tracking branch in the same step. See [`Self::run_git`].
        debug!("Pushing via git CLI to '{}'", remote_name);
        let output = self.run_git(&["push", "--set-upstream", remote_name, branch_name])?;

        if output.status.success() {
            info!(
                "Successfully pushed branch '{}' to remote '{}'",
                branch_name, remote_name
            );
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = stderr.trim();
            error!("Failed to push branch: {}", stderr);
            anyhow::bail!(
                "Failed to push branch '{branch_name}' to remote '{remote_name}': {stderr}"
            )
        }
    }

    /// Checks if a branch exists on remote.
    pub fn branch_exists_on_remote(&self, branch_name: &str, remote_name: &str) -> Result<bool> {
        debug!(
            "Checking if branch '{}' exists on remote '{}'",
            branch_name, remote_name
        );

        // Query the remote via `git ls-remote` so the lookup works across all
        // URL schemes and uses the user's configured authentication. See
        // [`Self::run_git`].
        debug!("Listing remote refs via git CLI from '{}'", remote_name);
        let output = self.run_git(&["ls-remote", "--heads", remote_name, branch_name])?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = stderr.trim();
            error!("Failed to list remote refs: {}", stderr);
            anyhow::bail!(
                "Failed to check remote '{remote_name}' for branch '{branch_name}': {stderr}"
            )
        }

        // `git ls-remote --heads <remote> <branch>` emits one `<sha>\t<ref>`
        // line per matching head. The branch argument is a glob pattern that
        // matches on the ref tail, so compare the ref column exactly to avoid
        // false positives like `refs/heads/foo/<branch>`.
        let remote_branch_ref = format!("refs/heads/{branch_name}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let exists = stdout
            .lines()
            .filter_map(|line| line.split('\t').nth(1))
            .any(|reference| reference == remote_branch_ref);

        if exists {
            info!(
                "Branch '{}' exists on remote '{}'",
                branch_name, remote_name
            );
        } else {
            info!(
                "Branch '{}' does not exist on remote '{}'",
                branch_name, remote_name
            );
        }
        Ok(exists)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── format_status_flags ────────────────────────────────────────

    #[test]
    fn status_flags_new_index() {
        let status = format_status_flags(Status::INDEX_NEW);
        assert_eq!(status, "A ");
    }

    #[test]
    fn status_flags_modified_index() {
        let status = format_status_flags(Status::INDEX_MODIFIED);
        assert_eq!(status, "M ");
    }

    #[test]
    fn status_flags_deleted_index() {
        let status = format_status_flags(Status::INDEX_DELETED);
        assert_eq!(status, "D ");
    }

    #[test]
    fn status_flags_wt_new() {
        let status = format_status_flags(Status::WT_NEW);
        assert_eq!(status, " ?");
    }

    #[test]
    fn status_flags_wt_modified() {
        let status = format_status_flags(Status::WT_MODIFIED);
        assert_eq!(status, " M");
    }

    #[test]
    fn status_flags_combined() {
        let status = format_status_flags(Status::INDEX_NEW | Status::WT_MODIFIED);
        assert_eq!(status, "AM");
    }

    #[test]
    fn status_flags_empty() {
        let status = format_status_flags(Status::empty());
        assert_eq!(status, "  ");
    }

    // ── GitRepository with temp repo ───────────────────────────────

    /// Creates an empty git-inited tempdir anchored at `$CARGO_MANIFEST_DIR/tmp`.
    ///
    /// Centralising the setup avoids scattering four copies of the same
    /// `?`-laced boilerplate across these tests, which also gives codecov a
    /// single place to attribute coverage for the directory-creation
    /// machinery.
    #[allow(clippy::unwrap_used)]
    fn init_tmp_repo() -> tempfile::TempDir {
        let tmp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
        std::fs::create_dir_all(&tmp_root).unwrap();
        let temp_dir = tempfile::tempdir_in(&tmp_root).unwrap();
        git2::Repository::init(temp_dir.path()).unwrap();
        temp_dir
    }

    #[test]
    fn open_at_temp_repo() -> Result<()> {
        let temp_dir = init_tmp_repo();
        let repo = GitRepository::open_at(temp_dir.path())?;
        assert!(repo.path().exists());
        Ok(())
    }

    #[test]
    fn working_directory_clean_empty_repo() -> Result<()> {
        let temp_dir = init_tmp_repo();
        let repo = GitRepository::open_at(temp_dir.path())?;
        let status = repo.get_working_directory_status()?;
        assert!(status.clean);
        assert!(status.untracked_changes.is_empty());
        Ok(())
    }

    #[test]
    fn working_directory_dirty_with_file() -> Result<()> {
        let temp_dir = init_tmp_repo();
        std::fs::write(temp_dir.path().join("new_file.txt"), "content")?;
        let repo = GitRepository::open_at(temp_dir.path())?;
        let status = repo.get_working_directory_status()?;
        assert!(!status.clean);
        assert!(!status.untracked_changes.is_empty());
        Ok(())
    }

    #[test]
    fn is_working_directory_clean_delegator() -> Result<()> {
        let temp_dir = init_tmp_repo();
        let repo = GitRepository::open_at(temp_dir.path())?;
        assert!(repo.is_working_directory_clean()?);
        Ok(())
    }

    #[test]
    fn current_branch_on_a_branch() -> Result<()> {
        let temp_dir = init_tmp_repo();
        let p = temp_dir.path();
        std::fs::write(p.join("f.txt"), "x")?;
        git_in(p, &["add", "."]);
        git_in(p, &["commit", "-m", "init"]);
        let repo = GitRepository::open_at(p)?;
        // The branch name is whichever the local git default is (main/master);
        // either way it must resolve to a non-"HEAD" shorthand.
        assert_ne!(repo.get_current_branch()?, "HEAD");
        Ok(())
    }

    #[test]
    fn current_branch_errors_in_detached_head() -> Result<()> {
        // CI checks PRs out as a detached HEAD, which is what makes the bail at
        // the end of `get_current_branch` flicker run-to-run; pin it here.
        let temp_dir = init_tmp_repo();
        let p = temp_dir.path();
        std::fs::write(p.join("f.txt"), "x")?;
        git_in(p, &["add", "."]);
        git_in(p, &["commit", "-m", "init"]);
        git_in(p, &["checkout", "--detach", "HEAD"]);
        let repo = GitRepository::open_at(p)?;
        let result = repo.get_current_branch();
        assert!(
            matches!(&result, Err(e) if e.to_string().contains("detached HEAD")),
            "expected detached-HEAD error, got: {result:?}"
        );
        Ok(())
    }

    // ── remote operations via the git CLI (issue #903) ─────────────

    /// Runs `git` in `dir` with a deterministic identity, asserting success.
    #[allow(clippy::unwrap_used)]
    fn git_in(dir: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args([
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=Test",
                // Disable signing so the tests stay hermetic regardless of the
                // developer's global `commit.gpgsign` / `tag.gpgsign` config —
                // GPG signing also races under parallel test execution.
                "-c",
                "commit.gpgsign=false",
                "-c",
                "tag.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "git {args:?} failed: {stderr}");
    }

    /// Builds a work repo with one commit on `feature-branch` and a bare
    /// `origin` remote it can push to. Both temp dirs are returned so the
    /// caller keeps them alive for the duration of the test.
    #[allow(clippy::unwrap_used)]
    fn repo_with_bare_remote() -> (tempfile::TempDir, tempfile::TempDir, GitRepository) {
        let tmp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
        std::fs::create_dir_all(&tmp_root).unwrap();
        let bare = tempfile::tempdir_in(&tmp_root).unwrap();
        git_in(bare.path(), &["init", "--bare"]);

        let work = init_tmp_repo();
        std::fs::write(work.path().join("file.txt"), "content").unwrap();
        git_in(work.path(), &["checkout", "-b", "feature-branch"]);
        git_in(work.path(), &["add", "."]);
        git_in(work.path(), &["commit", "-m", "initial"]);
        git_in(
            work.path(),
            &["remote", "add", "origin", bare.path().to_str().unwrap()],
        );

        let repo = GitRepository::open_at(work.path()).unwrap();
        (work, bare, repo)
    }

    #[test]
    fn branch_absent_on_remote_before_push() -> Result<()> {
        let (_work, _bare, repo) = repo_with_bare_remote();
        assert!(!repo.branch_exists_on_remote("feature-branch", "origin")?);
        Ok(())
    }

    #[test]
    fn push_branch_then_present_on_remote() -> Result<()> {
        let (_work, _bare, repo) = repo_with_bare_remote();
        repo.push_branch("feature-branch", "origin")?;
        assert!(repo.branch_exists_on_remote("feature-branch", "origin")?);
        assert!(!repo.branch_exists_on_remote("absent-branch", "origin")?);
        Ok(())
    }

    #[test]
    fn branch_exists_requires_exact_ref_match() -> Result<()> {
        // `git ls-remote <branch>` matches on the ref tail, so a sibling like
        // `team/feature-branch` would glob-match `feature-branch`. The exact
        // ref comparison must reject it as a false positive.
        let (work, _bare, repo) = repo_with_bare_remote();
        git_in(work.path(), &["checkout", "-b", "team/feature-branch"]);
        repo.push_branch("team/feature-branch", "origin")?;
        assert!(repo.branch_exists_on_remote("team/feature-branch", "origin")?);
        assert!(!repo.branch_exists_on_remote("feature-branch", "origin")?);
        Ok(())
    }

    #[test]
    fn push_branch_reports_failure_for_unknown_remote() {
        let (_work, _bare, repo) = repo_with_bare_remote();
        let result = repo.push_branch("feature-branch", "nonexistent");
        assert!(matches!(&result, Err(e) if e.to_string().contains("Failed to push branch")));
    }

    #[test]
    fn branch_exists_reports_failure_for_unknown_remote() {
        let (_work, _bare, repo) = repo_with_bare_remote();
        let result = repo.branch_exists_on_remote("feature-branch", "nonexistent");
        assert!(matches!(&result, Err(e) if e.to_string().contains("Failed to check remote")));
    }
}
