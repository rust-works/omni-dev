//! Data processing and serialization.

use serde::{Deserialize, Serialize};

use crate::git::{CommitInfo, CommitInfoForAI, RemoteInfo};

pub mod amendments;
pub mod check;
pub mod context;
pub mod yaml;

pub use amendments::*;
pub use check::*;
pub use context::*;
pub use yaml::*;

/// Complete repository view output structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryView {
    /// Version information for the omni-dev tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub versions: Option<VersionInfo>,
    /// Explanation of field meanings and structure.
    pub explanation: FieldExplanation,
    /// Working directory status information.
    pub working_directory: WorkingDirectoryInfo,
    /// List of remote repositories and their main branches.
    pub remotes: Vec<RemoteInfo>,
    /// AI-related information.
    pub ai: AiInfo,
    /// Branch information (only present when using branch commands).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_info: Option<BranchInfo>,
    /// Pull request template content (only present in branch commands when template exists).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_template: Option<String>,
    /// Location of the pull request template file (only present when pr_template exists).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_template_location: Option<String>,
    /// Pull requests created from the current branch (only present in branch commands).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_prs: Option<Vec<PullRequest>>,
    /// List of analyzed commits with metadata and analysis.
    pub commits: Vec<CommitInfo>,
}

/// Enhanced repository view for AI processing with full diff content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryViewForAI {
    /// Version information for the omni-dev tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub versions: Option<VersionInfo>,
    /// Explanation of field meanings and structure.
    pub explanation: FieldExplanation,
    /// Working directory status information.
    pub working_directory: WorkingDirectoryInfo,
    /// List of remote repositories and their main branches.
    pub remotes: Vec<RemoteInfo>,
    /// AI-related information.
    pub ai: AiInfo,
    /// Branch information (only present when using branch commands).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_info: Option<BranchInfo>,
    /// Pull request template content (only present in branch commands when template exists).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_template: Option<String>,
    /// Location of the pull request template file (only present when pr_template exists).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_template_location: Option<String>,
    /// Pull requests created from the current branch (only present in branch commands).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_prs: Option<Vec<PullRequest>>,
    /// List of analyzed commits with enhanced metadata including full diff content.
    pub commits: Vec<CommitInfoForAI>,
}

/// Field explanation for the YAML output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldExplanation {
    /// Descriptive text explaining the overall structure.
    pub text: String,
    /// Documentation for individual fields in the output.
    pub fields: Vec<FieldDocumentation>,
}

/// Individual field documentation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldDocumentation {
    /// Name of the field being documented.
    pub name: String,
    /// Descriptive text explaining what the field contains.
    pub text: String,
    /// Git command that corresponds to this field (if applicable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Whether this field is present in the current output.
    pub present: bool,
}

/// Working directory information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkingDirectoryInfo {
    /// Whether the working directory has no changes.
    pub clean: bool,
    /// List of files with uncommitted changes.
    pub untracked_changes: Vec<FileStatusInfo>,
}

/// File status information for working directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileStatusInfo {
    /// Git status flags (e.g., "AM", "??", "M ").
    pub status: String,
    /// Path to the file relative to repository root.
    pub file: String,
}

/// Version information for tools and environment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionInfo {
    /// Version of the omni-dev tool.
    pub omni_dev: String,
}

/// AI-related information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiInfo {
    /// Path to AI scratch directory.
    pub scratch: String,
}

/// Branch information for branch-specific commands.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchInfo {
    /// Current branch name.
    pub branch: String,
}

/// Pull request information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullRequest {
    /// PR number.
    pub number: u64,
    /// PR title.
    pub title: String,
    /// PR state (open, closed, merged).
    pub state: String,
    /// PR URL.
    pub url: String,
    /// PR description/body content.
    pub body: String,
    /// Base branch the PR targets.
    #[serde(default)]
    pub base: String,
}

/// Level of diff detail included in the AI prompt after budget fitting.
///
/// When a commit's diff exceeds the model's context window, the system
/// progressively reduces detail through these levels until the prompt fits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffDetail {
    /// Full diff content included.
    Full,
    /// Diff content truncated to fit budget.
    Truncated,
    /// Only `diff --stat` summary included (no line-level diff).
    StatOnly,
    /// Only file list included (no diff content or summary).
    FileListOnly,
}

