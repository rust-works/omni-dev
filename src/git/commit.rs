//! Git commit operations and analysis

use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset};
use git2::{Commit, Repository};
use serde::{Deserialize, Serialize};

/// Commit information structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitInfo {
    /// Full SHA-1 hash of the commit
    pub hash: String,
    /// Commit author name and email address
    pub author: String,
    /// Commit date in ISO format with timezone
    pub date: DateTime<FixedOffset>,
    /// The original commit message as written by the author
    pub original_message: String,
    /// Array of remote main branches that contain this commit
    pub in_main_branches: Vec<String>,
    /// Automated analysis of the commit including type detection and proposed message
    pub analysis: CommitAnalysis,
}

/// Commit analysis information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitAnalysis {
    /// Automatically detected conventional commit type (feat, fix, docs, test, chore, etc.)
    pub detected_type: String,
    /// Automatically detected scope based on file paths (cli, git, data, etc.)
    pub detected_scope: String,
    /// AI-generated conventional commit message based on file changes
    pub proposed_message: String,
    /// Detailed statistics about file changes in this commit
    pub file_changes: FileChanges,
    /// Git diff --stat output showing lines changed per file
    pub diff_summary: String,
    /// Full diff content showing line-by-line changes
    pub diff_content: String,
}

/// File changes statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChanges {
    /// Total number of files modified in this commit
    pub total_files: usize,
    /// Number of new files added in this commit
    pub files_added: usize,
    /// Number of files deleted in this commit
    pub files_deleted: usize,
    /// Array of files changed with their git status (M=modified, A=added, D=deleted)
    pub file_list: Vec<FileChange>,
}

/// Individual file change
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChange {
    /// Git status code (A=added, M=modified, D=deleted, R=renamed)
    pub status: String,
    /// Path to the file relative to repository root
    pub file: String,
}

impl CommitInfo {
    /// Create CommitInfo from git2::Commit
    pub fn from_git_commit(repo: &Repository, commit: &Commit) -> Result<Self> {
        let hash = commit.id().to_string();

        let author = format!(
            "{} <{}>",
            commit.author().name().unwrap_or("Unknown"),
            commit.author().email().unwrap_or("unknown@example.com")
        );

        let timestamp = commit.author().when();
        let date = DateTime::from_timestamp(timestamp.seconds(), 0)
            .context("Invalid commit timestamp")?
            .with_timezone(
                &FixedOffset::east_opt(timestamp.offset_minutes() * 60)
                    .unwrap_or_else(|| FixedOffset::east_opt(0).unwrap()),
            );

        let original_message = commit.message().unwrap_or("").to_string();

        // TODO: Implement main branch detection
        let in_main_branches = Vec::new();

        // TODO: Implement commit analysis
        let analysis = CommitAnalysis::analyze_commit(repo, commit)?;

        Ok(Self {
            hash,
            author,
            date,
            original_message,
            in_main_branches,
            analysis,
        })
    }
}

impl CommitAnalysis {
    /// Analyze a commit and generate analysis information
    pub fn analyze_commit(repo: &Repository, commit: &Commit) -> Result<Self> {
        // Get file changes
        let file_changes = Self::analyze_file_changes(repo, commit)?;

        // Detect conventional commit type based on files and message
        let detected_type = Self::detect_commit_type(commit, &file_changes);

        // Detect scope based on file paths
        let detected_scope = Self::detect_scope(&file_changes);

        // Generate proposed conventional commit message
        let proposed_message =
            Self::generate_proposed_message(commit, &detected_type, &detected_scope, &file_changes);

        // Get diff summary
        let diff_summary = Self::get_diff_summary(repo, commit)?;

        // Get full diff content
        let diff_content = Self::get_diff_content(repo, commit)?;

        Ok(Self {
            detected_type,
            detected_scope,
            proposed_message,
            file_changes,
            diff_summary,
            diff_content,
        })
    }

