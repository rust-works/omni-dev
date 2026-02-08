//! Git commit operations and analysis

use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset};
use git2::{Commit, Repository};
use globset::Glob;
use serde::{Deserialize, Serialize};
use std::fs;

use regex::Regex;

use crate::data::context::ScopeDefinition;

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
    /// Path to diff file showing line-by-line changes
    pub diff_file: String,
}

/// Enhanced commit analysis for AI processing with full diff content
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitAnalysisForAI {
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
    /// Path to diff file showing line-by-line changes
    pub diff_file: String,
    /// Full diff content for AI analysis
    pub diff_content: String,
}

/// Commit information with enhanced analysis for AI processing
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitInfoForAI {
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
    /// Enhanced automated analysis of the commit including diff content
    pub analysis: CommitAnalysisForAI,
    /// Deterministic checks already performed; the LLM should treat these as authoritative
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pre_validated_checks: Vec<String>,
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

        // Write diff to file and get path
        let diff_file = Self::write_diff_to_file(repo, commit)?;

        Ok(Self {
            detected_type,
            detected_scope,
            proposed_message,
            file_changes,
            diff_summary,
            diff_file,
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

    /// Detect scope from file paths
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

    /// Re-detect scope using file_patterns from scope definitions.
    ///
    /// More specific patterns (more literal path components) win regardless of
    /// definition order in scopes.yaml. Equally specific matches are joined
    /// with ", ". If no scope definitions match, the existing detected_scope
    /// is kept as a fallback.
    pub fn refine_scope(&mut self, scope_defs: &[ScopeDefinition]) {
        if scope_defs.is_empty() {
            return;
        }
        let files: Vec<&str> = self
            .file_changes
            .file_list
            .iter()
            .map(|f| f.file.as_str())
            .collect();
        if files.is_empty() {
            return;
        }

        let mut matches: Vec<(&str, usize)> = Vec::new();
        for scope_def in scope_defs {
            if let Some(specificity) = Self::scope_matches_files(&files, &scope_def.file_patterns) {
                matches.push((&scope_def.name, specificity));
            }
        }

        if matches.is_empty() {
            return;
        }

        let max_specificity = matches.iter().map(|(_, s)| *s).max().unwrap();
        let best: Vec<&str> = matches
            .into_iter()
            .filter(|(_, s)| *s == max_specificity)
            .map(|(name, _)| name)
            .collect();

        self.detected_scope = best.join(", ");
    }

    /// Check if a scope's file_patterns match any of the given files.
    ///
    /// Returns `Some(max_specificity)` if at least one file matches the scope
    /// (after applying negation patterns), or `None` if no file matches.
    fn scope_matches_files(files: &[&str], patterns: &[String]) -> Option<usize> {
        let mut positive = Vec::new();
        let mut negative = Vec::new();
        for pat in patterns {
            if let Some(stripped) = pat.strip_prefix('!') {
                negative.push(stripped);
            } else {
                positive.push(pat.as_str());
            }
        }

        // Build negative matchers
        let neg_matchers: Vec<_> = negative
            .iter()
            .filter_map(|p| Glob::new(p).ok().map(|g| g.compile_matcher()))
            .collect();

        let mut max_specificity: Option<usize> = None;
        for pat in &positive {
            let glob = match Glob::new(pat) {
                Ok(g) => g,
                Err(_) => continue,
            };
            let matcher = glob.compile_matcher();
            for file in files {
                if matcher.is_match(file) && !neg_matchers.iter().any(|neg| neg.is_match(file)) {
                    let specificity = Self::count_specificity(pat);
                    max_specificity =
                        Some(max_specificity.map_or(specificity, |cur| cur.max(specificity)));
                }
            }
        }
        max_specificity
    }

    /// Count the number of literal (non-wildcard) path segments in a glob pattern.
    ///
    /// - `docs/adrs/**` → 2 (`docs`, `adrs`)
    /// - `docs/**` → 1 (`docs`)
    /// - `*.md` → 0
    /// - `src/main/scala/**` → 3
    fn count_specificity(pattern: &str) -> usize {
        pattern
            .split('/')
            .filter(|segment| !segment.contains('*') && !segment.contains('?'))
            .count()
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

    /// Write full diff content to a file and return the path
    fn write_diff_to_file(repo: &Repository, commit: &Commit) -> Result<String> {
        // Get AI scratch directory
        let ai_scratch_path = crate::utils::ai_scratch::get_ai_scratch_dir()
            .context("Failed to determine AI scratch directory")?;

        // Create diffs subdirectory
        let diffs_dir = ai_scratch_path.join("diffs");
        fs::create_dir_all(&diffs_dir).context("Failed to create diffs directory")?;

        // Create filename with commit hash
        let commit_hash = commit.id().to_string();
        let diff_filename = format!("{}.diff", commit_hash);
        let diff_path = diffs_dir.join(&diff_filename);

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

        // Write diff content to file
        fs::write(&diff_path, diff_content).context("Failed to write diff file")?;

        // Return the path as a string
        Ok(diff_path.to_string_lossy().to_string())
    }
}

impl CommitInfoForAI {
    /// Convert from basic CommitInfo by loading diff content
    pub fn from_commit_info(commit_info: CommitInfo) -> Result<Self> {
        let analysis = CommitAnalysisForAI::from_commit_analysis(commit_info.analysis)?;

        Ok(Self {
            hash: commit_info.hash,
            author: commit_info.author,
            date: commit_info.date,
            original_message: commit_info.original_message,
            in_main_branches: commit_info.in_main_branches,
            analysis,
            pre_validated_checks: Vec::new(),
        })
    }

    /// Run deterministic pre-validation checks on the commit message.
    /// Passing checks are recorded in pre_validated_checks so the LLM
    /// can skip re-checking them. Failing checks are not recorded.
    pub fn run_pre_validation_checks(&mut self) {
        let re = Regex::new(r"^[a-z]+!\(([^)]+)\):|^[a-z]+\(([^)]+)\):").unwrap();
        if let Some(caps) = re.captures(&self.original_message) {
            let scope = caps.get(1).or_else(|| caps.get(2)).map(|m| m.as_str());
            if let Some(scope) = scope {
                if scope.contains(',') && !scope.contains(", ") {
                    self.pre_validated_checks.push(format!(
                        "Scope format verified: multi-scope '{}' correctly uses commas without spaces",
                        scope
                    ));
                }
            }
        }
    }
}

impl CommitAnalysisForAI {
    /// Convert from basic CommitAnalysis by loading diff content from file
    pub fn from_commit_analysis(analysis: CommitAnalysis) -> Result<Self> {
        // Read the actual diff content from the file
        let diff_content = fs::read_to_string(&analysis.diff_file)
            .with_context(|| format!("Failed to read diff file: {}", analysis.diff_file))?;

        Ok(Self {
            detected_type: analysis.detected_type,
            detected_scope: analysis.detected_scope,
            proposed_message: analysis.proposed_message,
            file_changes: analysis.file_changes,
            diff_summary: analysis.diff_summary,
            diff_file: analysis.diff_file,
            diff_content,
        })
    }
}
