//! CLI adapter for recursive Drive → local mirroring.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{ensure, Result};
use clap::Parser;

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::drive::client::DriveClient;
use crate::drive::sync::{run_sync, SyncOptions, SyncReport};

/// Mirrors a Drive folder recursively to local disk using the read-only scope.
#[derive(Parser)]
pub struct SyncCommand {
    /// Root Drive folder ID (not a search query).
    pub folder_id: String,
    /// Local destination. Must be empty on the first run.
    #[arg(long, value_name = "DIR")]
    pub dest: PathBuf,
    /// Export MIME type for Google-native files. Defaults: Docs → Markdown,
    /// Sheets → CSV (first sheet only), Slides → plain text.
    #[arg(long)]
    pub export_mime_type: Option<String>,
    /// Verify downloaded and skipped binary files against Drive's SHA-256.
    /// Native exports have no checksum and are not verified.
    /// Interrupted writes are retried using the local manifest.
    #[arg(long)]
    pub verify: bool,
    /// Report planned actions without creating directories or downloading content.
    #[arg(long)]
    pub dry_run: bool,
    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl SyncCommand {
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = SyncOptions {
            folder_id: self.folder_id,
            dest: self.dest,
            export_mime_type: self.export_mime_type,
            verify: self.verify,
            dry_run: self.dry_run,
        };
        run_sync_cmd(client, &opts, &self.output).await
    }
}

async fn run_sync_cmd(
    client: &DriveClient,
    opts: &SyncOptions,
    output: &OutputFormat,
) -> Result<()> {
    let report = run_sync(client, opts).await?;
    if !output_as(&report, output)? {
        render_sync_table(&report, &mut std::io::stdout().lock())?;
    }
    ensure!(
        report.failed == 0,
        "Drive sync failed for {} item(s)",
        report.failed
    );
    Ok(())
}

fn render_sync_table(report: &SyncReport, out: &mut impl Write) -> Result<()> {
    if report.dry_run {
        writeln!(out, "Dry run (planned actions):")?;
    }
    for item in &report.items {
        writeln!(
            out,
            "{}\t{}\t{}{}",
            item.action,
            sanitize_for_terminal(&item.id),
            sanitize_for_terminal(&item.path.to_string_lossy()),
            item.error
                .as_ref()
                .map(|e| format!("\t{}", sanitize_for_terminal(e)))
                .unwrap_or_default()
        )?;
    }
    writeln!(
        out,
        "created={} updated={} skipped={} failed={} orphaned={}",
        report.created, report.updated, report.skipped, report.failed, report.orphaned
    )?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::drive::sync::tests::{fixture, list, options};
    use serde_json::json;

    #[tokio::test]
    async fn json_and_yaml_output_and_execute_succeed() {
        let (server, client) = fixture().await;
        list(&server, "root", json!([])).await;
        let dir = tempfile::tempdir().unwrap();
        let opts = options(dir.path());
        for output in [
            OutputFormat::Json,
            OutputFormat::Yaml,
            OutputFormat::Jsonl,
            OutputFormat::Yamls,
        ] {
            run_sync_cmd(&client, &opts, &output).await.unwrap();
        }
        SyncCommand {
            folder_id: "root".into(),
            dest: dir.path().into(),
            export_mime_type: None,
            verify: false,
            dry_run: false,
            output: OutputFormat::Table,
        }
        .execute(&client)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn command_errors_after_reporting_failed_files() {
        let (server, client) = fixture().await;
        list(
            &server,
            "root",
            json!([{"id":"form", "name":"form", "mimeType":"application/vnd.google-apps.form"}]),
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        assert!(
            run_sync_cmd(&client, &options(dir.path()), &OutputFormat::Json)
                .await
                .unwrap_err()
                .to_string()
                .contains("1 item")
        );
    }

    #[test]
    fn table_sanitizes_remote_errors_and_labels_planned_actions() {
        let mut report = SyncReport {
            dry_run: true,
            failed: 1,
            ..Default::default()
        };
        report.items.push(crate::drive::sync::SyncItem {
            id: "id\x1b".into(),
            path: "name\n".into(),
            action: "failed",
            error: Some("bad\r".into()),
        });
        let mut out = Vec::new();
        render_sync_table(&report, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("planned actions"));
        assert!(!text.contains(['\x1b', '\r']));
        assert!(text.contains("failed=1"));
    }

    #[test]
    fn table_propagates_a_writer_failure_from_every_line() {
        let mut report = SyncReport {
            dry_run: true,
            ..Default::default()
        };
        report.items.push(crate::drive::sync::SyncItem {
            id: "id".into(),
            path: "name".into(),
            action: "failed",
            error: Some("bad".into()),
        });
        let mut full = Vec::new();
        render_sync_table(&report, &mut full).unwrap();
        // A fixed-size slice fails with WriteZero once full, so every shorter
        // capacity fails inside the header, item or summary write in turn.
        for capacity in 0..full.len() {
            let mut buf = vec![0u8; capacity];
            let mut out = buf.as_mut_slice();
            assert!(
                render_sync_table(&report, &mut out).is_err(),
                "capacity {capacity}"
            );
        }
    }
}
