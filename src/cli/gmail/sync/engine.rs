//! `gmail sync`'s control flow: backfill vs. incremental, 404-triggered
//! reconciliation, and the throttled fetch fan-out.
//!
//! [`run_sync`] does no direct stdout/stderr I/O itself — it (via
//! [`run_sync_with_progress`]) returns a [`SyncReport`] for the caller
//! (`src/cli/gmail/sync.rs`) to render and turn into a process exit code,
//! mirroring `src/cli/ai/claude/history/sync.rs::run`'s compute → render →
//! decide split. It may optionally emit [`super::progress::SyncProgressEvent`]s
//! over a caller-supplied channel — see ADR-0064's amendment for #1502 —
//! but never touches a terminal itself; only `sync.rs` does that.
//!
//! Presence-on-disk is the real idempotence mechanism (an interrupted
//! backfill needs no cursor to resume correctly): backfill, `--full`, and
//! 404-triggered reconciliation are therefore all the *same* code path,
//! [`run_full_sync`], which lists the whole mailbox and fetches only what's
//! missing on disk — listing and fetching are pipelined (#1502), so the
//! fetch fan-out for early-listed messages starts immediately rather than
//! waiting for the whole mailbox to be listed first. For idempotence to
//! actually hold across a real interruption (not just a clean run), the
//! manifest itself must reach disk periodically during the fetch fan-out,
//! not only once at the very end — see
//! [`fetch_and_archive_messages_streaming`]'s [`MANIFEST_CHECKPOINT_INTERVAL`]
//! (#1467).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;
use futures::stream::{self, FuturesUnordered, StreamExt as _};
use tokio::sync::{mpsc, Semaphore};

use crate::gmail::attachments::extract_attachments;
use crate::gmail::client::GmailClient;
use crate::gmail::error::GmailError;
use crate::gmail::history_api::HistoryApi;
use crate::gmail::messages_api::{
    ListingProgress, MessageFormat, MessagesApi, GMAIL_QUOTA_UNITS_PER_SECOND, MAX_CONCURRENCY,
    MESSAGES_GET_COST_UNITS,
};
use crate::gmail::profile_api::{Profile, ProfileApi};
use crate::gmail::raw_message::{
    decode_raw_message, extract_attachment_filenames, extract_headers,
};
use crate::utils::rate_limit::TokenBucket;

use super::manifest::{Manifest, ManifestRecord};
use super::progress::SyncProgressEvent;
use super::report::{SyncAction, SyncError, SyncReport};
use super::shard::{attachments_dir, shard_path};
use super::state::{self, ArchiveState, LoadOutcome};

/// Options for one `gmail sync` invocation.
pub(crate) struct SyncOptions {
    pub(crate) output_dir: PathBuf,
    pub(crate) query: Option<String>,
    pub(crate) full: bool,
    pub(crate) concurrency: usize,
    pub(crate) dry_run: bool,
    pub(crate) extract_attachments: bool,
    /// A concurrency cap shared across every account's fetches in one
    /// `gmail sync-all` run (ADR-0067), layered underneath `concurrency`
    /// (each account's own local `buffer_unordered`/`FuturesUnordered`
    /// clamp). `None` for every single-account caller — `gmail sync`'s
    /// behavior is unaffected.
    pub(crate) shared_pool: Option<Arc<Semaphore>>,
}

fn state_path(output_dir: &Path) -> PathBuf {
    output_dir.join("state.json")
}

fn manifest_path(output_dir: &Path) -> PathBuf {
    output_dir.join("manifest.jsonl")
}

/// Number of successfully-fetched messages between manifest checkpoints
/// during [`fetch_and_archive_messages`]'s fetch fan-out.
///
/// Bounds how much re-fetch work a crash mid-backfill can cost to about one
/// interval's worth (~4s at the documented 50 msg/s quota ceiling for `200`
/// messages), while keeping the number of full-manifest rewrites
/// proportional to `total / interval` rather than `total` (#1467).
const MANIFEST_CHECKPOINT_INTERVAL: usize = 200;

/// Runs one sync: resolves identity, decides backfill vs. incremental (with
/// 404 fallback), fetches whatever's missing, and returns a report. Never
/// panics on per-message failures — see
/// [`fetch_and_archive_messages_streaming`].
///
/// A thin wrapper around [`run_sync_with_progress`] with no progress
/// channel — kept as its own function so every existing caller/test stays
/// unaffected by #1502's progress-reporting addition.
pub(crate) async fn run_sync(client: &GmailClient, opts: &SyncOptions) -> Result<SyncReport> {
    run_sync_with_progress(client, opts, None).await
}