    /// Analyze file changes in the commit
    fn analyze_file_changes(repo: &Repository, commit: &Commit) -> Result<FileChanges> {
        let mut file_list = Vec::new();
        let mut files_added = 0;
        let mut files_deleted = 0;

        // Get the tree for this commit
        let commit_tree = commit.tree().context("Failed to get commit tree")?;

        // Get parent tree if available
        let parent_tree = if commit.parent_count() > 0 {
            Some(
                commit
                    .parent(0)
                    .context("Failed to get parent commit")?
                    .tree()
                    .context("Failed to get parent tree")?,
            )
        } else {
            None
        };

        // Create diff between parent and commit
        let diff = if let Some(parent_tree) = parent_tree {
            repo.diff_tree_to_tree(Some(&parent_tree), Some(&commit_tree), None)
                .context("Failed to create diff")?
        } else {
            // Initial commit - diff against empty tree
            repo.diff_tree_to_tree(None, Some(&commit_tree), None)
                .context("Failed to create diff for initial commit")?
        };

        // Process each diff delta
        diff.foreach(
            &mut |delta, _progress| {
                let status = match delta.status() {
                    git2::Delta::Added => {
                        files_added += 1;
                        "A"
                    }
                    git2::Delta::Deleted => {
                        files_deleted += 1;
                        "D"
                    }
                    git2::Delta::Modified => "M",
                    git2::Delta::Renamed => "R",
                    git2::Delta::Copied => "C",
                    git2::Delta::Typechange => "T",
                    _ => "?",
                };

                if let Some(path) = delta.new_file().path() {
                    if let Some(path_str) = path.to_str() {
                        file_list.push(FileChange {
                            status: status.to_string(),
                            file: path_str.to_string(),
                        });
                    }
                }

                true
            },
            None,
            None,
            None,
        )
        .context("Failed to process diff")?;

        let total_files = file_list.len();

        Ok(FileChanges {
            total_files,
            files_added,
            files_deleted,
            file_list,
        })
    }

    /// Detect conventional commit type based on files and existing message
    fn detect_commit_type(commit: &Commit, file_changes: &FileChanges) -> String {
        let message = commit.message().unwrap_or("");

        // Check if message already has conventional commit format
        if let Some(existing_type) = Self::extract_conventional_type(message) {
            return existing_type;
        }

        // Analyze file patterns
        let files: Vec<&str> = file_changes
            .file_list
            .iter()
            .map(|f| f.file.as_str())
            .collect();

        // Check for specific patterns
        if files
            .iter()
            .any(|f| f.contains("test") || f.contains("spec"))
        {
            "test".to_string()
        } else if files
            .iter()
            .any(|f| f.ends_with(".md") || f.contains("README") || f.contains("docs/"))
        {
            "docs".to_string()
        } else if files
            .iter()
            .any(|f| f.contains("Cargo.toml") || f.contains("package.json") || f.contains("config"))
        {
            if file_changes.files_added > 0 {
                "feat".to_string()
            } else {
                "chore".to_string()
            }
        } else if file_changes.files_added > 0
            && files
                .iter()
                .any(|f| f.ends_with(".rs") || f.ends_with(".js") || f.ends_with(".py"))
        {
            "feat".to_string()
        } else if message.to_lowercase().contains("fix") || message.to_lowercase().contains("bug") {
            "fix".to_string()
        } else if file_changes.files_deleted > file_changes.files_added {
            "refactor".to_string()
        } else {
            "chore".to_string()
        }
    }

    /// Extract conventional commit type from existing message
    fn extract_conventional_type(message: &str) -> Option<String> {
        let first_line = message.lines().next().unwrap_or("");
        if let Some(colon_pos) = first_line.find(':') {
            let prefix = &first_line[..colon_pos];
            if let Some(paren_pos) = prefix.find('(') {
                let type_part = &prefix[..paren_pos];
                if Self::is_valid_conventional_type(type_part) {
                    return Some(type_part.to_string());
                }
            } else if Self::is_valid_conventional_type(prefix) {
                return Some(prefix.to_string());
            }
        }
        None
    }

    /// Check if a string is a valid conventional commit type
    fn is_valid_conventional_type(s: &str) -> bool {
        matches!(
            s,
            "feat"
                | "fix"
                | "docs"
                | "style"
                | "refactor"
                | "test"
                | "chore"
                | "build"
                | "ci"
                | "perf"
        )
    }

    /// Detect scope based on file paths
    fn detect_scope(file_changes: &FileChanges) -> String {
        let files: Vec<&str> = file_changes
            .file_list
            .iter()
            .map(|f| f.file.as_str())
            .collect();

        // Analyze common path patterns
        if files.iter().any(|f| f.starts_with("src/cli/")) {
            "cli".to_string()
        } else if files.iter().any(|f| f.starts_with("src/git/")) {
            "git".to_string()
        } else if files.iter().any(|f| f.starts_with("src/data/")) {
            "data".to_string()
        } else if files.iter().any(|f| f.starts_with("tests/")) {
            "test".to_string()
        } else if files.iter().any(|f| f.starts_with("docs/")) {
            "docs".to_string()
        } else if files
            .iter()
            .any(|f| f.contains("Cargo.toml") || f.contains("deny.toml"))
        {
            "deps".to_string()
        } else {
            "".to_string()
        }
    }

