//! Git commit amendment operations.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use git2::{Oid, Repository};
use tracing::debug;

use crate::data::amendments::{Amendment, AmendmentFile};
use crate::git::SHORT_HASH_LEN;

/// Amendment operation handler.
pub struct AmendmentHandler {
    repo: Repository,
    /// Workdir the `git` subprocesses are pinned to, so amendments target the
    /// injected repository rather than the process current working directory.
    repo_root: PathBuf,
    /// Permits amending commits that already exist in remote main branches.
    allow_pushed: bool,
}

impl AmendmentHandler {
    /// Creates a new amendment handler for the repository at `repo_root`.
    pub fn new(repo_root: &Path) -> Result<Self> {
        let repo = Repository::open(repo_root).context("Failed to open git repository")?;
        Ok(Self {
            repo,
            repo_root: repo_root.to_path_buf(),
            allow_pushed: false,
        })
    }

    /// Permits amending commits that already exist in remote main branches.
    ///
    /// Off by default: amending a pushed commit rewrites published history, so
    /// callers must opt in explicitly (the `--allow-pushed` CLI flag).
    #[must_use]
    pub fn with_allow_pushed(mut self, allow_pushed: bool) -> Self {
        self.allow_pushed = allow_pushed;
        self
    }

    /// Builds a `git` subprocess pinned to the handler's repo workdir, so every
    /// rebase/commit/read operation targets the injected repository rather than
    /// the process current working directory.
    fn git_command(&self) -> Command {
        let mut cmd = Command::new("git");
        cmd.current_dir(&self.repo_root);
        cmd
    }

    /// Applies amendments from a YAML file.
    pub fn apply_amendments(&self, yaml_file: &str) -> Result<()> {
        // Load and validate amendment file
        let amendment_file = AmendmentFile::load_from_file(yaml_file)?;
        self.apply_amendment_file(&amendment_file)
    }

    /// Applies an already-parsed amendment file.
    ///
    /// The core of [`Self::apply_amendments`], split out so callers that hold
    /// the amendments in memory (the MCP `git_amend_commits` tool, which
    /// receives them as an inline YAML string) reuse the identical
    /// safety-check + apply path without a round-trip through a temp file.
    pub fn apply_amendment_file(&self, amendment_file: &AmendmentFile) -> Result<()> {
        // Safety checks
        self.perform_safety_checks(amendment_file)?;

        // Group amendments by their position in history
        let amendments = self.organize_amendments(&amendment_file.amendments)?;

        if amendments.is_empty() {
            println!("No valid amendments found to apply.");
            return Ok(());
        }

        // Check if we only need to amend HEAD
        if amendments.len() == 1 && self.is_head_commit(&amendments[0].0)? {
            println!(
                "Amending HEAD commit: {}",
                &amendments[0].0[..SHORT_HASH_LEN]
            );
            self.amend_head_commit(&amendments[0].1)?;
        } else {
            println!(
                "Amending {} commits using interactive rebase",
                amendments.len()
            );
            self.amend_via_rebase(amendments)?;
        }

        println!("✅ Amendment operations completed successfully");
        Ok(())
    }

    /// Performs safety checks before amendment.
    fn perform_safety_checks(&self, amendment_file: &AmendmentFile) -> Result<()> {
        // Check if working directory is clean
        crate::utils::preflight::check_working_directory_clean_at(&self.repo_root)
            .context("Cannot amend commits with uncommitted changes")?;

        // Check if commits exist and are not in remote main branches
        let main_tips = crate::git::main_branches::detect_main_branch_tips(&self.repo)?;
        for amendment in &amendment_file.amendments {
            self.validate_commit_amendable(&amendment.commit, &main_tips)?;
        }

        Ok(())
    }

    /// Validates that a commit can be safely amended.
    fn validate_commit_amendable(
        &self,
        commit_hash: &str,
        main_tips: &[crate::git::main_branches::MainBranchTip],
    ) -> Result<()> {
        // Check if commit exists
        let oid = Oid::from_str(commit_hash)
            .with_context(|| format!("Invalid commit hash: {commit_hash}"))?;

        let _commit = self
            .repo
            .find_commit(oid)
            .with_context(|| format!("Commit not found: {commit_hash}"))?;

        let containing =
            crate::git::main_branches::branches_containing(&self.repo, main_tips, oid)?;
        if !containing.is_empty() {
            let short_hash = &commit_hash[..SHORT_HASH_LEN.min(commit_hash.len())];
            let branches = containing.join(", ");
            if !self.allow_pushed {
                anyhow::bail!(
                    "Refusing to amend commit {short_hash}: it is already in remote main \
                     branch(es): {branches}\n\
                     Amending pushed commits rewrites published history. Re-run with \
                     --allow-pushed to override."
                );
            }
            println!("⚠️  Amending commit {short_hash} that exists in {branches} (--allow-pushed)");
        }

        Ok(())
    }

