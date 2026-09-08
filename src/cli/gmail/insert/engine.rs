//! The algorithm behind `gmail insert` (#1655): restore archived `.eml`
//! messages into a mailbox via `messages.insert`, deduplicated against a
//! local ledger so a re-run or a resumed interrupted run never duplicates
//! mail.
//!
//! Structured as: resolve the plan entirely from local state (no network),
//! two cheap preflight reads (destination identity, label resolution),
//! short-circuit under `--dry-run`, then a throttled, bounded fan-out that
//! owns the ledger and report single-threaded as results arrive — mirroring
//! `sync::engine::fetch_and_archive_messages`'s shape (the non-streaming
//! variant: the whole plan is already known up front here, so there is no
//! `sync`-style pipelined listing to interleave with). Performs no
//! stdout/stderr I/O itself (ADR-0064 Decision 4) — the CLI layer renders
//! the report and only then decides the process exit code.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use futures::stream::{self, StreamExt as _};
use tokio::sync::mpsc;

use crate::cli::gmail::selection::Selection;
use crate::cli::gmail::sync::engine::manifest_path;
use crate::cli::gmail::sync::manifest::{Manifest, ManifestRecord};
use crate::gmail::client::GmailClient;
use crate::gmail::labels_api::LabelsApi;
use crate::gmail::messages_api::{
    MessagesApi, MAX_CONCURRENCY, MESSAGES_INSERT_COST_UNITS, MESSAGES_LIST_COST_UNITS,
};
use crate::gmail::profile_api::ProfileApi;
use crate::gmail::raw_message::extract_headers;

use super::labels::{
    lands_in_inbox_or_unread, lands_in_trash_or_spam, resolve_label_id_by_name, resolve_label_ids,
};
use super::ledger::{
    dedupe_key, ledger_path, InsertLedger, InsertLedgerRecord, InsertOrigin, LedgerLock,
};
use super::progress::InsertProgressEvent;
use super::report::{InsertAction, InsertError, InsertReport, SkipReason};

/// Default `--concurrency`. Deliberately far below `sync`'s default of 20:
/// `messages.insert` costs [`MESSAGES_INSERT_COST_UNITS`] (25) against a
/// 250-units/second bucket, so the limiter admits roughly 10 requests/second
/// regardless of how wide the fan-out is — a wider `buffer_unordered` window
/// buys no extra throughput here, and only widens the set of inserts that
/// are in flight (and thus un-ledgered) if the process crashes mid-run.
pub(crate) const DEFAULT_INSERT_CONCURRENCY: usize = 4;

/// Number of ledger entries between checkpoints during the fan-out — an
/// eighth of `sync::engine::MANIFEST_CHECKPOINT_INTERVAL` (200), because
/// there a lost checkpoint interval costs an idempotent re-fetch and here it
/// costs messages duplicated into a live mailbox that no re-run can undo.
const INSERT_LEDGER_CHECKPOINT_INTERVAL: usize = 25;

/// Quota-unit cost of one `rfc822msgid:` probe search — a `messages.list`
/// call, so it shares that endpoint's cost.
const REMOTE_PROBE_COST_UNITS: u32 = MESSAGES_LIST_COST_UNITS;

/// One message queued for the fan-out: `(source id, .eml path relative to
/// the archive dir, resolved label ids, dedupe key, archived Message-ID)`.
type PlannedInsert = (String, PathBuf, Vec<String>, String, Option<String>);

/// Options for [`run_insert`]/[`run_insert_with_progress`].
pub(crate) struct InsertOptions {
    pub(crate) archive_dir: PathBuf,
    pub(crate) selection: Selection,
    /// `0` means "no limit" — applied after selection, to the oldest-first
    /// plan.
    pub(crate) limit: usize,
    /// The `--label` tag name, resolved against `labels.list` during
    /// preflight.
    pub(crate) label: Option<String>,
    pub(crate) drop_label_ids: Vec<String>,
    pub(crate) concurrency: usize,
    pub(crate) verify_remote: bool,
    pub(crate) dry_run: bool,
}

