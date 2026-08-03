//! CLI command for `omni-dev gmail sync`.
//!
//! Maintains a durable, incrementally-updated local archive of a mailbox as
//! `.eml` files + a JSONL manifest (#1467, Phase 2 of the Gmail
//! integration). See `docs/gmail.md`'s Sync section and
//! [ADR-0064](../../../../docs/adrs/adr-0064.md) for the archive format and
//! its watermark/reconciliation contract; the actual logic lives in
//! `engine.rs` — this file is CLI glue only.

pub(crate) mod engine;
pub(crate) mod manifest;
pub(crate) mod report;
pub(crate) mod shard;
pub(crate) mod state;

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use crate::cli::gmail::format::{output_as, OutputFormat};
use crate::gmail::client::GmailClient;

use engine::SyncOptions;
use report::{SyncAction, SyncReport};

/// Default `--concurrency`: an in-flight-request cap layered under the
/// token-bucket rate limiter (which is the actual quota-compliance
/// mechanism — see `engine.rs`), so it can be generous without risking a
/// quota burst.
const DEFAULT_SYNC_CONCURRENCY: usize = 20;

/// Maintains a durable local archive of a Gmail mailbox (no MCP equivalent
/// — a bulk, potentially long-running filesystem operation is a poor fit
/// for a synchronous MCP tool call).
#[derive(Parser)]
pub struct SyncCommand {
    /// Directory to maintain the archive in. Created if missing.
    #[arg(long, value_name = "PATH")]
    pub output_dir: PathBuf,

    /// Restrict the archive to messages matching this Gmail search query
    /// (same syntax as `gmail search --query`). Only applied on
    /// backfill/`--full`/reconciliation passes — an incremental sync's
    /// `history.list` has no query filter, so newly-arrived mail matching
    /// the query is only picked up by a later `--full` re-run (see
    /// docs/gmail.md's Sync section).
    #[arg(long)]
    pub query: Option<String>,

    /// Forces a full backfill/reconciliation pass even if a valid watermark
    /// exists.
    #[arg(long)]
    pub full: bool,

    /// Bounds concurrent message fetches. Clamped to
    /// `1..=gmail::messages_api::MAX_CONCURRENCY`.
    #[arg(long, default_value_t = DEFAULT_SYNC_CONCURRENCY)]
    pub concurrency: usize,

    /// Reports the planned actions without writing any files.
    #[arg(long)]
    pub dry_run: bool,

    /// Report format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl SyncCommand {
    /// Runs the command against the shared client resolved by the parent
    /// `GmailCommand::execute`.
    pub async fn execute(self, client: &GmailClient) -> Result<()> {
        run_sync_command(
            client,
            SyncOptions {
                output_dir: self.output_dir,
                query: self.query,
                full: self.full,
                concurrency: self.concurrency,
                dry_run: self.dry_run,
            },
            &self.output,
        )
        .await
    }
}

/// Runs the sync and renders its report.
///
/// Split from [`SyncCommand::execute`] so tests can inject a wiremock
/// client without going through the credential-loading path. Mirrors
/// `src/cli/ai/claude/history/sync.rs`'s `execute`: compute the report,
/// render it, and only *then* decide the process exit condition — a
/// non-empty `errors` becomes a failing exit code after everything already
/// ran and printed, never silently.
async fn run_sync_command(
    client: &GmailClient,
    opts: SyncOptions,
    output: &OutputFormat,
) -> Result<()> {
    let report = engine::run_sync(client, &opts).await?;

    if !output_as(&report, output)? {
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        render_report_text(&report, &mut handle)?;
    }

    if !report.errors.is_empty() {
        anyhow::bail!(
            "{} message(s) failed to sync; see errors above",
            report.errors.len()
        );
    }
    Ok(())
}

