//! Git commit operations and analysis.

use std::fs;
use std::sync::LazyLock;

use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset};
use git2::{Commit, Repository};
use globset::Glob;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::data::context::ScopeDefinition;

/// Matches conventional commit scope patterns including breaking-change syntax.
#[allow(clippy::unwrap_used)] // Compile-time constant regex pattern
static SCOPE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z]+!\(([^)]+)\):|^[a-z]+\(([^)]+)\):").unwrap());

/// Commit information structure, generic over analysis type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitInfo<A = CommitAnalysis> {
    /// Full SHA-1 hash of the commit.
    pub hash: String,
    /// Commit author name and email address.
    pub author: String,
    /// Commit date in ISO format with timezone.
    pub date: DateTime<FixedOffset>,
    /// The original commit message as written by the author.
    pub original_message: String,
    /// Array of remote main branches that contain this commit.
    pub in_main_branches: Vec<String>,
    /// Automated analysis of the commit including type detection and proposed message.
    pub analysis: A,
}

/// Commit analysis information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitAnalysis {
    /// Automatically detected conventional commit type (feat, fix, docs, test, chore, etc.).
    pub detected_type: String,
    /// Automatically detected scope based on file paths (cli, git, data, etc.).
    pub detected_scope: String,
    /// AI-generated conventional commit message based on file changes.
    pub proposed_message: String,
    /// Detailed statistics about file changes in this commit.
    pub file_changes: FileChanges,
    /// Git diff --stat output showing lines changed per file.
    pub diff_summary: String,
    /// Path to diff file showing line-by-line changes.
    pub diff_file: String,
}

/// Enhanced commit analysis for AI processing with full diff content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitAnalysisForAI {
    /// Base commit analysis fields.
    #[serde(flatten)]
    pub base: CommitAnalysis,
    /// Full diff content for AI analysis.
    pub diff_content: String,
}

/// Commit information with enhanced analysis for AI processing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitInfoForAI {
    /// Base commit information with AI-enhanced analysis.
    #[serde(flatten)]
    pub base: CommitInfo<CommitAnalysisForAI>,
    /// Deterministic checks already performed; the LLM should treat these as authoritative.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pre_validated_checks: Vec<String>,
}

/// File changes statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChanges {
    /// Total number of files modified in this commit.
    pub total_files: usize,
    /// Number of new files added in this commit.
    pub files_added: usize,
    /// Number of files deleted in this commit.
    pub files_deleted: usize,
    /// Array of files changed with their git status (M=modified, A=added, D=deleted).
    pub file_list: Vec<FileChange>,
}

/// Individual file change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChange {
    /// Git status code (A=added, M=modified, D=deleted, R=renamed).
    pub status: String,
    /// Path to the file relative to repository root.
    pub file: String,
}

impl CommitInfo {
    /// Creates a `CommitInfo` from a `git2::Commit`.
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
                #[allow(clippy::unwrap_used)] // Offset 0 is always valid
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
    /// Analyzes a commit and generates analysis information.
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

    /// Analyzes file changes in the commit.
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

    /// Detects conventional commit type based on files and existing message.
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

    /// Extracts conventional commit type from an existing message.
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

    /// Checks if a string is a valid conventional commit type.
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

