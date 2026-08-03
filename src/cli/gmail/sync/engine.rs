//! `gmail sync`'s control flow: backfill vs. incremental, 404-triggered
//! reconciliation, and the throttled fetch fan-out.
//!
//! [`run_sync`] does no stdout/stderr I/O itself — it returns a
//! [`SyncReport`] for the caller (`src/cli/gmail/sync/mod.rs`) to render and
//! turn into a process exit code, mirroring
//! `src/cli/ai/claude/history/sync.rs::run`'s compute → render → decide
//! split.
//!
//! Presence-on-disk is the real idempotence mechanism (an interrupted
//! backfill needs no cursor to resume correctly): backfill, `--full`, and
//! 404-triggered reconciliation are therefore all the *same* code path,
//! [`run_full_sync`], which lists the whole mailbox and fetches only what's
//! missing on disk.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use futures::stream::{self, StreamExt as _};

use crate::gmail::client::GmailClient;
use crate::gmail::error::GmailError;
use crate::gmail::history_api::HistoryApi;
use crate::gmail::messages_api::{
    MessageFormat, MessagesApi, GMAIL_QUOTA_UNITS_PER_SECOND, MAX_CONCURRENCY,
    MESSAGES_GET_COST_UNITS,
};
use crate::gmail::profile_api::{Profile, ProfileApi};
use crate::gmail::raw_message::{
    decode_raw_message, extract_attachment_filenames, extract_headers,
};
use crate::utils::rate_limit::TokenBucket;

use super::manifest::{Manifest, ManifestRecord};
use super::report::{SyncAction, SyncError, SyncReport};
use super::shard::shard_path;
use super::state::{self, ArchiveState, LoadOutcome};

/// Options for one `gmail sync` invocation.
pub(crate) struct SyncOptions {
    pub(crate) output_dir: PathBuf,
    pub(crate) query: Option<String>,
    pub(crate) full: bool,
    pub(crate) concurrency: usize,
    pub(crate) dry_run: bool,
}

fn state_path(output_dir: &Path) -> PathBuf {
    output_dir.join("state.json")
}

fn manifest_path(output_dir: &Path) -> PathBuf {
    output_dir.join("manifest.jsonl")
}

/// Runs one sync: resolves identity, decides backfill vs. incremental (with
/// 404 fallback), fetches whatever's missing, and returns a report. Never
/// panics on per-message failures — see [`fetch_and_archive_messages`].
pub(crate) async fn run_sync(client: &GmailClient, opts: &SyncOptions) -> Result<SyncReport> {
    guard_output_dir(&opts.output_dir)?;
    if !opts.dry_run {
        let messages_dir = opts.output_dir.join("messages");
        std::fs::create_dir_all(&messages_dir)
            .with_context(|| format!("Failed to create {}", messages_dir.display()))?;
    }

    let profile = ProfileApi::new(client).get().await?;
    let loaded = state::load(&state_path(&opts.output_dir));
    let mut manifest = Manifest::load(&manifest_path(&opts.output_dir))?;
    let mut report = SyncReport::default();
    let limiter = TokenBucket::new(GMAIL_QUOTA_UNITS_PER_SECOND, GMAIL_QUOTA_UNITS_PER_SECOND);

    let history_id = match loaded {
        LoadOutcome::Present(state) if !opts.full => {
            state::validate_identity(&state, &profile.email_address)?;
            match run_incremental(
                client,
                &mut manifest,
                &state.history_id,
                opts,
                &limiter,
                &mut report,
            )
            .await
            {
                Ok(id) => id,
                Err(e) if is_history_not_found(&e) => {
                    report.actions.push(SyncAction::Note {
                        message: "watermark expired (404 on startHistoryId); reconciling"
                            .to_string(),
                    });
                    run_full_sync(client, &mut manifest, &profile, opts, &limiter, &mut report)
                        .await?
                }
                Err(e) => return Err(e),
            }
        }
        LoadOutcome::Present(state) => {
            // `--full` with an otherwise-valid state.json still validates
            // identity — a forced reconciliation is not an excuse to skip
            // the one check that prevents mixing two mailboxes.
            state::validate_identity(&state, &profile.email_address)?;
            run_full_sync(client, &mut manifest, &profile, opts, &limiter, &mut report).await?
        }
        LoadOutcome::Absent => {
            run_full_sync(client, &mut manifest, &profile, opts, &limiter, &mut report).await?
        }
        LoadOutcome::Corrupt(reason) => {
            report.actions.push(SyncAction::Note {
                message: format!("state.json unreadable ({reason}); reconciling"),
            });
            run_full_sync(client, &mut manifest, &profile, opts, &limiter, &mut report).await?
        }
    };

    if !opts.dry_run {
        // Always saved: successful per-message mutations (fetches, label
        // deltas, soft-deletes) are independently safe to keep even when
        // this run also hit errors elsewhere.
        manifest.save(&manifest_path(&opts.output_dir))?;
        // The watermark, in contrast, only advances on a clean run — an
        // advanced watermark past a failed fetch would scroll that history
        // event out of Gmail's retention window forever. Withholding it
        // means the next run re-examines the same range for free (already
        //-fetched messages/labels are skipped via presence-on-disk /
        // idempotent mutation).
        if report.errors.is_empty() {
            state::save(
                &ArchiveState {
                    history_id,
                    email_address: profile.email_address,
                    last_sync: Utc::now(),
                    query: opts.query.clone(),
                },
                &state_path(&opts.output_dir),
            )?;
        }
    }
    Ok(report)
}