impl std::fmt::Display for DiffDetail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => write!(f, "full diff"),
            Self::Truncated => write!(f, "truncated diff"),
            Self::StatOnly => write!(f, "stat summary only"),
            Self::FileListOnly => write!(f, "file list only"),
        }
    }
}

impl RepositoryView {
    /// Updates the present field for all field documentation entries based on actual data.
    pub fn update_field_presence(&mut self) {
        for field in &mut self.explanation.fields {
            field.present = match field.name.as_str() {
                "working_directory.clean"
                | "working_directory.untracked_changes"
                | "remotes"
                | "ai.scratch" => true, // Always present
                "commits[].hash"
                | "commits[].author"
                | "commits[].date"
                | "commits[].original_message"
                | "commits[].in_main_branches"
                | "commits[].analysis.detected_type"
                | "commits[].analysis.detected_scope"
                | "commits[].analysis.proposed_message"
                | "commits[].analysis.file_changes.total_files"
                | "commits[].analysis.file_changes.files_added"
                | "commits[].analysis.file_changes.files_deleted"
                | "commits[].analysis.file_changes.file_list"
                | "commits[].analysis.diff_summary"
                | "commits[].analysis.diff_file" => !self.commits.is_empty(),
                "versions.omni_dev" => self.versions.is_some(),
                "branch_info.branch" => self.branch_info.is_some(),
                "pr_template" => self.pr_template.is_some(),
                "pr_template_location" => self.pr_template_location.is_some(),
                "branch_prs" => self.branch_prs.is_some(),
                "branch_prs[].number" => {
                    self.branch_prs.as_ref().is_some_and(|prs| !prs.is_empty())
                }
                "branch_prs[].title" => self.branch_prs.as_ref().is_some_and(|prs| !prs.is_empty()),
                "branch_prs[].state" => self.branch_prs.as_ref().is_some_and(|prs| !prs.is_empty()),
                "branch_prs[].url" => self.branch_prs.as_ref().is_some_and(|prs| !prs.is_empty()),
                "branch_prs[].body" => self.branch_prs.as_ref().is_some_and(|prs| !prs.is_empty()),
                _ => false, // Unknown fields are not present
            }
        }
    }

    /// Creates a minimal view containing a single commit for parallel dispatch.
    ///
    /// Strips metadata not relevant to per-commit AI analysis (versions,
    /// working directory status, remotes, PR templates) to reduce prompt size.
    /// Only retains `branch_info` (for scope context) and the single commit.
    #[must_use]
    pub fn single_commit_view(&self, commit: &CommitInfo) -> Self {
        Self {
            versions: None,
            explanation: FieldExplanation {
                text: String::new(),
                fields: Vec::new(),
            },
            working_directory: WorkingDirectoryInfo {
                clean: true,
                untracked_changes: Vec::new(),
            },
            remotes: Vec::new(),
            ai: AiInfo {
                scratch: String::new(),
            },
            branch_info: self.branch_info.clone(),
            pr_template: None,
            pr_template_location: None,
            branch_prs: None,
            commits: vec![commit.clone()],
        }
    }

    /// Creates a minimal view containing multiple commits for batched dispatch.
    ///
    /// Same metadata stripping as [`single_commit_view`] but with N commits.
    /// Used by the batching system to group commits into a single AI request.
    #[must_use]
    pub(crate) fn multi_commit_view(&self, commits: &[&CommitInfo]) -> Self {
        Self {
            versions: None,
            explanation: FieldExplanation {
                text: String::new(),
                fields: Vec::new(),
            },
            working_directory: WorkingDirectoryInfo {
                clean: true,
                untracked_changes: Vec::new(),
            },
            remotes: Vec::new(),
            ai: AiInfo {
                scratch: String::new(),
            },
            branch_info: self.branch_info.clone(),
            pr_template: None,
            pr_template_location: None,
            branch_prs: None,
            commits: commits.iter().map(|c| (*c).clone()).collect(),
        }
    }
}