/// Renders a report as one line per action, then one line per error.
fn render_report_text(report: &SyncReport, out: &mut dyn Write) -> Result<()> {
    if report.actions.is_empty() && report.errors.is_empty() {
        writeln!(out, "Nothing to do.").context("Failed to write sync report")?;
        return Ok(());
    }
    for action in &report.actions {
        let line = match action {
            SyncAction::Fetched { id, path, bytes } => {
                format!("Fetched {id} -> {} ({bytes} bytes)", path.display())
            }
            SyncAction::WouldFetch { id } => format!("Would fetch {id}"),
            SyncAction::LabelsUpdated { id, added, removed } => {
                let mut parts = Vec::new();
                if !added.is_empty() {
                    parts.push(format!("+{}", added.join(",")));
                }
                if !removed.is_empty() {
                    parts.push(format!("-{}", removed.join(",")));
                }
                format!("Labels updated on {id}: {}", parts.join(" "))
            }
            SyncAction::Deleted { id } => format!("Deleted {id}"),
            SyncAction::Undeleted { id } => format!("Undeleted {id}"),
            SyncAction::Note { message } => format!("Note: {message}"),
        };
        writeln!(out, "{line}").context("Failed to write sync report")?;
    }
    for error in &report.errors {
        writeln!(out, "Error: {} failed: {}", error.id, error.reason)
            .context("Failed to write sync report")?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::gmail::auth::{GmailCredentials, GmailScope};
    use crate::utils::secret::Secret;
    use report::SyncError;

    fn test_credentials() -> GmailCredentials {
        GmailCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: GmailScope::ReadOnly,
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

    // ── render_report_text ─────────────────────────────────────────────

    #[test]
    fn render_report_text_reports_nothing_to_do_when_empty() {
        let report = SyncReport::default();
        let mut buf = Vec::new();
        render_report_text(&report, &mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "Nothing to do.\n");
    }

    #[test]
    fn render_report_text_writes_one_line_per_action_and_error() {
        let report = SyncReport {
            actions: vec![
                SyncAction::Fetched {
                    id: "m1".to_string(),
                    path: PathBuf::from("messages/m1/m1.eml"),
                    bytes: 100,
                },
                SyncAction::Deleted {
                    id: "m2".to_string(),
                },
            ],
            errors: vec![SyncError {
                id: "m3".to_string(),
                reason: "boom".to_string(),
            }],
        };
        let mut buf = Vec::new();
        render_report_text(&report, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("Fetched m1"));
        assert!(text.contains("Deleted m2"));
        assert!(text.contains("Error: m3 failed: boom"));
        assert_eq!(text.lines().count(), 3);
    }

    #[test]
    fn render_report_text_formats_label_updates() {
        let report = SyncReport {
            actions: vec![SyncAction::LabelsUpdated {
                id: "m1".to_string(),
                added: vec!["IMPORTANT".to_string()],
                removed: vec!["UNREAD".to_string()],
            }],
            errors: vec![],
        };
        let mut buf = Vec::new();
        render_report_text(&report, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("+IMPORTANT"));
        assert!(text.contains("-UNREAD"));
    }

    // ── run_sync_command / SyncCommand::execute glue ────────────────────

    #[tokio::test]
    async fn run_sync_command_dry_run_reports_would_fetch_and_touches_no_files() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/profile"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "emailAddress": "user@example.com", "messagesTotal": 1, "threadsTotal": 1, "historyId": "1"
            })))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "messages": [{"id": "m1", "threadId": "t1"}]
                })),
            )
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("archive");
        run_sync_command(
            &client,
            SyncOptions {
                output_dir: output_dir.clone(),
                query: None,
                full: false,
                concurrency: 4,
                dry_run: true,
            },
            &OutputFormat::Table,
        )
        .await
        .unwrap();

        assert!(!output_dir.exists());
    }

    #[tokio::test]
    async fn run_sync_command_surfaces_a_non_zero_exit_on_per_item_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/profile"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "emailAddress": "user@example.com", "messagesTotal": 1, "threadsTotal": 1, "historyId": "1"
            })))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "messages": [{"id": "m1", "threadId": "t1"}]
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages/m1"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let err = run_sync_command(
            &client,
            SyncOptions {
                output_dir: dir.path().join("archive"),
                query: None,
                full: false,
                concurrency: 4,
                dry_run: false,
            },
            &OutputFormat::Table,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("1 message(s) failed"));
    }

    #[tokio::test]
    async fn execute_passes_flags_through() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/profile"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "emailAddress": "user@example.com", "messagesTotal": 0, "threadsTotal": 0, "historyId": "1"
            })))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"messages": []})),
            )
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let cmd = SyncCommand {
            output_dir: dir.path().join("archive"),
            query: None,
            full: false,
            concurrency: DEFAULT_SYNC_CONCURRENCY,
            dry_run: false,
            output: OutputFormat::Json,
        };
        cmd.execute(&client).await.unwrap();
    }
}
