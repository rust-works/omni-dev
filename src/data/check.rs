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
    /// Brief summary of what this commit changes (for cross-commit coherence).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
            Self::Error => write!(f, "ERROR"),
            Self::Warning => write!(f, "WARNING"),
            Self::Info => write!(f, "INFO"),
        }
    }
}

impl std::str::FromStr for IssueSeverity {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "error" => Ok(Self::Error),
            "warning" => Ok(Self::Warning),
            "info" => Ok(Self::Info),
            other => {
                tracing::debug!("Unknown severity {other:?}, defaulting to Warning");
                Ok(Self::Warning)
            }
        }
    }
}

impl IssueSeverity {
    /// Parses severity from a string (case-insensitive).
    #[must_use]
    pub fn parse(s: &str) -> Self {
        // FromStr impl is infallible (unknown values default to Warning with a log).
        #[allow(clippy::expect_used)] // FromStr for IssueSeverity always returns Ok
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
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            "yaml" => Ok(Self::Yaml),
            _ => Err(()),
        }
    }
}

impl fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text => write!(f, "text"),
            Self::Json => write!(f, "json"),
            Self::Yaml => write!(f, "yaml"),
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
    /// Brief summary of what this commit changes (for cross-commit coherence).
    #[serde(default)]
    pub summary: Option<String>,
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
            summary: ai.summary,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // ── IssueSeverity ────────────────────────────────────────────────

    #[test]
    fn severity_parse_known() {
        assert_eq!(IssueSeverity::parse("error"), IssueSeverity::Error);
        assert_eq!(IssueSeverity::parse("warning"), IssueSeverity::Warning);
        assert_eq!(IssueSeverity::parse("info"), IssueSeverity::Info);
    }

    #[test]
    fn severity_parse_case_insensitive() {
        assert_eq!(IssueSeverity::parse("ERROR"), IssueSeverity::Error);
        assert_eq!(IssueSeverity::parse("Warning"), IssueSeverity::Warning);
        assert_eq!(IssueSeverity::parse("INFO"), IssueSeverity::Info);
    }

    #[test]
    fn severity_parse_unknown_defaults_warning() {
        assert_eq!(IssueSeverity::parse("foo"), IssueSeverity::Warning);
        assert_eq!(IssueSeverity::parse(""), IssueSeverity::Warning);
    }

    #[test]
    fn severity_display() {
        assert_eq!(IssueSeverity::Error.to_string(), "ERROR");
        assert_eq!(IssueSeverity::Warning.to_string(), "WARNING");
        assert_eq!(IssueSeverity::Info.to_string(), "INFO");
    }

    // ── OutputFormat ─────────────────────────────────────────────────

    #[test]
    fn output_format_parsing() {
        assert_eq!("text".parse::<OutputFormat>(), Ok(OutputFormat::Text));
        assert_eq!("json".parse::<OutputFormat>(), Ok(OutputFormat::Json));
        assert_eq!("yaml".parse::<OutputFormat>(), Ok(OutputFormat::Yaml));
        assert!("unknown".parse::<OutputFormat>().is_err());
    }

    #[test]
    fn output_format_display() {
        assert_eq!(OutputFormat::Text.to_string(), "text");
        assert_eq!(OutputFormat::Json.to_string(), "json");
        assert_eq!(OutputFormat::Yaml.to_string(), "yaml");
    }

    // ── CheckSummary ─────────────────────────────────────────────────

    fn make_result(passes: bool, issues: Vec<CommitIssue>) -> CommitCheckResult {
        CommitCheckResult {
            hash: "abc123".to_string(),
            message: "test".to_string(),
            issues,
            suggestion: None,
            passes,
            summary: None,
        }
    }

    fn make_issue(severity: IssueSeverity) -> CommitIssue {
        CommitIssue {
            severity,
            section: "Format".to_string(),
            rule: "test-rule".to_string(),
            explanation: "test explanation".to_string(),
        }
    }

    #[test]
    fn summary_empty_results() {
        let summary = CheckSummary::from_results(&[]);
        assert_eq!(summary.total_commits, 0);
        assert_eq!(summary.passing_commits, 0);
        assert_eq!(summary.failing_commits, 0);
        assert_eq!(summary.error_count, 0);
        assert_eq!(summary.warning_count, 0);
        assert_eq!(summary.info_count, 0);
    }

    #[test]
    fn summary_mixed_results() {
        let results = vec![
            make_result(
                false,
                vec![
                    make_issue(IssueSeverity::Error),
                    make_issue(IssueSeverity::Warning),
                ],
            ),
            make_result(true, vec![make_issue(IssueSeverity::Info)]),
        ];
        let summary = CheckSummary::from_results(&results);
        assert_eq!(summary.total_commits, 2);
        assert_eq!(summary.passing_commits, 1);
        assert_eq!(summary.failing_commits, 1);
        assert_eq!(summary.error_count, 1);
        assert_eq!(summary.warning_count, 1);
        assert_eq!(summary.info_count, 1);
    }

    #[test]
    fn summary_all_passing() {
        let results = vec![make_result(true, vec![]), make_result(true, vec![])];
        let summary = CheckSummary::from_results(&results);
        assert_eq!(summary.passing_commits, 2);
        assert_eq!(summary.failing_commits, 0);
    }

    // ── CheckReport::exit_code ───────────────────────────────────────

    #[test]
    fn exit_code_no_issues() {
        let report = CheckReport::new(vec![make_result(true, vec![])]);
        assert_eq!(report.exit_code(false), 0);
        assert_eq!(report.exit_code(true), 0);
    }

    #[test]
    fn exit_code_errors() {
        let report = CheckReport::new(vec![make_result(
            false,
            vec![make_issue(IssueSeverity::Error)],
        )]);
        assert_eq!(report.exit_code(false), 1);
        assert_eq!(report.exit_code(true), 1);
    }

    #[test]
    fn exit_code_warnings_strict() {
        let report = CheckReport::new(vec![make_result(
            false,
            vec![make_issue(IssueSeverity::Warning)],
        )]);
        assert_eq!(report.exit_code(false), 0);
        assert_eq!(report.exit_code(true), 2);
    }

    #[test]
    fn has_errors_and_warnings() {
        let report = CheckReport::new(vec![make_result(
            false,
            vec![
                make_issue(IssueSeverity::Error),
                make_issue(IssueSeverity::Warning),
            ],
        )]);
        assert!(report.has_errors());
        assert!(report.has_warnings());
    }

    // ── From<AiCommitCheck> ──────────────────────────────────────────

    #[test]
    fn ai_check_converts_issues() {
        let ai = AiCommitCheck {
            commit: "abc123".to_string(),
            passes: false,
            issues: vec![AiIssue {
                severity: "error".to_string(),
                section: "Format".to_string(),
                rule: "subject-line".to_string(),
                explanation: "too long".to_string(),
            }],
            suggestion: None,
            summary: Some("Added feature".to_string()),
        };
        let result: CommitCheckResult = ai.into();
        assert_eq!(result.hash, "abc123");
        assert!(!result.passes);
        assert_eq!(result.issues.len(), 1);
        assert_eq!(result.issues[0].severity, IssueSeverity::Error);
        assert_eq!(result.issues[0].section, "Format");
        assert_eq!(result.summary, Some("Added feature".to_string()));
    }

    #[test]
    fn ai_check_converts_suggestion() {
        let ai = AiCommitCheck {
            commit: "def456".to_string(),
            passes: false,
            issues: vec![],
            suggestion: Some(AiSuggestion {
                message: "feat(cli): better message".to_string(),
                explanation: "improved clarity".to_string(),
            }),
            summary: None,
        };
        let result: CommitCheckResult = ai.into();
        let suggestion = result.suggestion.unwrap();
        assert_eq!(suggestion.message, "feat(cli): better message");
        assert_eq!(suggestion.explanation, "improved clarity");
    }

    #[test]
    fn ai_check_no_suggestion() {
        let ai = AiCommitCheck {
            commit: "abc".to_string(),
            passes: true,
            issues: vec![],
            suggestion: None,
            summary: None,
        };
        let result: CommitCheckResult = ai.into();
        assert!(result.suggestion.is_none());
        assert!(result.passes);
    }

    // ── property tests ────────────────────────────────────────────

    // ── IssueSeverity Hash ────────────────────────────────────────

    #[test]
    fn severity_hash_consistent_with_eq() {
        use std::collections::HashSet;

        let mut set = HashSet::new();
        set.insert(IssueSeverity::Error);
        set.insert(IssueSeverity::Warning);
        set.insert(IssueSeverity::Info);
        assert_eq!(set.len(), 3);

        // Duplicate insert should not increase size
        set.insert(IssueSeverity::Error);
        assert_eq!(set.len(), 3);
    }

    #[test]
    fn issue_dedup_by_rule_severity_section() {
        use std::collections::HashSet;

        let issues = vec![
            CommitIssue {
                severity: IssueSeverity::Error,
                section: "Format".to_string(),
                rule: "subject-line".to_string(),
                explanation: "too long".to_string(),
            },
            CommitIssue {
                severity: IssueSeverity::Error,
                section: "Format".to_string(),
                rule: "subject-line".to_string(),
                explanation: "different wording".to_string(),
            },
            CommitIssue {
                severity: IssueSeverity::Warning,
                section: "Content".to_string(),
                rule: "body-required".to_string(),
                explanation: "missing body".to_string(),
            },
        ];

        let mut seen = HashSet::new();
        let mut deduped = Vec::new();
        for issue in &issues {
            let key = (issue.rule.clone(), issue.severity, issue.section.clone());
            if seen.insert(key) {
                deduped.push(issue.clone());
            }
        }

        assert_eq!(deduped.len(), 2);
        assert_eq!(deduped[0].rule, "subject-line");
        assert_eq!(deduped[1].rule, "body-required");
    }

    mod prop {
        use super::*;
        use proptest::prelude::*;

        fn arb_severity() -> impl Strategy<Value = IssueSeverity> {
            prop_oneof![
                Just(IssueSeverity::Error),
                Just(IssueSeverity::Warning),
                Just(IssueSeverity::Info),
            ]
        }

        fn arb_issue() -> impl Strategy<Value = CommitIssue> {
            arb_severity().prop_map(make_issue)
        }

        fn arb_result() -> impl Strategy<Value = CommitCheckResult> {
            (any::<bool>(), proptest::collection::vec(arb_issue(), 0..5))
                .prop_map(|(passes, issues)| make_result(passes, issues))
        }

        proptest! {
            #[test]
            fn severity_display_roundtrip(sev in arb_severity()) {
                let displayed = sev.to_string();
                let parsed: IssueSeverity = displayed.parse().unwrap();
                prop_assert_eq!(parsed, sev);
            }

            #[test]
            fn severity_from_str_never_errors(s in ".*") {
                let result: Result<IssueSeverity, ()> = s.parse();
                prop_assert!(result.is_ok());
            }

            #[test]
            fn summary_total_is_passing_plus_failing(
                results in proptest::collection::vec(arb_result(), 0..20),
            ) {
                let summary = CheckSummary::from_results(&results);
                prop_assert_eq!(summary.total_commits, summary.passing_commits + summary.failing_commits);
                prop_assert_eq!(summary.total_commits, results.len());
            }

            #[test]
            fn summary_issue_counts_match(
                results in proptest::collection::vec(arb_result(), 0..20),
            ) {
                let summary = CheckSummary::from_results(&results);
                let total_issues: usize = results.iter().map(|r| r.issues.len()).sum();
                prop_assert_eq!(
                    summary.error_count + summary.warning_count + summary.info_count,
                    total_issues
                );
            }

            #[test]
            fn exit_code_bounded(
                results in proptest::collection::vec(arb_result(), 0..10),
                strict in any::<bool>(),
            ) {
                let report = CheckReport::new(results);
                let code = report.exit_code(strict);
                prop_assert!(code == 0 || code == 1 || code == 2);
            }

            #[test]
            fn exit_code_errors_always_one(
                mut results in proptest::collection::vec(arb_result(), 0..10),
                strict in any::<bool>(),
            ) {
                // Ensure at least one result with an error
                results.push(make_result(false, vec![make_issue(IssueSeverity::Error)]));
                let report = CheckReport::new(results);
                prop_assert_eq!(report.exit_code(strict), 1);
            }
        }
    }
}