impl Default for FieldExplanation {
    /// Creates default field explanation.
    fn default() -> Self {
        Self {
            text: [
                "Field documentation for the YAML output format. Each entry describes the purpose and content of fields returned by the view command.",
                "",
                "Field structure:",
                "- name: Specifies the YAML field path",
                "- text: Provides a description of what the field contains",
                "- command: Shows the corresponding command used to obtain that data (if applicable)",
                "- present: Indicates whether this field is present in the current output",
                "",
                "IMPORTANT FOR AI ASSISTANTS: If a field shows present=true, it is guaranteed to be somewhere in this document. AI assistants should search the entire document thoroughly for any field marked as present=true, as it is definitely included in the output."
            ].join("\n"),
            fields: vec![
                FieldDocumentation {
                    name: "working_directory.clean".to_string(),
                    text: "Boolean indicating if the working directory has no uncommitted changes".to_string(),
                    command: Some("git status".to_string()),
                    present: false, // Will be set dynamically when creating output
                },
                FieldDocumentation {
                    name: "working_directory.untracked_changes".to_string(),
                    text: "Array of files with uncommitted changes, showing git status and file path".to_string(),
                    command: Some("git status --porcelain".to_string()),
                    present: false,
                },
                FieldDocumentation {
                    name: "remotes".to_string(),
                    text: "Array of git remotes with their URLs and detected main branch names".to_string(),
                    command: Some("git remote -v".to_string()),
                    present: false,
                },
                FieldDocumentation {
                    name: "commits[].hash".to_string(),
                    text: "Full SHA-1 hash of the commit".to_string(),
                    command: Some("git log --format=%H".to_string()),
                    present: false,
                },
                FieldDocumentation {
                    name: "commits[].author".to_string(),
                    text: "Commit author name and email address".to_string(),
                    command: Some("git log --format=%an <%ae>".to_string()),
                    present: false,
                },
                FieldDocumentation {
                    name: "commits[].date".to_string(),
                    text: "Commit date in ISO format with timezone".to_string(),
                    command: Some("git log --format=%aI".to_string()),
                    present: false,
                },
                FieldDocumentation {
                    name: "commits[].original_message".to_string(),
                    text: "The original commit message as written by the author".to_string(),
                    command: Some("git log --format=%B".to_string()),
                    present: false,
                },
                FieldDocumentation {
                    name: "commits[].in_main_branches".to_string(),
                    text: "Array of remote main branches that contain this commit (empty if not pushed)".to_string(),
                    command: Some("git branch -r --contains <commit>".to_string()),
                    present: false,
                },
                FieldDocumentation {
                    name: "commits[].analysis.detected_type".to_string(),
                    text: "Automatically detected conventional commit type (feat, fix, docs, test, chore, etc.)".to_string(),
                    command: None,
                    present: false,
                },
                FieldDocumentation {
                    name: "commits[].analysis.detected_scope".to_string(),
                    text: "Automatically detected scope based on file paths (commands, config, tests, etc.)".to_string(),
                    command: None,
                    present: false,
                },
                FieldDocumentation {
                    name: "commits[].analysis.proposed_message".to_string(),
                    text: "AI-generated conventional commit message based on file changes".to_string(),
                    command: None,
                    present: false,
                },
                FieldDocumentation {
                    name: "commits[].analysis.file_changes.total_files".to_string(),
                    text: "Total number of files modified in this commit".to_string(),
                    command: Some("git show --name-only <commit>".to_string()),
                    present: false,
                },
                FieldDocumentation {
                    name: "commits[].analysis.file_changes.files_added".to_string(),
                    text: "Number of new files added in this commit".to_string(),
                    command: Some("git show --name-status <commit> | grep '^A'".to_string()),
                    present: false,
                },
                FieldDocumentation {
                    name: "commits[].analysis.file_changes.files_deleted".to_string(),
                    text: "Number of files deleted in this commit".to_string(),
                    command: Some("git show --name-status <commit> | grep '^D'".to_string()),
                    present: false,
                },
                FieldDocumentation {
                    name: "commits[].analysis.file_changes.file_list".to_string(),
                    text: "Array of files changed with their git status (M=modified, A=added, D=deleted)".to_string(),
                    command: Some("git show --name-status <commit>".to_string()),
                    present: false,
                },
                FieldDocumentation {
                    name: "commits[].analysis.diff_summary".to_string(),
                    text: "Git diff --stat output showing lines changed per file".to_string(),
                    command: Some("git show --stat <commit>".to_string()),
                    present: false,
                },
                FieldDocumentation {
                    name: "commits[].analysis.diff_file".to_string(),
                    text: "Path to file containing full diff content showing line-by-line changes with added, removed, and context lines.\n\
                           AI assistants should read this file to understand the specific changes made in the commit.".to_string(),
                    command: Some("git show <commit>".to_string()),
                    present: false,
                },
                FieldDocumentation {
                    name: "versions.omni_dev".to_string(),
                    text: "Version of the omni-dev tool".to_string(),
                    command: Some("omni-dev --version".to_string()),
                    present: false,
                },
                FieldDocumentation {
                    name: "ai.scratch".to_string(),
                    text: "Path to AI scratch directory (controlled by AI_SCRATCH environment variable)".to_string(),
                    command: Some("echo $AI_SCRATCH".to_string()),
                    present: false,
                },
                FieldDocumentation {
                    name: "branch_info.branch".to_string(),
                    text: "Current branch name (only present in branch commands)".to_string(),
                    command: Some("git branch --show-current".to_string()),
                    present: false,
                },
                FieldDocumentation {
                    name: "pr_template".to_string(),
                    text: "Pull request template content from .github/pull_request_template.md (only present in branch commands when file exists)".to_string(),
                    command: None,
                    present: false,
                },
                FieldDocumentation {
                    name: "pr_template_location".to_string(),
                    text: "Location of the pull request template file (only present when pr_template exists)".to_string(),
                    command: None,
                    present: false,
                },
                FieldDocumentation {
                    name: "branch_prs".to_string(),
                    text: "Pull requests created from the current branch (only present in branch commands)".to_string(),
                    command: None,
                    present: false,
                },
                FieldDocumentation {
                    name: "branch_prs[].number".to_string(),
                    text: "Pull request number".to_string(),
                    command: None,
                    present: false,
                },
                FieldDocumentation {
                    name: "branch_prs[].title".to_string(),
                    text: "Pull request title".to_string(),
                    command: None,
                    present: false,
                },
                FieldDocumentation {
                    name: "branch_prs[].state".to_string(),
                    text: "Pull request state (open, closed, merged)".to_string(),
                    command: None,
                    present: false,
                },
                FieldDocumentation {
                    name: "branch_prs[].url".to_string(),
                    text: "Pull request URL".to_string(),
                    command: None,
                    present: false,
                },
                FieldDocumentation {
                    name: "branch_prs[].body".to_string(),
                    text: "Pull request description/body content".to_string(),
                    command: None,
                    present: false,
                },
            ],
        }
    }
}