/// [`run_sync`], plus an optional channel to emit
/// [`SyncProgressEvent`]s to during the full-mailbox pass (backfill /
/// `--full` / 404-triggered reconciliation). `run_incremental`'s
/// `history.list` pass is typically a single page already, so it emits no
/// progress events regardless of whether `progress` is set.
///
/// Still performs no direct stdout/stderr I/O — `progress`, if present, is
/// just another channel this function writes structured data to, the same
/// as `report`; only the caller (`src/cli/gmail/sync.rs`) may render
/// anything (ADR-0064's amendment for #1502).
pub(crate) async fn run_sync_with_progress(
    client: &GmailClient,
    opts: &SyncOptions,
    progress: Option<&mpsc::UnboundedSender<SyncProgressEvent>>,
) -> Result<SyncReport> {
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
                progress,
            )
            .await
            {
                Ok(id) => id,
                Err(e) if is_history_not_found(&e) => {
                    report.actions.push(SyncAction::Note {
                        message: "watermark expired (404 on startHistoryId); reconciling"
                            .to_string(),
                    });
                    run_full_sync(
                        client,
                        &mut manifest,
                        &profile,
                        opts,
                        &limiter,
                        &mut report,
                        progress,
                    )
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
            run_full_sync(
                client,
                &mut manifest,
                &profile,
                opts,
                &limiter,
                &mut report,
                progress,
            )
            .await?
        }
        LoadOutcome::Absent => {
            run_full_sync(
                client,
                &mut manifest,
                &profile,
                opts,
                &limiter,
                &mut report,
                progress,
            )
            .await?
        }
        LoadOutcome::Corrupt(reason) => {
            report.actions.push(SyncAction::Note {
                message: format!("state.json unreadable ({reason}); reconciling"),
            });
            run_full_sync(
                client,
                &mut manifest,
                &profile,
                opts,
                &limiter,
                &mut report,
                progress,
            )
            .await?
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
///
/// Listing and fetching run *concurrently*, joined on this one task via
/// [`tokio::join!`] rather than [`tokio::spawn`] (#1502) — that's what lets
/// both sides keep sharing a single `&TokenBucket` borrow with no `Arc`, and
/// what confines `&mut Manifest` to exactly one side (the fetch consumer,
/// [`fetch_and_archive_messages_streaming`]) with no `Arc<Mutex<_>>` either.
/// The two manifest-mutating passes that need the *complete* listing
/// (undelete-on-reappearance, stale-id soft-deletion) both run after the
/// join — previously undelete ran before the (then-sequential) fetch phase
/// and stale-deletion ran after, so this can change the *order* actions
/// appear in a [`SyncReport`], never which actions occur.
async fn run_full_sync(
    client: &GmailClient,
    manifest: &mut Manifest,
    profile: &Profile,
    opts: &SyncOptions,
    limiter: &TokenBucket,
    report: &mut SyncReport,
    progress: Option<&mpsc::UnboundedSender<SyncProgressEvent>>,
) -> Result<String> {
    let (ids_tx, ids_rx) = mpsc::unbounded_channel::<String>();
    let messages_api = MessagesApi::new(client);

    let listing = messages_api.search_all_unbounded_streaming(
        opts.query.as_deref(),
        &[],
        limiter,
        ids_tx,
        |p: ListingProgress| {
            if let Some(tx) = progress {
                let _ = tx.send(SyncProgressEvent::ListingPage {
                    pages: p.page_no,
                    ids_discovered: p.ids_so_far,
                });
            }
        },
    );
    let fetching = fetch_and_archive_messages_streaming(
        client, manifest, ids_rx, limiter, opts, report, progress,
    );

    let (listing_result, fetch_result) = tokio::join!(listing, fetching);
    // A real `messages.list` failure is the more actionable root cause when
    // both sides error (the fetch side would just be draining a channel
    // that stopped growing) — check it first.
    listing_result?;
    let listed_ids = fetch_result?;
    if let Some(tx) = progress {
        let _ = tx.send(SyncProgressEvent::ListingDone);
    }

    for id in &listed_ids {
        if manifest.get(id).is_some_and(|r| r.deleted_at.is_some()) {
            if opts.dry_run {
                report
                    .actions
                    .push(SyncAction::WouldUndelete { id: id.clone() });
            } else {
                manifest.undelete(id);
                report
                    .actions
                    .push(SyncAction::Undeleted { id: id.clone() });
            }
        }
    }

    let stale: Vec<String> = manifest
        .ids_not_deleted()
        .filter(|id| !listed_ids.contains(*id))
        .map(str::to_string)
        .collect();
    for id in stale {
        if opts.dry_run {
            report.actions.push(SyncAction::WouldDelete { id });
        } else {
            manifest.mark_deleted(&id, Utc::now());
            report.actions.push(SyncAction::Deleted { id });
        }
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
///
/// A message can be added and deleted again within the same history
/// window (routine server-side churn — an auto-filtered message, a sent
/// mail immediately recalled) — every deleted id is pre-scanned across the
/// *whole* response first specifically so `to_fetch` can exclude them:
/// fetching one would just 404 (it's already gone by the time we'd ask),
/// and that 404 has nothing to do with a real failure. Symmetrically,
/// [`SyncAction::Deleted`] is only reported — and the manifest only
/// touched — for an id [`Manifest::mark_deleted`] actually had a record
/// for; a same-window churn id never got archived, so there's nothing to
/// report deleting.
///
/// `history.list`'s own pagination isn't streamed (unlike
/// [`run_full_sync`]'s listing — see its module-level rationale): this path
/// is typically a single page already (`docs/gmail.md`'s Sync section), so
/// this only emits one before/after [`SyncProgressEvent::ListingPage`]/
/// [`SyncProgressEvent::ListingDone`] pair around the whole call rather than
/// per-page updates — enough that `sync`'s progress bars don't sit frozen
/// at their initial state for an incremental run's entire duration, without
/// a second streaming primitive to get there.
async fn run_incremental(
    client: &GmailClient,
    manifest: &mut Manifest,
    start_history_id: &str,
    opts: &SyncOptions,
    limiter: &TokenBucket,
    report: &mut SyncReport,
    progress: Option<&mpsc::UnboundedSender<SyncProgressEvent>>,
) -> Result<String> {
    let history = HistoryApi::new(client)
        .list_all_unbounded(start_history_id, &[], limiter)
        .await?;

    let deleted_ids: HashSet<String> = history
        .history
        .iter()
        .flat_map(|record| &record.messages_deleted)
        .map(|deleted| deleted.message.id.clone())
        .collect();

    let mut seen = HashSet::new();
    let mut to_fetch = Vec::new();
    for record in &history.history {
        for added in &record.messages_added {
            if !deleted_ids.contains(&added.message.id) && seen.insert(added.message.id.clone()) {
                to_fetch.push(added.message.id.clone());
            }
        }
        for deleted in &record.messages_deleted {
            if manifest.get(&deleted.message.id).is_some() {
                manifest.mark_deleted(&deleted.message.id, Utc::now());
                report.actions.push(SyncAction::Deleted {
                    id: deleted.message.id.clone(),
                });
            }
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

    if let Some(tx) = progress {
        let _ = tx.send(SyncProgressEvent::ListingPage {
            pages: 1,
            ids_discovered: to_fetch.len(),
        });
        let _ = tx.send(SyncProgressEvent::ListingDone);
    }

    fetch_and_archive_messages(client, manifest, &to_fetch, limiter, opts, report, progress)
        .await?;

    Ok(history
        .history_id
        .unwrap_or_else(|| start_history_id.to_string()))
}

/// [`run_full_sync`]'s fetch/consumer side of the listing+fetch pipeline
/// (#1502): the presence-on-disk filter, the bounded/throttled fan-out, and
/// the manifest checkpointing all still work exactly as
/// [`fetch_and_archive_messages`] describes below — the only thing that
/// changed is that ids now arrive one at a time from `ids_rx` instead of as
/// a pre-collected `Vec`.
///
/// A plain `stream::iter(..).buffer_unordered(..)` (as
/// [`fetch_and_archive_messages`] uses) can't work here: that combinator
/// needs a complete `Vec` of ids *before* it borrows `manifest` for the
/// drain loop, so the presence-check filter and the drain loop never borrow
/// `manifest` at the same time. Once ids stream in instead, that filter has
/// to run per-id, interleaved with the drain loop — both wanting
/// `&mut Manifest` at once, which the borrow checker rejects as chained
/// stream combinators. Instead this is a manual pump loop: `tokio::select!`
/// alternates between pulling the next id (and synchronously filtering it
/// against `manifest`) and draining the next completed fetch (and
/// synchronously applying its result to `manifest`) — every manifest touch
/// is a synchronous statement inside a `select!` arm, never inside a future
/// stored in `in_flight`, so only one borrow of `manifest` is ever live.
///
/// Also returns every id seen on `ids_rx` (regardless of whether it needed
/// fetching) — [`run_full_sync`] needs that complete set, once listing
/// finishes, for its undelete/stale-deletion passes.
async fn fetch_and_archive_messages_streaming(
    client: &GmailClient,
    manifest: &mut Manifest,
    mut ids_rx: mpsc::UnboundedReceiver<String>,
    limiter: &TokenBucket,
    opts: &SyncOptions,
    report: &mut SyncReport,
    progress: Option<&mpsc::UnboundedSender<SyncProgressEvent>>,
) -> Result<HashSet<String>> {
    let output_dir = &opts.output_dir;
    let concurrency = opts.concurrency.clamp(1, MAX_CONCURRENCY);
    let mut seen = HashSet::new();
    let mut listed_ids = HashSet::new();
    let mut in_flight = FuturesUnordered::new();
    let mut since_checkpoint = 0usize;
    let mut ids_open = true;

    loop {
        tokio::select! {
            maybe_id = ids_rx.recv(), if ids_open && in_flight.len() < concurrency => {
                match maybe_id {
                    Some(id) => {
                        listed_ids.insert(id.clone());
                        if !seen.insert(id.clone()) {
                            continue;
                        }
                        // Not `shard_path(output_dir, id).exists()`: a
                        // not-yet-fetched message's shard depends on its
                        // `internal_date`, which isn't known until after
                        // it's fetched. The manifest's already-recorded
                        // `path` is the only presence check available
                        // before a fetch happens.
                        let already_archived = manifest
                            .get(&id)
                            .is_some_and(|record| output_dir.join(&record.path).exists());
                        if already_archived {
                            continue;
                        }
                        if opts.dry_run {
                            report.actions.push(SyncAction::WouldFetch { id });
                            continue;
                        }
                        let extract_attachments_flag = opts.extract_attachments;
                        let shared_pool = opts.shared_pool.clone();
                        in_flight.push(async move {
                            let _permit = match &shared_pool {
                                Some(pool) => match pool.acquire().await {
                                    Ok(permit) => Some(permit),
                                    Err(e) => {
                                        return (
                                            id,
                                            Err(anyhow::anyhow!(
                                                "sync-all's shared semaphore closed \
                                                 unexpectedly: {e}"
                                            )),
                                        );
                                    }
                                },
                                None => None,
                            };
                            limiter.acquire(MESSAGES_GET_COST_UNITS).await;
                            let result = fetch_and_write_one(
                                client,
                                output_dir,
                                &id,
                                extract_attachments_flag,
                            )
                            .await;
                            (id, result)
                        });
                        if let Some(tx) = progress {
                            let _ = tx.send(SyncProgressEvent::FetchQueued);
                        }
                    }
                    None => ids_open = false,
                }
            }
            Some((id, result)) = in_flight.next(), if !in_flight.is_empty() => {
                let failed = result.is_err();
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
                since_checkpoint += 1;
                if since_checkpoint >= MANIFEST_CHECKPOINT_INTERVAL {
                    manifest.save(&manifest_path(output_dir))?;
                    since_checkpoint = 0;
                }
                if let Some(tx) = progress {
                    let _ = tx.send(SyncProgressEvent::FetchCompleted { failed });
                }
            }
            else => break,
        }
    }
    Ok(listed_ids)
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
///
/// Results are applied as each fetch *completes* (not batched through an
/// intermediate `Vec` after the whole fan-out finishes), and the manifest is
/// checkpointed to disk every [`MANIFEST_CHECKPOINT_INTERVAL`] completions —
/// see the module doc's interruption note. The final partial interval is
/// still covered by `run_sync`'s own unconditional save once this function
/// returns, so no extra flush is needed here on the way out.
///
/// Used by [`run_incremental`] only — [`run_full_sync`]'s pipelined listing
/// uses [`fetch_and_archive_messages_streaming`] instead (#1502).
async fn fetch_and_archive_messages(
    client: &GmailClient,
    manifest: &mut Manifest,
    ids: &[String],
    limiter: &TokenBucket,
    opts: &SyncOptions,
    report: &mut SyncReport,
    progress: Option<&mpsc::UnboundedSender<SyncProgressEvent>>,
) -> Result<()> {
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
        return Ok(());
    }

    if opts.dry_run {
        for id in to_fetch {
            report.actions.push(SyncAction::WouldFetch { id });
        }
        return Ok(());
    }

    // The whole batch is already known (unlike run_full_sync's live-streamed
    // listing), so the fetch bar's total is knowable up front — one
    // `FetchQueued` per item gives a fully determinate bar from the start
    // rather than one that grows as ids trickle in.
    if let Some(tx) = progress {
        for _ in 0..to_fetch.len() {
            let _ = tx.send(SyncProgressEvent::FetchQueued);
        }
    }

    let concurrency = opts.concurrency.clamp(1, MAX_CONCURRENCY);
    let mut fetches = stream::iter(to_fetch)
        .map(|id| async move {
            let _permit = match opts.shared_pool.as_ref() {
                Some(pool) => match pool.acquire().await {
                    Ok(permit) => Some(permit),
                    Err(e) => {
                        return (
                            id,
                            Err(anyhow::anyhow!(
                                "sync-all's shared semaphore closed unexpectedly: {e}"
                            )),
                        );
                    }
                },
                None => None,
            };
            limiter.acquire(MESSAGES_GET_COST_UNITS).await;
            let result =
                fetch_and_write_one(client, output_dir, &id, opts.extract_attachments).await;
            (id, result)
        })
        .buffer_unordered(concurrency);

    let mut since_checkpoint = 0usize;
    while let Some((id, result)) = fetches.next().await {
        let failed = result.is_err();
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
        since_checkpoint += 1;
        if since_checkpoint >= MANIFEST_CHECKPOINT_INTERVAL {
            manifest.save(&manifest_path(output_dir))?;
            since_checkpoint = 0;
        }
        if let Some(tx) = progress {
            let _ = tx.send(SyncProgressEvent::FetchCompleted { failed });
        }
    }
    Ok(())
}

/// Fetches one message as `format=raw`, decodes it, writes the byte-exact
/// `.eml`, and builds its manifest record — `Subject`/`From`/`Message-Id`
/// come from scanning the already-decoded bytes ([`extract_headers`]), not
/// a second `format=metadata` round-trip, since a second fetch would double
/// the request volume for data already in hand.
///
/// When `extract_attachments` is set, also writes each `Content-Disposition:
/// attachment` part's decoded bytes into the message's `attachments/`
/// directory (see [`extract_attachments`] / ADR-0065) — a purely additive
/// step that never touches the manifest record built here, which keeps
/// coming from the cheap [`extract_attachment_filenames`] heuristic exactly
/// as before.
async fn fetch_and_write_one(
    client: &GmailClient,
    output_dir: &Path,
    id: &str,
    extract_attachments_flag: bool,
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
    let date = message.internal_date_utc();

    let path = shard_path(output_dir, id, date);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    write_atomic(&path, &bytes)?;
    let relative = path.strip_prefix(output_dir).unwrap_or(&path).to_path_buf();

    if extract_attachments_flag {
        let extracted = extract_attachments(&bytes);
        if !extracted.is_empty() {
            let dir = attachments_dir(output_dir, id, date);
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("Failed to create {}", dir.display()))?;
            for attachment in &extracted {
                write_atomic(&dir.join(&attachment.filename), &attachment.contents)?;
            }
        }
    }

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

/// A 404 on `history.list` is Google's documented signal for a
/// `startHistoryId` older than the mailbox's retention window. Treated as
/// history-not-found when the error's `reason` is absent/unparseable (fails
/// toward reconciling, matching `state.rs`'s disposable-state posture) or
/// equals `"notFound"`; a 404 with a *different*, present reason is
/// something else and must not silently trigger a full reconciliation —
/// mirroring how [`crate::gmail::client`]'s 403 quota check requires a
/// specific reason rather than matching on status alone.
fn is_history_not_found(err: &anyhow::Error) -> bool {
    match err.downcast_ref::<GmailError>() {
        Some(e @ GmailError::ApiRequestFailed { status: 404, .. }) => {
            e.reason().map_or(true, |r| r == "notFound")
        }
        _ => false,
    }
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
            extract_attachments: false,
            shared_pool: None,
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

    #[tokio::test]
    async fn run_sync_ignores_a_message_added_and_deleted_within_the_same_history_window() {
        // Routine server-side churn (an auto-filtered message, a sent mail
        // immediately recalled): a message can be added and deleted again
        // before the next sync ever sees it. Fetching it would just 404,
        // and that isn't a real failure — see `run_incremental`'s doc
        // comment.
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
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "history": [{
                        "id": "150",
                        "messagesAdded": [{"message": {"id": "churn1", "threadId": "t1"}}],
                        "messagesDeleted": [{"message": {"id": "churn1", "threadId": "t1"}}],
                    }],
                    "historyId": "300",
                })),
            )
            .mount(&server)
            .await;
        // Deliberately no mock for GET .../messages/churn1 — fetching it at
        // all (not just erroring) is the bug this test guards against.

        let report = run_sync(&client, &opts(output_dir.clone())).await.unwrap();

        assert!(report.errors.is_empty());
        assert!(
            !report
                .actions
                .iter()
                .any(|a| matches!(a, SyncAction::Deleted { id } if id == "churn1")),
            "a message never archived shouldn't be reported as deleted"
        );
        let manifest = Manifest::load(&manifest_path(&output_dir)).unwrap();
        assert!(
            manifest.get("churn1").is_none(),
            "a same-window add+delete should never create a manifest record"
        );
    }

    #[tokio::test]
    async fn run_incremental_via_run_sync_with_progress_emits_fetch_events() {
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
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "history": [{
                        "id": "150",
                        "messagesAdded": [{"message": {"id": "m1", "threadId": "t1"}}],
                    }],
                    "historyId": "300",
                })),
            )
            .mount(&server)
            .await;
        mount_raw_get(&server, "m1", "New message").await;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let report = run_sync_with_progress(&client, &opts(output_dir), Some(&tx))
            .await
            .unwrap();
        drop(tx);

        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }

        assert!(report.errors.is_empty());
        // The bars must not sit frozen at their initial state for an
        // incremental run's whole duration (the bug this test guards
        // against): at least one real listing update plus a matched
        // queued/completed pair for the one message fetched.
        assert!(events.iter().any(|e| matches!(
            e,
            SyncProgressEvent::ListingPage {
                ids_discovered: 1,
                ..
            }
        )));
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, SyncProgressEvent::ListingDone))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, SyncProgressEvent::FetchQueued))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, SyncProgressEvent::FetchCompleted { failed: false }))
                .count(),
            1
        );
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
                extract_attachments: false,
                shared_pool: None,
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

    // ── manifest checkpointing during the fetch loop (#1467) ─────────────

    #[tokio::test]
    async fn fetch_and_archive_messages_checkpoints_the_manifest_across_multiple_intervals() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("archive");
        std::fs::create_dir_all(&output_dir).unwrap();

        let total = MANIFEST_CHECKPOINT_INTERVAL * 2 + 5;
        let ids: Vec<String> = (0..total).map(|i| format!("m{i}")).collect();
        for id in &ids {
            mount_raw_get(&server, id, "Hello").await;
        }

        let mut manifest = Manifest::default();
        let limiter = TokenBucket::new(1_000_000, 1_000_000);
        let mut report = SyncReport::default();
        let opts = SyncOptions {
            output_dir: output_dir.clone(),
            query: None,
            full: false,
            concurrency: 20,
            dry_run: false,
            extract_attachments: false,
            shared_pool: None,
        };

        fetch_and_archive_messages(
            &client,
            &mut manifest,
            &ids,
            &limiter,
            &opts,
            &mut report,
            None,
        )
        .await
        .unwrap();
        assert!(report.errors.is_empty());

        // Two checkpoint boundaries' worth of records should already have
        // landed on disk *before* any final save — proving multiple
        // checkpoints accumulate correctly rather than each one clobbering
        // the last. `buffer_unordered` completes out of dispatch order, so
        // this checks a count, not which specific ids made it.
        let checkpointed = Manifest::load(&manifest_path(&output_dir)).unwrap();
        let expected_checkpointed =
            (total / MANIFEST_CHECKPOINT_INTERVAL) * MANIFEST_CHECKPOINT_INTERVAL;
        assert_eq!(
            checkpointed.ids_not_deleted().count(),
            expected_checkpointed,
            "expected exactly two checkpoints' worth of records on disk before any final save"
        );

        // The final partial interval (the last 5 ids) is only on disk once
        // the caller does its own final save — the same thing `run_sync`
        // always does after this function returns.
        manifest.save(&manifest_path(&output_dir)).unwrap();
        let final_on_disk = Manifest::load(&manifest_path(&output_dir)).unwrap();
        for id in &ids {
            assert!(
                final_on_disk.get(id).is_some(),
                "missing {id} after the final save"
            );
        }
    }

    #[tokio::test]
    async fn fetch_and_archive_messages_checkpoints_before_the_whole_batch_completes() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("archive");
        std::fs::create_dir_all(&output_dir).unwrap();

        let fast_ids: Vec<String> = (0..MANIFEST_CHECKPOINT_INTERVAL)
            .map(|i| format!("fast{i}"))
            .collect();
        let slow_ids: Vec<String> = (0..5).map(|i| format!("slow{i}")).collect();
        for id in &fast_ids {
            mount_raw_get(&server, id, "Hello").await;
        }
        for id in &slow_ids {
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path(format!(
                    "/gmail/v1/users/me/messages/{id}"
                )))
                .and(wiremock::matchers::query_param("format", "raw"))
                .respond_with(
                    wiremock::ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({
                            "id": id, "threadId": "t1", "labelIds": ["INBOX"],
                            "internalDate": "1700000000000", "historyId": "500",
                            "raw": raw_message_body(id, "Slow"),
                        }))
                        .set_delay(std::time::Duration::from_secs(3600)),
                )
                .mount(&server)
                .await;
        }

        let mut ids = fast_ids.clone();
        ids.extend(slow_ids.clone());
        let mut manifest = Manifest::default();
        let limiter = TokenBucket::new(1_000_000, 1_000_000);
        let mut report = SyncReport::default();
        let opts = SyncOptions {
            output_dir: output_dir.clone(),
            query: None,
            full: false,
            concurrency: 20,
            dry_run: false,
            extract_attachments: false,
            shared_pool: None,
        };

        // The slow ids' 3600s delay never elapses within this test, so the
        // only way `poll_checkpoint` can win this race is if the manifest
        // was actually flushed to disk *during* the fetch loop — proving
        // checkpointing isn't just an artifact of the final save `run_sync`
        // does once this function returns.
        let poll_checkpoint = async {
            loop {
                if let Ok(on_disk) = Manifest::load(&manifest_path(&output_dir)) {
                    if fast_ids.iter().all(|id| on_disk.get(id).is_some()) {
                        return;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        };

        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::select! {
                _ = fetch_and_archive_messages(
                    &client, &mut manifest, &ids, &limiter, &opts, &mut report, None,
                ) => {
                    panic!(
                        "fetch_and_archive_messages returned before the slow ids' delay could \
                         possibly elapse — checkpointing must have regressed"
                    );
                }
                () = poll_checkpoint => {}
            }
        })
        .await
        .expect("manifest was never checkpointed for the fast batch within 10s");

        let on_disk = Manifest::load(&manifest_path(&output_dir)).unwrap();
        for id in &fast_ids {
            assert!(
                on_disk.get(id).is_some(),
                "checkpoint should have covered {id}"
            );
        }
    }

    // ── pipelined listing+fetch and progress events (#1502) ──────────────

    #[tokio::test]
    async fn fetch_and_archive_messages_streaming_checkpoints_the_manifest_across_multiple_intervals(
    ) {
        // Mirrors `fetch_and_archive_messages_checkpoints_the_manifest_across_multiple_intervals`
        // above, but feeds ids through a channel instead of a slice — the
        // highest-risk piece of new async control flow (the `select!` pump)
        // must preserve the exact same checkpointing behavior.
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("archive");
        std::fs::create_dir_all(&output_dir).unwrap();

        let total = MANIFEST_CHECKPOINT_INTERVAL * 2 + 5;
        let ids: Vec<String> = (0..total).map(|i| format!("m{i}")).collect();
        for id in &ids {
            mount_raw_get(&server, id, "Hello").await;
        }

        let mut manifest = Manifest::default();
        let limiter = TokenBucket::new(1_000_000, 1_000_000);
        let mut report = SyncReport::default();
        let opts = SyncOptions {
            output_dir: output_dir.clone(),
            query: None,
            full: false,
            concurrency: 20,
            dry_run: false,
            extract_attachments: false,
            shared_pool: None,
        };

        let (ids_tx, ids_rx) = mpsc::unbounded_channel();
        for id in &ids {
            ids_tx.send(id.clone()).unwrap();
        }
        drop(ids_tx);

        let listed_ids = fetch_and_archive_messages_streaming(
            &client,
            &mut manifest,
            ids_rx,
            &limiter,
            &opts,
            &mut report,
            None,
        )
        .await
        .unwrap();
        assert!(report.errors.is_empty());
        assert_eq!(listed_ids.len(), total);

        let checkpointed = Manifest::load(&manifest_path(&output_dir)).unwrap();
        let expected_checkpointed =
            (total / MANIFEST_CHECKPOINT_INTERVAL) * MANIFEST_CHECKPOINT_INTERVAL;
        assert_eq!(
            checkpointed.ids_not_deleted().count(),
            expected_checkpointed,
            "expected exactly two checkpoints' worth of records on disk before any final save"
        );

        manifest.save(&manifest_path(&output_dir)).unwrap();
        let final_on_disk = Manifest::load(&manifest_path(&output_dir)).unwrap();
        for id in &ids {
            assert!(
                final_on_disk.get(id).is_some(),
                "missing {id} after the final save"
            );
        }
    }

    #[tokio::test]
    async fn fetch_and_archive_messages_streaming_emits_queued_and_completed_progress_events() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("archive");
        std::fs::create_dir_all(&output_dir).unwrap();

        mount_raw_get(&server, "ok1", "Hello").await;
        mount_raw_get(&server, "ok2", "Hello").await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages/bad1"))
            .and(wiremock::matchers::query_param("format", "raw"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let mut manifest = Manifest::default();
        let limiter = TokenBucket::new(1_000_000, 1_000_000);
        let mut report = SyncReport::default();
        let opts = opts(output_dir.clone());

        let (ids_tx, ids_rx) = mpsc::unbounded_channel();
        for id in ["ok1", "ok2", "bad1"] {
            ids_tx.send(id.to_string()).unwrap();
        }
        drop(ids_tx);

        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();
        fetch_and_archive_messages_streaming(
            &client,
            &mut manifest,
            ids_rx,
            &limiter,
            &opts,
            &mut report,
            Some(&progress_tx),
        )
        .await
        .unwrap();
        drop(progress_tx);

        let mut events = Vec::new();
        while let Some(event) = progress_rx.recv().await {
            events.push(event);
        }

        assert_eq!(report.errors.len(), 1);
        let queued = events
            .iter()
            .filter(|e| matches!(e, SyncProgressEvent::FetchQueued))
            .count();
        let completed_ok = events
            .iter()
            .filter(|e| matches!(e, SyncProgressEvent::FetchCompleted { failed: false }))
            .count();
        let completed_failed = events
            .iter()
            .filter(|e| matches!(e, SyncProgressEvent::FetchCompleted { failed: true }))
            .count();
        assert_eq!(queued, 3, "one FetchQueued per dispatched fetch");
        assert_eq!(completed_ok, 2);
        assert_eq!(completed_failed, 1);
    }

    #[tokio::test]
    async fn run_sync_with_progress_emits_listing_events_ending_in_listing_done() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_profile(&server, "user@example.com", "500").await;
        mount_message_list(&server, &["m1", "m2"]).await;
        mount_raw_get(&server, "m1", "Hello").await;
        mount_raw_get(&server, "m2", "Hello").await;

        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("archive");

        let (tx, mut rx) = mpsc::unbounded_channel();
        let report = run_sync_with_progress(&client, &opts(output_dir), Some(&tx))
            .await
            .unwrap();
        drop(tx);

        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }

        assert!(report.errors.is_empty());
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SyncProgressEvent::ListingPage { .. })),
            "expected at least one ListingPage event"
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, SyncProgressEvent::ListingDone))
                .count(),
            1,
            "expected exactly one ListingDone event"
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, SyncProgressEvent::FetchQueued))
                .count(),
            2
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, SyncProgressEvent::FetchCompleted { failed: false }))
                .count(),
            2
        );
        // Both sides of the `tokio::join!` have already finished by the
        // time `run_full_sync` sends `ListingDone` — it's always the last
        // event this run emits.
        assert!(matches!(
            events.last(),
            Some(SyncProgressEvent::ListingDone)
        ));
    }

    #[tokio::test]
    async fn run_sync_full_reconciliation_undeletes_reappeared_mail_and_deletes_vanished_mail() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_profile(&server, "user@example.com", "999").await;
        // Only "back" is listed as still present on the server; "gone" no
        // longer appears at all.
        mount_message_list(&server, &["back"]).await;
        mount_raw_get(&server, "back", "Hello").await;

        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("archive");
        std::fs::create_dir_all(&output_dir).unwrap();

        let back_path = shard_path(&output_dir, "back", None);
        std::fs::create_dir_all(back_path.parent().unwrap()).unwrap();
        std::fs::write(&back_path, b"From: a@example.com\r\n\r\nbody").unwrap();
        let gone_path = shard_path(&output_dir, "gone", None);
        std::fs::write(&gone_path, b"From: a@example.com\r\n\r\nbody").unwrap();

        let mut manifest = Manifest::default();
        manifest.upsert(ManifestRecord {
            id: "back".to_string(),
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
            path: back_path.strip_prefix(&output_dir).unwrap().to_path_buf(),
            size: 4,
            history_id: Some("1".to_string()),
            // Previously soft-deleted; the server lists it again this run.
            deleted_at: Some(Utc::now()),
        });
        manifest.upsert(ManifestRecord {
            id: "gone".to_string(),
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
            path: gone_path.strip_prefix(&output_dir).unwrap().to_path_buf(),
            size: 4,
            history_id: Some("1".to_string()),
            deleted_at: None,
        });
        manifest.save(&manifest_path(&output_dir)).unwrap();

        let report = run_sync(&client, &opts(output_dir.clone())).await.unwrap();

        assert!(report.errors.is_empty());
        assert!(report
            .actions
            .iter()
            .any(|a| matches!(a, SyncAction::Undeleted { id } if id == "back")));
        assert!(report
            .actions
            .iter()
            .any(|a| matches!(a, SyncAction::Deleted { id } if id == "gone")));

        let on_disk = Manifest::load(&manifest_path(&output_dir)).unwrap();
        assert!(on_disk.get("back").unwrap().deleted_at.is_none());
        assert!(on_disk.get("gone").unwrap().deleted_at.is_some());
    }

    // ── --dry-run reconciliation reports planned actions (#1467) ────────

    #[tokio::test]
    async fn run_sync_dry_run_reconciliation_reports_would_delete_without_mutating_manifest() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("archive");
        std::fs::create_dir_all(&output_dir).unwrap();

        let path = shard_path(&output_dir, "m1", None);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"From: a@example.com\r\n\r\nbody").unwrap();
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
            path: path.strip_prefix(&output_dir).unwrap().to_path_buf(),
            size: 4,
            history_id: Some("1".to_string()),
            deleted_at: None,
        });
        manifest.save(&manifest_path(&output_dir)).unwrap();
        let manifest_bytes_before = std::fs::read(manifest_path(&output_dir)).unwrap();

        mount_profile(&server, "user@example.com", "999").await;
        mount_message_list(&server, &[]).await; // m1 no longer appears server-side

        let report = run_sync(
            &client,
            &SyncOptions {
                output_dir: output_dir.clone(),
                query: None,
                full: false,
                concurrency: 4,
                dry_run: true,
                extract_attachments: false,
                shared_pool: None,
            },
        )
        .await
        .unwrap();

        assert!(matches!(
            report.actions.as_slice(),
            [SyncAction::WouldDelete { id }] if id == "m1"
        ));
        assert_eq!(
            std::fs::read(manifest_path(&output_dir)).unwrap(),
            manifest_bytes_before,
            "dry-run must not mutate the manifest, in memory or on disk"
        );
    }

    // ── 404 reason-check tightening (#1467) ──────────────────────────────

    #[tokio::test]
    async fn run_sync_404_with_a_different_reason_is_not_treated_as_history_not_found() {
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
                    "error": {"message": "Backend Error", "errors": [{"reason": "backendError"}]}
                })),
            )
            .mount(&server)
            .await;
        // Deliberately no mock for messages.list — a wrongly-triggered
        // reconciliation would attempt one and fail differently than
        // asserted below.

        let err = run_sync(&client, &opts(output_dir.clone()))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("404"));
        assert!(msg.contains("backendError"));

        match state::load(&state_path(&output_dir)) {
            LoadOutcome::Present(s) => {
                assert_eq!(
                    s.history_id, "1",
                    "never treated as a reconciliation trigger"
                );
            }
            _ => panic!("expected the original state to survive"),
        }
    }

    // ── shared concurrency pool for `sync-all` (ADR-0067) ────────────────

    #[tokio::test]
    async fn shared_pool_of_one_never_lets_two_fetches_be_in_flight_at_once() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("archive");
        std::fs::create_dir_all(&output_dir).unwrap();

        // Both ids' responses never arrive within this test — the only way
        // a second `messages.get` request could ever reach the server is if
        // the shared semaphore (sized to 1) incorrectly let a second fetch
        // start before the first one's permit was released.
        let slow_ids = ["slow0", "slow1"];
        for id in slow_ids {
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path(format!(
                    "/gmail/v1/users/me/messages/{id}"
                )))
                .and(wiremock::matchers::query_param("format", "raw"))
                .respond_with(
                    wiremock::ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({
                            "id": id, "threadId": "t1", "labelIds": ["INBOX"],
                            "internalDate": "1700000000000", "historyId": "500",
                            "raw": raw_message_body(id, "Slow"),
                        }))
                        .set_delay(std::time::Duration::from_secs(3600)),
                )
                .mount(&server)
                .await;
        }

        let mut manifest = Manifest::default();
        let limiter = TokenBucket::new(1_000_000, 1_000_000);
        let mut report = SyncReport::default();
        let ids: Vec<String> = slow_ids.iter().copied().map(str::to_string).collect();
        let opts = SyncOptions {
            output_dir: output_dir.clone(),
            query: None,
            full: false,
            // A generous *local* concurrency: only the shared pool should
            // be the thing capping in-flight requests to 1 here.
            concurrency: 20,
            dry_run: false,
            extract_attachments: false,
            shared_pool: Some(Arc::new(Semaphore::new(1))),
        };

        let never_more_than_one_in_flight = async {
            // Wait for the one fetch the semaphore should admit.
            loop {
                let received = server.received_requests().await.unwrap_or_default();
                if received
                    .iter()
                    .any(|r| r.url.path().contains("/messages/slow"))
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            // Give a broken (unbounded) implementation ample opportunity to
            // also have started the second fetch by now.
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let received = server.received_requests().await.unwrap_or_default();
            let in_flight = received
                .iter()
                .filter(|r| r.url.path().contains("/messages/slow"))
                .count();
            assert_eq!(
                in_flight, 1,
                "shared_pool sized to 1 should never admit a second concurrent fetch"
            );
        };

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::select! {
                _ = fetch_and_archive_messages(
                    &client, &mut manifest, &ids, &limiter, &opts, &mut report, None,
                ) => {
                    panic!(
                        "fetch_and_archive_messages returned before the slow ids' 3600s delay \
                         could possibly elapse"
                    );
                }
                () = never_more_than_one_in_flight => {}
            }
        })
        .await
        .expect("the single admitted fetch was never observed within 5s");
    }

    // A closed `Semaphore` can't happen through any code path today (nothing
    // ever calls `Semaphore::close`) — these two tests exercise the
    // defensive `pool.acquire()` error arms directly, the same way a real
    // `AcquireError` would surface, rather than leaving them unreachable in
    // practice and untested in principle.

    #[tokio::test]
    async fn fetch_and_archive_messages_streaming_reports_error_when_shared_pool_closed() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("archive");
        std::fs::create_dir_all(&output_dir).unwrap();

        let mut manifest = Manifest::default();
        let limiter = TokenBucket::new(1_000_000, 1_000_000);
        let mut report = SyncReport::default();
        let pool = Arc::new(Semaphore::new(1));
        pool.close();
        let opts = SyncOptions {
            output_dir,
            query: None,
            full: false,
            concurrency: 4,
            dry_run: false,
            extract_attachments: false,
            shared_pool: Some(pool),
        };

        let (ids_tx, ids_rx) = mpsc::unbounded_channel();
        ids_tx.send("id1".to_string()).unwrap();
        drop(ids_tx);

        fetch_and_archive_messages_streaming(
            &client,
            &mut manifest,
            ids_rx,
            &limiter,
            &opts,
            &mut report,
            None,
        )
        .await
        .unwrap();

        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0]
            .reason
            .contains("sync-all's shared semaphore closed unexpectedly"));
    }

    #[tokio::test]
    async fn fetch_and_archive_messages_reports_error_when_shared_pool_closed() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("archive");
        std::fs::create_dir_all(&output_dir).unwrap();

        let mut manifest = Manifest::default();
        let limiter = TokenBucket::new(1_000_000, 1_000_000);
        let mut report = SyncReport::default();
        let pool = Arc::new(Semaphore::new(1));
        pool.close();
        let opts = SyncOptions {
            output_dir,
            query: None,
            full: false,
            concurrency: 4,
            dry_run: false,
            extract_attachments: false,
            shared_pool: Some(pool),
        };

        fetch_and_archive_messages(
            &client,
            &mut manifest,
            &["id1".to_string()],
            &limiter,
            &opts,
            &mut report,
            None,
        )
        .await
        .unwrap();

        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0]
            .reason
            .contains("sync-all's shared semaphore closed unexpectedly"));
    }
}
