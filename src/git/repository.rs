//! Git repository operations.

use std::io::BufReader;
use std::path::PathBuf;

use anyhow::{Context, Result};
use git2::{Repository, Status};
use ssh2_config::{ParseRule, SshConfig};
use tracing::{debug, error, info};

use crate::git::CommitInfo;

/// Maximum credential callback attempts before giving up.
const MAX_AUTH_ATTEMPTS: u32 = 3;

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
    /// Opens a repository at the current directory.
    pub fn open() -> Result<Self> {
        let repo = Repository::open(".").context("Not in a git repository")?;

        Ok(Self { repo })
    }

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
            if let Some(path) = entry.path() {
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

        if let Some(name) = head.shorthand() {
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
                anyhow::bail!("Invalid range format: {}", range);
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

/// Extracts hostname from a git URL (e.g., "git@github.com:user/repo.git" -> "github.com").
fn extract_hostname_from_git_url(url: &str) -> Option<String> {
    if let Some(ssh_url) = url.strip_prefix("git@") {
        // SSH URL format: git@hostname:path
        ssh_url.split(':').next().map(str::to_string)
    } else if let Some(https_url) = url.strip_prefix("https://") {
        // HTTPS URL format: https://hostname/path
        https_url.split('/').next().map(str::to_string)
    } else if let Some(http_url) = url.strip_prefix("http://") {
        // HTTP URL format: http://hostname/path
        http_url.split('/').next().map(str::to_string)
    } else {
        None
    }
}

/// Returns the SSH identity file for a given host from SSH config.
fn get_ssh_identity_for_host(hostname: &str) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let ssh_config_path = PathBuf::from(&home).join(".ssh/config");

    if !ssh_config_path.exists() {
        debug!("SSH config file not found at: {:?}", ssh_config_path);
        return None;
    }

    // Open and parse the SSH config file
    let file = std::fs::File::open(&ssh_config_path).ok()?;
    let mut reader = BufReader::new(file);

    let config = SshConfig::default()
        .parse(&mut reader, ParseRule::ALLOW_UNKNOWN_FIELDS)
        .ok()?;

    // Query the config for the specific host
    let params = config.query(hostname);

    // Get the identity file from the config
    if let Some(identity_files) = &params.identity_file {
        if let Some(first_identity) = identity_files.first() {
            // Expand ~ to home directory
            let identity_str = first_identity.to_string_lossy();
            let identity_path = identity_str.replace('~', &home);
            let path = PathBuf::from(identity_path);

            if path.exists() {
                debug!("Found SSH key for host '{}': {:?}", hostname, path);
                return Some(path);
            }
            debug!("SSH key specified in config but not found: {:?}", path);
        }
    }

    None
}

/// Creates `RemoteCallbacks` with SSH credential resolution for the given hostname.
///
/// Tries credentials in order: SSH config identity → SSH agent → default key
/// locations (`~/.ssh/id_ed25519`, `~/.ssh/id_rsa`). Bails after
/// [`MAX_AUTH_ATTEMPTS`] to prevent infinite callback loops.
fn make_auth_callbacks(hostname: String) -> git2::RemoteCallbacks<'static> {
    let mut callbacks = git2::RemoteCallbacks::new();
    let mut auth_attempts: u32 = 0;

    callbacks.credentials(move |url, username_from_url, allowed_types| {
        auth_attempts += 1;
        debug!(
            "Credential callback attempt {} - URL: {}, Username: {:?}, Allowed types: {:?}",
            auth_attempts, url, username_from_url, allowed_types
        );

        if auth_attempts > MAX_AUTH_ATTEMPTS {
            error!(
                "Too many authentication attempts ({}), giving up",
                auth_attempts
            );
            return Err(git2::Error::from_str(
                "Authentication failed after multiple attempts",
            ));
        }

        let username = username_from_url.unwrap_or("git");

        if allowed_types.contains(git2::CredentialType::SSH_KEY) {
            // Try SSH config identity first — avoids agent returning OK with no valid keys
            if let Some(ssh_key_path) = get_ssh_identity_for_host(&hostname) {
                let pub_key_path = ssh_key_path.with_extension("pub");
                debug!("Trying SSH key from config: {:?}", ssh_key_path);

                match git2::Cred::ssh_key(username, Some(&pub_key_path), &ssh_key_path, None) {
                    Ok(cred) => {
                        debug!(
                            "Successfully loaded SSH key from config: {:?}",
                            ssh_key_path
                        );
                        return Ok(cred);
                    }
                    Err(e) => {
                        debug!("Failed to load SSH key from config: {}", e);
                    }
                }
            }

            // Only try SSH agent on first attempt
            if auth_attempts == 1 {
                match git2::Cred::ssh_key_from_agent(username) {
                    Ok(cred) => {
                        debug!("SSH agent credentials obtained (attempt {})", auth_attempts);
                        return Ok(cred);
                    }
                    Err(e) => {
                        debug!("SSH agent failed: {}, trying default keys", e);
                    }
                }
            }

            // Try default SSH key locations as fallback
            let home = std::env::var("HOME").unwrap_or_else(|_| "~".to_string());
            let ssh_keys = [
                format!("{home}/.ssh/id_ed25519"),
                format!("{home}/.ssh/id_rsa"),
            ];

            for key_path in &ssh_keys {
                let key_path = PathBuf::from(key_path);
                if key_path.exists() {
                    let pub_key_path = key_path.with_extension("pub");
                    debug!("Trying default SSH key: {:?}", key_path);

                    match git2::Cred::ssh_key(username, Some(&pub_key_path), &key_path, None) {
                        Ok(cred) => {
                            debug!("Successfully loaded SSH key from {:?}", key_path);
                            return Ok(cred);
                        }
                        Err(e) => debug!("Failed to load SSH key from {:?}: {}", key_path, e),
                    }
                }
            }
        }

        debug!("Falling back to default credentials");
        git2::Cred::default()
    });

    callbacks
}