/// Runs an insert with no live progress reporting — see
/// [`run_insert_with_progress`].
pub(crate) async fn run_insert(client: &GmailClient, opts: &InsertOptions) -> Result<InsertReport> {
    run_insert_with_progress(client, opts, None).await
}

/// Runs one insert: resolves the plan locally, does preflight reads, then
/// (unless `--dry-run`) fans out inserts against the destination mailbox.
pub(crate) async fn run_insert_with_progress(
    client: &GmailClient,
    opts: &InsertOptions,
    progress: Option<&mpsc::UnboundedSender<InsertProgressEvent>>,
) -> Result<InsertReport> {
    let mut report = InsertReport::default();
    let manifest = Manifest::load(&manifest_path(&opts.archive_dir))?;

    // An explicitly-named id absent from the archive is an error, never a
    // silent drop — the caller asked for a specific message by id.
    for id in opts.selection.requested_ids() {
        anyhow::ensure!(
            manifest.get(id).is_some(),
            "--id {id} was not found in the archive manifest at {}",
            opts.archive_dir.display()
        );
    }

    let mut planned: Vec<&ManifestRecord> = manifest
        .records_not_deleted()
        .filter(|record| opts.selection.matches(record))
        .collect();
    // Oldest-first: maximises the chance a message's thread parent has
    // already landed on the destination by the time a reply is inserted,
    // and gives a deterministic order independent of the manifest's
    // by-Gmail-id `BTreeMap` iteration order. A record with no parseable
    // `internal_date` sorts first (treated as "unknown, assume oldest")
    // rather than panicking or being dropped.
    planned.sort_by_key(|record| record.internal_date_utc());
    if opts.limit > 0 {
        planned.truncate(opts.limit);
    }

    let profile = ProfileApi::new(client)
        .get()
        .await
        .context("Failed to fetch the destination mailbox's profile")?;
    let destination = profile.email_address;
    report.actions.push(InsertAction::Note {
        message: format!("Inserting into {destination}"),
    });

    let destination_label_id = match &opts.label {
        Some(name) => {
            let labels = LabelsApi::new(client)
                .list()
                .await
                .context("Failed to list destination labels")?;
            Some(resolve_label_id_by_name(&labels.labels, name)?)
        }
        None => None,
    };

    let mut inbox_or_unread_count = 0usize;
    let mut trash_or_spam_count = 0usize;
    let mut plan: Vec<(&ManifestRecord, Vec<String>)> = Vec::with_capacity(planned.len());
    for record in planned {
        let label_ids = resolve_label_ids(
            &record.label_ids,
            &opts.drop_label_ids,
            destination_label_id.as_deref(),
        );
        if lands_in_inbox_or_unread(&label_ids) {
            inbox_or_unread_count += 1;
        }
        if lands_in_trash_or_spam(&label_ids) {
            trash_or_spam_count += 1;
        }
        plan.push((record, label_ids));
    }
    if inbox_or_unread_count > 0 {
        report.actions.push(InsertAction::Note {
            message: format!(
                "{inbox_or_unread_count} message(s) will land in INBOX/UNREAD — use \
                 --drop-label INBOX --drop-label UNREAD to restore already-read/archived instead"
            ),
        });
    }
    if trash_or_spam_count > 0 {
        report.actions.push(InsertAction::Note {
            message: format!(
                "{trash_or_spam_count} message(s) will land in TRASH/SPAM, which Gmail \
                 auto-purges after 30 days"
            ),
        });
    }

    let ledger_file = ledger_path(&opts.archive_dir);
    let mut ledger = InsertLedger::load(&ledger_file)?;

    // Ledger-dedupe up front (cheap, no network) regardless of `--dry-run`
    // — this is also where a `WouldInsert`/`Skipped` split happens for the
    // dry-run report.
    let mut to_process: Vec<PlannedInsert> = Vec::new();
    for (record, label_ids) in plan {
        let key = dedupe_key(record.rfc822_msgid.as_deref(), &record.id);
        if ledger.contains(&destination, &key) {
            report.actions.push(InsertAction::Skipped {
                id: record.id.clone(),
                reason: SkipReason::AlreadyInserted,
            });
            continue;
        }
        if opts.dry_run {
            report.actions.push(InsertAction::WouldInsert {
                id: record.id.clone(),
                label_ids,
            });
            continue;
        }
        to_process.push((
            record.id.clone(),
            record.path.clone(),
            label_ids,
            key,
            record.rfc822_msgid.clone(),
        ));
    }

    if opts.dry_run || to_process.is_empty() {
        return Ok(report);
    }

    // Held for the run's duration only — `--dry-run` never reaches here, so
    // it never takes the lock or touches the ledger file.
    let _lock = LedgerLock::acquire(&opts.archive_dir)?;

    if let Some(tx) = progress {
        let _ = tx.send(InsertProgressEvent::Started {
            total: to_process.len(),
        });
    }

    let limiter = crate::utils::rate_limit::TokenBucket::new(
        crate::gmail::messages_api::GMAIL_QUOTA_UNITS_PER_SECOND,
        crate::gmail::messages_api::GMAIL_QUOTA_UNITS_PER_SECOND,
    );
    let concurrency = opts.concurrency.clamp(1, MAX_CONCURRENCY);

    let insert_result = insert_all(
        client,
        &opts.archive_dir,
        to_process,
        &limiter,
        concurrency,
        opts.verify_remote,
        &destination,
        &mut ledger,
        &mut report,
        progress,
    )
    .await;

    // Guaranteed final flush regardless of the fan-out's outcome — a failed
    // flush outranks a failed run: the run's own error costs a retry, the
    // flush's costs duplicates no re-run can undo, so it must never be
    // silently swallowed behind a run error.
    let flush_result = ledger.save(&ledger_file);
    match (insert_result, flush_result) {
        (Ok(()), Ok(())) => Ok(report),
        (Ok(()), Err(flush_err)) => Err(flush_err).context(
            "the insert run succeeded but the ledger failed to save afterward — re-run \
             `gmail insert --verify-remote` before inserting again, since this ledger is the \
             sole record of what has already been inserted",
        ),
        (Err(run_err), Ok(())) => Err(run_err),
        (Err(run_err), Err(flush_err)) => Err(flush_err)
            .context(format!("the insert run also failed: {run_err:#}"))
            .context(
                "the ledger failed to save after a failed run — re-run `gmail insert \
                 --verify-remote` before inserting again, since this ledger is the sole record \
                 of what has already been inserted",
            ),
    }
}

