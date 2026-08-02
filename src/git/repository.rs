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

    /// Returns the repo-relative paths of every file tracked in the git
    /// index (what `git ls-files` would print), sorted and deduplicated.
    ///
    /// Deduplication matters because an unresolved merge conflict produces
    /// one index entry per stage for the same path.
    pub fn tracked_files(&self) -> Result<Vec<String>> {
        let index = self.repo.index().context("Failed to open git index")?;
        let mut files: Vec<String> = index
            .iter()
            .map(|entry| String::from_utf8_lossy(&entry.path).into_owned())
            .collect();
        files.sort();
        files.dedup();
        Ok(files)
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

    /// Resolves the default base branch for commit-range defaults.
    ///
    /// Prefers remote-tracking refs so the default range binds to the remote's
    /// view of the mainline rather than a possibly-stale local branch:
    /// `origin/main` → `origin/master` → `main` → `master`.
    /// Returns `None` when none of these refs exist.
    pub fn resolve_default_base_branch(&self) -> Option<String> {
        const CANDIDATES: [(&str, git2::BranchType); 4] = [
            ("origin/main", git2::BranchType::Remote),
            ("origin/master", git2::BranchType::Remote),
            ("main", git2::BranchType::Local),
            ("master", git2::BranchType::Local),
        ];
        CANDIDATES
            .iter()
            .find(|(name, kind)| self.repo.find_branch(name, *kind).is_ok())
            .map(|(name, _)| (*name).to_string())
    }

    /// Parses a commit range and returns the commits.
    pub fn get_commits_in_range(&self, range: &str) -> Result<Vec<CommitInfo>> {
        let mut commits = Vec::new();

        // Resolved once per invocation; containment is checked per commit.
        let main_tips = crate::git::main_branches::detect_main_branch_tips(&self.repo)?;

        if range == "HEAD" {
            // Single HEAD commit
            let head = self.repo.head().context("Failed to get HEAD")?;
            let commit = head
                .peel_to_commit()
                .context("Failed to peel HEAD to commit")?;
            commits.push(CommitInfo::from_git_commit(
                &self.repo, &commit, &main_tips,
            )?);
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

            commits = self.collect_walk(walker, &main_tips, None)?;
        } else {
            // Single commit by hash or reference
            let obj = self
                .repo
                .revparse_single(range)
                .with_context(|| format!("Failed to parse commit: {range}"))?;
            let commit = obj
                .peel_to_commit()
                .context("Failed to peel object to commit")?;
            commits.push(CommitInfo::from_git_commit(
                &self.repo, &commit, &main_tips,
            )?);
        }

        Ok(commits)
    }

    /// Walks every commit reachable from `HEAD`, optionally capped to the
    /// newest `max_count` non-merge commits.
    ///
    /// Unlike [`Self::get_commits_in_range`] (which needs an explicit range),
    /// this is the whole-history default a reporting command like `config
    /// scopes usage` wants when the caller gave no range at all.
    pub fn get_commits_from_head(&self, max_count: Option<usize>) -> Result<Vec<CommitInfo>> {
        let main_tips = crate::git::main_branches::detect_main_branch_tips(&self.repo)?;
        let mut walker = self.repo.revwalk().context("Failed to create revwalk")?;
        walker.push_head().context("Failed to push HEAD")?;
        self.collect_walk(walker, &main_tips, max_count)
    }

    /// Drains a revwalk into commit info, skipping merges, honoring an
    /// optional count cap (checked after each non-merge commit is collected,
    /// so it always bounds the output length rather than raw traversal
    /// steps), then reverses to chronological order (oldest first) — the
    /// shared collection loop behind [`Self::get_commits_in_range`]'s range
    /// branch and [`Self::get_commits_from_head`].
    fn collect_walk(
        &self,
        walker: git2::Revwalk<'_>,
        main_tips: &[crate::git::main_branches::MainBranchTip],
        max_count: Option<usize>,
    ) -> Result<Vec<CommitInfo>> {
        let mut commits = Vec::new();

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

            commits.push(CommitInfo::from_git_commit(&self.repo, &commit, main_tips)?);

            if max_count.is_some_and(|n| commits.len() >= n) {
                break;
            }
        }

        // Reverse to get chronological order (oldest first)
        commits.reverse();
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

    // ── resolve_default_base_branch (issue #1106) ──────────────────

    #[test]
    fn resolve_default_base_prefers_origin_main_over_local() -> Result<()> {
        let (work, _bare, repo) = repo_with_bare_remote();
        git_in(work.path(), &["branch", "main"]);
        // Pushing creates `refs/remotes/origin/main` in the work repo, so both
        // the local and the remote-tracking branch exist; the remote must win.
        repo.push_branch("main", "origin")?;
        assert_eq!(
            repo.resolve_default_base_branch(),
            Some("origin/main".to_string())
        );
        Ok(())
    }

    #[test]
    fn resolve_default_base_prefers_origin_master_over_local_main() -> Result<()> {
        // Interleaved order: a remote-tracking `origin/master` outranks a
        // (possibly stale) local `main`.
        let (work, _bare, repo) = repo_with_bare_remote();
        git_in(work.path(), &["branch", "main"]);
        git_in(work.path(), &["branch", "master"]);
        repo.push_branch("master", "origin")?;
        assert_eq!(
            repo.resolve_default_base_branch(),
            Some("origin/master".to_string())
        );
        Ok(())
    }

    #[test]
    fn resolve_default_base_uses_local_main_without_remote() -> Result<()> {
        let temp_dir = init_tmp_repo();
        let p = temp_dir.path();
        std::fs::write(p.join("f.txt"), "x")?;
        git_in(p, &["checkout", "-b", "main"]);
        git_in(p, &["add", "."]);
        git_in(p, &["commit", "-m", "init"]);
        let repo = GitRepository::open_at(p)?;
        assert_eq!(repo.resolve_default_base_branch(), Some("main".to_string()));
        Ok(())
    }

    #[test]
    fn resolve_default_base_falls_back_to_local_master() -> Result<()> {
        let temp_dir = init_tmp_repo();
        let p = temp_dir.path();
        std::fs::write(p.join("f.txt"), "x")?;
        git_in(p, &["checkout", "-b", "master"]);
        git_in(p, &["add", "."]);
        git_in(p, &["commit", "-m", "init"]);
        let repo = GitRepository::open_at(p)?;
        assert_eq!(
            repo.resolve_default_base_branch(),
            Some("master".to_string())
        );
        Ok(())
    }

    #[test]
    fn resolve_default_base_none_without_mainline() -> Result<()> {
        let temp_dir = init_tmp_repo();
        let p = temp_dir.path();
        std::fs::write(p.join("f.txt"), "x")?;
        git_in(p, &["checkout", "-b", "dev"]);
        git_in(p, &["add", "."]);
        git_in(p, &["commit", "-m", "init"]);
        let repo = GitRepository::open_at(p)?;
        assert_eq!(repo.resolve_default_base_branch(), None);
        Ok(())
    }

    // ── in_main_branches population (issue #1105) ──────────────────

    #[test]
    fn commits_in_range_report_main_branch_containment() -> Result<()> {
        // Work repo on `main` with one pushed commit and one unpushed commit
        // on top of it.
        let tmp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
        std::fs::create_dir_all(&tmp_root)?;
        let bare = tempfile::tempdir_in(&tmp_root)?;
        git_in(bare.path(), &["init", "--bare"]);

        let work = init_tmp_repo();
        let p = work.path();
        git_in(p, &["checkout", "-b", "main"]);
        std::fs::write(p.join("a.txt"), "pushed")?;
        git_in(p, &["add", "."]);
        git_in(p, &["commit", "-m", "pushed commit"]);
        #[allow(clippy::unwrap_used)]
        git_in(
            p,
            &["remote", "add", "origin", bare.path().to_str().unwrap()],
        );
        git_in(p, &["push", "origin", "main"]);
        std::fs::write(p.join("b.txt"), "unpushed")?;
        git_in(p, &["add", "."]);
        git_in(p, &["commit", "-m", "unpushed commit"]);

        let repo = GitRepository::open_at(p)?;
        // Single-rev path: the pushed commit is contained in origin/main.
        let pushed = repo.get_commits_in_range("HEAD~1")?;
        assert_eq!(pushed.len(), 1);
        assert_eq!(pushed[0].in_main_branches, vec!["origin/main".to_string()]);
        // Range path: the unpushed commit on top is not contained.
        let unpushed = repo.get_commits_in_range("HEAD~1..HEAD")?;
        assert_eq!(unpushed.len(), 1);
        assert!(unpushed[0].in_main_branches.is_empty());
        Ok(())
    }

    #[test]
    fn commits_in_range_empty_containment_without_remotes() -> Result<()> {
        let work = init_tmp_repo();
        let p = work.path();
        std::fs::write(p.join("a.txt"), "x")?;
        git_in(p, &["add", "."]);
        git_in(p, &["commit", "-m", "local only"]);

        let repo = GitRepository::open_at(p)?;
        let commits = repo.get_commits_in_range("HEAD")?;
        assert_eq!(commits.len(), 1);
        assert!(commits[0].in_main_branches.is_empty());
        Ok(())
    }

    // ── get_commits_from_head (#1476) ───────────────────────────────

    /// Creates a linear chain of `n` commits (subjects "commit 0".."commit
    /// n-1", oldest first) in a fresh temp repo.
    #[allow(clippy::unwrap_used)]
    fn repo_with_linear_commits(n: usize) -> (tempfile::TempDir, std::path::PathBuf) {
        let temp_dir = init_tmp_repo();
        let p = temp_dir.path().to_path_buf();
        for i in 0..n {
            std::fs::write(p.join("f.txt"), format!("content {i}")).unwrap();
            git_in(&p, &["add", "."]);
            git_in(&p, &["commit", "-m", &format!("commit {i}")]);
        }
        (temp_dir, p)
    }

    #[test]
    fn commits_from_head_no_cap_returns_all_in_chronological_order() -> Result<()> {
        let (_tmp, p) = repo_with_linear_commits(3);
        let repo = GitRepository::open_at(&p)?;
        let commits = repo.get_commits_from_head(None)?;
        let subjects: Vec<&str> = commits.iter().map(|c| c.original_message.trim()).collect();
        assert_eq!(subjects, vec!["commit 0", "commit 1", "commit 2"]);
        Ok(())
    }

    #[test]
    fn commits_from_head_max_count_caps_to_newest() -> Result<()> {
        let (_tmp, p) = repo_with_linear_commits(5);
        let repo = GitRepository::open_at(&p)?;
        let commits = repo.get_commits_from_head(Some(2))?;
        let subjects: Vec<&str> = commits.iter().map(|c| c.original_message.trim()).collect();
        // The newest 2 commits, still returned oldest-first.
        assert_eq!(subjects, vec!["commit 3", "commit 4"]);
        Ok(())
    }

    #[test]
    fn commits_from_head_unborn_head_errors() -> Result<()> {
        // A freshly `git init`ed repo with zero commits has an unborn HEAD,
        // so `push_head()` errors — distinct from an empty *range* on an
        // otherwise-populated repo (e.g. `HEAD..HEAD`), which the CLI layer
        // handles as an empty report rather than a hard failure.
        let temp_dir = init_tmp_repo();
        let repo = GitRepository::open_at(temp_dir.path())?;
        let result = repo.get_commits_from_head(None);
        assert!(result.is_err(), "unborn HEAD is expected to error");
        Ok(())
    }

    #[test]
    fn commits_from_head_skips_merge_commits() -> Result<()> {
        let (_tmp, p) = repo_with_linear_commits(1);
        git_in(&p, &["checkout", "-b", "feature"]);
        std::fs::write(p.join("g.txt"), "feature")?;
        git_in(&p, &["add", "."]);
        git_in(&p, &["commit", "-m", "feature commit"]);
        // Back to the branch `feature` was cut from, then merge it in with a
        // real merge commit (--no-ff, so a fast-forward can't collapse it away).
        git_in(&p, &["checkout", "-"]);
        git_in(&p, &["merge", "--no-ff", "feature", "-m", "merge feature"]);

        let repo = GitRepository::open_at(&p)?;
        let commits = repo.get_commits_from_head(None)?;
        let subjects: Vec<&str> = commits.iter().map(|c| c.original_message.trim()).collect();
        assert!(
            !subjects.contains(&"merge feature"),
            "merge commit must be excluded, got: {subjects:?}"
        );
        assert!(subjects.contains(&"commit 0"));
        assert!(subjects.contains(&"feature commit"));
        Ok(())
    }

    // ── tracked_files (issue #1475) ─────────────────────────────────

    #[test]
    fn tracked_files_includes_committed_and_staged() -> Result<()> {
        let work = init_tmp_repo();
        let p = work.path();
        std::fs::write(p.join("a.txt"), "committed")?;
        git_in(p, &["add", "."]);
        git_in(p, &["commit", "-m", "init"]);
        std::fs::write(p.join("b.txt"), "staged only")?;
        git_in(p, &["add", "b.txt"]);

        let repo = GitRepository::open_at(p)?;
        let files = repo.tracked_files()?;
        assert!(files.contains(&"a.txt".to_string()));
        assert!(files.contains(&"b.txt".to_string()));
        Ok(())
    }

    #[test]
    fn tracked_files_excludes_untracked() -> Result<()> {
        let work = init_tmp_repo();
        let p = work.path();
        std::fs::write(p.join("a.txt"), "committed")?;
        git_in(p, &["add", "."]);
        git_in(p, &["commit", "-m", "init"]);
        std::fs::write(p.join("untracked.txt"), "never added")?;

        let repo = GitRepository::open_at(p)?;
        let files = repo.tracked_files()?;
        assert!(!files.contains(&"untracked.txt".to_string()));
        Ok(())
    }

    #[test]
    fn tracked_files_sorted() -> Result<()> {
        let work = init_tmp_repo();
        let p = work.path();
        std::fs::write(p.join("zeta.txt"), "z")?;
        std::fs::write(p.join("alpha.txt"), "a")?;
        git_in(p, &["add", "."]);
        git_in(p, &["commit", "-m", "init"]);

        let repo = GitRepository::open_at(p)?;
        let files = repo.tracked_files()?;
        let mut sorted = files.clone();
        sorted.sort();
        assert_eq!(files, sorted);
        Ok(())
    }

    #[test]
    fn tracked_files_empty_repo() -> Result<()> {
        let temp_dir = init_tmp_repo();
        let repo = GitRepository::open_at(temp_dir.path())?;
        assert!(repo.tracked_files()?.is_empty());
        Ok(())
    }
}
