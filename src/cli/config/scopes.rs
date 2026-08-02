//! Usage command — tallies declared commit scopes against `scopes.yaml`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;

use crate::data::check::OutputFormat;
use crate::git::commit::{tally_scope_usage, ScopeUsageReport};

/// Usage command options — tallies declared commit scopes against history.
#[derive(Parser)]
pub struct UsageCommand {
    /// Commit range to analyze (e.g., HEAD~300..HEAD, abc123..def456).
    /// Defaults to the entire history reachable from HEAD.
    #[arg(value_name = "COMMIT_RANGE", conflicts_with = "max_count")]
    pub commit_range: Option<String>,

    /// Limits analysis to the newest N commits reachable from HEAD (ignored
    /// when COMMIT_RANGE is given). Useful for comparing the answer at
    /// different window sizes, e.g. `-n 150` vs `-n 400`.
    #[arg(short = 'n', long = "max-count", value_name = "N")]
    pub max_count: Option<usize>,

    /// Path to custom context directory (defaults to .omni-dev/).
    #[arg(long)]
    pub context_dir: Option<PathBuf>,

    /// Excludes ecosystem default scopes (cargo/core/lib/test, …) from the
    /// known set, so they are reported as unknown rather than accepted.
    #[arg(long)]
    pub project_only: bool,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Text)]
    pub output: OutputFormat,
}

impl UsageCommand {
    /// Executes the usage command: tallies declared scopes across the
    /// resolved commit range and prints a report. Always exits 0 on a
    /// successful tally (including an empty range) — this is a reporting
    /// command, never a gate.
    pub fn execute(self, repo: Option<&Path>) -> Result<()> {
        let repo_root = match repo {
            Some(p) => p.to_path_buf(),
            None => std::env::current_dir().context("Failed to determine current directory")?,
        };
        let repo_root = repo_root.as_path();

        let git_repo = crate::git::GitRepository::open_at(repo_root)
            .context("Failed to open git repository at the given path")?;

        let commits = match &self.commit_range {
            Some(range) => git_repo.get_commits_in_range(range)?,
            None => git_repo.get_commits_from_head(self.max_count)?,
        };

        let subjects: Vec<&str> = commits
            .iter()
            .map(|c| c.original_message.lines().next().unwrap_or("").trim())
            .collect();

        let context_dir =
            crate::claude::context::resolve_context_dir_at(self.context_dir.as_deref(), repo_root);
        let scopes_yaml_only = crate::claude::context::load_project_scopes_only(&context_dir);
        let known_scopes = if self.project_only {
            scopes_yaml_only.clone()
        } else {
            crate::claude::context::load_project_scopes(&context_dir, repo_root)
        };

        let report = tally_scope_usage(&subjects, &known_scopes, &scopes_yaml_only);

        self.output_report(&report)
    }

    /// Renders `report` per `self.output`.
    fn output_report(&self, report: &ScopeUsageReport) -> Result<()> {
        match self.output {
            OutputFormat::Text => {
                print!("{}", format_text_report(report));
                Ok(())
            }
            OutputFormat::Json => {
                let json = serde_json::to_string_pretty(report)
                    .context("Failed to serialize report to JSON")?;
                println!("{json}");
                Ok(())
            }
            OutputFormat::Yaml => {
                let yaml =
                    crate::data::to_yaml(report).context("Failed to serialize report to YAML")?;
                println!("{yaml}");
                Ok(())
            }
        }
    }
}

/// Formats a [`ScopeUsageReport`] as human-readable text.
fn format_text_report(report: &ScopeUsageReport) -> String {
    use std::fmt::Write;

    let mut out = String::new();
    let _ = writeln!(
        out,
        "📊 Scope usage: {} commit(s) analyzed",
        report.total_commits
    );
    out.push('\n');

    if report.declared.is_empty() {
        out.push_str("Declared scopes: (none)\n");
    } else {
        out.push_str("Declared scopes:\n");
        for sc in &report.declared {
            let _ = writeln!(out, "  {:<20} {}", sc.name, sc.count);
        }
    }
    out.push('\n');

    if !report.unknown.is_empty() {
        out.push_str("⚠️  Unknown (declared, not in scopes.yaml):\n");
        for sc in &report.unknown {
            let _ = writeln!(out, "  {:<20} {}", sc.name, sc.count);
        }
        out.push('\n');
    }

    if !report.unused.is_empty() {
        out.push_str("💤 Unused (defined in scopes.yaml, never declared):\n");
        for name in &report.unused {
            let _ = writeln!(out, "  {name}");
        }
        out.push('\n');
    }

    let _ = writeln!(out, "Scope-less commits: {}", report.scope_less_count);

    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::git::commit::ScopeCount;

    fn make_report() -> ScopeUsageReport {
        ScopeUsageReport {
            total_commits: 5,
            declared: vec![
                ScopeCount {
                    name: "cli".to_string(),
                    count: 3,
                },
                ScopeCount {
                    name: "lib".to_string(),
                    count: 1,
                },
            ],
            unknown: vec![ScopeCount {
                name: "lib".to_string(),
                count: 1,
            }],
            unused: vec!["workflows".to_string()],
            scope_less_count: 1,
        }
    }

    #[test]
    fn text_report_includes_all_sections() {
        let report = make_report();
        let text = format_text_report(&report);
        assert!(text.contains("5 commit(s) analyzed"));
        assert!(text.contains("cli"));
        assert!(text.contains("Unknown"));
        assert!(text.contains("lib"));
        assert!(text.contains("Unused"));
        assert!(text.contains("workflows"));
        assert!(text.contains("Scope-less commits: 1"));
    }

    #[test]
    fn text_report_empty_omits_unknown_and_unused_sections() {
        let report = ScopeUsageReport {
            total_commits: 0,
            declared: vec![],
            unknown: vec![],
            unused: vec![],
            scope_less_count: 0,
        };
        let text = format_text_report(&report);
        assert!(text.contains("(none)"));
        assert!(!text.contains("Unknown"));
        assert!(!text.contains("Unused"));
        assert!(text.contains("Scope-less commits: 0"));
    }

    #[test]
    fn json_output_round_trips() {
        let report = make_report();
        let json = serde_json::to_string_pretty(&report).unwrap();
        let parsed: ScopeUsageReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, report);
    }
}