    /// Detects scope from file paths.
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
            String::new()
        }
    }

    /// Re-detects scope using file_patterns from scope definitions.
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

        // SAFETY: matches is non-empty (guarded by early return above)
        #[allow(clippy::expect_used)] // Guarded by is_empty() check above
        let max_specificity = matches.iter().map(|(_, s)| *s).max().expect("non-empty");
        let best: Vec<&str> = matches
            .into_iter()
            .filter(|(_, s)| *s == max_specificity)
            .map(|(name, _)| name)
            .collect();

        self.detected_scope = best.join(", ");
    }

    /// Checks if a scope's file_patterns match any of the given files.
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
            let Ok(glob) = Glob::new(pat) else {
                continue;
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

    /// Counts the number of literal (non-wildcard) path segments in a glob pattern.
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

    /// Generates a proposed conventional commit message.
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
            format!("{commit_type}: {description}")
        } else {
            format!("{commit_type}({scope}): {description}")
        }
    }

    /// Generates a description based on commit type and changes.
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

    /// Returns diff summary statistics.
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

    /// Writes full diff content to a file and returns the path.
    fn write_diff_to_file(repo: &Repository, commit: &Commit) -> Result<String> {
        // Get AI scratch directory
        let ai_scratch_path = crate::utils::ai_scratch::get_ai_scratch_dir()
            .context("Failed to determine AI scratch directory")?;

        // Create diffs subdirectory
        let diffs_dir = ai_scratch_path.join("diffs");
        fs::create_dir_all(&diffs_dir).context("Failed to create diffs directory")?;

        // Create filename with commit hash
        let commit_hash = commit.id().to_string();
        let diff_filename = format!("{commit_hash}.diff");
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
                _ => "", // Header, file header, and other origins
            };
            diff_content.push_str(&format!("{prefix}{content}"));
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
    /// Converts from a basic `CommitInfo` by loading diff content.
    pub fn from_commit_info(commit_info: CommitInfo) -> Result<Self> {
        let analysis = CommitAnalysisForAI::from_commit_analysis(commit_info.analysis)?;

        Ok(Self {
            base: CommitInfo {
                hash: commit_info.hash,
                author: commit_info.author,
                date: commit_info.date,
                original_message: commit_info.original_message,
                in_main_branches: commit_info.in_main_branches,
                analysis,
            },
            pre_validated_checks: Vec::new(),
        })
    }

    /// Runs deterministic pre-validation checks on the commit message.
    /// Passing checks are recorded in pre_validated_checks so the LLM
    /// can skip re-checking them. Failing checks are not recorded.
    pub fn run_pre_validation_checks(&mut self, valid_scopes: &[ScopeDefinition]) {
        if let Some(caps) = SCOPE_RE.captures(&self.base.original_message) {
            let scope = caps.get(1).or_else(|| caps.get(2)).map(|m| m.as_str());
            if let Some(scope) = scope {
                if scope.contains(',') && !scope.contains(", ") {
                    self.pre_validated_checks.push(format!(
                        "Scope format verified: multi-scope '{scope}' correctly uses commas without spaces"
                    ));
                }

                // Deterministic scope validity check
                if !valid_scopes.is_empty() {
                    let scope_parts: Vec<&str> = scope.split(',').collect();
                    let all_valid = scope_parts
                        .iter()
                        .all(|part| valid_scopes.iter().any(|s| s.name == *part));
                    if all_valid {
                        self.pre_validated_checks.push(format!(
                            "Scope validity verified: '{scope}' is in the valid scopes list"
                        ));
                    }
                }
            }
        }
    }
}

