//! CLI command for `omni-dev gmail insert` (#1655).
//!
//! Restores archived `.eml` messages into a mailbox via `messages.insert`,
//! closing the loop on the local archive: `sync` captures, `render`/
//! `extract-attachments` read it back, `insert` restores. The actual
//! algorithm lives in `engine.rs`; this file is CLI glue only, mirroring
//! `sync.rs`'s own split.

pub(crate) mod engine;
pub(crate) mod labels;
pub(crate) mod ledger;
pub(crate) mod progress;
pub(crate) mod report;

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use serde::Serialize;
use tokio::sync::mpsc;

use crate::cli::gmail::format::{output_as, write_scalar_jsonl, JsonlSerialize, OutputFormat};
use crate::cli::gmail::selection::{Selection, SelectionArgs};
use crate::gmail::client::GmailClient;

use engine::{InsertOptions, DEFAULT_INSERT_CONCURRENCY};
use progress::{InsertProgressBar, InsertProgressEvent};
use report::{InsertAction, InsertError, InsertReport, InsertSummary, SkipReason};

/// Restores archived `.eml` messages into a mailbox (CLI-only; no MCP
/// equivalent — a bulk, mutating, potentially long-running operation is a
/// poor fit for a synchronous MCP tool call, mirroring `sync`'s own
/// no-MCP rationale).
#[derive(Parser)]
pub struct InsertCommand {
    /// Archive directory previously populated by `gmail sync`/`sync-all`.
    #[arg(long, value_name = "PATH")]
    pub archive_dir: PathBuf,

    #[command(flatten)]
    pub selection: SelectionArgs,

    /// Caps how many selected messages are inserted, applied after
    /// selection to the oldest-first plan. `0` means no limit.
    #[arg(long, default_value_t = 0)]
    pub limit: usize,

    /// Tags every inserted message with this label, resolved by name
    /// against the destination mailbox's existing labels. Never
    /// auto-created — since inserted mail's raw headers still name the
    /// *original* recipient, `to:` searches won't match it, making this
    /// tag the only reliable handle for "what came from the archive".
    #[arg(long, value_name = "NAME")]
    pub label: Option<String>,

    /// Drops this label id from a message's replayed system-label set
    /// (applied after the system-label filter). Repeatable — e.g.
    /// `--drop-label INBOX --drop-label UNREAD` restores mail as
    /// already-read and archived rather than dumping it into a live Inbox.
    #[arg(long, value_name = "LABEL_ID")]
    pub drop_label: Vec<String>,

    /// Bounds concurrent inserts. Clamped to
    /// `1..=gmail::messages_api::MAX_CONCURRENCY`.
    #[arg(long, default_value_t = DEFAULT_INSERT_CONCURRENCY)]
    pub concurrency: usize,

    /// Before inserting, probes the destination for a message already
    /// carrying the same `Message-ID` (`rfc822msgid:` search) and skips it
    /// on a hit. A supplement to the local ledger, not a substitute — the
    /// probe costs quota and a round-trip per message and can lag a recent
    /// insert by seconds to minutes; useful mainly for a first run into a
    /// mailbox that may already hold some of this mail, or as a recovery
    /// probe after losing the ledger.
    #[arg(long)]
    pub verify_remote: bool,

    /// Reports what would be inserted without making any `messages.insert`
    /// call or writing the ledger.
    #[arg(long)]
    pub dry_run: bool,

    /// Only shows errors, suppresses per-message action lines and the live
    /// progress bar.
    #[arg(long)]
    pub quiet: bool,

    /// Report format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl InsertCommand {
    /// Runs the command against the shared client resolved by the parent
    /// `GmailCommand::execute`.
    pub async fn execute(self, client: &GmailClient) -> Result<()> {
        let selection = Selection::from_args(&self.selection)?;
        run_insert_command(
            client,
            InsertOptions {
                archive_dir: self.archive_dir,
                selection,
                limit: self.limit,
                label: self.label,
                drop_label_ids: self.drop_label,
                concurrency: self.concurrency,
                verify_remote: self.verify_remote,
                dry_run: self.dry_run,
            },
            self.quiet,
            &self.output,
        )
        .await
    }
}

