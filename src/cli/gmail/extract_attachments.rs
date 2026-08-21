//! CLI command for `omni-dev gmail extract-attachments`.
//!
//! Retroactively extracts attachments for messages already archived by
//! `gmail sync`/`sync-all`, without any network call — closes the gap
//! ADR-0065's Consequences section names ("No retroactive backfill, for
//! free") and docs/gmail.md's Sync section previously worked around by
//! deleting `.eml` files and re-running `--full --extract-attachments`. The
//! actual algorithm lives in `engine.rs`; this file is CLI glue only,
//! mirroring `sync.rs`'s own split.

pub(crate) mod engine;
pub(crate) mod report;

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use serde::Serialize;

use crate::cli::gmail::format::{output_as, write_scalar_jsonl, JsonlSerialize, OutputFormat};

use engine::ExtractAttachmentsOptions;
use report::{ExtractAction, ExtractAttachmentsReport, ExtractError, ExtractSummary};

/// Retroactively extracts attachments for messages already archived by
/// `gmail sync`/`sync-all` (CLI-only; no MCP equivalent — a bulk filesystem
/// operation is a poor fit for a synchronous MCP tool call, mirroring
/// `sync`'s own no-MCP rationale). Purely local: reads the manifest and
/// `.eml` files already on disk under `--archive-dir`; never contacts
/// Gmail, so no credentials or `--account` are needed.
#[derive(Parser)]
pub struct ExtractAttachmentsCommand {
    /// Archive directory previously populated by `gmail sync`/`sync-all`.
    #[arg(long, value_name = "PATH")]
    pub archive_dir: PathBuf,

    /// Reports which messages would gain extracted attachments without
    /// writing any file.
    #[arg(long)]
    pub dry_run: bool,

    /// Only shows errors, suppresses the per-message action lines.
    #[arg(long)]
    pub quiet: bool,

    /// Report format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ExtractAttachmentsCommand {
    /// Purely local and synchronous — no client, no `.await` anywhere in
    /// this command, unlike `SyncCommand::execute`.
    pub fn execute(self) -> Result<()> {
        run_extract_attachments_command(
            ExtractAttachmentsOptions {
                archive_dir: self.archive_dir,
                dry_run: self.dry_run,
            },
            self.quiet,
            &self.output,
        )
    }
}

/// Runs the extraction and renders its report.
///
/// Split from [`ExtractAttachmentsCommand::execute`] so tests can call it
/// directly. Mirrors `sync.rs::run_sync_command`'s compute → render →
/// decide split (ADR-0064 Decision 4): compute the report, render it, and
/// only *then* decide the process exit condition.
fn run_extract_attachments_command(
    opts: ExtractAttachmentsOptions,
    quiet: bool,
    output: &OutputFormat,
) -> Result<()> {
    let report = engine::run_extract_attachments(&opts)?;

    let output_view = ExtractAttachmentsReportOutput {
        actions: &report.actions,
        errors: &report.errors,
        summary: report.summary(),
    };
    if !output_as(&output_view, output)? {
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        render_report_text(&report, &mut handle, !quiet)?;
    }

    if !report.errors.is_empty() {
        anyhow::bail!(
            "{} message(s) failed to extract; see errors above",
            report.errors.len()
        );
    }
    Ok(())
}

/// `-o json`/`-o yaml`/`-o yamls`/`-o jsonl` view of an
/// [`ExtractAttachmentsReport`]: the same `actions`/`errors` plus a
/// computed `summary` field — mirrors `SyncReportOutput` in `sync.rs`.
#[derive(Serialize)]
struct ExtractAttachmentsReportOutput<'a> {
    actions: &'a [ExtractAction],
    errors: &'a [ExtractError],
    summary: ExtractSummary,
}

impl JsonlSerialize for ExtractAttachmentsReportOutput<'_> {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Renders a report as one line per action, then one line per error, then
/// a trailing summary line. `show_action_detail` suppresses the per-item
/// lines under `--quiet`, mirroring `sync.rs::render_report_text`.
fn render_report_text(
    report: &ExtractAttachmentsReport,
    out: &mut dyn Write,
    show_action_detail: bool,
) -> Result<()> {
    if report.actions.is_empty() && report.errors.is_empty() {
        writeln!(out, "Nothing to do.").context("Failed to write extract-attachments report")?;
        return Ok(());
    }
    if show_action_detail {
        for action in &report.actions {
            let line = match action {
                ExtractAction::Extracted { id, count } => {
                    format!("Extracted {count} attachment(s) from {id}")
                }
                ExtractAction::WouldExtract { id, count } => {
                    format!("Would extract {count} attachment(s) from {id}")
                }
            };
            writeln!(out, "{line}").context("Failed to write extract-attachments report")?;
        }
    }
    for error in &report.errors {
        writeln!(out, "Error: {} failed: {}", error.id, error.reason)
            .context("Failed to write extract-attachments report")?;
    }
    writeln!(out, "{}", format_summary_line(&report.summary()))
        .context("Failed to write extract-attachments report")?;
    Ok(())
}