/// Backfill, `--full`, and 404-triggered reconciliation are all this same
/// path: list every message currently on the server, fetch whatever's
/// missing on disk (an interrupted prior run's already-archived messages
/// are skipped for free), and soft-delete manifest records for ids that no
/// longer appear.
async fn run_full_sync(
    client: &GmailClient,
    manifest: &mut Manifest,
    profile: &Profile,
    opts: &SyncOptions,
    limiter: &TokenBucket,
    report: &mut SyncReport,
) -> Result<String> {
    let listed = MessagesApi::new(client)
        .search_all_unbounded(opts.query.as_deref(), &[], limiter)
        .await?;
    let listed_ids: HashSet<String> = listed.messages.iter().map(|m| m.id.clone()).collect();

    let to_fetch: Vec<String> = listed_ids
        .iter()
        .filter(|id| match manifest.get(id) {
            None => true,
            Some(record) => !opts.output_dir.join(&record.path).exists(),
        })
        .cloned()
        .collect();

    for id in &listed_ids {
        if manifest.get(id).is_some_and(|r| r.deleted_at.is_some()) {
            manifest.undelete(id);
            report
                .actions
                .push(SyncAction::Undeleted { id: id.clone() });
        }
    }

    fetch_and_archive_messages(client, manifest, &to_fetch, limiter, opts, report).await;

    let stale: Vec<String> = manifest
        .ids_not_deleted()
        .filter(|id| !listed_ids.contains(*id))
        .map(str::to_string)
        .collect();
    for id in stale {
        manifest.mark_deleted(&id, Utc::now());
        report.actions.push(SyncAction::Deleted { id });
    }

    // Snapshotted *before* the listing began (by the caller, via `profile`)
    // — mail arriving mid-listing is simply re-observed as an ordinary
    // `messagesAdded` event on the next incremental run, a self-healing
    // race window rather than a gap.
    Ok(profile.history_id.clone())
}

/// Applies `messagesAdded`/`messagesDeleted`/`labelsAdded`/`labelsRemoved`
/// history events since `start_history_id`. A 404 (watermark past Gmail's
/// retention window) propagates unchanged for [`run_sync`] to catch.
async fn run_incremental(
    client: &GmailClient,
    manifest: &mut Manifest,
    start_history_id: &str,
    opts: &SyncOptions,
    limiter: &TokenBucket,
    report: &mut SyncReport,
) -> Result<String> {
    let history = HistoryApi::new(client)
        .list_all_unbounded(start_history_id, &[], limiter)
        .await?;

    let mut seen = HashSet::new();
    let mut to_fetch = Vec::new();
    for record in &history.history {
        for added in &record.messages_added {
            if seen.insert(added.message.id.clone()) {
                to_fetch.push(added.message.id.clone());
            }
        }
        for deleted in &record.messages_deleted {
            manifest.mark_deleted(&deleted.message.id, Utc::now());
            report.actions.push(SyncAction::Deleted {
                id: deleted.message.id.clone(),
            });
        }
        for change in &record.labels_added {
            manifest.add_labels(&change.message.id, &change.label_ids);
            report.actions.push(SyncAction::LabelsUpdated {
                id: change.message.id.clone(),
                added: change.label_ids.clone(),
                removed: Vec::new(),
            });
        }
        for change in &record.labels_removed {
            manifest.remove_labels(&change.message.id, &change.label_ids);
            report.actions.push(SyncAction::LabelsUpdated {
                id: change.message.id.clone(),
                added: Vec::new(),
                removed: change.label_ids.clone(),
            });
        }
    }

    fetch_and_archive_messages(client, manifest, &to_fetch, limiter, opts, report).await;

    Ok(history
        .history_id
        .unwrap_or_else(|| start_history_id.to_string()))
}

