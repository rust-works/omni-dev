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

/// Complete repository view output structure, generic over commit type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryView<C = CommitInfo> {
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
    pub commits: Vec<C>,
}

/// Enhanced repository view for AI processing with full diff content.
pub type RepositoryViewForAI = RepositoryView<CommitInfoForAI>;

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
                | "commits[].analysis.diff_file"
                | "commits[].analysis.file_diffs"
                | "commits[].analysis.file_diffs[].path"
                | "commits[].analysis.file_diffs[].diff_file"
                | "commits[].analysis.file_diffs[].byte_len" => !self.commits.is_empty(),
                "versions.omni_dev" => self.versions.is_some(),
                "branch_info.branch" => self.branch_info.is_some(),
                "pr_template" => self.pr_template.is_some(),
                "pr_template_location" => self.pr_template_location.is_some(),
                "branch_prs" => self.branch_prs.is_some(),
                "branch_prs[].number"
                | "branch_prs[].title"
                | "branch_prs[].state"
                | "branch_prs[].url"
                | "branch_prs[].body"
                | "branch_prs[].base" => {
                    self.branch_prs.as_ref().is_some_and(|prs| !prs.is_empty())
                }
                _ => false, // Unknown fields are not present
            }
        }
    }

    /// Serializes this view to YAML, calling [`update_field_presence`] first.
    ///
    /// Use this instead of calling `update_field_presence` followed by
    /// `crate::data::to_yaml` separately.  Keeping the two steps together
    /// prevents the explanation section from being stale in the output.
    ///
    /// [`update_field_presence`]: Self::update_field_presence
    pub fn to_yaml_output(&mut self) -> anyhow::Result<String> {
        self.update_field_presence();
        yaml::to_yaml(self)
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
                    name: "commits[].analysis.file_diffs".to_string(),
                    text: "Array of per-file diff references, each containing the file path, \
                           absolute path to the diff file on disk, and byte length of the diff content.\n\
                           AI assistants can use these to analyze individual file changes without loading the full diff."
                        .to_string(),
                    command: None,
                    present: false,
                },
                FieldDocumentation {
                    name: "commits[].analysis.file_diffs[].path".to_string(),
                    text: "Repository-relative path of the changed file.".to_string(),
                    command: None,
                    present: false,
                },
                FieldDocumentation {
                    name: "commits[].analysis.file_diffs[].diff_file".to_string(),
                    text: "Absolute path to the per-file diff file on disk.".to_string(),
                    command: None,
                    present: false,
                },
                FieldDocumentation {
                    name: "commits[].analysis.file_diffs[].byte_len".to_string(),
                    text: "Byte length of the per-file diff content.".to_string(),
                    command: None,
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
                FieldDocumentation {
                    name: "branch_prs[].base".to_string(),
                    text: "Base branch the pull request targets".to_string(),
                    command: None,
                    present: false,
                },
            ],
        }
    }
}

impl<C> RepositoryView<C> {
    /// Transforms commits while preserving all other fields.
    pub fn map_commits<D>(
        self,
        f: impl FnMut(C) -> anyhow::Result<D>,
    ) -> anyhow::Result<RepositoryView<D>> {
        let commits: anyhow::Result<Vec<D>> = self.commits.into_iter().map(f).collect();
        Ok(RepositoryView {
            versions: self.versions,
            explanation: self.explanation,
            working_directory: self.working_directory,
            remotes: self.remotes,
            ai: self.ai,
            branch_info: self.branch_info,
            pr_template: self.pr_template,
            pr_template_location: self.pr_template_location,
            branch_prs: self.branch_prs,
            commits: commits?,
        })
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
        repo_view.map_commits(|commit| {
            let mut ai_commit = CommitInfoForAI::from_commit_info(commit)?;
            if fresh {
                ai_commit.base.original_message =
                    "(Original message hidden - generate fresh message from diff)".to_string();
            }
            Ok(ai_commit)
        })
    }

