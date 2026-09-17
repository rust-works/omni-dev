//! `drive lease prune` — the ADR-0080 Consequences fast-follow (#1678) that
//! bounds the ledger's and the backup directory/folder's otherwise-unbounded
//! growth. Modeled on `omni-dev log prune` (`request_log::prune`):
//! `--older-than`/`--max-size`, applied sequentially (age first, then size
//! trims what's left), `--dry-run` reports without mutating anything.
//!
//! One invariant is non-negotiable and enforced structurally, not by a
//! flag: a live lease ([`LeaseRecord::is_live`]) is never a removal
//! candidate, regardless of `--older-than`/`--max-size`. The other is the
//! ADR's own: a ledger row and the backup it points at are dropped
//! **together, never one without the other** — a
//! [`LeaseBackup::Bytes`] backup is deleted from local disk, a
//! [`LeaseBackup::DriveCopy`] backup is moved to Drive Trash
//! ([`FilesApi::trash`]), and only once that step succeeds (or finds the
//! backup already gone) does the row itself get dropped from the ledger —
//! and the ledger is saved immediately, per row, not batched until the run
//! finishes, so a crash mid-run strands at most the one row it interrupts.
//! A backup-deletion failure for one row skips just that row, logging and
//! auditing why — it is left for a future prune, not treated as a hard
//! error for the whole command.
//!
//! `--max-size` bounds *local* bytes only: a `DriveCopy` backup counts as
//! zero and never participates in that budget, so it is reachable only
//! through `--older-than` — a `DriveCopy` sorted behind an oversized
//! `Bytes` backup must never be dropped as that backup's collateral
//! damage. The ledger lock is held only briefly per step (once to decide
//! candidates, then once per row to persist that row's removal) rather
//! than for the whole run, so a large batch never blocks a concurrent
//! `drive lease acquire`/`restore`/write for longer than a single row's
//! own local disk I/O — the same lock discipline every other lease command
//! already uses.

use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::cli::drive::format::JsonlSerialize;
use crate::drive::client::DriveClient;
use crate::drive::error::DriveError;
use crate::drive::files_api::FilesApi;
use crate::drive::lease::ledger::{LeaseBackup, LeaseLedger, LeaseRecord, LedgerLock};

/// Options controlling [`prune`].
pub struct PruneOptions {
    /// Drop non-live rows whose `expires_at` is strictly before this
    /// cutoff (a row expiring exactly at the cutoff survives) — mirrors
    /// `request_log::keep_by_age`'s inclusive boundary. A live row is never
    /// a candidate regardless of this bound.
    pub older_than: Option<DateTime<Utc>>,
    /// After the age filter, additionally drop the oldest-expiring
    /// survivors until the local backup bytes they account for total at
    /// most this many bytes. A `DriveCopy` backup counts as zero bytes here
    /// (it consumes no local disk) but remains eligible via `older_than`.
    pub max_size: Option<u64>,
    /// Compute and report the outcome without deleting/trashing any backup
    /// or modifying the ledger.
    pub dry_run: bool,
    /// Path to the lease ledger. Production callers pass
    /// [`crate::drive::lease::ledger::ledger_path`]'s own result; tests
    /// pass a path under a `tempdir`.
    pub ledger_path: PathBuf,
}

/// What a [`prune`] run did (or, when `dry_run`, would do).
#[derive(Debug, Clone, Default, Serialize)]
pub struct PruneOutcome {
    /// Rows (and their backups) removed.
    pub removed: usize,
    /// Rows remaining in the ledger afterward (live and non-live alike).
    pub kept: usize,
    /// Bytes freed from local disk by removed `Bytes` backups.
    pub bytes_freed: u64,
    /// `DriveCopy` backups moved to Drive Trash.
    pub trashed_drive_copies: usize,
    /// Removal candidates skipped this run because deleting/trashing their
    /// backup failed — left in the ledger for a future prune.
    pub failed: usize,
}

impl JsonlSerialize for PruneOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> Result<()> {
        crate::cli::drive::format::write_scalar_jsonl(self, out)
    }
}

/// A row's local backup byte footprint — `0` for a `DriveCopy`, which
/// consumes no local disk.
fn backup_size(backup: &LeaseBackup) -> u64 {
    match backup {
        LeaseBackup::Bytes { size, .. } => *size,
        LeaseBackup::DriveCopy { .. } => 0,
    }
}

/// Whether `err` is Drive's 404 for an already-absent file — tolerated as
/// "already clean" rather than a failure, since a prior manual cleanup or a
/// previous partially-failed prune run could have already trashed it.
fn is_drive_not_found(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<DriveError>(),
        Some(DriveError::ApiRequestFailed { status: 404, .. })
    )
}