/// The one place presence-on-disk is checked before any fetch, and the
/// bounded, throttled fan-out over whatever's actually missing.
///
/// A per-message failure is pushed to `report.errors` and never aborts the
/// batch — the opposite of the Facebook harvester's whole-run-fatal
/// `?`-propagation. Concurrency is bounded by `buffer_unordered` (order
/// doesn't matter for archival, unlike `search`'s `buffered`), and each
/// future acquires its slot in that bound before drawing from `limiter` —
/// debiting quota units before a concurrency slot is actually available
/// would desync the bucket's notion of "spent" from real request issuance.
async fn fetch_and_archive_messages(
    client: &GmailClient,
    manifest: &mut Manifest,
    ids: &[String],
    limiter: &TokenBucket,
    opts: &SyncOptions,
    report: &mut SyncReport,
) {
    let output_dir = &opts.output_dir;
    let mut seen = HashSet::new();
    let mut to_fetch = Vec::new();
    for id in ids {
        if !seen.insert(id.clone()) {
            continue;
        }
        // Not `shard_path(output_dir, id).exists()`: a not-yet-fetched
        // message's shard depends on its `internal_date`, which isn't known
        // until after it's fetched. The manifest's already-recorded `path`
        // (the same check `run_full_sync` uses) is the only presence check
        // available before a fetch happens.
        let already_archived = manifest
            .get(id)
            .is_some_and(|record| output_dir.join(&record.path).exists());
        if !already_archived {
            to_fetch.push(id.clone());
        }
    }
    if to_fetch.is_empty() {
        return;
    }

    if opts.dry_run {
        for id in to_fetch {
            report.actions.push(SyncAction::WouldFetch { id });
        }
        return;
    }

    let concurrency = opts.concurrency.clamp(1, MAX_CONCURRENCY);
    let results: Vec<(String, Result<ManifestRecord>)> = stream::iter(to_fetch)
        .map(|id| async move {
            limiter.acquire(MESSAGES_GET_COST_UNITS).await;
            let result = fetch_and_write_one(client, output_dir, &id).await;
            (id, result)
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;

    for (id, result) in results {
        match result {
            Ok(record) => {
                report.actions.push(SyncAction::Fetched {
                    id,
                    path: record.path.clone(),
                    bytes: record.size,
                });
                manifest.upsert(record);
            }
            Err(e) => report.errors.push(SyncError {
                id,
                reason: format!("{e:#}"),
            }),
        }
    }
}

/// Fetches one message as `format=raw`, decodes it, writes the byte-exact
/// `.eml`, and builds its manifest record — `Subject`/`From`/`Message-Id`
/// come from scanning the already-decoded bytes ([`extract_headers`]), not
/// a second `format=metadata` round-trip, since a second fetch would double
/// the request volume for data already in hand.
async fn fetch_and_write_one(
    client: &GmailClient,
    output_dir: &Path,
    id: &str,
) -> Result<ManifestRecord> {
    let message = MessagesApi::new(client)
        .get(id, MessageFormat::Raw, &[])
        .await?;
    let bytes = decode_raw_message(&message)?;
    let headers = extract_headers(
        &bytes,
        &[
            "Subject",
            "From",
            "To",
            "Message-Id",
            "In-Reply-To",
            "References",
        ],
    );
    let attachments = extract_attachment_filenames(&bytes);

    let path = shard_path(output_dir, id, message.internal_date_utc());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    write_atomic(&path, &bytes)?;
    let relative = path.strip_prefix(output_dir).unwrap_or(&path).to_path_buf();

    Ok(ManifestRecord {
        id: id.to_string(),
        thread_id: message.thread_id,
        label_ids: message.label_ids,
        internal_date: message.internal_date,
        subject: headers.get("Subject").cloned(),
        from: headers.get("From").cloned(),
        to: headers.get("To").cloned(),
        rfc822_msgid: headers.get("Message-Id").cloned(),
        in_reply_to: headers.get("In-Reply-To").cloned(),
        references: headers.get("References").cloned(),
        attachment_count: attachments.count as u32,
        attachment_filenames: attachments.filenames,
        path: relative,
        size: bytes.len() as u64,
        history_id: message.history_id,
        deleted_at: None,
    })
}

/// Writes `contents` to `path` via a sibling dotfile + rename, so a crash
/// mid-write can never leave a partial `.eml` that a later run's
/// presence-on-disk check would mistake for a complete archive.
fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("out");
    let tmp = path.with_file_name(format!(".{file_name}.tmp"));
    std::fs::write(&tmp, contents).with_context(|| format!("Failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("Failed to finalise {}", path.display()))
}

fn is_history_not_found(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<GmailError>(),
        Some(GmailError::ApiRequestFailed { status: 404, .. })
    )
}

/// Refuses an obviously-wrong `--output-dir`: `$HOME` itself, or anywhere
/// inside omni-dev's own settings directory.
///
/// `output_dir` may not exist yet (sync creates it), so ancestors are
/// canonicalised walking up to the deepest existing one first — the same
/// approach `src/cli/ai/claude/history/common.rs::is_inside` uses, kept as
/// a small local copy here rather than a new shared extraction (sync's
/// "source" is the network, not a local tree, so this guards a different
/// thing: an obviously-wrong destination, not source/target overlap).
fn guard_output_dir(output_dir: &Path) -> Result<()> {
    let Some(home) = dirs::home_dir() else {
        return Ok(());
    };
    let canon_output = canonicalize_best_effort(output_dir);
    if canon_output == canonicalize_best_effort(&home) {
        anyhow::bail!(
            "refusing to sync directly into your home directory ({}); use a dedicated \
             subdirectory",
            home.display()
        );
    }
    let omni_dev_dir = canonicalize_best_effort(&home.join(".omni-dev"));
    if canon_output == omni_dev_dir || canon_output.starts_with(&omni_dev_dir) {
        anyhow::bail!(
            "refusing to sync into {}: it is inside omni-dev's own settings directory",
            output_dir.display()
        );
    }
    Ok(())
}

fn canonicalize_best_effort(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    loop {
        if let Ok(mut canon) = existing.canonicalize() {
            for component in tail.into_iter().rev() {
                canon.push(component);
            }
            return canon;
        }
        match existing.parent() {
            Some(parent) => {
                if let Some(name) = existing.file_name() {
                    tail.push(name);
                }
                existing = parent;
            }
            None => return path.to_path_buf(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use chrono::DateTime;
    use std::sync::atomic::Ordering;

    use crate::gmail::auth::{GmailCredentials, GmailScope};
    use crate::utils::secret::Secret;

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

    async fn mount_profile(server: &wiremock::MockServer, email: &str, history_id: &str) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/profile"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "emailAddress": email,
                    "messagesTotal": 1,
                    "threadsTotal": 1,
                    "historyId": history_id,
                })),
            )
            .mount(server)
            .await;
    }

    async fn mount_message_list(server: &wiremock::MockServer, ids: &[&str]) {
        let messages: Vec<serde_json::Value> = ids
            .iter()
            .map(|id| serde_json::json!({"id": id, "threadId": "t1"}))
            .collect();
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"messages": messages})),
            )
            .mount(server)
            .await;
    }

    fn raw_message_body(id: &str, subject: &str) -> String {
        let source = format!("Subject: {subject}\r\nFrom: a@example.com\r\n\r\nBody of {id}.");
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(source)
    }

    async fn mount_raw_get(server: &wiremock::MockServer, id: &str, subject: &str) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!(
                "/gmail/v1/users/me/messages/{id}"
            )))
            .and(wiremock::matchers::query_param("format", "raw"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": id,
                    "threadId": "t1",
                    "labelIds": ["INBOX"],
                    "internalDate": "1700000000000",
                    "historyId": "500",
                    "raw": raw_message_body(id, subject),
                })),
            )
            .mount(server)
            .await;
    }

    /// The date `mount_raw_get`'s `internalDate` ("1700000000000") resolves
    /// to — shared so tests asserting a fetched message's on-disk shard
    /// location don't hardcode a second, independently-computed date.
    fn mock_internal_date() -> DateTime<Utc> {
        DateTime::from_timestamp_millis(1_700_000_000_000).unwrap()
    }

    fn opts(output_dir: PathBuf) -> SyncOptions {
        SyncOptions {
            output_dir,
            query: None,
            full: false,
            concurrency: 4,
            dry_run: false,
        }
    }

    // ── backfill ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn run_sync_backfills_on_first_run_and_persists_the_watermark() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_profile(&server, "user@example.com", "500").await;
        mount_message_list(&server, &["m1"]).await;
        mount_raw_get(&server, "m1", "Hello").await;

        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("archive");
        let report = run_sync(&client, &opts(output_dir.clone())).await.unwrap();

        assert!(report.errors.is_empty());
        assert!(report
            .actions
            .iter()
            .any(|a| matches!(a, SyncAction::Fetched { id, .. } if id == "m1")));
        assert!(shard_path(&output_dir, "m1", Some(mock_internal_date())).exists());

        match state::load(&state_path(&output_dir)) {
            LoadOutcome::Present(s) => {
                assert_eq!(s.history_id, "500");
                assert_eq!(s.email_address, "user@example.com");
            }
            _ => panic!("expected a persisted state after a clean backfill"),
        }

        let manifest = Manifest::load(&manifest_path(&output_dir)).unwrap();
        let record = manifest.get("m1").unwrap();
        assert_eq!(record.subject.as_deref(), Some("Hello"));
        assert_eq!(record.from.as_deref(), Some("a@example.com"));
    }

    #[tokio::test]
    async fn run_sync_records_threading_headers_and_attachment_metadata() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_profile(&server, "user@example.com", "500").await;
        mount_message_list(&server, &["m1"]).await;

        let source = "Subject: Report\r\n\
From: a@example.com\r\n\
To: b@example.com\r\n\
Message-Id: <m1@example.com>\r\n\
In-Reply-To: <parent@example.com>\r\n\
References: <parent@example.com>\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"BOUNDARY\"\r\n\
\r\n\
--BOUNDARY\r\n\
Content-Type: text/plain\r\n\
\r\n\
Hello\r\n\
--BOUNDARY\r\n\
Content-Type: application/pdf\r\n\
Content-Disposition: attachment; filename=\"report.pdf\"\r\n\
\r\n\
not-really-a-pdf\r\n\
--BOUNDARY--\r\n";
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(source);
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages/m1"))
            .and(wiremock::matchers::query_param("format", "raw"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "m1",
                    "threadId": "t1",
                    "labelIds": ["INBOX"],
                    "internalDate": "1700000000000",
                    "historyId": "500",
                    "raw": encoded,
                })),
            )
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("archive");
        let report = run_sync(&client, &opts(output_dir.clone())).await.unwrap();
        assert!(report.errors.is_empty());

        let manifest = Manifest::load(&manifest_path(&output_dir)).unwrap();
        let record = manifest.get("m1").unwrap();
        assert_eq!(record.to.as_deref(), Some("b@example.com"));
        assert_eq!(record.in_reply_to.as_deref(), Some("<parent@example.com>"));
        assert_eq!(record.references.as_deref(), Some("<parent@example.com>"));
        assert_eq!(record.attachment_count, 1);
        assert_eq!(record.attachment_filenames, vec!["report.pdf".to_string()]);
    }

    // ── 404 → reconciliation ─────────────────────────────────────────

    #[tokio::test]
    async fn run_sync_404_on_watermark_triggers_reconciliation_not_a_gap() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("archive");
        std::fs::create_dir_all(&output_dir).unwrap();
        state::save(
            &ArchiveState {
                history_id: "1".to_string(),
                email_address: "user@example.com".to_string(),
                last_sync: Utc::now(),
                query: None,
            },
            &state_path(&output_dir),
        )
        .unwrap();

        mount_profile(&server, "user@example.com", "999").await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/history"))
            .respond_with(
                wiremock::ResponseTemplate::new(404).set_body_json(serde_json::json!({
                    "error": {"message": "Not Found", "errors": [{"reason": "notFound"}]}
                })),
            )
            .mount(&server)
            .await;
        mount_message_list(&server, &["m1"]).await;
        mount_raw_get(&server, "m1", "Hello").await;

        let report = run_sync(&client, &opts(output_dir.clone())).await.unwrap();

        assert!(report
            .actions
            .iter()
            .any(|a| matches!(a, SyncAction::Note { message } if message.contains("reconciling"))));
        assert!(report
            .actions
            .iter()
            .any(|a| matches!(a, SyncAction::Fetched { id, .. } if id == "m1")));
        match state::load(&state_path(&output_dir)) {
            LoadOutcome::Present(s) => assert_eq!(s.history_id, "999"),
            _ => panic!("expected reconciliation to persist the profile's current historyId"),
        }
    }

    // ── valid watermark applies all 4 event types ─────────────────────

    #[tokio::test]
    async fn run_sync_applies_added_deleted_and_label_events_from_a_valid_watermark() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("archive");
        std::fs::create_dir_all(&output_dir).unwrap();
        state::save(
            &ArchiveState {
                history_id: "100".to_string(),
                email_address: "user@example.com".to_string(),
                last_sync: Utc::now(),
                query: None,
            },
            &state_path(&output_dir),
        )
        .unwrap();

        // Pre-existing m2 (to be deleted) and m3 (to have labels changed) —
        // both already archived, neither should be re-fetched or rewritten.
        let mut manifest = Manifest::default();
        for id in ["m2", "m3"] {
            let path = shard_path(&output_dir, id, None);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, format!("From: a@example.com\r\n\r\n{id} body")).unwrap();
            manifest.upsert(ManifestRecord {
                id: id.to_string(),
                thread_id: Some("t1".to_string()),
                label_ids: vec!["INBOX".to_string(), "UNREAD".to_string()],
                internal_date: None,
                subject: None,
                from: None,
                to: None,
                rfc822_msgid: None,
                in_reply_to: None,
                references: None,
                attachment_count: 0,
                attachment_filenames: Vec::new(),
                path: path.strip_prefix(&output_dir).unwrap().to_path_buf(),
                size: std::fs::metadata(&path).unwrap().len(),
                history_id: Some("50".to_string()),
                deleted_at: None,
            });
        }
        manifest.save(&manifest_path(&output_dir)).unwrap();
        let m3_bytes_before = std::fs::read(shard_path(&output_dir, "m3", None)).unwrap();

        mount_profile(&server, "user@example.com", "999").await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/history"))
            .and(wiremock::matchers::query_param("startHistoryId", "100"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "history": [{
                    "id": "150",
                    "messagesAdded": [{"message": {"id": "m1", "threadId": "t1", "labelIds": ["INBOX"]}}],
                    "messagesDeleted": [{"message": {"id": "m2", "threadId": "t1"}}],
                    "labelsAdded": [{"message": {"id": "m3", "threadId": "t1"}, "labelIds": ["IMPORTANT"]}],
                    "labelsRemoved": [{"message": {"id": "m3", "threadId": "t1"}, "labelIds": ["UNREAD"]}],
                }],
                "historyId": "300",
            })))
            .mount(&server)
            .await;
        mount_raw_get(&server, "m1", "New message").await;
        // Deliberately no mock for GET .../messages/m3 — a label-only change
        // must never fetch the message it applies to.

        let report = run_sync(&client, &opts(output_dir.clone())).await.unwrap();
        assert!(report.errors.is_empty());

        let manifest = Manifest::load(&manifest_path(&output_dir)).unwrap();
        assert!(manifest.get("m1").is_some(), "m1 should have been fetched");
        assert!(
            manifest.get("m2").unwrap().deleted_at.is_some(),
            "m2 should be soft-deleted"
        );
        assert!(
            shard_path(&output_dir, "m2", None).exists(),
            "m2's .eml must survive a soft-delete"
        );
        let m3 = manifest.get("m3").unwrap();
        assert!(m3.label_ids.contains(&"IMPORTANT".to_string()));
        assert!(!m3.label_ids.contains(&"UNREAD".to_string()));
        assert_eq!(
            std::fs::read(shard_path(&output_dir, "m3", None)).unwrap(),
            m3_bytes_before,
            "a label-only change must never rewrite the .eml"
        );

        match state::load(&state_path(&output_dir)) {
            LoadOutcome::Present(s) => assert_eq!(s.history_id, "300"),
            _ => panic!("expected the new historyId to be persisted"),
        }
    }

    // ── account-identity validation ────────────────────────────────────

    #[tokio::test]
    async fn run_sync_rejects_a_state_json_for_a_different_account() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("archive");
        std::fs::create_dir_all(&output_dir).unwrap();
        state::save(
            &ArchiveState {
                history_id: "1".to_string(),
                email_address: "wrong@example.com".to_string(),
                last_sync: Utc::now(),
                query: None,
            },
            &state_path(&output_dir),
        )
        .unwrap();
        let manifest_before = b"".to_vec();
        std::fs::write(manifest_path(&output_dir), &manifest_before).unwrap();

        mount_profile(&server, "user@example.com", "999").await;

        let err = run_sync(&client, &opts(output_dir.clone()))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("wrong@example.com"));
        assert!(msg.contains("user@example.com"));

        // Neither file was touched by the rejected run.
        assert_eq!(
            std::fs::read(manifest_path(&output_dir)).unwrap(),
            manifest_before
        );
        match state::load(&state_path(&output_dir)) {
            LoadOutcome::Present(s) => assert_eq!(s.email_address, "wrong@example.com"),
            _ => panic!("expected the original (mismatched) state to survive untouched"),
        }
    }

    // ── idempotence ────────────────────────────────────────────────────

    #[tokio::test]
    async fn run_sync_rerun_with_no_server_side_changes_fetches_and_writes_nothing_new() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_profile(&server, "user@example.com", "500").await;
        mount_message_list(&server, &["m1"]).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages/m1"))
            .and(wiremock::matchers::query_param("format", "raw"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "m1", "threadId": "t1", "labelIds": ["INBOX"],
                "internalDate": "1700000000000", "historyId": "500",
                "raw": raw_message_body("m1", "Hello"),
            })))
            .expect(1) // exactly once across BOTH runs below
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("archive");
        let o = opts(output_dir.clone());
        let first = run_sync(&client, &o).await.unwrap();
        assert!(first.errors.is_empty());
        let manifest_before = std::fs::read(manifest_path(&output_dir)).unwrap();

        // Second run takes the incremental path against the watermark the
        // first run just persisted; an empty history means nothing to do.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/history"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "history": [], "historyId": "500",
                })),
            )
            .mount(&server)
            .await;

        let second = run_sync(&client, &o).await.unwrap();
        assert!(second
            .actions
            .iter()
            .all(|a| !matches!(a, SyncAction::Fetched { .. })));
        let manifest_after = std::fs::read(manifest_path(&output_dir)).unwrap();
        assert_eq!(manifest_before, manifest_after);
    }

    // ── interruption / the Facebook-harvester regression ────────────────

    #[tokio::test]
    async fn run_sync_survives_a_missing_state_json_without_refetching_or_truncating() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("archive");

        // An existing archive from a prior, already-completed run.
        let eml_path = shard_path(&output_dir, "m1", None);
        std::fs::create_dir_all(eml_path.parent().unwrap()).unwrap();
        let original_bytes = b"From: a@example.com\r\n\r\nOriginal body.".to_vec();
        std::fs::write(&eml_path, &original_bytes).unwrap();
        let mut manifest = Manifest::default();
        manifest.upsert(ManifestRecord {
            id: "m1".to_string(),
            thread_id: Some("t1".to_string()),
            label_ids: vec!["INBOX".to_string()],
            internal_date: None,
            subject: None,
            from: None,
            to: None,
            rfc822_msgid: None,
            in_reply_to: None,
            references: None,
            attachment_count: 0,
            attachment_filenames: Vec::new(),
            path: eml_path.strip_prefix(&output_dir).unwrap().to_path_buf(),
            size: original_bytes.len() as u64,
            history_id: Some("1".to_string()),
            deleted_at: None,
        });
        manifest.save(&manifest_path(&output_dir)).unwrap();
        // `state.json` is deliberately absent — as if it were deleted, or
        // this is the very first run after `preload_prior`/`open_sink`-style
        // machinery would otherwise have truncated an existing archive.

        mount_profile(&server, "user@example.com", "999").await;
        mount_message_list(&server, &["m1"]).await;
        // Deliberately no mock for GET .../messages/m1 — a re-fetch here
        // would mean the regression reappeared.

        let report = run_sync(&client, &opts(output_dir.clone())).await.unwrap();

        assert!(report
            .actions
            .iter()
            .all(|a| !matches!(a, SyncAction::Fetched { id, .. } if id == "m1")));
        assert_eq!(
            std::fs::read(&eml_path).unwrap(),
            original_bytes,
            "the pre-existing archive must survive a missing state.json intact"
        );
        let manifest_after = Manifest::load(&manifest_path(&output_dir)).unwrap();
        assert_eq!(
            manifest_after.get("m1").unwrap().size,
            original_bytes.len() as u64
        );
    }

    // ── per-item failure ────────────────────────────────────────────────

    #[tokio::test]
    async fn run_sync_one_unfetchable_message_yields_an_error_and_the_run_completes() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_profile(&server, "user@example.com", "999").await;
        mount_message_list(&server, &["m1", "m2"]).await;
        mount_raw_get(&server, "m1", "Good").await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages/m2"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("archive");
        let report = run_sync(&client, &opts(output_dir.clone())).await.unwrap();

        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.errors[0].id, "m2");
        assert!(report
            .actions
            .iter()
            .any(|a| matches!(a, SyncAction::Fetched { id, .. } if id == "m1")));

        // The watermark must not advance past a run with outstanding errors.
        assert!(!state_path(&output_dir).exists());
        // But the successfully-fetched message's manifest entry survives.
        let manifest = Manifest::load(&manifest_path(&output_dir)).unwrap();
        assert!(manifest.get("m1").is_some());
    }

    #[tokio::test]
    async fn run_sync_leaves_the_watermark_untouched_after_an_incremental_failure() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("archive");
        std::fs::create_dir_all(&output_dir).unwrap();
        state::save(
            &ArchiveState {
                history_id: "100".to_string(),
                email_address: "user@example.com".to_string(),
                last_sync: Utc::now(),
                query: None,
            },
            &state_path(&output_dir),
        )
        .unwrap();

        mount_profile(&server, "user@example.com", "999").await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/history"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "history": [{"id": "150", "messagesAdded": [{"message": {"id": "m1", "threadId": "t1"}}]}],
                "historyId": "200",
            })))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages/m1"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let report = run_sync(&client, &opts(output_dir.clone())).await.unwrap();
        assert_eq!(report.errors.len(), 1);

        match state::load(&state_path(&output_dir)) {
            LoadOutcome::Present(s) => {
                assert_eq!(s.history_id, "100", "watermark must not advance");
            }
            _ => panic!("expected the pre-existing state to survive"),
        }
    }

    // ── --dry-run ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn run_sync_dry_run_reports_planned_actions_and_touches_no_files() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_profile(&server, "user@example.com", "999").await;
        mount_message_list(&server, &["m1"]).await;
        // Deliberately no mock for GET .../messages/m1 — dry-run must never
        // fetch a message body.

        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("archive");
        let report = run_sync(
            &client,
            &SyncOptions {
                output_dir: output_dir.clone(),
                query: None,
                full: false,
                concurrency: 4,
                dry_run: true,
            },
        )
        .await
        .unwrap();

        assert!(matches!(
            report.actions.as_slice(),
            [SyncAction::WouldFetch { id }] if id == "m1"
        ));
        assert!(
            !output_dir.exists(),
            "dry-run must not create the output directory"
        );
    }

    // ── guard_output_dir ─────────────────────────────────────────────────

    #[test]
    fn guard_output_dir_rejects_home_directory_itself() {
        let _guard = crate::gmail::test_support::EnvGuard::take();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", home.path());
        let err = guard_output_dir(home.path()).unwrap_err();
        assert!(err.to_string().contains("home directory"));
    }

    #[test]
    fn guard_output_dir_rejects_inside_omni_dev_settings_dir() {
        let _guard = crate::gmail::test_support::EnvGuard::take();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", home.path());
        let target = home.path().join(".omni-dev").join("mail-archive");
        std::fs::create_dir_all(&target).unwrap();
        let err = guard_output_dir(&target).unwrap_err();
        assert!(err.to_string().contains("settings directory"));
    }

    #[test]
    fn guard_output_dir_accepts_an_ordinary_subdirectory() {
        let _guard = crate::gmail::test_support::EnvGuard::take();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", home.path());
        assert!(guard_output_dir(&home.path().join("mail-archive")).is_ok());
    }

    // ── corrupt manifest is a hard failure ──────────────────────────────

    #[tokio::test]
    async fn run_sync_propagates_a_corrupt_manifest_as_a_hard_error() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_profile(&server, "user@example.com", "1").await;
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("archive");
        std::fs::create_dir_all(&output_dir).unwrap();
        std::fs::write(manifest_path(&output_dir), "not json\n").unwrap();

        let err = run_sync(&client, &opts(output_dir)).await.unwrap_err();
        assert!(err.to_string().contains("Failed to parse manifest"));
    }

    // ── reconciliation must not truncate at HARD_CAP (#1467) ────────────

    #[tokio::test]
    async fn run_sync_full_reconciliation_does_not_mislabel_mail_past_hard_cap() {
        use crate::gmail::messages_api::HARD_CAP;

        /// Serves `ids` back across as many `messages.list` pages as it
        /// takes, terminating only when exhausted — mirrors real Gmail
        /// pagination rather than hand-mounting one `Mock` per page.
        struct SequentialIdPages {
            ids: Vec<String>,
            calls: std::sync::atomic::AtomicUsize,
        }
        impl wiremock::Respond for SequentialIdPages {
            fn respond(&self, _req: &wiremock::Request) -> wiremock::ResponseTemplate {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                let page_size = 500usize;
                let start = call * page_size;
                let page: Vec<serde_json::Value> = self
                    .ids
                    .iter()
                    .skip(start)
                    .take(page_size)
                    .map(|id| serde_json::json!({"id": id, "threadId": "t1"}))
                    .collect();
                let mut body = serde_json::json!({"messages": page});
                if start + page_size < self.ids.len() {
                    body["nextPageToken"] = serde_json::json!(format!("token-{}", call + 1));
                }
                wiremock::ResponseTemplate::new(200).set_body_json(body)
            }
        }

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("archive");
        std::fs::create_dir_all(&output_dir).unwrap();

        // One more id than `HARD_CAP`. Before the fix, `run_full_sync`'s
        // listing truncated to `HARD_CAP`, so every id past that point
        // would have been wrongly soft-deleted below, even though the
        // listing (mocked here to return every one of them) says otherwise.
        let ids: Vec<String> = (0..HARD_CAP + 10).map(|i| format!("m{i}")).collect();
        let mut manifest = Manifest::default();
        for id in &ids {
            let path = shard_path(&output_dir, id, None);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"From: a@example.com\r\n\r\nbody").unwrap();
            manifest.upsert(ManifestRecord {
                id: id.clone(),
                thread_id: Some("t1".to_string()),
                label_ids: vec!["INBOX".to_string()],
                internal_date: None,
                subject: None,
                from: None,
                to: None,
                rfc822_msgid: None,
                in_reply_to: None,
                references: None,
                attachment_count: 0,
                attachment_filenames: Vec::new(),
                path: path.strip_prefix(&output_dir).unwrap().to_path_buf(),
                size: 4,
                history_id: Some("1".to_string()),
                deleted_at: None,
            });
        }
        manifest.save(&manifest_path(&output_dir)).unwrap();

        mount_profile(&server, "user@example.com", "999").await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages"))
            .respond_with(SequentialIdPages {
                ids: ids.clone(),
                calls: std::sync::atomic::AtomicUsize::new(0),
            })
            .mount(&server)
            .await;
        // Deliberately no mock for any `messages/<id>?format=raw` fetch —
        // every id is already archived on disk, so a re-fetch here would
        // mean the presence-on-disk check regressed too.

        let report = run_sync(&client, &opts(output_dir.clone())).await.unwrap();

        assert!(report.errors.is_empty(), "no id should need re-fetching");
        assert!(
            !report
                .actions
                .iter()
                .any(|a| matches!(a, SyncAction::Deleted { .. })),
            "no already-archived message should be marked deleted just because it fell \
             outside a truncated listing"
        );
        let manifest_after = Manifest::load(&manifest_path(&output_dir)).unwrap();
        assert_eq!(
            manifest_after.ids_not_deleted().count(),
            ids.len(),
            "every id should still be present and not soft-deleted"
        );
    }
}