/// One planned insert's outcome, before it's translated into a
/// report action / ledger record.
enum InsertOutcome {
    Inserted {
        inserted_id: String,
        inserted_thread_id: Option<String>,
    },
    FoundRemote,
}

/// The bounded, throttled fan-out over `to_process`, and the single-threaded
/// drain loop that owns `ledger`/`report` as results arrive. A per-message
/// failure is pushed to `report.errors` and never aborts the batch —
/// mirrors `sync::engine`'s fetch loop.
#[allow(clippy::too_many_arguments)]
async fn insert_all(
    client: &GmailClient,
    archive_dir: &Path,
    to_process: Vec<PlannedInsert>,
    limiter: &crate::utils::rate_limit::TokenBucket,
    concurrency: usize,
    verify_remote: bool,
    destination: &str,
    ledger: &mut InsertLedger,
    report: &mut InsertReport,
    progress: Option<&mpsc::UnboundedSender<InsertProgressEvent>>,
) -> Result<()> {
    let mut fetches = stream::iter(to_process)
        .map(|(id, relative_path, label_ids, key, rfc822_msgid)| {
            let archive_dir = archive_dir.to_path_buf();
            async move {
                let eml_path = archive_dir.join(&relative_path);
                let bytes = match std::fs::read(&eml_path) {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        return (
                            id,
                            key,
                            label_ids,
                            Err(anyhow::anyhow!(
                                "Failed to read {}: {e}",
                                eml_path.display()
                            )),
                        )
                    }
                };

                if verify_remote && rfc822_msgid.is_some() {
                    limiter.acquire(REMOTE_PROBE_COST_UNITS).await;
                    match probe_remote(client, &key).await {
                        Ok(true) => return (id, key, label_ids, Ok(InsertOutcome::FoundRemote)),
                        Ok(false) => {}
                        Err(e) => return (id, key, label_ids, Err(e)),
                    }
                }

                limiter.acquire(MESSAGES_INSERT_COST_UNITS).await;
                let label_refs: Vec<&str> = label_ids.iter().map(String::as_str).collect();
                let result = MessagesApi::new(client)
                    .insert(&bytes, &label_refs)
                    .await
                    .map(|message| InsertOutcome::Inserted {
                        inserted_id: message.id,
                        inserted_thread_id: message.thread_id,
                    });
                (id, key, label_ids, result)
            }
        })
        .buffer_unordered(concurrency);

    let mut since_checkpoint = 0usize;
    while let Some((id, key, label_ids, result)) = fetches.next().await {
        let mut failed = false;
        match result {
            Ok(InsertOutcome::Inserted {
                inserted_id,
                inserted_thread_id,
            }) => {
                ledger.record(InsertLedgerRecord {
                    destination: destination.to_string(),
                    key,
                    source_id: id.clone(),
                    inserted_id: Some(inserted_id.clone()),
                    inserted_thread_id,
                    inserted_at: Utc::now(),
                    label_ids: label_ids.clone(),
                    origin: InsertOrigin::Inserted,
                });
                report.actions.push(InsertAction::Inserted {
                    id,
                    inserted_id,
                    label_ids,
                });
            }
            Ok(InsertOutcome::FoundRemote) => {
                ledger.record(InsertLedgerRecord {
                    destination: destination.to_string(),
                    key,
                    source_id: id.clone(),
                    inserted_id: None,
                    inserted_thread_id: None,
                    inserted_at: Utc::now(),
                    label_ids: Vec::new(),
                    origin: InsertOrigin::RemoteProbe,
                });
                report.actions.push(InsertAction::Skipped {
                    id,
                    reason: SkipReason::FoundRemote,
                });
            }
            Err(e) => {
                failed = true;
                report.errors.push(InsertError {
                    id,
                    reason: format!("{e:#}"),
                });
            }
        }
        since_checkpoint += 1;
        if since_checkpoint >= INSERT_LEDGER_CHECKPOINT_INTERVAL {
            ledger.save(&ledger_path(archive_dir))?;
            since_checkpoint = 0;
        }
        if let Some(tx) = progress {
            let _ = tx.send(InsertProgressEvent::Completed { failed });
        }
    }
    Ok(())
}

