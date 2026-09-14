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
//! backup already gone) does the row itself get dropped from the ledger.
//! A backup-deletion failure for one row skips just that row — it is left
//! for a future prune, not treated as a hard error for the whole command.

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
    /// Drop non-live rows whose `expires_at` is at or before this cutoff. A
    /// live row is never a candidate regardless of this bound.
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

/// Longest prefix of `sorted_desc` (sorted newest-`expires_at`-first) whose
/// backup bytes fit within `max` — but never fewer than the single
/// most-recently-expired candidate, even if it alone exceeds the budget.
/// Mirrors `request_log::keep_by_size`, adapted from reverse iteration over
/// chronological lines to forward iteration over an already-descending
/// slice.
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
/// Runs under [`LedgerLock`] for its whole duration (load through save), so
/// a concurrent `drive lease acquire`/`restore` can't race the rewrite —
/// the same lock those commands themselves hold while mutating the ledger.
pub async fn prune(client: &DriveClient, opts: &PruneOptions) -> Result<PruneOutcome> {
    let _lock = LedgerLock::acquire(&opts.ledger_path)?;
    let mut ledger = LeaseLedger::load(&opts.ledger_path)?;
    let now = Utc::now();

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

    // Age filter: a non-live row survives into the size filter unless it's
    // past the `--older-than` cutoff (no cutoff means every non-live row
    // proceeds to the size filter, mirroring `request_log::prune` with
    // `older_than: None`).
    let (mut age_survivors, mut removed): (Vec<LeaseRecord>, Vec<LeaseRecord>) =
        non_live.into_iter().partition(|rec| match opts.older_than {
            Some(cutoff) => rec.expires_at > cutoff,
            None => true,
        });

    // Size filter, applied only to the age survivors: keep the
    // most-recently-expired ones whose backups fit `max_size`, moving the
    // rest into `removed` too.
    if let Some(max) = opts.max_size {
        age_survivors.sort_by_key(|rec| std::cmp::Reverse(rec.expires_at));
        let refs: Vec<&LeaseRecord> = age_survivors.iter().collect();
        let keep = keep_count_by_size(&refs, max);
        removed.extend(age_survivors.split_off(keep));
    }

    let files_api = FilesApi::new(client);
    let mut outcome = PruneOutcome {
        kept: live_count + age_survivors.len(),
        ..Default::default()
    };

    for rec in &removed {
        if opts.dry_run {
            outcome.removed += 1;
            match &rec.backup {
                LeaseBackup::Bytes { size, .. } => outcome.bytes_freed += size,
                LeaseBackup::DriveCopy { .. } => outcome.trashed_drive_copies += 1,
            }
            continue;
        }
        if clear_backup(&files_api, &rec.backup).await.is_ok() {
            ledger.remove(&rec.token);
            outcome.removed += 1;
            match &rec.backup {
                LeaseBackup::Bytes { size, .. } => outcome.bytes_freed += size,
                LeaseBackup::DriveCopy { .. } => outcome.trashed_drive_copies += 1,
            }
        } else {
            outcome.failed += 1;
            outcome.kept += 1;
        }
    }

    if !opts.dry_run && outcome.removed > 0 {
        ledger.save(&opts.ledger_path)?;
    }

    Ok(outcome)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
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

    #[tokio::test]
    async fn a_live_lease_is_never_pruned_regardless_of_bounds() {
        let dir = tempfile::tempdir().unwrap();
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
    async fn max_size_alone_keeps_the_most_recently_expired_that_fit() {
        let dir = tempfile::tempdir().unwrap();
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
    async fn a_drive_copy_backup_is_trashed_and_dropped_together_with_its_row() {
        let dir = tempfile::tempdir().unwrap();
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
    async fn a_backup_deletion_failure_leaves_the_row_in_place() {
        let dir = tempfile::tempdir().unwrap();
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
    async fn prune_refuses_while_another_lock_is_held() {
        let dir = tempfile::tempdir().unwrap();
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
