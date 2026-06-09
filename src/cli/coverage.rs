//! Coverage analysis CLI commands.

pub(crate) mod diff;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Coverage analysis: diff/patch coverage for PR comments.
#[derive(Parser)]
pub struct CoverageCommand {
    /// The coverage subcommand to execute.
    #[command(subcommand)]
    pub command: CoverageSubcommands,
}

/// Coverage subcommands.
#[derive(Subcommand)]
pub enum CoverageSubcommands {
    /// Analyses diff/patch coverage from a per-line report and a git diff.
    Diff(diff::DiffCommand),
}

impl CoverageCommand {
    /// Executes the coverage command.
    ///
    /// `repo` is the repository location resolved at the CLI boundary
    /// (`None` = current working directory).
    pub async fn execute(self, repo: Option<&std::path::Path>) -> Result<()> {
        match self.command {
            CoverageSubcommands::Diff(cmd) => cmd.execute(repo).await,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// The `coverage` command dispatches to `diff`; a missing report makes the
    /// leaf command error, which exercises the dispatch path end-to-end.
    #[tokio::test]
    async fn dispatches_to_diff() {
        let cmd = CoverageCommand {
            command: CoverageSubcommands::Diff(diff::DiffCommand {
                report: std::path::PathBuf::from("/nonexistent/report.lcov"),
                report_format: diff::ReportFormat::Auto,
                base_ref: Some("HEAD".to_string()),
                head_ref: None,
                baseline_report: None,
                baseline_report_format: diff::ReportFormat::Auto,
                format: diff::OutputFormatArg::Markdown,
                fail_under_patch: None,
                strip_prefix: None,
                collapse_ranges: false,
                artifact_url: None,
                run_url: None,
                base_sha: None,
                head_sha: None,
                commit_url: None,
            }),
        };
        // Reaches the leaf command and fails on the missing report file.
        assert!(cmd.execute(None).await.is_err());
    }
}