    /// Creates a minimal AI view containing a single commit for split dispatch.
    ///
    /// Analogous to [`RepositoryView::single_commit_view`] but operates on
    /// the AI-enhanced type. Strips metadata not relevant to per-commit
    /// analysis to reduce prompt size.
    #[must_use]
    pub(crate) fn single_commit_view_for_ai(&self, commit: &CommitInfoForAI) -> Self {
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
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::git::commit::FileChanges;
    use crate::git::{CommitAnalysis, CommitInfo};
    use chrono::Utc;

    // ── update_field_presence ────────────────────────────────────────

    fn make_repo_view(commits: Vec<crate::git::CommitInfo>) -> RepositoryView {
        RepositoryView {
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

    fn field_present(view: &RepositoryView, name: &str) -> Option<bool> {
        view.explanation
            .fields
            .iter()
            .find(|f| f.name == name)
            .map(|f| f.present)
    }

    #[test]
    fn field_presence_no_commits() {
        let mut view = make_repo_view(vec![]);
        view.update_field_presence();

        // Always-present fields
        assert_eq!(field_present(&view, "working_directory.clean"), Some(true));
        assert_eq!(field_present(&view, "remotes"), Some(true));
        assert_eq!(field_present(&view, "ai.scratch"), Some(true));

        // Commit-dependent fields
        assert_eq!(field_present(&view, "commits[].hash"), Some(false));
        assert_eq!(
            field_present(&view, "commits[].analysis.detected_type"),
            Some(false)
        );

        // Optional fields
        assert_eq!(field_present(&view, "versions.omni_dev"), Some(false));
        assert_eq!(field_present(&view, "branch_info.branch"), Some(false));
        assert_eq!(field_present(&view, "pr_template"), Some(false));
        assert_eq!(field_present(&view, "branch_prs"), Some(false));
    }

    #[test]
    fn field_presence_with_versions() {
        let mut view = make_repo_view(vec![]);
        view.versions = Some(VersionInfo {
            omni_dev: "1.0.0".to_string(),
        });
        view.update_field_presence();

        assert_eq!(field_present(&view, "versions.omni_dev"), Some(true));
    }

    #[test]
    fn field_presence_with_branch_info() {
        let mut view = make_repo_view(vec![]);
        view.branch_info = Some(BranchInfo {
            branch: "main".to_string(),
        });
        view.update_field_presence();

        assert_eq!(field_present(&view, "branch_info.branch"), Some(true));
    }

    #[test]
    fn field_presence_with_pr_template() {
        let mut view = make_repo_view(vec![]);
        view.pr_template = Some("template content".to_string());
        view.pr_template_location = Some(".github/pull_request_template.md".to_string());
        view.update_field_presence();

        assert_eq!(field_present(&view, "pr_template"), Some(true));
        assert_eq!(field_present(&view, "pr_template_location"), Some(true));
    }

    #[test]
    fn field_presence_with_branch_prs() {
        let mut view = make_repo_view(vec![]);
        view.branch_prs = Some(vec![PullRequest {
            number: 42,
            title: "Test PR".to_string(),
            state: "open".to_string(),
            url: "https://github.com/test/test/pull/42".to_string(),
            body: "PR body".to_string(),
            base: "main".to_string(),
        }]);
        view.update_field_presence();

        assert_eq!(field_present(&view, "branch_prs"), Some(true));
        assert_eq!(field_present(&view, "branch_prs[].number"), Some(true));
        assert_eq!(field_present(&view, "branch_prs[].title"), Some(true));
    }

    #[test]
    fn field_presence_empty_branch_prs() {
        let mut view = make_repo_view(vec![]);
        view.branch_prs = Some(vec![]);
        view.update_field_presence();

        assert_eq!(field_present(&view, "branch_prs"), Some(true));
        assert_eq!(field_present(&view, "branch_prs[].number"), Some(false));
    }

    #[test]
    fn field_presence_unknown_field_is_false() {
        let mut view = make_repo_view(vec![]);
        view.explanation.fields.push(FieldDocumentation {
            name: "nonexistent.field".to_string(),
            text: "should be false".to_string(),
            command: None,
            present: true, // Start true, should become false
        });
        view.update_field_presence();

        assert_eq!(field_present(&view, "nonexistent.field"), Some(false));
    }

    #[test]
    fn all_documented_fields_present_with_full_data() {
        // Build a view where every optional field is populated and commits are
        // non-empty.  After update_field_presence() every documented field must
        // be present=true.  If a new FieldDocumentation entry is added without
        // a corresponding match arm the catch-all arm returns false and this
        // test fails, catching the drift at test time.
        let commit = make_commit_info("abc123");
        let mut view = make_repo_view(vec![commit]);
        view.versions = Some(VersionInfo {
            omni_dev: "1.0.0".to_string(),
        });
        view.branch_info = Some(BranchInfo {
            branch: "main".to_string(),
        });
        view.pr_template = Some("template".to_string());
        view.pr_template_location = Some(".github/pull_request_template.md".to_string());
        view.branch_prs = Some(vec![PullRequest {
            number: 1,
            title: "Test".to_string(),
            state: "open".to_string(),
            url: "https://github.com/example/repo/pull/1".to_string(),
            body: "body".to_string(),
            base: "main".to_string(),
        }]);
        view.update_field_presence();

        for field in &view.explanation.fields {
            assert!(
                field.present,
                "Field '{}' is documented but not matched in update_field_presence()",
                field.name
            );
        }
    }

    // ── single_commit_view / multi_commit_view ───────────────────────

    fn make_commit_info(hash: &str) -> crate::git::CommitInfo {
        crate::git::CommitInfo {
            hash: hash.to_string(),
            author: "Test <test@test.com>".to_string(),
            date: chrono::Utc::now().fixed_offset(),
            original_message: "test".to_string(),
            in_main_branches: Vec::new(),
            analysis: crate::git::CommitAnalysis {
                detected_type: "feat".to_string(),
                detected_scope: "test".to_string(),
                proposed_message: String::new(),
                file_changes: crate::git::commit::FileChanges {
                    total_files: 0,
                    files_added: 0,
                    files_deleted: 0,
                    file_list: Vec::new(),
                },
                diff_summary: String::new(),
                diff_file: String::new(),
                file_diffs: Vec::new(),
            },
        }
    }

    #[test]
    fn single_commit_view_strips_metadata() {
        let mut view = make_repo_view(vec![make_commit_info("aaa"), make_commit_info("bbb")]);
        view.versions = Some(VersionInfo {
            omni_dev: "1.0.0".to_string(),
        });
        view.branch_info = Some(BranchInfo {
            branch: "feature/test".to_string(),
        });
        view.pr_template = Some("template".to_string());

        let single = view.single_commit_view(&view.commits[0].clone());

        assert!(single.versions.is_none());
        assert!(single.pr_template.is_none());
        assert!(single.remotes.is_empty());
        assert_eq!(single.commits.len(), 1);
        assert_eq!(single.commits[0].hash, "aaa");
        // branch_info IS preserved
        assert!(single.branch_info.is_some());
        assert_eq!(single.branch_info.unwrap().branch, "feature/test");
    }

    #[test]
    fn multi_commit_view_preserves_order() {
        let commits = vec![
            make_commit_info("aaa"),
            make_commit_info("bbb"),
            make_commit_info("ccc"),
        ];
        let view = make_repo_view(commits.clone());

        let refs: Vec<&crate::git::CommitInfo> = commits.iter().collect();
        let multi = view.multi_commit_view(&refs);

        assert_eq!(multi.commits.len(), 3);
        assert_eq!(multi.commits[0].hash, "aaa");
        assert_eq!(multi.commits[1].hash, "bbb");
        assert_eq!(multi.commits[2].hash, "ccc");
    }

    #[test]
    fn multi_commit_view_empty() {
        let view = make_repo_view(vec![]);
        let multi = view.multi_commit_view(&[]);

        assert!(multi.commits.is_empty());
        assert!(multi.versions.is_none());
    }

    // ── single_commit_view_for_ai ──────────────────────────────────

    #[test]
    fn single_commit_view_for_ai_strips_metadata() {
        use crate::git::commit::CommitInfoForAI;

        let commit_info = make_commit_info("aaa");
        let ai_commit = CommitInfoForAI {
            base: crate::git::CommitInfo {
                hash: commit_info.hash,
                author: commit_info.author,
                date: commit_info.date,
                original_message: commit_info.original_message,
                in_main_branches: commit_info.in_main_branches,
                analysis: crate::git::commit::CommitAnalysisForAI {
                    base: commit_info.analysis,
                    diff_content: "diff content".to_string(),
                },
            },
            pre_validated_checks: Vec::new(),
        };

        let ai_view = RepositoryViewForAI {
            versions: Some(VersionInfo {
                omni_dev: "1.0.0".to_string(),
            }),
            explanation: FieldExplanation::default(),
            working_directory: WorkingDirectoryInfo {
                clean: true,
                untracked_changes: Vec::new(),
            },
            remotes: vec![RemoteInfo {
                name: "origin".to_string(),
                uri: "https://example.com".to_string(),
                main_branch: "main".to_string(),
            }],
            ai: AiInfo {
                scratch: String::new(),
            },
            branch_info: Some(BranchInfo {
                branch: "feature/test".to_string(),
            }),
            pr_template: Some("template".to_string()),
            pr_template_location: Some(".github/PULL_REQUEST_TEMPLATE.md".to_string()),
            branch_prs: None,
            commits: vec![ai_commit.clone()],
        };

        let single = ai_view.single_commit_view_for_ai(&ai_commit);

        assert!(single.versions.is_none());
        assert!(single.pr_template.is_none());
        assert!(single.remotes.is_empty());
        assert_eq!(single.commits.len(), 1);
        assert_eq!(single.commits[0].base.hash, "aaa");
        // branch_info IS preserved (for scope context)
        assert!(single.branch_info.is_some());
        assert_eq!(single.branch_info.unwrap().branch, "feature/test");
    }

    // ── FieldExplanation::default ────────────────────────────────────

    #[test]
    fn field_explanation_default_has_all_expected_fields() {
        let explanation = FieldExplanation::default();

        let field_names: Vec<&str> = explanation.fields.iter().map(|f| f.name.as_str()).collect();

        // Core fields that must be documented
        assert!(field_names.contains(&"working_directory.clean"));
        assert!(field_names.contains(&"remotes"));
        assert!(field_names.contains(&"commits[].hash"));
        assert!(field_names.contains(&"commits[].author"));
        assert!(field_names.contains(&"commits[].date"));
        assert!(field_names.contains(&"commits[].original_message"));
        assert!(field_names.contains(&"commits[].analysis.detected_type"));
        assert!(field_names.contains(&"commits[].analysis.diff_file"));
        assert!(field_names.contains(&"ai.scratch"));
        assert!(field_names.contains(&"versions.omni_dev"));
        assert!(field_names.contains(&"branch_info.branch"));
        assert!(field_names.contains(&"pr_template"));
        assert!(field_names.contains(&"branch_prs"));
    }

    #[test]
    fn field_explanation_default_all_start_not_present() {
        let explanation = FieldExplanation::default();
        for field in &explanation.fields {
            assert!(
                !field.present,
                "field '{}' should start as present=false",
                field.name
            );
        }
    }

    // ── map_commits / from_repository_view ──────────────────────────

    fn make_human_view_with_diff_files(
        dir: &tempfile::TempDir,
        messages: &[&str],
    ) -> RepositoryView {
        let commits = messages
            .iter()
            .enumerate()
            .map(|(i, msg)| {
                let diff_path = dir.path().join(format!("{i}.diff"));
                std::fs::write(&diff_path, format!("+line from commit {i}\n")).unwrap();
                CommitInfo {
                    hash: format!("{:0>40}", i),
                    author: "Test <test@test.com>".to_string(),
                    date: Utc::now().fixed_offset(),
                    original_message: msg.to_string(),
                    in_main_branches: Vec::new(),
                    analysis: CommitAnalysis {
                        detected_type: "feat".to_string(),
                        detected_scope: "test".to_string(),
                        proposed_message: format!("feat(test): {msg}"),
                        file_changes: FileChanges {
                            total_files: 1,
                            files_added: 0,
                            files_deleted: 0,
                            file_list: Vec::new(),
                        },
                        diff_summary: "file.rs | 1 +".to_string(),
                        diff_file: diff_path.to_string_lossy().to_string(),
                        file_diffs: Vec::new(),
                    },
                }
            })
            .collect();

        RepositoryView {
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
    fn map_commits_transforms_all_commits() {
        let dir = tempfile::tempdir().unwrap();
        let view = make_human_view_with_diff_files(&dir, &["first", "second"]);
        assert_eq!(view.commits.len(), 2);

        let mapped: RepositoryView<String> = view
            .map_commits(|c| Ok(c.original_message.clone()))
            .unwrap();
        assert_eq!(
            mapped.commits,
            vec!["first".to_string(), "second".to_string()]
        );
    }

    #[test]
    fn from_repository_view_loads_diffs() {
        let dir = tempfile::tempdir().unwrap();
        let view = make_human_view_with_diff_files(&dir, &["commit one"]);

        let ai_view = RepositoryViewForAI::from_repository_view(view).unwrap();
        assert_eq!(ai_view.commits.len(), 1);
        assert_eq!(
            ai_view.commits[0].base.analysis.diff_content,
            "+line from commit 0\n"
        );
        assert_eq!(ai_view.commits[0].base.original_message, "commit one");
    }

    #[test]
    fn from_repository_view_fresh_hides_messages() {
        let dir = tempfile::tempdir().unwrap();
        let view = make_human_view_with_diff_files(&dir, &["original msg"]);

        let ai_view = RepositoryViewForAI::from_repository_view_with_options(view, true).unwrap();
        assert!(ai_view.commits[0].base.original_message.contains("hidden"));
    }
}