/// Deletes/trashes one candidate's backup. `Ok(())` means the row's backup
/// is gone (deleted just now, or already absent) and the row may be
/// dropped; an `Err` means it is left in place for a future prune.
async fn clear_backup(files_api: &FilesApi<'_>, backup: &LeaseBackup) -> Result<()> {
    match backup {
        LeaseBackup::Bytes { path, .. } => match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        },
        LeaseBackup::DriveCopy { file_id } => match files_api.trash(file_id).await {
            Ok(_) => Ok(()),
            Err(e) if is_drive_not_found(&e) => Ok(()),
            Err(e) => Err(e),
        },
    }
}

/// Applies one removed row's backup to `outcome`'s tallies. Shared by the
/// dry-run and real-removal paths so their accounting can never drift
/// apart — both must treat "this row is being removed" identically.
fn account_removal(outcome: &mut PruneOutcome, backup: &LeaseBackup) {
    outcome.removed += 1;
    match backup {
        LeaseBackup::Bytes { size, .. } => outcome.bytes_freed += size,
        LeaseBackup::DriveCopy { .. } => outcome.trashed_drive_copies += 1,
    }
}

/// Writes `drive lease prune`'s own best-effort audit record (ADR-0080
/// §11) for one row, mirroring `acquire.rs`'s `record_attempt`'s
/// backup-field mapping. Never fails the prune itself — a logging failure
/// here only warns, the same posture `acquire`/`restore` already take,
/// since the destructive step (the backup already cleared, or refused to
/// be) has already happened by the time this is called.
fn record_prune_attempt(rec: &LeaseRecord, verdict: &str, error: Option<String>) {
    let (backup_location, backup_sha256, backup_size) = match &rec.backup {
        LeaseBackup::Bytes { path, sha256, size } => (
            Some(path.display().to_string()),
            Some(sha256.clone()),
            Some(*size),
        ),
        LeaseBackup::DriveCopy { file_id } => (Some(file_id.clone()), None, None),
    };
    let outcome = crate::request_log::AuditOutcome {
        command: vec!["drive".to_string(), "lease-prune".to_string()],
        integration: "drive",
        file_id: rec.file_id.clone(),
        lease_id: Some(rec.token.clone()),
        verdict: verdict.to_string(),
        backup_location,
        backup_sha256,
        backup_size,
        error,
        ..Default::default()
    };
    if let Err(err) = crate::request_log::record_audit_event(outcome) {
        tracing::warn!("drive lease prune: failed to write audit record: {err}");
    }
}

/// Longest prefix of `sorted_desc` (sorted newest-`expires_at`-first) whose
/// backup bytes fit within `max` — but never fewer than the single
/// most-recently-expired candidate, even if it alone exceeds the budget.
/// Mirrors `request_log::keep_by_size`, adapted from reverse iteration over
/// chronological lines to forward iteration over an already-descending
/// slice. Only ever called with `Bytes`-backed candidates (see
/// [`prune`]) — a `DriveCopy` never participates in this budget at all.
fn keep_count_by_size(sorted_desc: &[&LeaseRecord], max: u64) -> usize {
    let mut acc = 0u64;
    let mut keep = 0usize;
    for rec in sorted_desc {
        acc += backup_size(&rec.backup);
        if acc > max {
            break;
        }
        keep += 1;
    }
    if keep == 0 && !sorted_desc.is_empty() {
        keep = 1;
    }
    keep
}