/// Truncation marker appended when diff content is shortened to fit budget.
const DIFF_TRUNCATION_MARKER: &str = "\n\n[... diff truncated to fit model context window ...]\n";

/// Minimum characters to retain when truncating a diff.
/// Below this threshold, stat-only is preferred.
const MIN_TRUNCATED_DIFF_LEN: usize = 500;

impl RepositoryViewForAI {
    /// Converts from basic RepositoryView by loading diff content for all commits.
    pub fn from_repository_view(repo_view: RepositoryView) -> anyhow::Result<Self> {
        Self::from_repository_view_with_options(repo_view, false)
    }

    /// Converts from basic RepositoryView with options.
    ///
    /// If `fresh` is true, clears original commit messages to force AI to generate
    /// new messages based solely on the diff content.
    pub fn from_repository_view_with_options(
        repo_view: RepositoryView,
        fresh: bool,
    ) -> anyhow::Result<Self> {
        // Convert all commits to AI-enhanced versions
        let commits: anyhow::Result<Vec<_>> = repo_view
            .commits
            .into_iter()
            .map(|commit| {
                let mut ai_commit = CommitInfoForAI::from_commit_info(commit)?;
                if fresh {
                    ai_commit.original_message =
                        "(Original message hidden - generate fresh message from diff)".to_string();
                }
                Ok(ai_commit)
            })
            .collect();

        Ok(Self {
            versions: repo_view.versions,
            explanation: repo_view.explanation,
            working_directory: repo_view.working_directory,
            remotes: repo_view.remotes,
            ai: repo_view.ai,
            branch_info: repo_view.branch_info,
            pr_template: repo_view.pr_template,
            pr_template_location: repo_view.pr_template_location,
            branch_prs: repo_view.branch_prs,
            commits: commits?,
        })
    }

