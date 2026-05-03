//! Claude Code conversation history management.
//!
//! See the issue for the full design rationale. The exported corpus is a
//! **behavioural transcript** — prompts, responses, thinking, tool calls, and
//! tool-result metadata, sized for analyst use cases (behavioural coaching,
//! work-log generation). Sub-agent internals, tool-result `*.txt` sidecars,
//! PDF rasters, and auto-memory are deliberately excluded; see the issue for
//! the rationale and the planned follow-ups.

pub mod common;
pub mod sync;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Serialize;

pub use common::OutputFormat;

/// Conversation-history operations.
#[derive(Parser)]
pub struct HistoryCommand {
    /// History subcommand to execute.
    #[command(subcommand)]
    pub command: HistorySubcommands,
}

/// History subcommands.
#[derive(Subcommand)]
pub enum HistorySubcommands {
    /// Exports Claude Code conversation history to a target directory.
    Sync(sync::SyncCommand),
}

impl HistoryCommand {
    /// Executes the history command.
    pub fn execute(self) -> Result<()> {
        match self.command {
            HistorySubcommands::Sync(cmd) => cmd.execute(),
        }
    }
}

#[derive(Serialize)]
struct SyncOutput<'a> {
    dry_run: bool,
    actions: &'a [sync::SyncAction],
    errors: &'a [sync::SyncError],
}

pub(super) fn print_report(
    report: &sync::SyncReport,
    dry_run: bool,
    format: OutputFormat,
) -> Result<()> {
    match format {
        OutputFormat::Text => {
            print_report_text(report, dry_run);
            Ok(())
        }
        OutputFormat::Yaml => {
            let output = SyncOutput {
                dry_run,
                actions: &report.actions,
                errors: &report.errors,
            };
            let yaml = serde_yaml::to_string(&output)
                .context("Failed to serialize sync report as YAML")?;
            print!("{yaml}");
            Ok(())
        }
    }
}

fn print_report_text(report: &sync::SyncReport, dry_run: bool) {
    let prefix = if dry_run { "[dry-run] " } else { "" };
    for action in &report.actions {
        match action {
            sync::SyncAction::Created {
                project,
                session,
                target,
                bytes,
            } => println!(
                "{prefix}created {}/{} -> {} ({bytes} bytes)",
                project,
                session,
                target.display()
            ),
            sync::SyncAction::Updated {
                project,
                session,
                target,
                bytes,
            } => println!(
                "{prefix}updated {}/{} -> {} ({bytes} bytes)",
                project,
                session,
                target.display()
            ),
            sync::SyncAction::Skipped {
                project,
                session,
                target,
                reason,
            } => println!(
                "{prefix}skipped {}/{} ({}) -> {}",
                project,
                session,
                skip_reason_label(reason),
                target.display()
            ),
            sync::SyncAction::Pruned {
                project,
                session,
                target,
            } => println!(
                "{prefix}pruned {}/{} -> {}",
                project,
                session,
                target.display()
            ),
        }
    }
    for err in &report.errors {
        eprintln!("error: {}/{} -- {}", err.project, err.session, err.reason);
    }
}

fn skip_reason_label(reason: &sync::SkipReason) -> &'static str {
    match reason {
        sync::SkipReason::Unchanged => "unchanged",
        sync::SkipReason::FilteredBySince => "filtered-by-since",
        sync::SkipReason::FilteredByProject => "filtered-by-project",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::sync::{SkipReason, SyncAction, SyncError, SyncReport};
    use super::*;
    use std::path::PathBuf;

    fn report_with_one_of_each() -> SyncReport {
        SyncReport {
            actions: vec![
                SyncAction::Created {
                    project: "slug".into(),
                    session: "abc".into(),
                    target: PathBuf::from("/t/slug/abc.jsonl"),
                    bytes: 42,
                },
                SyncAction::Updated {
                    project: "slug".into(),
                    session: "def".into(),
                    target: PathBuf::from("/t/slug/def.jsonl"),
                    bytes: 99,
                },
                SyncAction::Skipped {
                    project: "slug".into(),
                    session: "ghi".into(),
                    target: PathBuf::from("/t/slug/ghi.jsonl"),
                    reason: SkipReason::Unchanged,
                },
                SyncAction::Skipped {
                    project: "slug".into(),
                    session: "jkl".into(),
                    target: PathBuf::from("/t/slug/jkl.jsonl"),
                    reason: SkipReason::FilteredBySince,
                },
                SyncAction::Skipped {
                    project: "slug".into(),
                    session: "mno".into(),
                    target: PathBuf::from("/t/slug/mno.jsonl"),
                    reason: SkipReason::FilteredByProject,
                },
                SyncAction::Pruned {
                    project: "slug".into(),
                    session: "old".into(),
                    target: PathBuf::from("/t/slug/old.jsonl"),
                },
            ],
            errors: vec![SyncError {
                project: "slug".into(),
                session: "bad".into(),
                reason: "kapow".into(),
            }],
        }
    }

    #[test]
    fn yaml_render_includes_top_level_keys() {
        let report = SyncReport::default();
        let mut buf = Vec::new();
        let yaml = serde_yaml::to_string(&SyncOutput {
            dry_run: false,
            actions: &report.actions,
            errors: &report.errors,
        })
        .unwrap();
        write!(buf, "{yaml}").unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("dry_run: false"));
        assert!(s.contains("actions:"));
        assert!(s.contains("errors:"));
    }

    #[test]
    fn yaml_render_serialises_each_action_variant() {
        let report = report_with_one_of_each();
        let yaml = serde_yaml::to_string(&SyncOutput {
            dry_run: true,
            actions: &report.actions,
            errors: &report.errors,
        })
        .unwrap();
        assert!(yaml.contains("type: created"), "missing created: {yaml}");
        assert!(yaml.contains("type: updated"), "missing updated: {yaml}");
        assert!(yaml.contains("type: skipped"), "missing skipped: {yaml}");
        assert!(yaml.contains("type: pruned"), "missing pruned: {yaml}");
        assert!(yaml.contains("reason: unchanged"), "missing reason: {yaml}");
        assert!(
            yaml.contains("reason: filtered_by_since"),
            "missing reason: {yaml}"
        );
        assert!(
            yaml.contains("reason: filtered_by_project"),
            "missing reason: {yaml}"
        );
    }

    #[test]
    fn skip_reason_labels_distinct() {
        assert_eq!(skip_reason_label(&SkipReason::Unchanged), "unchanged");
        assert_eq!(
            skip_reason_label(&SkipReason::FilteredBySince),
            "filtered-by-since"
        );
        assert_eq!(
            skip_reason_label(&SkipReason::FilteredByProject),
            "filtered-by-project"
        );
    }

    #[test]
    fn print_report_text_does_not_panic_on_each_variant() {
        // Smoke: the helper writes to stdout/stderr; we just want to ensure
        // every match arm is reachable without panicking.
        print_report_text(&report_with_one_of_each(), true);
        print_report_text(&report_with_one_of_each(), false);
    }

    use std::io::Write as _;
}