/// Formats a user-friendly SSH authentication error message with troubleshooting steps.
fn format_auth_error(operation: &str, error: &git2::Error) -> String {
    if error.message().contains("authentication") || error.message().contains("SSH") {
        format!(
            "Failed to {operation}: {error}. \n\nTroubleshooting steps:\n\
            1. Check if your SSH key is loaded: ssh-add -l\n\
            2. Test GitHub SSH connection: ssh -T git@github.com\n\
            3. Use GitHub CLI auth instead: gh auth setup-git",
        )
    } else {
        format!("Failed to {operation}: {error}")
    }
}

impl GitRepository {
    /// Pushes the current branch to remote.
    pub fn push_branch(&self, branch_name: &str, remote_name: &str) -> Result<()> {
        info!(
            "Pushing branch '{}' to remote '{}'",
            branch_name, remote_name
        );

        // Get remote
        debug!("Finding remote '{}'", remote_name);
        let mut remote = self
            .repo
            .find_remote(remote_name)
            .context("Failed to find remote")?;

        let remote_url = remote.url().unwrap_or("<unknown>");
        debug!("Remote URL: {}", remote_url);

        // Set up refspec for push
        let refspec = format!("refs/heads/{branch_name}:refs/heads/{branch_name}");
        debug!("Using refspec: {}", refspec);

        // Extract hostname from remote URL for SSH config lookup
        let hostname =
            extract_hostname_from_git_url(remote_url).unwrap_or("github.com".to_string());
        debug!(
            "Extracted hostname '{}' from URL '{}'",
            hostname, remote_url
        );

        // Push with authentication callbacks
        let mut push_options = git2::PushOptions::new();
        let callbacks = make_auth_callbacks(hostname);
        push_options.remote_callbacks(callbacks);

        // Perform the push
        debug!("Attempting to push to remote...");
        match remote.push(&[&refspec], Some(&mut push_options)) {
            Ok(()) => {
                info!(
                    "Successfully pushed branch '{}' to remote '{}'",
                    branch_name, remote_name
                );

                // Set upstream branch after successful push
                debug!("Setting upstream branch for '{}'", branch_name);
                match self.repo.find_branch(branch_name, git2::BranchType::Local) {
                    Ok(mut branch) => {
                        let remote_ref = format!("{remote_name}/{branch_name}");
                        match branch.set_upstream(Some(&remote_ref)) {
                            Ok(()) => {
                                info!(
                                    "Successfully set upstream to '{}'/{}",
                                    remote_name, branch_name
                                );
                            }
                            Err(e) => {
                                // Log but don't fail - the push succeeded
                                error!("Failed to set upstream branch: {}", e);
                            }
                        }
                    }
                    Err(e) => {
                        // Log but don't fail - the push succeeded
                        error!("Failed to find local branch to set upstream: {}", e);
                    }
                }

                Ok(())
            }
            Err(e) => {
                error!("Failed to push branch: {}", e);
                Err(anyhow::anyhow!(format_auth_error(
                    "push branch to remote",
                    &e
                )))
            }
        }
    }