    /// Truncates diff content across all commits to remove approximately
    /// `excess_chars` characters total.
    ///
    /// Distributes cuts proportionally by each commit's diff size. Cuts at
    /// newline boundaries and appends a truncation marker. Commits whose
    /// remaining content would be below [`MIN_TRUNCATED_DIFF_LEN`] are skipped
    /// (the caller should fall through to [`replace_diffs_with_stat`]).
    pub(crate) fn truncate_diffs(&mut self, excess_chars: usize) {
        let total_diff_len: usize = self
            .commits
            .iter()
            .map(|c| c.analysis.diff_content.len())
            .sum();

        if total_diff_len == 0 {
            return;
        }

        for commit in &mut self.commits {
            let diff_len = commit.analysis.diff_content.len();
            if diff_len == 0 {
                continue;
            }

            // Proportional share of excess for this commit
            let share =
                ((diff_len as f64 / total_diff_len as f64) * excess_chars as f64).ceil() as usize;
            let target_len = diff_len.saturating_sub(share + DIFF_TRUNCATION_MARKER.len());

            if target_len < MIN_TRUNCATED_DIFF_LEN {
                // Would leave too little; caller should try stat-only instead.
                continue;
            }

            // Find nearest newline boundary at or before target_len (include the newline)
            let cut_point = commit.analysis.diff_content[..target_len]
                .rfind('\n')
                .map_or(target_len, |p| p + 1);

            commit.analysis.diff_content.truncate(cut_point);
            commit
                .analysis
                .diff_content
                .push_str(DIFF_TRUNCATION_MARKER);
        }
    }

    /// Replaces full diff content with the `diff --stat` summary for all commits.
    pub(crate) fn replace_diffs_with_stat(&mut self) {
        for commit in &mut self.commits {
            commit.analysis.diff_content = format!(
                "[diff replaced with stat summary to fit model context window]\n\n{}",
                commit.analysis.diff_summary
            );
        }
    }

