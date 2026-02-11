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

impl RepositoryView {
    /// Updates the present field for all field documentation entries based on actual data.
    pub fn update_field_presence(&mut self) {
        for field in &mut self.explanation.fields {
            field.present = match field.name.as_str() {
                "working_directory.clean" => true,             // Always present
                "working_directory.untracked_changes" => true, // Always present
                "remotes" => true,                             // Always present
                "commits[].hash" => !self.commits.is_empty(),
                "commits[].author" => !self.commits.is_empty(),
                "commits[].date" => !self.commits.is_empty(),
                "commits[].original_message" => !self.commits.is_empty(),
                "commits[].in_main_branches" => !self.commits.is_empty(),
                "commits[].analysis.detected_type" => !self.commits.is_empty(),
                "commits[].analysis.detected_scope" => !self.commits.is_empty(),
                "commits[].analysis.proposed_message" => !self.commits.is_empty(),
                "commits[].analysis.file_changes.total_files" => !self.commits.is_empty(),
                "commits[].analysis.file_changes.files_added" => !self.commits.is_empty(),
                "commits[].analysis.file_changes.files_deleted" => !self.commits.is_empty(),
                "commits[].analysis.file_changes.file_list" => !self.commits.is_empty(),
                "commits[].analysis.diff_summary" => !self.commits.is_empty(),
                "commits[].analysis.diff_file" => !self.commits.is_empty(),
                "versions.omni_dev" => self.versions.is_some(),
                "ai.scratch" => true,
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
}