/// Formats `summary` as a trailing comma-separated line, e.g. `"2
/// extracted, 0 errors"`. Zero counts are omitted except `errors`, which is
/// always shown so a clean run is visible at a glance — mirrors
/// `sync.rs::format_summary_line`.
fn format_summary_line(summary: &ExtractSummary) -> String {
    let mut parts = Vec::new();
    if summary.extracted > 0 {
        parts.push(format!("{} extracted", summary.extracted));
    }
    if summary.would_extract > 0 {
        parts.push(format!("{} would extract", summary.would_extract));
    }
    parts.push(format!("{} errors", summary.errors));
    parts.join(", ")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn write_manifest_with_attachment(archive_dir: &std::path::Path) {
        use base64::Engine as _;

        use crate::cli::gmail::sync::manifest::{Manifest, ManifestRecord};
        use crate::cli::gmail::sync::shard::shard_path;

        let internal_date = chrono::DateTime::from_timestamp_millis(1_700_000_000_000).unwrap();
        let encoded_attachment = base64::engine::general_purpose::STANDARD.encode(b"PDF-CONTENT");
        let raw = format!(
            "Subject: Report\r\n\
From: a@example.com\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"BOUNDARY\"\r\n\
\r\n\
--BOUNDARY\r\n\
Content-Type: application/pdf\r\n\
Content-Transfer-Encoding: base64\r\n\
Content-Disposition: attachment; filename=\"report.pdf\"\r\n\
\r\n\
{encoded_attachment}\r\n\
--BOUNDARY--\r\n"
        );
        let path = shard_path(archive_dir, "m1", Some(internal_date));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &raw).unwrap();
        let relative = path.strip_prefix(archive_dir).unwrap().to_path_buf();

        let mut manifest = Manifest::default();
        manifest.upsert(ManifestRecord {
            id: "m1".to_string(),
            thread_id: None,
            label_ids: Vec::new(),
            internal_date: Some("1700000000000".to_string()),
            subject: None,
            from: None,
            to: None,
            rfc822_msgid: None,
            in_reply_to: None,
            references: None,
            attachment_count: 1,
            attachment_filenames: Vec::new(),
            path: relative,
            size: raw.len() as u64,
            history_id: None,
            deleted_at: None,
        });
        manifest
            .save(&crate::cli::gmail::sync::engine::manifest_path(archive_dir))
            .unwrap();
    }

    #[test]
    fn run_extract_attachments_command_writes_attachment_files() {
        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("archive");
        write_manifest_with_attachment(&archive_dir);

        run_extract_attachments_command(
            ExtractAttachmentsOptions {
                archive_dir: archive_dir.clone(),
                dry_run: false,
            },
            true,
            &OutputFormat::Table,
        )
        .unwrap();

        let internal_date = chrono::DateTime::from_timestamp_millis(1_700_000_000_000).unwrap();
        let written = crate::cli::gmail::sync::shard::attachments_dir(
            &archive_dir,
            "m1",
            Some(internal_date),
        )
        .join("report.pdf");
        assert_eq!(std::fs::read(written).unwrap(), b"PDF-CONTENT");
    }

    #[test]
    fn run_extract_attachments_command_dry_run_touches_no_files() {
        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("archive");
        write_manifest_with_attachment(&archive_dir);

        run_extract_attachments_command(
            ExtractAttachmentsOptions {
                archive_dir: archive_dir.clone(),
                dry_run: true,
            },
            true,
            &OutputFormat::Table,
        )
        .unwrap();

        let internal_date = chrono::DateTime::from_timestamp_millis(1_700_000_000_000).unwrap();
        assert!(!crate::cli::gmail::sync::shard::attachments_dir(
            &archive_dir,
            "m1",
            Some(internal_date)
        )
        .exists());
    }

    #[test]
    fn run_extract_attachments_command_surfaces_a_non_zero_exit_on_errors() {
        use crate::cli::gmail::sync::manifest::{Manifest, ManifestRecord};

        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("archive");
        std::fs::create_dir_all(&archive_dir).unwrap();
        let mut manifest = Manifest::default();
        manifest.upsert(ManifestRecord {
            id: "m1".to_string(),
            thread_id: None,
            label_ids: Vec::new(),
            internal_date: Some("1700000000000".to_string()),
            subject: None,
            from: None,
            to: None,
            rfc822_msgid: None,
            in_reply_to: None,
            references: None,
            attachment_count: 1,
            attachment_filenames: Vec::new(),
            path: PathBuf::from("messages/missing/m1.eml"),
            size: 0,
            history_id: None,
            deleted_at: None,
        });
        manifest
            .save(&crate::cli::gmail::sync::engine::manifest_path(
                &archive_dir,
            ))
            .unwrap();

        let err = run_extract_attachments_command(
            ExtractAttachmentsOptions {
                archive_dir,
                dry_run: false,
            },
            true,
            &OutputFormat::Table,
        )
        .unwrap_err();
        assert!(err.to_string().contains("1 message(s) failed to extract"));
    }

    #[test]
    fn run_extract_attachments_command_renders_json_summary() {
        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("archive");
        write_manifest_with_attachment(&archive_dir);

        run_extract_attachments_command(
            ExtractAttachmentsOptions {
                archive_dir,
                dry_run: false,
            },
            true,
            &OutputFormat::Json,
        )
        .unwrap();
    }

    #[test]
    fn empty_archive_reports_nothing_to_do() {
        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("archive");
        std::fs::create_dir_all(&archive_dir).unwrap();

        run_extract_attachments_command(
            ExtractAttachmentsOptions {
                archive_dir,
                dry_run: false,
            },
            false,
            &OutputFormat::Table,
        )
        .unwrap();
    }
}