/// Runs the insert and renders its report. Split from
/// [`InsertCommand::execute`] so tests can inject a wiremock client without
/// going through the credential-loading path — mirrors
/// `sync.rs::run_sync_command`'s compute → render → decide split (ADR-0064
/// Decision 4).
async fn run_insert_command(
    client: &GmailClient,
    opts: InsertOptions,
    quiet: bool,
    output: &OutputFormat,
) -> Result<()> {
    let show_progress = should_show_progress(quiet, output, std::io::stderr().is_terminal());
    let report = if show_progress {
        let (tx, rx) = mpsc::unbounded_channel();
        let bar = InsertProgressBar::new();
        let render_task = tokio::spawn(bar.drain(rx));
        let notify_tx = tx.clone();
        client.set_retry_notify(Arc::new(move |status, delay_secs, attempt| {
            let _ = notify_tx.send(InsertProgressEvent::RateLimited {
                status,
                delay_secs,
                attempt,
            });
        }));
        let result = engine::run_insert_with_progress(client, &opts, Some(&tx)).await;
        drop(tx);
        let _ = render_task.await;
        result?
    } else {
        engine::run_insert(client, &opts).await?
    };

    let output_view = InsertReportOutput {
        actions: &report.actions,
        errors: &report.errors,
        summary: report.summary(),
    };
    if !output_as(&output_view, output)? {
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        render_report_text(&report, &mut handle, !quiet && !show_progress)?;
    }

    if !report.errors.is_empty() {
        anyhow::bail!(
            "{} message(s) failed to insert; see errors above",
            report.errors.len()
        );
    }
    Ok(())
}

/// Mirrors `sync.rs::should_show_progress` exactly (see its doc comment for
/// the full rationale): live bars only on an interactive `-o table` run.
fn should_show_progress(quiet: bool, output: &OutputFormat, stderr_is_terminal: bool) -> bool {
    !quiet && matches!(output, OutputFormat::Table) && stderr_is_terminal
}

/// `-o json`/`-o yaml`/`-o yamls`/`-o jsonl` view of an [`InsertReport`].
#[derive(Serialize)]
struct InsertReportOutput<'a> {
    actions: &'a [InsertAction],
    errors: &'a [InsertError],
    summary: InsertSummary,
}

impl JsonlSerialize for InsertReportOutput<'_> {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Renders a report as one line per action, then one line per error, then a
/// trailing summary line. `show_action_detail` suppresses per-item
/// `Inserted`/`WouldInsert`/`Skipped` lines under `--quiet` or when a live
/// progress bar already showed them, but a `Note` always survives — mirrors
/// `sync.rs::render_report_text`.
fn render_report_text(
    report: &InsertReport,
    out: &mut dyn Write,
    show_action_detail: bool,
) -> Result<()> {
    if report.actions.is_empty() && report.errors.is_empty() {
        writeln!(out, "Nothing to do.").context("Failed to write insert report")?;
        return Ok(());
    }
    for action in &report.actions {
        if !show_action_detail && !matches!(action, InsertAction::Note { .. }) {
            continue;
        }
        let line = match action {
            InsertAction::Inserted {
                id,
                inserted_id,
                label_ids,
            } => format!("Inserted {id} -> {inserted_id} ({})", label_ids.join(",")),
            InsertAction::WouldInsert { id, label_ids } => {
                format!("Would insert {id} ({})", label_ids.join(","))
            }
            InsertAction::Skipped {
                id,
                reason: SkipReason::AlreadyInserted,
            } => format!("Skipped {id} (already inserted)"),
            InsertAction::Skipped {
                id,
                reason: SkipReason::FoundRemote,
            } => format!("Skipped {id} (found on destination)"),
            InsertAction::Note { message } => format!("Note: {message}"),
        };
        writeln!(out, "{line}").context("Failed to write insert report")?;
    }
    for error in &report.errors {
        writeln!(out, "Error: {} failed: {}", error.id, error.reason)
            .context("Failed to write insert report")?;
    }
    writeln!(out, "{}", format_summary_line(&report.summary()))
        .context("Failed to write insert report")?;
    Ok(())
}