    /// Organizes amendments by their order in git history.
    fn organize_amendments(&self, amendments: &[Amendment]) -> Result<Vec<(String, String)>> {
        let mut valid_amendments = Vec::new();
        let mut commit_depths = HashMap::new();

        // Calculate depth of each commit from HEAD
        for amendment in amendments {
            if let Ok(depth) = self.get_commit_depth_from_head(&amendment.commit) {
                commit_depths.insert(amendment.commit.clone(), depth);
                valid_amendments.push((amendment.commit.clone(), amendment.message.clone()));
            } else {
                println!(
                    "Warning: Skipping invalid commit {}",
                    &amendment.commit[..SHORT_HASH_LEN]
                );
            }
        }

        // Sort by depth (deepest first for rebase order)
        valid_amendments.sort_by_key(|(commit, _)| commit_depths.get(commit).copied().unwrap_or(0));

        // Reverse so we process from oldest to newest
        valid_amendments.reverse();

        Ok(valid_amendments)
    }

    /// Returns the depth of a commit from HEAD (0 = HEAD, 1 = HEAD~1, etc.).
    fn get_commit_depth_from_head(&self, commit_hash: &str) -> Result<usize> {
        let target_oid = Oid::from_str(commit_hash)?;
        let mut revwalk = self.repo.revwalk()?;
        revwalk.push_head()?;

        for (depth, oid_result) in revwalk.enumerate() {
            let oid = oid_result?;
            if oid == target_oid {
                return Ok(depth);
            }
        }

        anyhow::bail!("Commit {commit_hash} not found in current branch history");
    }

    /// Checks if a commit hash is the current HEAD.
    fn is_head_commit(&self, commit_hash: &str) -> Result<bool> {
        let head_oid = self.repo.head()?.target().context("HEAD has no target")?;
        let target_oid = Oid::from_str(commit_hash)?;
        Ok(head_oid == target_oid)
    }

    /// Amends the HEAD commit message.
    fn amend_head_commit(&self, new_message: &str) -> Result<()> {
        let head_commit = self.repo.head()?.peel_to_commit()?;

        // Use the simpler approach: git commit --amend
        let output = self
            .git_command()
            .args(["commit", "--amend", "--message", new_message])
            .output()
            .context("Failed to execute git commit --amend")?;

        if !output.status.success() {
            let error_msg = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to amend HEAD commit: {error_msg}");
        }

        // Get the new commit ID for logging
        let new_head = self.repo.head()?.peel_to_commit()?;

        println!(
            "✅ Amended HEAD commit {} -> {}",
            &head_commit.id().to_string()[..SHORT_HASH_LEN],
            &new_head.id().to_string()[..SHORT_HASH_LEN]
        );

        Ok(())
    }

    /// Amends commits via individual interactive rebases (following shell script strategy).
    fn amend_via_rebase(&self, amendments: Vec<(String, String)>) -> Result<()> {
        if amendments.is_empty() {
            return Ok(());
        }

        println!("Amending commits individually in reverse order (newest to oldest)");

        // Sort amendments by commit depth (newest first, following shell script approach)
        let mut sorted_amendments = amendments;
        sorted_amendments
            .sort_by_key(|(hash, _)| self.get_commit_depth_from_head(hash).unwrap_or(usize::MAX));

        // Process each commit individually
        for (commit_hash, new_message) in sorted_amendments {
            let depth = self.get_commit_depth_from_head(&commit_hash)?;

            if depth == 0 {
                // This is HEAD - simple amendment
                println!("Amending HEAD commit: {}", &commit_hash[..SHORT_HASH_LEN]);
                self.amend_head_commit(&new_message)?;
            } else {
                // This is an older commit - use individual interactive rebase
                println!(
                    "Amending commit at depth {}: {}",
                    depth,
                    &commit_hash[..SHORT_HASH_LEN]
                );
                self.amend_single_commit_via_rebase(&commit_hash, &new_message)?;
            }
        }

        Ok(())
    }