impl CommitAnalysisForAI {
    /// Converts from a basic `CommitAnalysis` by loading diff content from file.
    pub fn from_commit_analysis(analysis: CommitAnalysis) -> Result<Self> {
        // Read the actual diff content from the file
        let diff_content = fs::read_to_string(&analysis.diff_file)
            .with_context(|| format!("Failed to read diff file: {}", analysis.diff_file))?;

        Ok(Self {
            base: analysis,
            diff_content,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::data::context::ScopeDefinition;

    // ── extract_conventional_type ────────────────────────────────────

    #[test]
    fn conventional_type_feat_with_scope() {
        assert_eq!(
            CommitAnalysis::extract_conventional_type("feat(cli): add flag"),
            Some("feat".to_string())
        );
    }

    #[test]
    fn conventional_type_without_scope() {
        assert_eq!(
            CommitAnalysis::extract_conventional_type("fix: resolve bug"),
            Some("fix".to_string())
        );
    }

    #[test]
    fn conventional_type_invalid_message() {
        assert_eq!(
            CommitAnalysis::extract_conventional_type("random message without colon"),
            None
        );
    }

    #[test]
    fn conventional_type_unknown_type() {
        assert_eq!(
            CommitAnalysis::extract_conventional_type("yolo(scope): stuff"),
            None
        );
    }

    #[test]
    fn conventional_type_all_valid_types() {
        let types = [
            "feat", "fix", "docs", "style", "refactor", "test", "chore", "build", "ci", "perf",
        ];
        for t in types {
            let msg = format!("{t}: description");
            assert_eq!(
                CommitAnalysis::extract_conventional_type(&msg),
                Some(t.to_string()),
                "expected Some for type '{t}'"
            );
        }
    }

    // ── is_valid_conventional_type ───────────────────────────────────

    #[test]
    fn valid_conventional_types() {
        for t in [
            "feat", "fix", "docs", "style", "refactor", "test", "chore", "build", "ci", "perf",
        ] {
            assert!(
                CommitAnalysis::is_valid_conventional_type(t),
                "'{t}' should be valid"
            );
        }
    }

    #[test]
    fn invalid_conventional_types() {
        for t in ["yolo", "Feat", "", "FEAT", "feature", "bugfix"] {
            assert!(
                !CommitAnalysis::is_valid_conventional_type(t),
                "'{t}' should be invalid"
            );
        }
    }

    // ── detect_scope ─────────────────────────────────────────────────

    fn make_file_changes(files: &[(&str, &str)]) -> FileChanges {
        FileChanges {
            total_files: files.len(),
            files_added: files.iter().filter(|(s, _)| *s == "A").count(),
            files_deleted: files.iter().filter(|(s, _)| *s == "D").count(),
            file_list: files
                .iter()
                .map(|(status, file)| FileChange {
                    status: (*status).to_string(),
                    file: (*file).to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn scope_from_cli_files() {
        let changes = make_file_changes(&[("M", "src/cli/commands.rs")]);
        assert_eq!(CommitAnalysis::detect_scope(&changes), "cli");
    }

    #[test]
    fn scope_from_git_files() {
        let changes = make_file_changes(&[("M", "src/git/remote.rs")]);
        assert_eq!(CommitAnalysis::detect_scope(&changes), "git");
    }

    #[test]
    fn scope_from_docs_files() {
        let changes = make_file_changes(&[("M", "docs/README.md")]);
        assert_eq!(CommitAnalysis::detect_scope(&changes), "docs");
    }

    #[test]
    fn scope_from_data_files() {
        let changes = make_file_changes(&[("M", "src/data/yaml.rs")]);
        assert_eq!(CommitAnalysis::detect_scope(&changes), "data");
    }

    #[test]
    fn scope_from_test_files() {
        let changes = make_file_changes(&[("A", "tests/new_test.rs")]);
        assert_eq!(CommitAnalysis::detect_scope(&changes), "test");
    }

    #[test]
    fn scope_from_deps_files() {
        let changes = make_file_changes(&[("M", "Cargo.toml")]);
        assert_eq!(CommitAnalysis::detect_scope(&changes), "deps");
    }

    #[test]
    fn scope_unknown_files() {
        let changes = make_file_changes(&[("M", "random/path/file.txt")]);
        assert_eq!(CommitAnalysis::detect_scope(&changes), "");
    }

    // ── count_specificity ────────────────────────────────────────────

    #[test]
    fn count_specificity_deep_path() {
        assert_eq!(CommitAnalysis::count_specificity("src/main/scala/**"), 3);
    }

    #[test]
    fn count_specificity_shallow() {
        assert_eq!(CommitAnalysis::count_specificity("docs/**"), 1);
    }

    #[test]
    fn count_specificity_wildcard_only() {
        assert_eq!(CommitAnalysis::count_specificity("*.md"), 0);
    }

    #[test]
    fn count_specificity_no_wildcards() {
        assert_eq!(CommitAnalysis::count_specificity("src/lib.rs"), 2);
    }

    // ── scope_matches_files ──────────────────────────────────────────

    #[test]
    fn scope_matches_positive_patterns() {
        let patterns = vec!["src/cli/**".to_string()];
        let files = &["src/cli/commands.rs"];
        assert!(CommitAnalysis::scope_matches_files(files, &patterns).is_some());
    }

    #[test]
    fn scope_matches_no_match() {
        let patterns = vec!["src/cli/**".to_string()];
        let files = &["src/git/remote.rs"];
        assert!(CommitAnalysis::scope_matches_files(files, &patterns).is_none());
    }

    #[test]
    fn scope_matches_with_negation() {
        let patterns = vec!["src/**".to_string(), "!src/test/**".to_string()];
        // File in src/ but not in src/test/ should match
        let files = &["src/lib.rs"];
        assert!(CommitAnalysis::scope_matches_files(files, &patterns).is_some());

        // File in src/test/ should be excluded
        let test_files = &["src/test/helper.rs"];
        assert!(CommitAnalysis::scope_matches_files(test_files, &patterns).is_none());
    }

    // ── refine_scope ─────────────────────────────────────────────────

    fn make_scope_def(name: &str, patterns: &[&str]) -> ScopeDefinition {
        ScopeDefinition {
            name: name.to_string(),
            description: String::new(),
            examples: vec![],
            file_patterns: patterns.iter().map(|p| (*p).to_string()).collect(),
        }
    }

    #[test]
    fn refine_scope_empty_defs() {
        let mut analysis = CommitAnalysis {
            detected_type: "feat".to_string(),
            detected_scope: "original".to_string(),
            proposed_message: String::new(),
            file_changes: make_file_changes(&[("M", "src/cli/commands.rs")]),
            diff_summary: String::new(),
            diff_file: String::new(),
        };
        analysis.refine_scope(&[]);
        assert_eq!(analysis.detected_scope, "original");
    }

    #[test]
    fn refine_scope_most_specific_wins() {
        let scope_defs = vec![
            make_scope_def("lib", &["src/**"]),
            make_scope_def("cli", &["src/cli/**"]),
        ];
        let mut analysis = CommitAnalysis {
            detected_type: "feat".to_string(),
            detected_scope: String::new(),
            proposed_message: String::new(),
            file_changes: make_file_changes(&[("M", "src/cli/commands.rs")]),
            diff_summary: String::new(),
            diff_file: String::new(),
        };
        analysis.refine_scope(&scope_defs);
        assert_eq!(analysis.detected_scope, "cli");
    }

    #[test]
    fn refine_scope_no_matching_files() {
        let scope_defs = vec![make_scope_def("cli", &["src/cli/**"])];
        let mut analysis = CommitAnalysis {
            detected_type: "feat".to_string(),
            detected_scope: "original".to_string(),
            proposed_message: String::new(),
            file_changes: make_file_changes(&[("M", "README.md")]),
            diff_summary: String::new(),
            diff_file: String::new(),
        };
        analysis.refine_scope(&scope_defs);
        // No match → keeps original
        assert_eq!(analysis.detected_scope, "original");
    }

    #[test]
    fn refine_scope_equal_specificity_joins() {
        let scope_defs = vec![
            make_scope_def("cli", &["src/cli/**"]),
            make_scope_def("git", &["src/git/**"]),
        ];
        let mut analysis = CommitAnalysis {
            detected_type: "feat".to_string(),
            detected_scope: String::new(),
            proposed_message: String::new(),
            file_changes: make_file_changes(&[
                ("M", "src/cli/commands.rs"),
                ("M", "src/git/remote.rs"),
            ]),
            diff_summary: String::new(),
            diff_file: String::new(),
        };
        analysis.refine_scope(&scope_defs);
        // Both have specificity 2 and both match → joined
        assert!(
            analysis.detected_scope == "cli, git" || analysis.detected_scope == "git, cli",
            "expected joined scopes, got: {}",
            analysis.detected_scope
        );
    }

    // ── run_pre_validation_checks ────────────────────────────────────

    fn make_commit_info_for_ai(message: &str) -> CommitInfoForAI {
        CommitInfoForAI {
            base: CommitInfo {
                hash: "a".repeat(40),
                author: "Test <test@example.com>".to_string(),
                date: chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00+00:00").unwrap(),
                original_message: message.to_string(),
                in_main_branches: vec![],
                analysis: CommitAnalysisForAI {
                    base: CommitAnalysis {
                        detected_type: "feat".to_string(),
                        detected_scope: String::new(),
                        proposed_message: String::new(),
                        file_changes: make_file_changes(&[]),
                        diff_summary: String::new(),
                        diff_file: String::new(),
                    },
                    diff_content: String::new(),
                },
            },
            pre_validated_checks: vec![],
        }
    }

    #[test]
    fn pre_validation_valid_single_scope() {
        let scopes = vec![make_scope_def("cli", &["src/cli/**"])];
        let mut info = make_commit_info_for_ai("feat(cli): add command");
        info.run_pre_validation_checks(&scopes);
        assert!(
            info.pre_validated_checks
                .iter()
                .any(|c| c.contains("Scope validity verified")),
            "expected scope validity check, got: {:?}",
            info.pre_validated_checks
        );
    }

    #[test]
    fn pre_validation_multi_scope() {
        let scopes = vec![
            make_scope_def("cli", &["src/cli/**"]),
            make_scope_def("git", &["src/git/**"]),
        ];
        let mut info = make_commit_info_for_ai("feat(cli,git): cross-cutting change");
        info.run_pre_validation_checks(&scopes);
        assert!(info
            .pre_validated_checks
            .iter()
            .any(|c| c.contains("Scope validity verified")),);
        assert!(info
            .pre_validated_checks
            .iter()
            .any(|c| c.contains("multi-scope")),);
    }

    #[test]
    fn pre_validation_invalid_scope_not_added() {
        let scopes = vec![make_scope_def("cli", &["src/cli/**"])];
        let mut info = make_commit_info_for_ai("feat(unknown): something");
        info.run_pre_validation_checks(&scopes);
        assert!(
            !info
                .pre_validated_checks
                .iter()
                .any(|c| c.contains("Scope validity verified")),
            "should not validate unknown scope"
        );
    }

    #[test]
    fn pre_validation_no_scope_message() {
        let scopes = vec![make_scope_def("cli", &["src/cli/**"])];
        let mut info = make_commit_info_for_ai("feat: no scope here");
        info.run_pre_validation_checks(&scopes);
        assert!(info.pre_validated_checks.is_empty());
    }

    // ── property tests ────────────────────────────────────────────

    mod prop {
        use super::*;
        use proptest::prelude::*;

        fn arb_conventional_type() -> impl Strategy<Value = &'static str> {
            prop_oneof![
                Just("feat"),
                Just("fix"),
                Just("docs"),
                Just("style"),
                Just("refactor"),
                Just("test"),
                Just("chore"),
                Just("build"),
                Just("ci"),
                Just("perf"),
            ]
        }

        proptest! {
            #[test]
            fn valid_conventional_format_extracts_type(
                ctype in arb_conventional_type(),
                scope in "[a-z]{1,10}",
                desc in "[a-zA-Z ]{1,50}",
            ) {
                let message = format!("{ctype}({scope}): {desc}");
                let result = CommitAnalysis::extract_conventional_type(&message);
                prop_assert_eq!(result, Some(ctype.to_string()));
            }

            #[test]
            fn no_colon_returns_none(s in "[^:]{0,100}") {
                let result = CommitAnalysis::extract_conventional_type(&s);
                prop_assert!(result.is_none());
            }

            #[test]
            fn count_specificity_nonnegative(pattern in ".*") {
                // usize is always >= 0; this test catches panics on arbitrary input
                let _ = CommitAnalysis::count_specificity(&pattern);
            }

            #[test]
            fn count_specificity_bounded_by_segments(
                segments in proptest::collection::vec("[a-z*?]{1,10}", 1..6),
            ) {
                let pattern = segments.join("/");
                let result = CommitAnalysis::count_specificity(&pattern);
                prop_assert!(result <= segments.len());
            }
        }
    }

    // ── conversion tests ────────────────────────────────────────────

    #[test]
    fn from_commit_analysis_loads_diff_content() {
        let dir = tempfile::tempdir().unwrap();
        let diff_path = dir.path().join("test.diff");
        std::fs::write(&diff_path, "+added line\n-removed line\n").unwrap();

        let analysis = CommitAnalysis {
            detected_type: "feat".to_string(),
            detected_scope: "cli".to_string(),
            proposed_message: "feat(cli): test".to_string(),
            file_changes: make_file_changes(&[]),
            diff_summary: "file.rs | 2 +-".to_string(),
            diff_file: diff_path.to_string_lossy().to_string(),
        };

        let ai = CommitAnalysisForAI::from_commit_analysis(analysis.clone()).unwrap();
        assert_eq!(ai.diff_content, "+added line\n-removed line\n");
        assert_eq!(ai.base.detected_type, analysis.detected_type);
        assert_eq!(ai.base.diff_file, analysis.diff_file);
    }

    #[test]
    fn from_commit_info_wraps_and_loads_diff() {
        let dir = tempfile::tempdir().unwrap();
        let diff_path = dir.path().join("test.diff");
        std::fs::write(&diff_path, "diff content").unwrap();

        let info = CommitInfo {
            hash: "a".repeat(40),
            author: "Test <test@example.com>".to_string(),
            date: chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00+00:00").unwrap(),
            original_message: "feat(cli): add flag".to_string(),
            in_main_branches: vec!["origin/main".to_string()],
            analysis: CommitAnalysis {
                detected_type: "feat".to_string(),
                detected_scope: "cli".to_string(),
                proposed_message: "feat(cli): add flag".to_string(),
                file_changes: make_file_changes(&[("M", "src/cli.rs")]),
                diff_summary: "cli.rs | 1 +".to_string(),
                diff_file: diff_path.to_string_lossy().to_string(),
            },
        };

        let ai = CommitInfoForAI::from_commit_info(info).unwrap();
        assert_eq!(ai.base.analysis.diff_content, "diff content");
        assert_eq!(ai.base.hash, "a".repeat(40));
        assert_eq!(ai.base.original_message, "feat(cli): add flag");
        assert!(ai.pre_validated_checks.is_empty());
    }
}
