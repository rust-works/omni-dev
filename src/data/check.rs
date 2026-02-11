//! Check command result types for commit message validation.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Complete check report containing all commit analysis results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckReport {
    /// Individual commit check results.
    pub commits: Vec<CommitCheckResult>,
    /// Summary statistics.
    pub summary: CheckSummary,
}

/// Result of checking a single commit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitCheckResult {
    /// Commit hash (short form).
    pub hash: String,
    /// Original commit message (first line).
    pub message: String,
    /// List of issues found.
    pub issues: Vec<CommitIssue>,
    /// Suggested improved message (if issues were found).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<CommitSuggestion>,
    /// Whether the commit passes all checks.
    pub passes: bool,
}

/// A single issue found in a commit message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitIssue {
    /// Severity level of the issue.
    pub severity: IssueSeverity,
    /// Which guideline section was violated.
    pub section: String,
    /// Specific rule that was violated.
    pub rule: String,
    /// Explanation of why this is a violation.
    pub explanation: String,
}

/// Suggested correction for a commit message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitSuggestion {
    /// The suggested improved commit message.
    pub message: String,
    /// Explanation of why this message is better.
    pub explanation: String,
}

/// Severity level for issues.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IssueSeverity {
    /// Errors block CI (exit code 1).
    Error,
    /// Advisory issues (exit code 0, or 2 with --strict).
    Warning,
    /// Suggestions only (never affect exit code).
    Info,
}

impl fmt::Display for IssueSeverity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IssueSeverity::Error => write!(f, "ERROR"),
            IssueSeverity::Warning => write!(f, "WARNING"),
            IssueSeverity::Info => write!(f, "INFO"),
        }
    }
}

impl std::str::FromStr for IssueSeverity {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "error" => Ok(IssueSeverity::Error),
            "warning" => Ok(IssueSeverity::Warning),
            "info" => Ok(IssueSeverity::Info),
            other => {
                tracing::debug!("Unknown severity {other:?}, defaulting to Warning");
                Ok(IssueSeverity::Warning)
            }
        }
    }
}

impl IssueSeverity {
    /// Parses severity from a string (case-insensitive).
    #[must_use]
    pub fn parse(s: &str) -> Self {
        // FromStr impl is infallible (unknown values default to Warning with a log).
        s.parse().expect("IssueSeverity::from_str is infallible")
    }
}

/// Summary statistics for a check report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckSummary {
    /// Total number of commits checked.
    pub total_commits: usize,
    /// Number of commits that pass all checks.
    pub passing_commits: usize,
    /// Number of commits with issues.
    pub failing_commits: usize,
    /// Total number of errors found.
    pub error_count: usize,
    /// Total number of warnings found.
    pub warning_count: usize,
    /// Total number of info-level issues found.
    pub info_count: usize,
}

impl CheckSummary {
    /// Creates a summary from a list of commit check results.
    pub fn from_results(results: &[CommitCheckResult]) -> Self {
        let total_commits = results.len();
        let passing_commits = results.iter().filter(|r| r.passes).count();
        let failing_commits = total_commits - passing_commits;

        let mut error_count = 0;
        let mut warning_count = 0;
        let mut info_count = 0;

        for result in results {
            for issue in &result.issues {
                match issue.severity {
                    IssueSeverity::Error => error_count += 1,
                    IssueSeverity::Warning => warning_count += 1,
                    IssueSeverity::Info => info_count += 1,
                }
            }
        }

        Self {
            total_commits,
            passing_commits,
            failing_commits,
            error_count,
            warning_count,
            info_count,
        }
    }
}

impl CheckReport {
    /// Creates a new check report from commit results.
    pub fn new(commits: Vec<CommitCheckResult>) -> Self {
        let summary = CheckSummary::from_results(&commits);
        Self { commits, summary }
    }

    /// Checks if the report has any errors.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.summary.error_count > 0
    }

    /// Checks if the report has any warnings.
    #[must_use]
    pub fn has_warnings(&self) -> bool {
        self.summary.warning_count > 0
    }

    /// Determines exit code based on report and options.
    pub fn exit_code(&self, strict: bool) -> i32 {
        if self.has_errors() {
            1
        } else if strict && self.has_warnings() {
            2
        } else {
            0
        }
    }
}

/// Output format for check results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    /// Human-readable text format.
    #[default]
    Text,
    /// JSON format.
    Json,
    /// YAML format.
    Yaml,
}

impl std::str::FromStr for OutputFormat {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "text" => Ok(OutputFormat::Text),
            "json" => Ok(OutputFormat::Json),
            "yaml" => Ok(OutputFormat::Yaml),
            _ => Err(()),
        }
    }
}

impl fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OutputFormat::Text => write!(f, "text"),
            OutputFormat::Json => write!(f, "json"),
            OutputFormat::Yaml => write!(f, "yaml"),
        }
    }
}

/// AI response structure for parsing check results.
#[derive(Debug, Clone, Deserialize)]
pub struct AiCheckResponse {
    /// List of commit checks.
    pub checks: Vec<AiCommitCheck>,
}

/// Single commit check from AI response.
#[derive(Debug, Clone, Deserialize)]
pub struct AiCommitCheck {
    /// Commit hash (short or full).
    pub commit: String,
    /// Whether the commit passes all checks.
    pub passes: bool,
    /// List of issues found.
    #[serde(default)]
    pub issues: Vec<AiIssue>,
    /// Suggested message improvement.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<AiSuggestion>,
}

/// Issue from AI response.
#[derive(Debug, Clone, Deserialize)]
pub struct AiIssue {
    /// Severity level.
    pub severity: String,
    /// Guideline section.
    pub section: String,
    /// Specific rule violated.
    pub rule: String,
    /// Explanation.
    pub explanation: String,
}

/// Suggestion from AI response.
#[derive(Debug, Clone, Deserialize)]
pub struct AiSuggestion {
    /// Suggested message.
    pub message: String,
    /// Explanation of improvements.
    pub explanation: String,
}

impl From<AiCommitCheck> for CommitCheckResult {
    fn from(ai: AiCommitCheck) -> Self {
        let issues: Vec<CommitIssue> = ai
            .issues
            .into_iter()
            .map(|i| CommitIssue {
                severity: IssueSeverity::parse(&i.severity),
                section: i.section,
                rule: i.rule,
                explanation: i.explanation,
            })
            .collect();

        let suggestion = ai.suggestion.map(|s| CommitSuggestion {
            message: s.message,
            explanation: s.explanation,
        });

        Self {
            hash: ai.commit,
            message: String::new(), // Will be filled in by caller
            issues,
            suggestion,
            passes: ai.passes,
        }
    }
}