    /// Removes all diff content, keeping only file list metadata.
    pub(crate) fn remove_diffs(&mut self) {
        for commit in &mut self.commits {
            commit.analysis.diff_content =
                "[diff content removed to fit model context window — only file list available]"
                    .to_string();
            commit.analysis.diff_summary = String::new();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::commit::FileChanges;
    use crate::git::{CommitAnalysisForAI, CommitInfoForAI};
    use chrono::Utc;

    fn make_commit(hash: &str, diff_content: &str, diff_summary: &str) -> CommitInfoForAI {
        CommitInfoForAI {
            hash: hash.to_string(),
            author: "Test <test@test.com>".to_string(),
            date: Utc::now().fixed_offset(),
            original_message: "test commit".to_string(),
            in_main_branches: Vec::new(),
            analysis: CommitAnalysisForAI {
                detected_type: "feat".to_string(),
                detected_scope: "test".to_string(),
                proposed_message: "feat(test): test".to_string(),
                file_changes: FileChanges {
                    total_files: 1,
                    files_added: 0,
                    files_deleted: 0,
                    file_list: Vec::new(),
                },
                diff_summary: diff_summary.to_string(),
                diff_file: "/tmp/test.diff".to_string(),
                diff_content: diff_content.to_string(),
            },
            pre_validated_checks: Vec::new(),
        }
    }

    fn make_view(commits: Vec<CommitInfoForAI>) -> RepositoryViewForAI {
        RepositoryViewForAI {
            versions: None,
            explanation: FieldExplanation::default(),
            working_directory: WorkingDirectoryInfo {
                clean: true,
                untracked_changes: Vec::new(),
            },
            remotes: Vec::new(),
            ai: AiInfo {
                scratch: String::new(),
            },
            branch_info: None,
            pr_template: None,
            pr_template_location: None,
            branch_prs: None,
            commits,
        }
    }

    #[test]
    fn truncate_diffs_at_newline_boundary() {
        // Create a diff with ~2000 chars across many lines (well above MIN_TRUNCATED_DIFF_LEN)
        let lines: Vec<String> = (0..100)
            .map(|i| format!("+line {i:03} with some padding content here\n"))
            .collect();
        let diff_content = lines.join("");
        let original_len = diff_content.len();
        assert!(
            original_len > 2000,
            "test diff should be large: {original_len}"
        );

        let commit = make_commit("abc123", &diff_content, "file.rs | 100 +++");
        let mut view = make_view(vec![commit]);

        // Remove ~500 chars
        view.truncate_diffs(500);

        let result = &view.commits[0].analysis.diff_content;
        // Should be shorter than original
        assert!(result.len() < original_len);
        // Should end with truncation marker
        assert!(result.contains("[... diff truncated to fit model context window ...]"));
        // Content before marker should end at a newline boundary
        let before_marker = result.split("\n\n[...").next().unwrap();
        assert!(before_marker.ends_with('\n'));
    }

    #[test]
    fn truncate_diffs_skips_when_remainder_too_small() {
        // Create a diff with exactly 600 chars
        let diff_content = "x".repeat(600);

        let commit = make_commit("abc123", &diff_content, "file.rs | 1 +");
        let mut view = make_view(vec![commit]);

        // Try to remove 500 chars — would leave only ~100 chars < MIN_TRUNCATED_DIFF_LEN
        view.truncate_diffs(500);

        // Should be left unchanged since remainder < 500
        assert_eq!(view.commits[0].analysis.diff_content.len(), 600);
    }

    #[test]
    fn truncate_diffs_proportional_multi_commit() {
        // Two commits: one with 1000 chars, one with 3000 chars
        let small_diff = "a\n".repeat(500); // ~1000 chars
        let large_diff = "b\n".repeat(1500); // ~3000 chars

        let c1 = make_commit("aaa", &small_diff, "small.rs | 1 +");
        let c2 = make_commit("bbb", &large_diff, "large.rs | 3 +++");
        let mut view = make_view(vec![c1, c2]);

        let orig_small = view.commits[0].analysis.diff_content.len();
        let orig_large = view.commits[1].analysis.diff_content.len();

        // Remove 1000 chars total — should remove ~250 from small, ~750 from large
        view.truncate_diffs(1000);

        let new_small = view.commits[0].analysis.diff_content.len();
        let new_large = view.commits[1].analysis.diff_content.len();

        // Both should be reduced
        assert!(new_small < orig_small);
        assert!(new_large < orig_large);
        // Large commit should be reduced more
        assert!(orig_large - new_large > orig_small - new_small);
    }

    #[test]
    fn replace_diffs_with_stat_preserves_summary() {
        let commit = make_commit(
            "abc123",
            "full diff content here",
            " file.rs | 10 +++++++---",
        );
        let mut view = make_view(vec![commit]);

        view.replace_diffs_with_stat();

        let result = &view.commits[0].analysis.diff_content;
        assert!(result.contains("stat summary"));
        assert!(result.contains("file.rs | 10 +++++++---"));
        assert!(!result.contains("full diff content here"));
    }

    #[test]
    fn remove_diffs_clears_content_and_summary() {
        let commit = make_commit("abc123", "full diff", "file.rs | 1 +");
        let mut view = make_view(vec![commit]);

        view.remove_diffs();

        let result = &view.commits[0].analysis.diff_content;
        assert!(result.contains("only file list available"));
        assert!(!result.contains("full diff"));
        assert!(view.commits[0].analysis.diff_summary.is_empty());
    }

    #[test]
    fn truncate_diffs_empty_diff_noop() {
        let commit = make_commit("abc123", "", "");
        let mut view = make_view(vec![commit]);

        view.truncate_diffs(1000);

        assert!(view.commits[0].analysis.diff_content.is_empty());
    }

    #[test]
    fn diff_detail_display() {
        assert_eq!(DiffDetail::Full.to_string(), "full diff");
        assert_eq!(DiffDetail::Truncated.to_string(), "truncated diff");
        assert_eq!(DiffDetail::StatOnly.to_string(), "stat summary only");
        assert_eq!(DiffDetail::FileListOnly.to_string(), "file list only");
    }
}