/// Prunes the lease ledger at `opts.ledger_path` by age and/or size,
/// dropping each removed row together with the backup it points at.
///
/// The ledger lock is held only briefly, not for this whole call: once to
/// decide the removal candidates (below), then once per row, held across
/// both that row's backup deletion *and* its ledger removal (issue #1687
/// point 4 — the lock is taken before the backup is touched, so a
/// collision leaves the row and its backup both untouched rather than
/// stranding one without the other) — rather than one continuous lock for
/// a potentially long batch of sequential Drive `trash` calls. Nothing
/// else in this codebase ever removes a ledger row, so re-removing a
/// candidate's token by name from whatever the ledger looks like at that
/// later moment is always safe, even if a concurrent `acquire`/`restore`
/// changed unrelated rows in the gap between the snapshot and this row's
/// own turn.
pub async fn prune(client: &DriveClient, opts: &PruneOptions) -> Result<PruneOutcome> {
    let now = Utc::now();

    let (kept_after_filters, removed) = {
        let _lock = LedgerLock::acquire(&opts.ledger_path)?;
        let ledger = LeaseLedger::load(&opts.ledger_path)?;

        let (live_count, non_live): (usize, Vec<LeaseRecord>) = {
            let mut live_count = 0usize;
            let mut non_live = Vec::new();
            for rec in ledger.iter() {
                if rec.is_live(now) {
                    live_count += 1;
                } else {
                    non_live.push(rec.clone());
                }
            }
            (live_count, non_live)
        };

        // Age filter: a non-live row survives into the size filter unless
        // it's strictly past the `--older-than` cutoff (no cutoff means
        // every non-live row proceeds to the size filter, mirroring
        // `request_log::prune` with `older_than: None`; a row expiring
        // exactly at the cutoff survives, mirroring
        // `request_log::keep_by_age`'s `>=`).
        let (mut age_survivors, mut removed): (Vec<LeaseRecord>, Vec<LeaseRecord>) =
            non_live.into_iter().partition(|rec| match opts.older_than {
                Some(cutoff) => rec.expires_at >= cutoff,
                None => true,
            });

        // Size filter, applied only to the `Bytes`-backed age survivors —
        // a `DriveCopy` contributes zero local bytes and must stay
        // reachable only through `--older-than`, so it is set aside first
        // and always kept here regardless of position, never dropped as
        // collateral damage from an oversized `Bytes` backup sorted ahead
        // of it in the same budget.
        if let Some(max) = opts.max_size {
            let (mut bytes_survivors, copy_survivors): (Vec<LeaseRecord>, Vec<LeaseRecord>) =
                age_survivors
                    .into_iter()
                    .partition(|rec| matches!(rec.backup, LeaseBackup::Bytes { .. }));
            bytes_survivors.sort_by_key(|rec| std::cmp::Reverse(rec.expires_at));
            let refs: Vec<&LeaseRecord> = bytes_survivors.iter().collect();
            let keep = keep_count_by_size(&refs, max);
            removed.extend(bytes_survivors.split_off(keep));
            age_survivors = bytes_survivors;
            age_survivors.extend(copy_survivors);
        }

        (live_count + age_survivors.len(), removed)
    };

    let mut outcome = PruneOutcome {
        kept: kept_after_filters,
        ..Default::default()
    };

    if opts.dry_run {
        for rec in &removed {
            account_removal(&mut outcome, &rec.backup);
        }
        return Ok(outcome);
    }

    let files_api = FilesApi::new(client);
    for rec in &removed {
        // Secure the lock *before* touching the backup (issue #1687 point
        // 4) and hold it across both the backup deletion and the ledger
        // removal below, via `LeaseLedger::mutate` — never `mutate_locked`,
        // which would try to acquire this same lock again and self-deadlock
        // (`flock` conflicts against a second `open()` in the *same*
        // process, even from the same caller). A collision here is fully
        // recoverable: nothing has been deleted yet, so this row is simply
        // left for a future prune, exactly like a `clear_backup` failure
        // below — unlike the crash window `LeaseLedger::mutate`'s own
        // failure (after the backup import *is* already gone) can still
        // strand a row, which is a separate, unavoidable failure mode
        // across two storage systems, not a locking bug.
        let lock = match LedgerLock::acquire(&opts.ledger_path) {
            Ok(lock) => lock,
            Err(err) => {
                tracing::warn!(
                    "drive lease prune: failed to lock the ledger to remove lease {} (file \
                     {}): {err:#}; leaving it for a future prune",
                    rec.token,
                    rec.file_id
                );
                record_prune_attempt(rec, "prune-failed", Some(err.to_string()));
                outcome.failed += 1;
                outcome.kept += 1;
                continue;
            }
        };

        match clear_backup(&files_api, &rec.backup).await {
            Ok(()) => {
                // The backup is already gone (or was found already gone),
                // so the ledger must catch up before anything else — a
                // crash right after this call strands at most this one
                // row, never the rest of the batch. A failure *here*
                // (distinct from a crash) can't be made fully atomic with
                // the backup step above across two different storage
                // systems, so make it loud and actionable instead of
                // silently leaving a dangling row: name the stranded token
                // and stop, rather than compounding the same failure
                // across every remaining candidate.
                if let Err(err) = LeaseLedger::mutate(&opts.ledger_path, |ledger| {
                    ledger.remove(&rec.token);
                }) {
                    return Err(err.context(format!(
                        "drive lease prune: cleared the backup for lease {} but failed to \
                         persist its ledger removal — that row may now dangle, pointing at a \
                         backup that no longer exists; remove it from the ledger by hand",
                        rec.token
                    )));
                }
                account_removal(&mut outcome, &rec.backup);
                record_prune_attempt(rec, "pruned", None);
            }
            Err(err) => {
                tracing::warn!(
                    "drive lease prune: failed to clear the backup for lease {} (file {}): \
                     {err:#}; leaving it for a future prune",
                    rec.token,
                    rec.file_id
                );
                record_prune_attempt(rec, "prune-failed", Some(err.to_string()));
                outcome.failed += 1;
                outcome.kept += 1;
            }
        }
        drop(lock);
    }

    Ok(outcome)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::test_support::AuditLogGuard as AuditGuard;
    use crate::utils::secret::Secret;
    use chrono::Duration as ChronoDuration;

    fn test_credentials() -> DriveCredentials {
        DriveCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: DriveGrantedScopes::READONLY,
        }
    }

    async fn client_with_bootstrapped_token(server: &wiremock::MockServer) -> DriveClient {
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
        let mut client = DriveClient::new(&server.uri(), &test_credentials()).unwrap();
        crate::drive::client::test_support::replace_session(
            &mut client,
            &test_credentials(),
            &format!("{}/token", server.uri()),
        );
        client
    }

    fn bytes_record(
        token: &str,
        expires_at: DateTime<Utc>,
        size: u64,
        path: PathBuf,
    ) -> LeaseRecord {
        LeaseRecord {
            token: token.to_string(),
            file_id: format!("file-{token}"),
            version: "1".to_string(),
            modified_time: None,
            backup: LeaseBackup::Bytes {
                path,
                sha256: "deadbeef".to_string(),
                size,
            },
            acquired_at: expires_at - ChronoDuration::minutes(30),
            expires_at,
            released_at: None,
            restored_at: None,
        }
    }

    fn drive_copy_record(token: &str, expires_at: DateTime<Utc>, file_id: &str) -> LeaseRecord {
        LeaseRecord {
            token: token.to_string(),
            file_id: format!("file-{token}"),
            version: "1".to_string(),
            modified_time: None,
            backup: LeaseBackup::DriveCopy {
                file_id: file_id.to_string(),
            },
            acquired_at: expires_at - ChronoDuration::minutes(30),
            expires_at,
            released_at: None,
            restored_at: None,
        }
    }

    #[test]
    fn backup_size_is_zero_for_a_drive_copy() {
        // A `DriveCopy` never reaches `backup_size` through `prune`'s own
        // call site today (only `Bytes`-backed survivors are ever passed to
        // `keep_count_by_size`) — covered directly here so the zero-bytes
        // contract documented on `backup_size` itself stays pinned even if
        // that call site ever changes.
        assert_eq!(
            backup_size(&LeaseBackup::DriveCopy {
                file_id: "drive-copy-1".to_string(),
            }),
            0
        );
    }

    #[test]
    fn prune_outcome_serializes_to_jsonl() {
        let mut buf = Vec::new();
        PruneOutcome {
            removed: 1,
            kept: 2,
            bytes_freed: 3,
            trashed_drive_copies: 1,
            failed: 0,
        }
        .write_jsonl(&mut buf)
        .unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("\"removed\":1"), "{text}");
        assert!(text.contains("\"kept\":2"), "{text}");
    }

    #[tokio::test]
    async fn a_live_lease_is_never_pruned_regardless_of_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        let backup_path = dir.path().join("live-backup");
        std::fs::write(&backup_path, b"x").unwrap();

        let mut ledger = LeaseLedger::default();
        ledger.insert(bytes_record(
            "live",
            Utc::now() + ChronoDuration::hours(1),
            1,
            backup_path.clone(),
        ));
        ledger.save(&ledger_path).unwrap();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let outcome = prune(
            &client,
            &PruneOptions {
                older_than: Some(Utc::now() + ChronoDuration::days(365)),
                max_size: Some(0),
                dry_run: false,
                ledger_path: ledger_path.clone(),
            },
        )
        .await
        .unwrap();

        assert_eq!(outcome.removed, 0);
        assert_eq!(outcome.kept, 1);
        assert!(backup_path.exists());
        assert_eq!(LeaseLedger::load(&ledger_path).unwrap().iter().count(), 1);
    }

    #[tokio::test]
    async fn older_than_alone_drops_only_rows_past_the_cutoff() {
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        let old_backup = dir.path().join("old-backup");
        let recent_backup = dir.path().join("recent-backup");
        std::fs::write(&old_backup, b"old").unwrap();
        std::fs::write(&recent_backup, b"recent").unwrap();

        let mut ledger = LeaseLedger::default();
        ledger.insert(bytes_record(
            "old",
            Utc::now() - ChronoDuration::days(10),
            3,
            old_backup.clone(),
        ));
        ledger.insert(bytes_record(
            "recent",
            Utc::now() - ChronoDuration::hours(1),
            6,
            recent_backup.clone(),
        ));
        ledger.save(&ledger_path).unwrap();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let outcome = prune(
            &client,
            &PruneOptions {
                older_than: Some(Utc::now() - ChronoDuration::days(1)),
                max_size: None,
                dry_run: false,
                ledger_path: ledger_path.clone(),
            },
        )
        .await
        .unwrap();

        assert_eq!(outcome.removed, 1);
        assert_eq!(outcome.kept, 1);
        assert_eq!(outcome.bytes_freed, 3);
        assert!(!old_backup.exists());
        assert!(recent_backup.exists());
        let reloaded = LeaseLedger::load(&ledger_path).unwrap();
        assert!(reloaded.get("old").is_none());
        assert!(reloaded.get("recent").is_some());
    }

    #[tokio::test]
    async fn older_than_boundary_keeps_a_row_expiring_exactly_at_the_cutoff() {
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        let backup_path = dir.path().join("backup");
        std::fs::write(&backup_path, b"x").unwrap();

        let cutoff = Utc::now() - ChronoDuration::days(1);
        let mut ledger = LeaseLedger::default();
        ledger.insert(bytes_record(
            "on-the-boundary",
            cutoff,
            1,
            backup_path.clone(),
        ));
        ledger.save(&ledger_path).unwrap();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let outcome = prune(
            &client,
            &PruneOptions {
                older_than: Some(cutoff),
                max_size: None,
                dry_run: false,
                ledger_path: ledger_path.clone(),
            },
        )
        .await
        .unwrap();

        assert_eq!(
            outcome.removed, 0,
            "a row expiring exactly at the cutoff must survive, mirroring \
             `request_log::keep_by_age`'s inclusive `>=` boundary"
        );
        assert_eq!(outcome.kept, 1);
        assert!(backup_path.exists());
        assert!(LeaseLedger::load(&ledger_path)
            .unwrap()
            .get("on-the-boundary")
            .is_some());
    }

    #[tokio::test]
    async fn a_mixed_batch_persists_successful_removals_independently_of_a_failing_one() {
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");

        let mut ledger = LeaseLedger::default();
        ledger.insert(drive_copy_record(
            "ok",
            Utc::now() - ChronoDuration::hours(2),
            "drive-copy-ok",
        ));
        ledger.insert(drive_copy_record(
            "bad",
            Utc::now() - ChronoDuration::hours(2),
            "drive-copy-bad",
        ));
        ledger.save(&ledger_path).unwrap();

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/drive/v3/files/drive-copy-ok"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "drive-copy-ok", "name": "backup", "trashed": true,
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/drive/v3/files/drive-copy-bad"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let outcome = prune(
            &client,
            &PruneOptions {
                older_than: Some(Utc::now()),
                max_size: None,
                dry_run: false,
                ledger_path: ledger_path.clone(),
            },
        )
        .await
        .unwrap();

        assert_eq!(outcome.removed, 1);
        assert_eq!(outcome.failed, 1);
        assert_eq!(outcome.kept, 1);
        let reloaded = LeaseLedger::load(&ledger_path).unwrap();
        assert!(
            reloaded.get("ok").is_none(),
            "the successful removal must be persisted regardless of the other row's outcome"
        );
        assert!(reloaded.get("bad").is_some());
    }

    #[tokio::test]
    async fn max_size_alone_keeps_the_most_recently_expired_that_fit() {
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        let b1 = dir.path().join("b1");
        let b2 = dir.path().join("b2");
        let b3 = dir.path().join("b3");
        for p in [&b1, &b2, &b3] {
            std::fs::write(p, b"x").unwrap();
        }

        let mut ledger = LeaseLedger::default();
        // Expired 3h, 2h, 1h ago — "t3" is the most recently expired.
        ledger.insert(bytes_record(
            "t1",
            Utc::now() - ChronoDuration::hours(3),
            10,
            b1.clone(),
        ));
        ledger.insert(bytes_record(
            "t2",
            Utc::now() - ChronoDuration::hours(2),
            10,
            b2.clone(),
        ));
        ledger.insert(bytes_record(
            "t3",
            Utc::now() - ChronoDuration::hours(1),
            10,
            b3.clone(),
        ));
        ledger.save(&ledger_path).unwrap();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        // Budget for exactly the two most-recently-expired (t3 + t2 = 20).
        let outcome = prune(
            &client,
            &PruneOptions {
                older_than: None,
                max_size: Some(20),
                dry_run: false,
                ledger_path: ledger_path.clone(),
            },
        )
        .await
        .unwrap();

        assert_eq!(outcome.removed, 1);
        assert_eq!(outcome.bytes_freed, 10);
        assert!(!b1.exists(), "the oldest-expiring backup should be dropped");
        assert!(b2.exists());
        assert!(b3.exists());
        let reloaded = LeaseLedger::load(&ledger_path).unwrap();
        assert!(reloaded.get("t1").is_none());
        assert!(reloaded.get("t2").is_some());
        assert!(reloaded.get("t3").is_some());
    }

    #[tokio::test]
    async fn max_size_alone_never_prunes_a_drive_copy_since_it_is_zero_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");

        let mut ledger = LeaseLedger::default();
        ledger.insert(drive_copy_record(
            "copy",
            Utc::now() - ChronoDuration::hours(2),
            "drive-copy-1",
        ));
        ledger.save(&ledger_path).unwrap();

        // Deliberately no PATCH mock mounted — a DriveCopy is 0 bytes, so
        // even `--max-size 0` never makes it a removal candidate on its
        // own; only `--older-than` does. Reaching `FilesApi::trash` here
        // fails the test.
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let outcome = prune(
            &client,
            &PruneOptions {
                older_than: None,
                max_size: Some(0),
                dry_run: false,
                ledger_path: ledger_path.clone(),
            },
        )
        .await
        .unwrap();

        assert_eq!(outcome.removed, 0);
        assert_eq!(outcome.kept, 1);
        assert!(LeaseLedger::load(&ledger_path)
            .unwrap()
            .get("copy")
            .is_some());
    }

    #[tokio::test]
    async fn max_size_never_trashes_a_drive_copy_sorted_behind_an_oversized_bytes_backup() {
        // Regression test: `--max-size` must never touch a `DriveCopy` just
        // because it happens to sort behind (i.e. expire earlier than) an
        // oversized `Bytes` backup that alone exceeds the budget on its
        // own — a `DriveCopy` is 0 local bytes and is only ever reachable
        // through `--older-than`. Before the fix, the size filter's single
        // combined "keep from newest until budget exceeded" pass treated
        // position, not backup kind, as what determined removal, so a
        // `DriveCopy` sorted after an oversized `Bytes` record was dropped
        // (and trashed on Drive) purely as that record's collateral
        // damage.
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        let big_backup = dir.path().join("big-backup");
        std::fs::write(&big_backup, b"x").unwrap();

        let mut ledger = LeaseLedger::default();
        // "big" is the more-recently-expired of the two, so it sorts
        // ahead of "copy" in the size filter's newest-first order.
        ledger.insert(bytes_record(
            "big",
            Utc::now() - ChronoDuration::hours(1),
            1_000_000,
            big_backup.clone(),
        ));
        ledger.insert(drive_copy_record(
            "copy",
            Utc::now() - ChronoDuration::hours(2),
            "drive-copy-1",
        ));
        ledger.save(&ledger_path).unwrap();

        // Deliberately no PATCH mock mounted — reaching `FilesApi::trash`
        // for "copy" fails the test.
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let outcome = prune(
            &client,
            &PruneOptions {
                older_than: None,
                max_size: Some(1), // far below "big"'s own size
                dry_run: false,
                ledger_path: ledger_path.clone(),
            },
        )
        .await
        .unwrap();

        // "big" alone exceeds the budget, so the always-keep-the-newest
        // rule (mirroring `log prune`) keeps it regardless — the point
        // under test is that "copy" is untouched either way.
        assert_eq!(outcome.removed, 0);
        assert_eq!(outcome.trashed_drive_copies, 0);
        let reloaded = LeaseLedger::load(&ledger_path).unwrap();
        assert!(
            reloaded.get("copy").is_some(),
            "a DriveCopy must never be pruned by --max-size alone"
        );
        assert!(reloaded.get("big").is_some());
    }

    #[tokio::test]
    async fn a_drive_copy_backup_is_trashed_and_dropped_together_with_its_row() {
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");

        let mut ledger = LeaseLedger::default();
        ledger.insert(drive_copy_record(
            "copy",
            Utc::now() - ChronoDuration::hours(2),
            "drive-copy-1",
        ));
        ledger.save(&ledger_path).unwrap();

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/drive/v3/files/drive-copy-1"))
            .and(wiremock::matchers::body_json(
                serde_json::json!({"trashed": true}),
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "drive-copy-1", "name": "backup", "trashed": true,
                })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let outcome = prune(
            &client,
            &PruneOptions {
                older_than: Some(Utc::now()),
                max_size: None,
                dry_run: false,
                ledger_path: ledger_path.clone(),
            },
        )
        .await
        .unwrap();

        assert_eq!(outcome.removed, 1);
        assert_eq!(outcome.trashed_drive_copies, 1);
        assert!(LeaseLedger::load(&ledger_path)
            .unwrap()
            .get("copy")
            .is_none());
    }

    #[tokio::test]
    async fn a_drive_copy_already_trashed_is_tolerated_as_success() {
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");

        let mut ledger = LeaseLedger::default();
        ledger.insert(drive_copy_record(
            "copy",
            Utc::now() - ChronoDuration::hours(2),
            "drive-copy-1",
        ));
        ledger.save(&ledger_path).unwrap();

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/drive/v3/files/drive-copy-1"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let outcome = prune(
            &client,
            &PruneOptions {
                older_than: Some(Utc::now()),
                max_size: None,
                dry_run: false,
                ledger_path: ledger_path.clone(),
            },
        )
        .await
        .unwrap();

        assert_eq!(outcome.removed, 1);
        assert_eq!(outcome.failed, 0);
        assert!(LeaseLedger::load(&ledger_path)
            .unwrap()
            .get("copy")
            .is_none());
    }

    #[tokio::test]
    async fn a_bytes_backup_already_removed_is_tolerated_as_success() {
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        // Deliberately never written to disk — a prior manual cleanup or a
        // previous partially-failed prune run could have already removed
        // it; `clear_backup` must tolerate `NotFound` the same way it does
        // for a `DriveCopy`'s already-trashed 404.
        let backup_path = dir.path().join("already-gone");

        let mut ledger = LeaseLedger::default();
        ledger.insert(bytes_record(
            "old",
            Utc::now() - ChronoDuration::days(1),
            1,
            backup_path,
        ));
        ledger.save(&ledger_path).unwrap();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let outcome = prune(
            &client,
            &PruneOptions {
                older_than: Some(Utc::now()),
                max_size: None,
                dry_run: false,
                ledger_path: ledger_path.clone(),
            },
        )
        .await
        .unwrap();

        assert_eq!(outcome.removed, 1);
        assert_eq!(outcome.failed, 0);
        assert!(LeaseLedger::load(&ledger_path)
            .unwrap()
            .get("old")
            .is_none());
    }

    #[tokio::test]
    async fn a_bytes_backup_deletion_failure_leaves_the_row_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        // A directory in place of the backup file forces `remove_file` to
        // fail with something other than `NotFound` (mirrors the same
        // trick used elsewhere in this crate to force a non-`NotFound`
        // I/O failure deterministically).
        let backup_path = dir.path().join("not-a-file");
        std::fs::create_dir(&backup_path).unwrap();

        let mut ledger = LeaseLedger::default();
        ledger.insert(bytes_record(
            "old",
            Utc::now() - ChronoDuration::days(1),
            1,
            backup_path,
        ));
        ledger.save(&ledger_path).unwrap();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let outcome = prune(
            &client,
            &PruneOptions {
                older_than: Some(Utc::now()),
                max_size: None,
                dry_run: false,
                ledger_path: ledger_path.clone(),
            },
        )
        .await
        .unwrap();

        assert_eq!(outcome.removed, 0);
        assert_eq!(outcome.failed, 1);
        assert_eq!(outcome.kept, 1);
        assert!(LeaseLedger::load(&ledger_path)
            .unwrap()
            .get("old")
            .is_some());
    }

    #[tokio::test]
    async fn a_backup_deletion_failure_leaves_the_row_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");

        let mut ledger = LeaseLedger::default();
        ledger.insert(drive_copy_record(
            "copy",
            Utc::now() - ChronoDuration::hours(2),
            "drive-copy-1",
        ));
        ledger.save(&ledger_path).unwrap();

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/drive/v3/files/drive-copy-1"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let outcome = prune(
            &client,
            &PruneOptions {
                older_than: Some(Utc::now()),
                max_size: None,
                dry_run: false,
                ledger_path: ledger_path.clone(),
            },
        )
        .await
        .unwrap();

        assert_eq!(outcome.removed, 0);
        assert_eq!(outcome.failed, 1);
        assert_eq!(outcome.kept, 1);
        assert!(LeaseLedger::load(&ledger_path)
            .unwrap()
            .get("copy")
            .is_some());
    }

    #[tokio::test]
    async fn dry_run_reports_without_deleting_or_saving() {
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        let backup_path = dir.path().join("backup");
        std::fs::write(&backup_path, b"x").unwrap();

        let mut ledger = LeaseLedger::default();
        ledger.insert(bytes_record(
            "old",
            Utc::now() - ChronoDuration::days(1),
            1,
            backup_path.clone(),
        ));
        ledger.save(&ledger_path).unwrap();
        let before = std::fs::read(&ledger_path).unwrap();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let outcome = prune(
            &client,
            &PruneOptions {
                older_than: Some(Utc::now()),
                max_size: None,
                dry_run: true,
                ledger_path: ledger_path.clone(),
            },
        )
        .await
        .unwrap();

        assert_eq!(outcome.removed, 1);
        assert_eq!(outcome.bytes_freed, 1);
        assert!(backup_path.exists(), "dry-run must not delete the backup");
        let after = std::fs::read(&ledger_path).unwrap();
        assert_eq!(before, after, "dry-run must not modify the ledger");
    }

    #[tokio::test]
    async fn no_op_prune_does_not_rewrite_the_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        let mut ledger = LeaseLedger::default();
        ledger.insert(bytes_record(
            "live",
            Utc::now() + ChronoDuration::hours(1),
            1,
            dir.path().join("backup"),
        ));
        ledger.save(&ledger_path).unwrap();
        let before_mtime = std::fs::metadata(&ledger_path).unwrap().modified().unwrap();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        prune(
            &client,
            &PruneOptions {
                older_than: Some(Utc::now() - ChronoDuration::days(1)),
                max_size: None,
                dry_run: false,
                ledger_path: ledger_path.clone(),
            },
        )
        .await
        .unwrap();

        let after_mtime = std::fs::metadata(&ledger_path).unwrap().modified().unwrap();
        assert_eq!(before_mtime, after_mtime);
    }

    #[tokio::test]
    async fn a_successful_removal_writes_a_pruned_audit_record() {
        let dir = tempfile::tempdir().unwrap();
        let audit = AuditGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        let backup_path = dir.path().join("backup");
        std::fs::write(&backup_path, b"x").unwrap();

        let mut ledger = LeaseLedger::default();
        ledger.insert(bytes_record(
            "old",
            Utc::now() - ChronoDuration::days(1),
            1,
            backup_path,
        ));
        ledger.save(&ledger_path).unwrap();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        prune(
            &client,
            &PruneOptions {
                older_than: Some(Utc::now()),
                max_size: None,
                dry_run: false,
                ledger_path: ledger_path.clone(),
            },
        )
        .await
        .unwrap();

        assert_eq!(
            audit.verdicts(),
            vec!["pruned".to_string()],
            "a pruned lease's removal must be independently discoverable via `omni-dev log \
             --audit`, not just the transient CLI summary"
        );
    }

    #[tokio::test]
    async fn a_failed_removal_writes_a_prune_failed_audit_record_carrying_the_error() {
        let dir = tempfile::tempdir().unwrap();
        let audit = AuditGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");

        let mut ledger = LeaseLedger::default();
        ledger.insert(drive_copy_record(
            "copy",
            Utc::now() - ChronoDuration::hours(2),
            "drive-copy-1",
        ));
        ledger.save(&ledger_path).unwrap();

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/drive/v3/files/drive-copy-1"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        prune(
            &client,
            &PruneOptions {
                older_than: Some(Utc::now()),
                max_size: None,
                dry_run: false,
                ledger_path: ledger_path.clone(),
            },
        )
        .await
        .unwrap();

        assert_eq!(audit.verdicts(), vec!["prune-failed".to_string()]);
        let records = audit.records();
        assert_eq!(records.len(), 1);
        assert!(
            records[0].error.is_some(),
            "the underlying clear_backup error must be captured, not just an opaque count"
        );
    }

    #[tokio::test]
    async fn a_best_effort_audit_write_failure_is_warned_and_swallowed() {
        // Mirrors `restore.rs`'s identically-named test: a directory in
        // place of the audit file forces `record_audit_event` to fail.
        // `record_prune_attempt` is best-effort — the prune itself (already
        // committed by the time this is called) must still succeed.
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(dir.path());
        std::fs::create_dir(dir.path().join("audit.jsonl")).unwrap();
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        let backup_path = dir.path().join("backup");
        std::fs::write(&backup_path, b"x").unwrap();

        let mut ledger = LeaseLedger::default();
        ledger.insert(bytes_record(
            "old",
            Utc::now() - ChronoDuration::days(1),
            1,
            backup_path.clone(),
        ));
        ledger.save(&ledger_path).unwrap();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let outcome = prune(
            &client,
            &PruneOptions {
                older_than: Some(Utc::now()),
                max_size: None,
                dry_run: false,
                ledger_path: ledger_path.clone(),
            },
        )
        .await
        .unwrap();

        assert_eq!(outcome.removed, 1);
        assert!(!backup_path.exists());
        assert!(LeaseLedger::load(&ledger_path)
            .unwrap()
            .get("old")
            .is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_ledger_persist_failure_after_clearing_the_backup_is_reported_loudly() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");

        let mut ledger = LeaseLedger::default();
        ledger.insert(drive_copy_record(
            "copy",
            Utc::now() - ChronoDuration::hours(2),
            "drive-copy-1",
        ));
        ledger.save(&ledger_path).unwrap();

        let server = wiremock::MockServer::start().await;
        // An artificial delay on the `trash` response gives the background
        // thread below a wide window to break the ledger directory's
        // writability *after* this row's own lock has already been
        // acquired (so the lock file, created during candidate selection,
        // just needs re-opening — no dir-write required) but *before*
        // `LeaseLedger::mutate`'s save reaches its temp-file creation,
        // which does need dir-write — the exact gap `prune`'s own doc
        // comment calls out as the one case a crash (or, here, a
        // filesystem fault) can still strand a row even with the lock
        // held throughout.
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/drive/v3/files/drive-copy-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({
                        "id": "drive-copy-1", "name": "backup", "trashed": true,
                    }))
                    .set_delay(std::time::Duration::from_millis(150)),
            )
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;

        let dir_path = dir.path().to_path_buf();
        let jammer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            std::fs::set_permissions(&dir_path, std::fs::Permissions::from_mode(0o500)).unwrap();
        });

        let err = prune(
            &client,
            &PruneOptions {
                older_than: Some(Utc::now()),
                max_size: None,
                dry_run: false,
                ledger_path: ledger_path.clone(),
            },
        )
        .await
        .unwrap_err();

        jammer.join().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(err.to_string().contains("may now dangle"), "{err:?}");
    }

    #[tokio::test]
    async fn prune_refuses_while_another_lock_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        let _held = LedgerLock::acquire(&ledger_path).unwrap();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let err = prune(
            &client,
            &PruneOptions {
                older_than: Some(Utc::now()),
                max_size: None,
                dry_run: false,
                ledger_path: ledger_path.clone(),
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("already be in progress"));
    }
}