/// Formats `summary` as a trailing comma-separated line, e.g. `"2 inserted,
/// 1 skipped, 0 errors"`. Zero counts are omitted except `errors`, which is
/// always shown so a clean run is visible at a glance — mirrors
/// `sync.rs::format_summary_line`.
fn format_summary_line(summary: &InsertSummary) -> String {
    let mut parts = Vec::new();
    if summary.inserted > 0 {
        parts.push(format!("{} inserted", summary.inserted));
    }
    if summary.would_insert > 0 {
        parts.push(format!("{} would insert", summary.would_insert));
    }
    if summary.skipped > 0 {
        parts.push(format!("{} skipped", summary.skipped));
    }
    parts.push(format!("{} errors", summary.errors));
    parts.join(", ")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::cli::gmail::sync::engine::manifest_path;
    use crate::cli::gmail::sync::manifest::{Manifest, ManifestRecord};
    use crate::gmail::auth::{GmailCredentials, GmailScope};
    use crate::utils::secret::Secret;

    fn test_credentials() -> GmailCredentials {
        GmailCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: GmailScope::Modify,
        }
    }

    async fn client_with_bootstrapped_token(server: &wiremock::MockServer) -> GmailClient {
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/token"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "test-token",
                    "expires_in": 3600,
                })),
            )
            .mount(server)
            .await;

        let mut client = GmailClient::new(&server.uri(), &test_credentials()).unwrap();
        crate::gmail::client::test_support::replace_session(
            &mut client,
            &test_credentials(),
            &format!("{}/token", server.uri()),
        );
        client
    }

    fn write_archived_message(archive_dir: &std::path::Path, id: &str) {
        let path = PathBuf::from(format!("messages/{id}.eml"));
        std::fs::create_dir_all(archive_dir.join(path.parent().unwrap())).unwrap();
        std::fs::write(archive_dir.join(&path), "From: a@example.com\r\n\r\nbody").unwrap();

        let mut manifest = Manifest::load(&manifest_path(archive_dir)).unwrap();
        manifest.upsert(ManifestRecord {
            id: id.to_string(),
            thread_id: None,
            label_ids: vec!["INBOX".to_string()],
            internal_date: Some("1700000000000".to_string()),
            subject: None,
            from: None,
            to: None,
            rfc822_msgid: Some(format!("<{id}@example.com>")),
            in_reply_to: None,
            references: None,
            attachment_count: 0,
            attachment_filenames: Vec::new(),
            path,
            size: 0,
            history_id: None,
            deleted_at: None,
        });
        manifest.save(&manifest_path(archive_dir)).unwrap();
    }

    // ── should_show_progress ─────────────────────────────────────────

    #[test]
    fn should_show_progress_gate() {
        assert!(should_show_progress(false, &OutputFormat::Table, true));
        assert!(!should_show_progress(true, &OutputFormat::Table, true));
        assert!(!should_show_progress(false, &OutputFormat::Json, true));
        assert!(!should_show_progress(false, &OutputFormat::Table, false));
    }

    // ── render_report_text ────────────────────────────────────────────

    #[test]
    fn render_report_text_reports_nothing_to_do_when_empty() {
        let report = InsertReport::default();
        let mut buf = Vec::new();
        render_report_text(&report, &mut buf, true).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "Nothing to do.\n");
    }

    #[test]
    fn render_report_text_writes_one_line_per_action_and_error() {
        let report = InsertReport {
            actions: vec![
                InsertAction::Inserted {
                    id: "m1".to_string(),
                    inserted_id: "new1".to_string(),
                    label_ids: vec!["INBOX".to_string()],
                },
                InsertAction::Skipped {
                    id: "m2".to_string(),
                    reason: SkipReason::AlreadyInserted,
                },
                InsertAction::WouldInsert {
                    id: "m4".to_string(),
                    label_ids: vec!["INBOX".to_string()],
                },
                InsertAction::Skipped {
                    id: "m5".to_string(),
                    reason: SkipReason::FoundRemote,
                },
            ],
            errors: vec![InsertError {
                id: "m3".to_string(),
                reason: "boom".to_string(),
            }],
        };
        let mut buf = Vec::new();
        render_report_text(&report, &mut buf, true).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("Inserted m1 -> new1"));
        assert!(text.contains("Skipped m2 (already inserted)"));
        assert!(text.contains("Would insert m4 (INBOX)"));
        assert!(text.contains("Skipped m5 (found on destination)"));
        assert!(text.contains("Error: m3 failed: boom"));
        assert!(text.contains("1 inserted, 1 would insert, 2 skipped, 1 errors"));
    }

    #[test]
    fn render_report_text_suppresses_per_item_actions_but_keeps_notes() {
        let report = InsertReport {
            actions: vec![
                InsertAction::Inserted {
                    id: "m1".to_string(),
                    inserted_id: "new1".to_string(),
                    label_ids: vec![],
                },
                InsertAction::Note {
                    message: "Inserting into dest@example.com".to_string(),
                },
            ],
            errors: vec![],
        };
        let mut buf = Vec::new();
        render_report_text(&report, &mut buf, false).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(!text.contains("Inserted m1"));
        assert!(text.contains("Note: Inserting into dest@example.com"));
    }

    // ── run_insert_command / InsertCommand::execute glue ───────────────

    #[tokio::test]
    async fn run_insert_command_dry_run_reports_would_insert_and_writes_no_ledger() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/profile"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "emailAddress": "dest@example.com", "messagesTotal": 0, "threadsTotal": 0, "historyId": "1"
            })))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("archive");
        write_archived_message(&archive_dir, "m1");

        let selection = Selection::from_args(&SelectionArgs {
            all: true,
            ..Default::default()
        })
        .unwrap();

        run_insert_command(
            &client,
            InsertOptions {
                archive_dir: archive_dir.clone(),
                selection,
                limit: 0,
                label: None,
                drop_label_ids: Vec::new(),
                concurrency: DEFAULT_INSERT_CONCURRENCY,
                verify_remote: false,
                dry_run: true,
            },
            true,
            &OutputFormat::Table,
        )
        .await
        .unwrap();

        assert!(!ledger::ledger_path(&archive_dir).exists());
    }

    #[tokio::test]
    async fn run_insert_command_surfaces_a_non_zero_exit_on_per_item_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/profile"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "emailAddress": "dest@example.com", "messagesTotal": 0, "threadsTotal": 0, "historyId": "1"
            })))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/upload/gmail/v1/users/me/messages",
            ))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("archive");
        write_archived_message(&archive_dir, "m1");

        let selection = Selection::from_args(&SelectionArgs {
            all: true,
            ..Default::default()
        })
        .unwrap();

        let err = run_insert_command(
            &client,
            InsertOptions {
                archive_dir,
                selection,
                limit: 0,
                label: None,
                drop_label_ids: Vec::new(),
                concurrency: DEFAULT_INSERT_CONCURRENCY,
                verify_remote: false,
                dry_run: false,
            },
            true,
            &OutputFormat::Table,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("1 message(s) failed to insert"));
    }

    #[tokio::test]
    async fn run_insert_command_writes_jsonl_report() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/profile"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "emailAddress": "dest@example.com", "messagesTotal": 0, "threadsTotal": 0, "historyId": "1"
            })))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("archive");
        write_archived_message(&archive_dir, "m1");

        let selection = Selection::from_args(&SelectionArgs {
            all: true,
            ..Default::default()
        })
        .unwrap();

        run_insert_command(
            &client,
            InsertOptions {
                archive_dir,
                selection,
                limit: 0,
                label: None,
                drop_label_ids: Vec::new(),
                concurrency: DEFAULT_INSERT_CONCURRENCY,
                verify_remote: false,
                dry_run: true,
            },
            true,
            &OutputFormat::Jsonl,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn execute_passes_flags_through() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/profile"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "emailAddress": "dest@example.com", "messagesTotal": 0, "threadsTotal": 0, "historyId": "1"
            })))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("archive");
        write_archived_message(&archive_dir, "m1");

        let cmd = InsertCommand {
            archive_dir,
            selection: SelectionArgs {
                all: true,
                ..Default::default()
            },
            limit: 0,
            label: None,
            drop_label: Vec::new(),
            concurrency: DEFAULT_INSERT_CONCURRENCY,
            verify_remote: false,
            dry_run: true,
            quiet: true,
            output: OutputFormat::Json,
        };
        cmd.execute(&client).await.unwrap();
    }
}
