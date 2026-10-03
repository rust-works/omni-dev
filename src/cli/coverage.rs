//! Coverage analysis CLI commands.

pub(crate) mod diff;
pub(crate) mod lint_markers;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Coverage analysis: diff/patch coverage for PR comments.
#[derive(Parser)]
pub struct CoverageCommand {
    /// The coverage subcommand to execute.
    #[command(subcommand)]
    pub command: CoverageSubcommands,

    /// `-C/--repo`, inherited by every coverage subcommand.
    #[command(flatten)]
    pub repo: crate::cli::repo_arg::RepoArg,
}

/// Coverage subcommands.
#[derive(Subcommand)]
pub enum CoverageSubcommands {
    /// Analyses diff/patch coverage from a per-line report and a git diff.
    Diff(Box<diff::DiffCommand>),
    /// Checks source coverage markers without generating a coverage report.
    LintMarkers(lint_markers::LintMarkersCommand),
}

impl CoverageCommand {
    /// Executes the coverage command.
    ///
    /// `-C/--repo` is resolved here (`None` = current working directory) and
    /// threaded explicitly to the leaf.
    pub fn execute(self) -> Result<()> {
        let repo = self.repo.path();
        match self.command {
            CoverageSubcommands::Diff(cmd) => cmd.execute(repo),
            CoverageSubcommands::LintMarkers(cmd) => cmd.execute(repo),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// The `coverage` command dispatches to `diff`; a missing report makes the
    /// leaf command error, which exercises the dispatch path end-to-end.
    #[test]
    fn dispatches_to_diff() {
        let cmd = CoverageCommand {
            command: CoverageSubcommands::Diff(Box::new(diff::DiffCommand {
                report: std::path::PathBuf::from("/nonexistent/report.lcov"),
                report_format: diff::ReportFormat::Auto,
                base_ref: Some("HEAD".to_string()),
                head_ref: None,
                baseline_report: None,
                baseline_report_format: diff::ReportFormat::Auto,
                output: diff::OutputFormatArg::Markdown,
                format: None,
                fail_under_patch: None,
                fail_under_lines: None,
                strip_prefix: None,
                ignore_filename_regex: Vec::new(),
                context_dir: None,
                collapse_ranges: false,
                all_files: false,
                artifact_url: None,
                run_url: None,
                base_sha: None,
                head_sha: None,
                commit_url: None,
            })),
            repo: crate::cli::repo_arg::RepoArg::default(),
        };
        // Reaches the leaf command and fails on the missing report file.
        assert!(cmd.execute().is_err());
    }
}
