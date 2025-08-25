//! Data processing and serialization

use crate::git::{CommitInfo, RemoteInfo};
use serde::{Deserialize, Serialize};

pub mod amendments;
pub mod yaml;

pub use amendments::*;
pub use yaml::*;

/// Complete repository view output structure
#[derive(Debug, Serialize, Deserialize)]
pub struct RepositoryView {
    /// Explanation of field meanings and structure
    pub explanation: FieldExplanation,
    /// Working directory status information
    pub working_directory: WorkingDirectoryInfo,
    /// List of remote repositories and their main branches
    pub remotes: Vec<RemoteInfo>,
    /// List of analyzed commits with metadata and analysis
    pub commits: Vec<CommitInfo>,
    /// Branch information (only present when using branch commands)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_info: Option<BranchInfo>,
}

/// Field explanation for the YAML output
#[derive(Debug, Serialize, Deserialize)]
pub struct FieldExplanation {
    /// Descriptive text explaining the overall structure
    pub text: String,
    /// Documentation for individual fields in the output
    pub fields: Vec<FieldDocumentation>,
}

/// Individual field documentation
#[derive(Debug, Serialize, Deserialize)]
pub struct FieldDocumentation {
    /// Name of the field being documented
    pub name: String,
    /// Descriptive text explaining what the field contains
    pub text: String,
}

/// Working directory information
#[derive(Debug, Serialize, Deserialize)]
pub struct WorkingDirectoryInfo {
    /// Whether the working directory has no changes
    pub clean: bool,
    /// List of files with uncommitted changes
    pub untracked_changes: Vec<FileStatusInfo>,
}

/// File status information for working directory
#[derive(Debug, Serialize, Deserialize)]
pub struct FileStatusInfo {
    /// Git status flags (e.g., "AM", "??", "M ")
    pub status: String,
    /// Path to the file relative to repository root
    pub file: String,
}

/// Branch information for branch-specific commands
#[derive(Debug, Serialize, Deserialize)]
pub struct BranchInfo {
    /// Current branch name
    pub branch: String,
}

impl Default for FieldExplanation {
    /// Create default field explanation
    fn default() -> Self {
        Self {
            text: "Field documentation for the YAML output format. Each entry describes the purpose and content of fields returned by the view command.".to_string(),
            fields: vec![
                FieldDocumentation {
                    name: "working_directory.clean".to_string(),
                    text: "Boolean indicating if the working directory has no uncommitted changes".to_string(),
                },
                FieldDocumentation {
                    name: "working_directory.untracked_changes".to_string(),
                    text: "Array of files with uncommitted changes, showing git status and file path".to_string(),
                },
                FieldDocumentation {
                    name: "remotes".to_string(),
                    text: "Array of git remotes with their URLs and detected main branch names".to_string(),
                },
                FieldDocumentation {
                    name: "commits[].hash".to_string(),
                    text: "Full SHA-1 hash of the commit".to_string(),
                },
                FieldDocumentation {
                    name: "commits[].author".to_string(),
                    text: "Commit author name and email address".to_string(),
                },
                FieldDocumentation {
                    name: "commits[].date".to_string(),
                    text: "Commit date in ISO format with timezone".to_string(),
                },
                FieldDocumentation {
                    name: "commits[].original_message".to_string(),
                    text: "The original commit message as written by the author".to_string(),
                },
                FieldDocumentation {
                    name: "commits[].in_main_branches".to_string(),
                    text: "Array of remote main branches that contain this commit (empty if not pushed)".to_string(),
                },
                FieldDocumentation {
                    name: "commits[].analysis.detected_type".to_string(),
                    text: "Automatically detected conventional commit type (feat, fix, docs, test, chore, etc.)".to_string(),
                },
                FieldDocumentation {
                    name: "commits[].analysis.detected_scope".to_string(),
                    text: "Automatically detected scope based on file paths (commands, config, tests, etc.)".to_string(),
                },
                FieldDocumentation {
                    name: "commits[].analysis.proposed_message".to_string(),
                    text: "AI-generated conventional commit message based on file changes".to_string(),
                },
                FieldDocumentation {
                    name: "commits[].analysis.file_changes.total_files".to_string(),
                    text: "Total number of files modified in this commit".to_string(),
                },
                FieldDocumentation {
                    name: "commits[].analysis.file_changes.files_added".to_string(),
                    text: "Number of new files added in this commit".to_string(),
                },
                FieldDocumentation {
                    name: "commits[].analysis.file_changes.files_deleted".to_string(),
                    text: "Number of files deleted in this commit".to_string(),
                },
                FieldDocumentation {
                    name: "commits[].analysis.file_changes.file_list".to_string(),
                    text: "Array of files changed with their git status (M=modified, A=added, D=deleted)".to_string(),
                },
                FieldDocumentation {
                    name: "commits[].analysis.diff_summary".to_string(),
                    text: "Git diff --stat output showing lines changed per file".to_string(),
                },
                FieldDocumentation {
                    name: "commits[].analysis.diff_content".to_string(),
                    text: "Full diff content showing line-by-line changes with added, removed, and context lines".to_string(),
                },
                FieldDocumentation {
                    name: "branch_info.branch".to_string(),
                    text: "Current branch name (only present in branch commands)".to_string(),
                },
            ],
        }
    }
}