    /// Amends a single commit using individual interactive rebase (shell script strategy).
    fn amend_single_commit_via_rebase(&self, commit_hash: &str, new_message: &str) -> Result<()> {
        // Get the parent of the target commit to use as rebase base
        let base_commit = format!("{commit_hash}^");

        // Create temporary sequence file for this specific rebase
        let temp_dir = tempfile::tempdir()?;
        let sequence_file = temp_dir.path().join("rebase-sequence");

        // Generate rebase sequence: edit the target commit, pick the rest
        let mut sequence_content = String::new();
        let commit_list_output = self
            .git_command()
            .args(["rev-list", "--reverse", &format!("{base_commit}..HEAD")])
            .output()
            .context("Failed to get commit list for rebase")?;

        if !commit_list_output.status.success() {
            anyhow::bail!("Failed to generate commit list for rebase");
        }

        let commit_list = String::from_utf8_lossy(&commit_list_output.stdout);
        for line in commit_list.lines() {
            let commit = line.trim();
            if commit.is_empty() {
                continue;
            }

            // Get short commit message for the sequence file
            let subject_output = self
                .git_command()
                .args(["log", "--format=%s", "-n", "1", commit])
                .output()
                .context("Failed to get commit subject")?;

            let subject = String::from_utf8_lossy(&subject_output.stdout)
                .trim()
                .to_string();

            if commit.starts_with(&commit_hash[..commit.len().min(commit_hash.len())]) {
                // This is our target commit - mark it for editing
                sequence_content.push_str(&format!("edit {commit} {subject}\n"));
            } else {
                // Other commits - just pick them
                sequence_content.push_str(&format!("pick {commit} {subject}\n"));
            }
        }

        // Write sequence file
        std::fs::write(&sequence_file, sequence_content)?;

        println!(
            "Starting interactive rebase to amend commit: {}",
            &commit_hash[..SHORT_HASH_LEN]
        );

        // Execute rebase with custom sequence editor
        let rebase_result = self.git_command()
            .args(["rebase", "-i", &base_commit])
            .env(
                "GIT_SEQUENCE_EDITOR",
                format!("cp {}", sequence_file.display()),
            )
            .env("GIT_EDITOR", "true") // Prevent interactive editor
            .output()
            .context("Failed to start interactive rebase")?;

        if !rebase_result.status.success() {
            let error_msg = String::from_utf8_lossy(&rebase_result.stderr);

            // Best-effort cleanup; the rebase may not have started.
            if let Err(e) = self.git_command().args(["rebase", "--abort"]).output() {
                debug!("Rebase abort during cleanup failed: {e}");
            }

            anyhow::bail!("Interactive rebase failed: {error_msg}");
        }

        // Check if we're now in a rebase state where we can amend
        let repo_state = self.repo.state();
        if repo_state == git2::RepositoryState::RebaseInteractive {
            // We should be stopped at the target commit - amend it
            let current_commit_output = self
                .git_command()
                .args(["rev-parse", "HEAD"])
                .output()
                .context("Failed to get current commit during rebase")?;

            let current_commit = String::from_utf8_lossy(&current_commit_output.stdout)
                .trim()
                .to_string();

            if current_commit
                .starts_with(&commit_hash[..current_commit.len().min(commit_hash.len())])
            {
                // Amend with new message
                let amend_result = self
                    .git_command()
                    .args(["commit", "--amend", "-m", new_message])
                    .output()
                    .context("Failed to amend commit during rebase")?;

                if !amend_result.status.success() {
                    let error_msg = String::from_utf8_lossy(&amend_result.stderr);
                    // Best-effort cleanup; abort so the repo isn't left mid-rebase.
                    if let Err(e) = self.git_command().args(["rebase", "--abort"]).output() {
                        debug!("Rebase abort during cleanup failed: {e}");
                    }
                    anyhow::bail!("Failed to amend commit: {error_msg}");
                }

                println!("✅ Amended commit: {}", &commit_hash[..SHORT_HASH_LEN]);

                // Continue the rebase
                let continue_result = self
                    .git_command()
                    .args(["rebase", "--continue"])
                    .output()
                    .context("Failed to continue rebase")?;

                if !continue_result.status.success() {
                    let error_msg = String::from_utf8_lossy(&continue_result.stderr);
                    // Best-effort cleanup; abort so the repo isn't left mid-rebase.
                    if let Err(e) = self.git_command().args(["rebase", "--abort"]).output() {
                        debug!("Rebase abort during cleanup failed: {e}");
                    }
                    anyhow::bail!("Failed to continue rebase: {error_msg}");
                }

                println!("✅ Rebase completed successfully");
            } else {
                // Best-effort cleanup; abort so the repo isn't left mid-rebase.
                if let Err(e) = self.git_command().args(["rebase", "--abort"]).output() {
                    debug!("Rebase abort during cleanup failed: {e}");
                }
                anyhow::bail!(
                    "Unexpected commit during rebase. Expected {}, got {}",
                    &commit_hash[..SHORT_HASH_LEN],
                    &current_commit[..SHORT_HASH_LEN]
                );
            }
        } else if repo_state != git2::RepositoryState::Clean {
            anyhow::bail!("Repository in unexpected state after rebase: {repo_state:?}");
        }

        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Runs `git` in `dir` with a deterministic identity, asserting success.
    fn git_in(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(dir)
            .args([
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=Test",
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

    /// Builds a work repo on `main` with one commit pushed to a bare `origin`
    /// remote. Local identity and no-signing config are set so the handler's
    /// own `git` subprocesses stay hermetic. All three temp dirs are returned
    /// so the caller keeps them alive: work repo, bare remote, and a scratch
    /// dir for amendment YAML files (outside the worktree, which must stay
    /// clean for the safety checks).
    fn repo_with_pushed_main() -> (tempfile::TempDir, tempfile::TempDir, tempfile::TempDir) {
        let tmp_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
        std::fs::create_dir_all(&tmp_root).unwrap();
        let bare = tempfile::tempdir_in(&tmp_root).unwrap();
        git_in(bare.path(), &["init", "--bare"]);

        let work = tempfile::tempdir_in(&tmp_root).unwrap();
        git_in(work.path(), &["init"]);
        git_in(work.path(), &["checkout", "-b", "main"]);
        git_in(work.path(), &["config", "user.email", "test@example.com"]);
        git_in(work.path(), &["config", "user.name", "Test"]);
        git_in(work.path(), &["config", "commit.gpgsign", "false"]);
        std::fs::write(work.path().join("file.txt"), "content").unwrap();
        git_in(work.path(), &["add", "."]);
        git_in(work.path(), &["commit", "-m", "initial pushed commit"]);
        git_in(
            work.path(),
            &["remote", "add", "origin", bare.path().to_str().unwrap()],
        );
        git_in(work.path(), &["push", "origin", "main"]);

        let scratch = tempfile::tempdir_in(&tmp_root).unwrap();
        (work, bare, scratch)
    }

    fn head_hash(dir: &Path) -> String {
        Repository::open(dir)
            .unwrap()
            .head()
            .unwrap()
            .target()
            .unwrap()
            .to_string()
    }

    fn head_message(dir: &Path) -> String {
        let repo = Repository::open(dir).unwrap();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        head.message().unwrap_or("").to_string()
    }

    /// Writes an amendment YAML targeting `hash` into `scratch` and returns
    /// its path as a string.
    fn amendment_yaml(scratch: &Path, hash: &str, message: &str) -> String {
        let file = crate::data::amendments::AmendmentFile {
            amendments: vec![crate::data::amendments::Amendment {
                commit: hash.to_string(),
                message: message.to_string(),
                summary: String::new(),
            }],
        };
        let path = scratch.join("amendments.yaml");
        file.save_to_file(&path).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn refuses_amending_commit_in_remote_main() {
        let (work, _bare, scratch) = repo_with_pushed_main();
        let hash = head_hash(work.path());
        let yaml = amendment_yaml(scratch.path(), &hash, "rewritten message");

        let handler = AmendmentHandler::new(work.path()).unwrap();
        let err = handler
            .apply_amendments(&yaml)
            .expect_err("amending a pushed commit must be refused");
        let msg = format!("{err:#}");
        assert!(
            msg.contains(&hash[..SHORT_HASH_LEN]),
            "error should name the short hash, got: {msg}"
        );
        assert!(
            msg.contains("origin/main"),
            "error should name the containing branch, got: {msg}"
        );
        assert!(
            msg.contains("--allow-pushed"),
            "error should mention the override flag, got: {msg}"
        );
        // The commit must be untouched.
        assert_eq!(head_hash(work.path()), hash);
    }

    #[test]
    fn allow_pushed_overrides_refusal() {
        let (work, _bare, scratch) = repo_with_pushed_main();
        let hash = head_hash(work.path());
        let yaml = amendment_yaml(scratch.path(), &hash, "rewritten message");

        let handler = AmendmentHandler::new(work.path())
            .unwrap()
            .with_allow_pushed(true);
        handler
            .apply_amendments(&yaml)
            .expect("--allow-pushed must permit amending a pushed commit");
        assert_eq!(head_message(work.path()).trim(), "rewritten message");
    }

    #[test]
    fn unpushed_commit_amends_without_flag() {
        let (work, _bare, scratch) = repo_with_pushed_main();
        std::fs::write(work.path().join("new.txt"), "more").unwrap();
        git_in(work.path(), &["add", "."]);
        git_in(work.path(), &["commit", "-m", "unpushed commit"]);
        let hash = head_hash(work.path());
        let yaml = amendment_yaml(scratch.path(), &hash, "improved unpushed message");

        let handler = AmendmentHandler::new(work.path()).unwrap();
        handler
            .apply_amendments(&yaml)
            .expect("amending an unpushed commit must not require the flag");
        assert_eq!(
            head_message(work.path()).trim(),
            "improved unpushed message"
        );
    }
}