/// Probes the destination mailbox for a message already carrying `key` as
/// its `Message-ID` (`--verify-remote`'s supplement to the ledger — see the
/// module doc and `ledger.rs`'s doc comment for why it's a supplement, not
/// a substitute: `rfc822msgid:` search lags a real insert by seconds to
/// minutes).
async fn probe_remote(client: &GmailClient, key: &str) -> Result<bool> {
    let query = format!("rfc822msgid:{key} in:anywhere");
    let result = MessagesApi::new(client)
        .search(Some(&query), &[], 1, None)
        .await?;
    Ok(!result.messages.is_empty())
}

/// Extracts the `Date` header from an already-read `.eml`'s bytes, purely to
/// let the caller emit an informational `Note` when it's missing or
/// unparseable — `insert` has no way to override the internal date Gmail
/// assigns in that case (unlike IMAP `APPEND`'s explicit `INTERNALDATE`),
/// and synthesising a `Date:` header is rejected: it would break
/// byte-exactness and invalidate DKIM, turning a faithful archive into a
/// forged one.
#[allow(dead_code)] // wired into the fan-out's per-message Note in a follow-up pass
fn date_header_present(raw_eml: &[u8]) -> bool {
    extract_headers(raw_eml, &["Date"]).contains_key("Date")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::cli::gmail::selection::SelectionArgs;

    fn selection_all() -> Selection {
        Selection::from_args(&SelectionArgs {
            all: true,
            ..SelectionArgs::default()
        })
        .unwrap()
    }

    fn base_opts(archive_dir: PathBuf) -> InsertOptions {
        InsertOptions {
            archive_dir,
            selection: selection_all(),
            limit: 0,
            label: None,
            drop_label_ids: Vec::new(),
            concurrency: DEFAULT_INSERT_CONCURRENCY,
            verify_remote: false,
            dry_run: false,
        }
    }

    fn write_archived_message(
        archive_dir: &Path,
        id: &str,
        rfc822_msgid: Option<&str>,
        label_ids: &[&str],
        raw: &str,
    ) {
        let path = PathBuf::from(format!("messages/{id}.eml"));
        std::fs::create_dir_all(archive_dir.join(path.parent().unwrap())).unwrap();
        std::fs::write(archive_dir.join(&path), raw).unwrap();

        let mut manifest = Manifest::load(&manifest_path(archive_dir)).unwrap();
        manifest.upsert(ManifestRecord {
            id: id.to_string(),
            thread_id: None,
            label_ids: label_ids
                .iter()
                .map(std::string::ToString::to_string)
                .collect(),
            internal_date: Some("1700000000000".to_string()),
            subject: None,
            from: None,
            to: None,
            rfc822_msgid: rfc822_msgid.map(str::to_string),
            in_reply_to: None,
            references: None,
            attachment_count: 0,
            attachment_filenames: Vec::new(),
            path,
            size: raw.len() as u64,
            history_id: None,
            deleted_at: None,
        });
        manifest.save(&manifest_path(archive_dir)).unwrap();
    }

    fn test_credentials() -> crate::gmail::auth::GmailCredentials {
        crate::gmail::auth::GmailCredentials {
            client_id: "client-1".to_string(),
            client_secret: crate::utils::secret::Secret::new("secret-1"),
            refresh_token: crate::utils::secret::Secret::new("refresh-1"),
            scope: crate::gmail::auth::GmailScope::Modify,
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

    async fn mount_profile(server: &wiremock::MockServer, address: &str) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/profile"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "emailAddress": address, "messagesTotal": 1, "threadsTotal": 1, "historyId": "1"
                })),
            )
            .mount(server)
            .await;
    }

    async fn mount_insert_success(server: &wiremock::MockServer, inserted_id: &str) {
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/upload/gmail/v1/users/me/messages",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": inserted_id})),
            )
            .mount(server)
            .await;
    }

    // ── dry run ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn dry_run_makes_no_upload_call_and_writes_no_ledger() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_profile(&server, "dest@example.com").await;
        // No insert mock mounted — a stray call would panic the mock server.

        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("archive");
        write_archived_message(
            &archive_dir,
            "m1",
            Some("<m1@example.com>"),
            &["INBOX"],
            "From: a@example.com\r\n\r\nbody",
        );

        let report = run_insert(
            &client,
            &InsertOptions {
                dry_run: true,
                ..base_opts(archive_dir.clone())
            },
        )
        .await
        .unwrap();

        assert_eq!(report.summary().would_insert, 1);
        assert!(!ledger_path(&archive_dir).exists());
    }

    // ── acceptance criteria ──────────────────────────────────────────

    #[tokio::test]
    async fn rerunning_the_same_selection_inserts_nothing_the_second_time() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_profile(&server, "dest@example.com").await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/upload/gmail/v1/users/me/messages",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "new1"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("archive");
        write_archived_message(
            &archive_dir,
            "m1",
            Some("<m1@example.com>"),
            &["INBOX"],
            "From: a@example.com\r\n\r\nbody",
        );

        let first = run_insert(&client, &base_opts(archive_dir.clone()))
            .await
            .unwrap();
        assert_eq!(first.summary().inserted, 1);

        let second = run_insert(&client, &base_opts(archive_dir.clone()))
            .await
            .unwrap();
        assert_eq!(second.summary().inserted, 0);
        assert_eq!(second.summary().skipped, 1);
    }

    #[tokio::test]
    async fn an_interrupted_run_resumes_without_duplicating() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_profile(&server, "dest@example.com").await;
        mount_insert_success(&server, "new-any").await;

        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("archive");
        for (id, msgid) in [
            ("m1", "<m1@example.com>"),
            ("m2", "<m2@example.com>"),
            ("m3", "<m3@example.com>"),
        ] {
            write_archived_message(
                &archive_dir,
                id,
                Some(msgid),
                &["INBOX"],
                &format!("From: a@example.com\r\nMessage-ID: {msgid}\r\n\r\nbody {id}"),
            );
        }

        // Pre-seed the ledger as if m1 and m2 were already inserted in an
        // earlier, interrupted run.
        let mut ledger = InsertLedger::load(&ledger_path(&archive_dir)).unwrap();
        for id in ["m1", "m2"] {
            ledger.record(InsertLedgerRecord {
                destination: "dest@example.com".to_string(),
                key: format!("{id}@example.com"),
                source_id: id.to_string(),
                inserted_id: Some(format!("already-{id}")),
                inserted_thread_id: None,
                inserted_at: Utc::now(),
                label_ids: vec!["INBOX".to_string()],
                origin: InsertOrigin::Inserted,
            });
        }
        ledger.save(&ledger_path(&archive_dir)).unwrap();

        let report = run_insert(&client, &base_opts(archive_dir.clone()))
            .await
            .unwrap();
        assert_eq!(report.summary().inserted, 1);
        assert_eq!(report.summary().skipped, 2);

        let requests = server.received_requests().await.unwrap();
        let insert_calls = requests
            .iter()
            .filter(|r| r.url.path() == "/upload/gmail/v1/users/me/messages")
            .count();
        assert_eq!(
            insert_calls, 1,
            "only the third (m3) message should be inserted"
        );
    }

    // ── system-label replay on the wire ───────────────────────────────

    #[tokio::test]
    async fn foreign_user_labels_are_dropped_on_the_wire() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_profile(&server, "dest@example.com").await;
        mount_insert_success(&server, "new1").await;

        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("archive");
        write_archived_message(
            &archive_dir,
            "m1",
            Some("<m1@example.com>"),
            &["INBOX", "Label_1", "DRAFT"],
            "From: a@example.com\r\n\r\nbody",
        );

        run_insert(&client, &base_opts(archive_dir.clone()))
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        let insert_request = requests
            .iter()
            .find(|r| r.url.path() == "/upload/gmail/v1/users/me/messages")
            .unwrap();
        let body = String::from_utf8_lossy(&insert_request.body);
        assert!(body.contains("\"labelIds\":[\"INBOX\"]"));
        assert!(!body.contains("Label_1"));
        assert!(!body.contains("DRAFT"));
    }

    // ── --verify-remote ────────────────────────────────────────────────

    #[tokio::test]
    async fn verify_remote_skips_a_probed_hit_and_inserts_on_a_miss() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_profile(&server, "dest@example.com").await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages"))
            .and(wiremock::matchers::query_param(
                "q",
                "rfc822msgid:m1@example.com in:anywhere",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "messages": [{"id": "already-there", "threadId": "t1"}]
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages"))
            .and(wiremock::matchers::query_param(
                "q",
                "rfc822msgid:m2@example.com in:anywhere",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"messages": []})),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/upload/gmail/v1/users/me/messages",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "new2"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("archive");
        write_archived_message(
            &archive_dir,
            "m1",
            Some("<m1@example.com>"),
            &["INBOX"],
            "From: a@example.com\r\n\r\nbody1",
        );
        write_archived_message(
            &archive_dir,
            "m2",
            Some("<m2@example.com>"),
            &["INBOX"],
            "From: a@example.com\r\n\r\nbody2",
        );

        let report = run_insert(
            &client,
            &InsertOptions {
                verify_remote: true,
                ..base_opts(archive_dir)
            },
        )
        .await
        .unwrap();

        assert_eq!(report.summary().inserted, 1);
        assert_eq!(report.summary().skipped, 1);
        assert!(report.actions.iter().any(|a| matches!(
            a,
            InsertAction::Skipped { id, reason: SkipReason::FoundRemote } if id == "m1"
        )));
    }

    // ── partial failure ──────────────────────────────────────────────

    #[tokio::test]
    async fn a_mid_batch_failure_leaves_the_others_inserted_and_a_partial_ledger() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_profile(&server, "dest@example.com").await;

        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("archive");
        write_archived_message(
            &archive_dir,
            "m1",
            Some("<m1@example.com>"),
            &["INBOX"],
            "From: a@example.com\r\nMessage-ID: <m1@example.com>\r\n\r\nbody1",
        );
        write_archived_message(
            &archive_dir,
            "m2",
            Some("<m2@example.com>"),
            &["INBOX"],
            "From: a@example.com\r\nMessage-ID: <m2@example.com>\r\n\r\nbody2",
        );

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/upload/gmail/v1/users/me/messages",
            ))
            .and(wiremock::matchers::body_string_contains("body1"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/upload/gmail/v1/users/me/messages",
            ))
            .and(wiremock::matchers::body_string_contains("body2"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "new2"})),
            )
            .mount(&server)
            .await;

        let report = run_insert(
            &client,
            &InsertOptions {
                concurrency: 1,
                ..base_opts(archive_dir.clone())
            },
        )
        .await
        .unwrap();

        assert_eq!(report.summary().inserted, 1);
        assert_eq!(report.summary().errors, 1);

        let ledger = InsertLedger::load(&ledger_path(&archive_dir)).unwrap();
        assert!(ledger.contains("dest@example.com", "m2@example.com"));
        assert!(!ledger.contains("dest@example.com", "m1@example.com"));
    }

    // ── requested-id validation ────────────────────────────────────────

    #[tokio::test]
    async fn a_requested_id_absent_from_the_manifest_is_an_error() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;

        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("archive");
        std::fs::create_dir_all(&archive_dir).unwrap();

        let selection = Selection::from_args(&SelectionArgs {
            ids: vec!["missing".to_string()],
            ..SelectionArgs::default()
        })
        .unwrap();

        let err = run_insert(
            &client,
            &InsertOptions {
                selection,
                ..base_opts(archive_dir)
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("missing"));
        assert!(err.to_string().contains("was not found"));
    }

    // ── destination scoping (the identity guard, end to end) ──────────

    #[tokio::test]
    async fn a_ledger_from_a_different_destination_does_not_suppress_a_new_insert() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_profile(&server, "dest@example.com").await;
        mount_insert_success(&server, "new1").await;

        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("archive");
        write_archived_message(
            &archive_dir,
            "m1",
            Some("<m1@example.com>"),
            &["INBOX"],
            "From: a@example.com\r\n\r\nbody",
        );

        let mut ledger = InsertLedger::load(&ledger_path(&archive_dir)).unwrap();
        ledger.record(InsertLedgerRecord {
            destination: "other@example.com".to_string(),
            key: "m1@example.com".to_string(),
            source_id: "m1".to_string(),
            inserted_id: Some("elsewhere".to_string()),
            inserted_thread_id: None,
            inserted_at: Utc::now(),
            label_ids: vec!["INBOX".to_string()],
            origin: InsertOrigin::Inserted,
        });
        ledger.save(&ledger_path(&archive_dir)).unwrap();

        let report = run_insert(&client, &base_opts(archive_dir)).await.unwrap();
        assert_eq!(report.summary().inserted, 1);
    }

    // ── date_header_present ──────────────────────────────────────────

    #[test]
    fn date_header_present_detects_presence_and_absence() {
        assert!(date_header_present(
            b"Date: Mon, 1 Jan 2026 00:00:00 +0000\r\n\r\nbody"
        ));
        assert!(!date_header_present(b"From: a@example.com\r\n\r\nbody"));
    }
}
