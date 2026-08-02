//! Report types for `omni-dev config scopes lint` (issue #1475).

use serde::{Deserialize, Serialize};

/// A `file_patterns` entry that matches no tracked file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeadPattern {
    /// Name of the scope the pattern belongs to.
    pub scope: String,
    /// The dead pattern itself.
    pub pattern: String,
}

/// Full report produced by `omni-dev config scopes lint`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopesLintReport {
    /// Roots checked for scope coverage.
    pub roots: Vec<String>,
    /// Whether ecosystem-injected default scopes were excluded.
    pub project_only: bool,
    /// Number of scopes validated against.
    pub scopes_checked: usize,
    /// Number of tracked files considered under `roots`, after `--allow`.
    pub files_checked: usize,
    /// `file_patterns` entries matching no tracked file.
    pub dead_patterns: Vec<DeadPattern>,
    /// Tracked files under `roots` matched by no scope.
    pub unscoped_files: Vec<String>,
}

impl ScopesLintReport {
    /// Whether the report found any violation.
    #[must_use]
    pub fn has_violations(&self) -> bool {
        !self.dead_patterns.is_empty() || !self.unscoped_files.is_empty()
    }

    /// Exit code for this report: `0` clean, `1` any violation.
    ///
    /// Unlike `data::check::CheckReport`, there is no severity gradation
    /// (both assertions are unconditional structural violations), so there
    /// is no `--strict` tier to toggle.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        i32::from(self.has_violations())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean_report() -> ScopesLintReport {
        ScopesLintReport {
            roots: vec!["src".to_string()],
            project_only: true,
            scopes_checked: 1,
            files_checked: 1,
            dead_patterns: Vec::new(),
            unscoped_files: Vec::new(),
        }
    }

    #[test]
    fn clean_report_has_no_violations() {
        let report = clean_report();
        assert!(!report.has_violations());
        assert_eq!(report.exit_code(), 0);
    }

    #[test]
    fn dead_pattern_only_is_a_violation() {
        let mut report = clean_report();
        report.dead_patterns.push(DeadPattern {
            scope: "cli".to_string(),
            pattern: "src/nonexistent/**".to_string(),
        });
        assert!(report.has_violations());
        assert_eq!(report.exit_code(), 1);
    }

    #[test]
    fn unscoped_file_only_is_a_violation() {
        let mut report = clean_report();
        report.unscoped_files.push("src/worktrees.rs".to_string());
        assert!(report.has_violations());
        assert_eq!(report.exit_code(), 1);
    }
}