    /// Checks if a branch exists on remote.
    pub fn branch_exists_on_remote(&self, branch_name: &str, remote_name: &str) -> Result<bool> {
        debug!(
            "Checking if branch '{}' exists on remote '{}'",
            branch_name, remote_name
        );

        let remote = self
            .repo
            .find_remote(remote_name)
            .context("Failed to find remote")?;

        let remote_url = remote.url().unwrap_or("<unknown>");
        debug!("Remote URL: {}", remote_url);

        // Extract hostname from remote URL for SSH config lookup
        let hostname =
            extract_hostname_from_git_url(remote_url).unwrap_or("github.com".to_string());
        debug!(
            "Extracted hostname '{}' from URL '{}'",
            hostname, remote_url
        );

        // Connect to remote to get refs
        let mut remote = remote;
        let callbacks = make_auth_callbacks(hostname);

        debug!("Attempting to connect to remote...");
        match remote.connect_auth(git2::Direction::Fetch, Some(callbacks), None) {
            Ok(_) => debug!("Successfully connected to remote"),
            Err(e) => {
                error!("Failed to connect to remote: {}", e);
                return Err(anyhow::anyhow!(format_auth_error("connect to remote", &e)));
            }
        }

        // Check if the remote branch exists
        debug!("Listing remote refs...");
        let refs = remote.list()?;
        let remote_branch_ref = format!("refs/heads/{branch_name}");
        debug!("Looking for remote branch ref: {}", remote_branch_ref);

        for remote_head in refs {
            debug!("Found remote ref: {}", remote_head.name());
            if remote_head.name() == remote_branch_ref {
                info!(
                    "Branch '{}' exists on remote '{}'",
                    branch_name, remote_name
                );
                return Ok(true);
            }
        }

        info!(
            "Branch '{}' does not exist on remote '{}'",
            branch_name, remote_name
        );
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── extract_hostname_from_git_url ──────────────────────────────

    #[test]
    fn hostname_from_ssh_url() {
        let hostname = extract_hostname_from_git_url("git@github.com:user/repo.git");
        assert_eq!(hostname, Some("github.com".to_string()));
    }

    #[test]
    fn hostname_from_https_url() {
        let hostname = extract_hostname_from_git_url("https://github.com/user/repo.git");
        assert_eq!(hostname, Some("github.com".to_string()));
    }

    #[test]
    fn hostname_from_http_url() {
        let hostname = extract_hostname_from_git_url("http://gitlab.com/user/repo.git");
        assert_eq!(hostname, Some("gitlab.com".to_string()));
    }

    #[test]
    fn hostname_from_unknown_scheme() {
        let hostname = extract_hostname_from_git_url("ftp://example.com/repo");
        assert_eq!(hostname, None);
    }

    #[test]
    fn hostname_from_ssh_custom_host() {
        let hostname = extract_hostname_from_git_url("git@gitlab.example.com:org/project.git");
        assert_eq!(hostname, Some("gitlab.example.com".to_string()));
    }

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

    // ── format_auth_error ──────────────────────────────────────────

    #[test]
    fn auth_error_with_ssh_message() {
        let error = git2::Error::from_str("SSH authentication failed");
        let msg = format_auth_error("push", &error);
        assert!(msg.contains("Troubleshooting steps"));
        assert!(msg.contains("ssh-add -l"));
    }

    #[test]
    fn auth_error_without_auth_message() {
        let error = git2::Error::from_str("network timeout");
        let msg = format_auth_error("fetch", &error);
        assert!(msg.contains("Failed to fetch"));
        assert!(!msg.contains("Troubleshooting"));
    }

    // ── GitRepository with temp repo ───────────────────────────────

    #[test]
    fn open_at_temp_repo() -> Result<()> {
        let temp_dir = tempfile::tempdir_in(".")?;
        git2::Repository::init(temp_dir.path())?;
        let repo = GitRepository::open_at(temp_dir.path())?;
        assert!(repo.path().exists());
        Ok(())
    }

    #[test]
    fn working_directory_clean_empty_repo() -> Result<()> {
        let temp_dir = tempfile::tempdir_in(".")?;
        git2::Repository::init(temp_dir.path())?;
        let repo = GitRepository::open_at(temp_dir.path())?;
        let status = repo.get_working_directory_status()?;
        assert!(status.clean);
        assert!(status.untracked_changes.is_empty());
        Ok(())
    }

    #[test]
    fn working_directory_dirty_with_file() -> Result<()> {
        let temp_dir = tempfile::tempdir_in(".")?;
        git2::Repository::init(temp_dir.path())?;
        std::fs::write(temp_dir.path().join("new_file.txt"), "content")?;
        let repo = GitRepository::open_at(temp_dir.path())?;
        let status = repo.get_working_directory_status()?;
        assert!(!status.clean);
        assert!(!status.untracked_changes.is_empty());
        Ok(())
    }

    #[test]
    fn is_working_directory_clean_delegator() -> Result<()> {
        let temp_dir = tempfile::tempdir_in(".")?;
        git2::Repository::init(temp_dir.path())?;
        let repo = GitRepository::open_at(temp_dir.path())?;
        assert!(repo.is_working_directory_clean()?);
        Ok(())
    }
}