    /// Generate a proposed conventional commit message
    fn generate_proposed_message(
        commit: &Commit,
        commit_type: &str,
        scope: &str,
        file_changes: &FileChanges,
    ) -> String {
        let current_message = commit.message().unwrap_or("").lines().next().unwrap_or("");

        // If already properly formatted, return as-is
        if Self::extract_conventional_type(current_message).is_some() {
            return current_message.to_string();
        }

        // Generate description based on changes
        let description =
            if !current_message.is_empty() && !current_message.eq_ignore_ascii_case("stuff") {
                current_message.to_string()
            } else {
                Self::generate_description(commit_type, file_changes)
            };

        // Format with scope if available
        if scope.is_empty() {
            format!("{}: {}", commit_type, description)
        } else {
            format!("{}({}): {}", commit_type, scope, description)
        }
    }

    /// Generate description based on commit type and changes
    fn generate_description(commit_type: &str, file_changes: &FileChanges) -> String {
        match commit_type {
            "feat" => {
                if file_changes.total_files == 1 {
                    format!("add {}", file_changes.file_list[0].file)
                } else {
                    format!("add {} new features", file_changes.total_files)
                }
            }
            "fix" => "resolve issues".to_string(),
            "docs" => "update documentation".to_string(),
            "test" => "add tests".to_string(),
            "refactor" => "improve code structure".to_string(),
            "chore" => "update project files".to_string(),
            _ => "update project".to_string(),
        }
    }

    /// Get diff summary statistics
    fn get_diff_summary(repo: &Repository, commit: &Commit) -> Result<String> {
        let commit_tree = commit.tree().context("Failed to get commit tree")?;

        let parent_tree = if commit.parent_count() > 0 {
            Some(
                commit
                    .parent(0)
                    .context("Failed to get parent commit")?
                    .tree()
                    .context("Failed to get parent tree")?,
            )
        } else {
            None
        };

        let diff = if let Some(parent_tree) = parent_tree {
            repo.diff_tree_to_tree(Some(&parent_tree), Some(&commit_tree), None)
                .context("Failed to create diff")?
        } else {
            repo.diff_tree_to_tree(None, Some(&commit_tree), None)
                .context("Failed to create diff for initial commit")?
        };

        let stats = diff.stats().context("Failed to get diff stats")?;

        let mut summary = String::new();
        for i in 0..stats.files_changed() {
            if let Some(path) = diff
                .get_delta(i)
                .and_then(|d| d.new_file().path())
                .and_then(|p| p.to_str())
            {
                let insertions = stats.insertions();
                let deletions = stats.deletions();
                summary.push_str(&format!(
                    " {} | {} +{} -{}\n",
                    path,
                    insertions + deletions,
                    insertions,
                    deletions
                ));
            }
        }

        Ok(summary)
    }

    /// Get full diff content for the commit
    fn get_diff_content(repo: &Repository, commit: &Commit) -> Result<String> {
        let commit_tree = commit.tree().context("Failed to get commit tree")?;

        let parent_tree = if commit.parent_count() > 0 {
            Some(
                commit
                    .parent(0)
                    .context("Failed to get parent commit")?
                    .tree()
                    .context("Failed to get parent tree")?,
            )
        } else {
            None
        };

        let diff = if let Some(parent_tree) = parent_tree {
            repo.diff_tree_to_tree(Some(&parent_tree), Some(&commit_tree), None)
                .context("Failed to create diff")?
        } else {
            repo.diff_tree_to_tree(None, Some(&commit_tree), None)
                .context("Failed to create diff for initial commit")?
        };

        let mut diff_content = String::new();

        diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
            let content = std::str::from_utf8(line.content()).unwrap_or("<binary>");
            let prefix = match line.origin() {
                '+' => "+",
                '-' => "-",
                ' ' => " ",
                '@' => "@",
                'H' => "", // Header
                'F' => "", // File header
                _ => "",
            };
            diff_content.push_str(&format!("{}{}", prefix, content));
            true
        })
        .context("Failed to format diff")?;

        // Ensure the diff content ends with a newline to encourage literal block style
        if !diff_content.ends_with('\n') {
            diff_content.push('\n');
        }

        Ok(diff_content)
    }
}
