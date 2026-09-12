//! `drive lease restore <TOKEN>` — the engine behind
//! [ADR-0080](../../../docs/adrs/adr-0080.md) §10: restore a file from the
//! backup a lease recorded, closing the recovery gap
//! [ADR-0077](../../../docs/adrs/adr-0077-sheets-deletion-via-batchupdate.md)
//! §5 admitted.
//!
//! `<TOKEN>` names the *backup* lease — the one whose row records where the
//! content to restore from actually lives — not a lease presented to
//! authorise this write. Restore mints its **own**, fresh lease internally
//! (reusing [`acquire`] verbatim: Touch ID, a backup of the file's *current*
//! state, a new ledger row) before ever writing, so the restore is itself
//! reversible by the same verb, and prints the new token for exactly that
//! reason. One command, one prompt.
//!
//! **Binary files only, for now.** A native document's backup is a Drive
//! copy, restorable today by a human via the Drive UI; ADR-0080 §10 names
//! exactly one typed restore path worth building for a native file — a
//! deleted sheet's backup copy still contains that sheet, so `restore`
//! could offer `spreadsheets.sheets.copyTo` from the backup into the live
//! spreadsheet — and reserves it as a following change rather than
//! papering over the gap: every native-document backup token restores as
//! [`RestoreResult::NoTypedRestorePath`], honestly reporting the backup's
//! location and stopping, exactly the fallback the ADR describes for "where
//! no typed path exists."

use std::path::Path;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::cli::drive::format::JsonlSerialize;
use crate::drive::client::DriveClient;
use crate::drive::files_api::FilesApi;
use crate::drive::folder_ancestry;
use crate::drive::lease::acquire::{self, AcquireOptions, AcquireResult};
use crate::drive::lease::authenticate::{AuthPolicy, Authenticator};
use crate::drive::lease::check::{
    finish_leased_write, gate_leased_write, record_failed_leased_write, LeaseGateRefusal,
    LeasedWrite,
};
use crate::drive::lease::ledger::{LeaseBackup, LeaseLedger};
use crate::drive::write_gate::{self, DecidingRule, DriveOperation, FolderPermissionRule};
use crate::request_log::AuditOutcome;

/// Per-call options for `drive lease restore`.
#[derive(Debug, Clone)]
pub struct RestoreOptions {
    /// The *backup* lease's token — locates the backup to restore from, and
    /// authorises nothing itself (a fresh prompt does).
    pub token: String,
    /// Local directory the fresh lease's own byte backup (of the file's
    /// *current*, pre-restore state) is written under.
    pub backup_dir: std::path::PathBuf,
    /// Destination folder for the fresh lease's own native-document backup,
    /// if the restore target somehow is one — see the module doc for why
    /// that path is unreachable today (every native backup token restores
    /// as [`RestoreResult::NoTypedRestorePath`] before a fresh lease is
    /// ever acquired).
    pub native_backup_folder_id: Option<String>,
    /// How long the fresh lease stays live.
    pub expiry: ChronoDuration,
    /// Which authentication policy the fresh lease's acquisition presents
    /// (ADR-0080 §7).
    pub auth_policy: AuthPolicy,
    /// Path to the lease ledger — both the backup token's own row and the
    /// fresh lease this restore mints live here.
    pub ledger_path: std::path::PathBuf,
}

/// What happened.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum RestoreResult {
    /// Restored, and a fresh lease now covers the file. Present `new_token`
    /// to a later `--lease` the same way any other acquired lease would be.
    Restored {
        /// The fresh lease's token.
        new_token: String,
        /// When the fresh lease expires.
        expires_at: DateTime<Utc>,
        /// Where the fresh lease's own backup (of the file's state
        /// immediately before this restore) landed.
        backup: LeaseBackup,
    },
    /// `<TOKEN>` names no row this ledger has ever recorded.
    NoSuchBackupToken,
    /// The backup is a native-document Drive copy, and no typed restore
    /// path exists for it yet — see the module doc. Restorable today by a
    /// human via the Drive UI at `backup_location`.
    NoTypedRestorePath {
        /// The backup copy's own Drive file id.
        backup_location: String,
    },
    /// The backup's bytes are too large for Drive's simple-upload endpoint
    /// (`files.update` with `uploadType=media`, capped at 5 MB — see
    /// [`crate::drive::files_api::check_upload_size`]); restoring it would
    /// need resumable upload, not yet supported. Refused before any network
    /// call — from the backup's own recorded size, no fresh lease minted,
    /// no Touch ID spent — since a backup this large is guaranteed to fail
    /// the same check the restore write's own `edit_content` call would
    /// make *after* the prompt (issue #1664 review finding: `acquire` can
    /// back up a binary file up to its own, much larger download cap, but
    /// only ever restore up to the upload cap — checking only at write
    /// time would waste a real authentication prompt, a fresh backup and a
    /// live ledger row on a restore that could never have succeeded).
    BackupTooLargeForSimpleUpload {
        /// The backup's size in bytes.
        size: u64,
    },
    /// The target has no parents this account can see and no `file_id`
    /// rule named it — mirrors `EditResult::RefusedNoVisibleParents`.
    RefusedNoVisibleParents,
    /// The folder write-permission gate refused it. Checked even though
    /// this write mints its own lease internally: the lease is a *third*,
    /// independent gate (ADR-0080 Consequences — "no bypass exists across
    /// all three"), never a substitute for the other two.
    Blocked {
        /// The rule that decided the refusal, if any.
        decided_by: Option<DecidingRule>,
    },
    /// A live lease already covers the file — bubbled from the internal
    /// [`acquire`] step. Present its token to `--lease` instead; re-running
    /// `restore` once it expires will proceed.
    AlreadyLeased {
        /// The existing lease's token.
        token: String,
        /// When it expires.
        expires_at: DateTime<Utc>,
    },
    /// The internal [`acquire`] step refused a native-document target. Reachable:
    /// the backup being restored from is a `Bytes` backup (a `DriveCopy`
    /// backup short-circuits to [`Self::NoTypedRestorePath`] before any
    /// network call), but the file at `file_id` has since become a
    /// Google-native document and this account has no
    /// `lease_backup_folder_id` configured — the same refusal `acquire`
    /// gives any other native-document target with nowhere to put its
    /// backup. When a backup folder *is* configured, `acquire` instead
    /// succeeds (taking a native Drive-copy backup of the file's current
    /// state), and the write itself is refused post-lease via
    /// [`Self::FreshLeaseButWriteFailed`] — see the mime-type re-check
    /// immediately before the restore write.
    RefusedNativeDocument,
    /// A human answered the fresh authentication prompt and refused, or it
    /// timed out.
    Denied {
        /// The platform's own message.
        detail: String,
    },
    /// No authenticator is available in this context (ADR-0080 §8).
    Unavailable {
        /// Why no authenticator is available.
        detail: String,
    },
    /// An API, filesystem, integrity or ledger error — before any fresh
    /// lease was minted. No Touch ID was spent and no backup was taken.
    Failed {
        /// A human-readable summary of what failed.
        detail: String,
    },
    /// A fresh lease *was* minted — Touch ID answered, the file's current
    /// state backed up, a ledger row written — but the restore write
    /// itself could not go ahead: either the write-permission gate, checked
    /// again immediately before the write (a human can answer the Touch ID
    /// prompt up to two minutes after it's presented, ADR-0080 §7, and a
    /// permission change landing in that window must not be ignored), now
    /// refuses it, or the mutating call itself failed. `token` is real and
    /// live regardless: present it to a later `--lease`, or re-run `drive
    /// lease restore token` to use it rather than spending another prompt.
    FreshLeaseButWriteFailed {
        /// The fresh lease's token — not orphaned, even though this
        /// attempt did not use it to write anything.
        token: String,
        /// When the fresh lease expires.
        expires_at: DateTime<Utc>,
        /// What stopped the write.
        detail: String,
    },
}

impl JsonlSerialize for RestoreResult {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> Result<(), anyhow::Error> {
        crate::cli::drive::format::write_scalar_jsonl(self, out)
    }
}

impl RestoreResult {
    /// The kebab-case status for the audit record — matches this enum's own
    /// `#[serde(tag = "status")]` shape (ADR-0080 §11's free-form `verdict`
    /// vocabulary).
    fn verdict(&self) -> &'static str {
        match self {
            Self::Restored { .. } => "restored",
            Self::NoSuchBackupToken => "no-such-backup-token",
            Self::NoTypedRestorePath { .. } => "no-typed-restore-path",
            Self::BackupTooLargeForSimpleUpload { .. } => "backup-too-large-for-simple-upload",
            Self::RefusedNoVisibleParents => "refused-no-visible-parents",
            Self::Blocked { .. } => "blocked",
            Self::AlreadyLeased { .. } => "already-leased",
            Self::RefusedNativeDocument => "refused-native-document",
            Self::Denied { .. } => "denied",
            Self::Unavailable { .. } => "unavailable",
            Self::Failed { .. } => "failed",
            Self::FreshLeaseButWriteFailed { .. } => "fresh-lease-but-write-failed",
        }
    }
}

/// Runs one restore attempt, then records it to the audit sink regardless
/// of outcome — best-effort, naming both tokens (ADR-0080 §11), the same
/// way [`acquire::acquire`]'s own top-level record is best-effort rather
/// than write-ahead: this record summarises the whole attempt, distinct
/// from the write-ahead/outcome pair [`gate_leased_write`]/
/// [`finish_leased_write`] already write, keyed on the *fresh* token, for
/// the actual mutating call below.
pub async fn restore(
    client: &DriveClient,
    opts: &RestoreOptions,
    authenticator: &dyn Authenticator,
    rules: &[FolderPermissionRule],
) -> RestoreResult {
    let result = restore_inner(client, opts, authenticator, rules).await;
    record_attempt(opts, &result);
    result
}

async fn restore_inner(
    client: &DriveClient,
    opts: &RestoreOptions,
    authenticator: &dyn Authenticator,
    rules: &[FolderPermissionRule],
) -> RestoreResult {
    let backup_record = match LeaseLedger::load(&opts.ledger_path) {
        Ok(ledger) => ledger.get(&opts.token).cloned(),
        Err(err) => {
            return RestoreResult::Failed {
                detail: err.to_string(),
            }
        }
    };
    let Some(backup_record) = backup_record else {
        return RestoreResult::NoSuchBackupToken;
    };
    let file_id = backup_record.file_id.clone();

    // Which restore path applies is decided from the backup's own shape
    // alone — no Drive call needed to know it — so a native-document token
    // this phase cannot yet act on (see the module doc) is refused before
    // the gate or any network call at all, mirroring how
    // `content_edit.rs` refuses a Google-native target before its own
    // gate: this is a structural fact, not a policy decision. Likewise, a
    // backup too large for Drive's simple-upload endpoint is refused from
    // its own recorded size alone (issue #1664 review finding) — see
    // `RestoreResult::BackupTooLargeForSimpleUpload`'s doc comment.
    let (backup_path, backup_sha256) = match &backup_record.backup {
        LeaseBackup::DriveCopy { file_id: copy_id } => {
            return RestoreResult::NoTypedRestorePath {
                backup_location: copy_id.clone(),
            };
        }
        LeaseBackup::Bytes { path, sha256, size } => {
            if crate::drive::files_api::check_upload_size(*size).is_err() {
                return RestoreResult::BackupTooLargeForSimpleUpload { size: *size };
            }
            (path.clone(), sha256.clone())
        }
    };

    let files_api = FilesApi::new(client);
    // The write-permission gate — a *third*, independent check alongside
    // OAuth scope and the lease this function is about to mint, never
    // substituted by either (ADR-0080 Consequences). The fetched target
    // itself is not needed past this point — only the re-check immediately
    // before the write (below) needs its own, fresher fetch.
    match check_write_permission_gate(&files_api, &file_id, rules).await {
        GateCheck::Ok(_) => {}
        GateCheck::Failed(detail) => return RestoreResult::Failed { detail },
        GateCheck::NoVisibleParents => return RestoreResult::RefusedNoVisibleParents,
        GateCheck::Denied(decided_by) => return RestoreResult::Blocked { decided_by },
    }

    // Synchronous filesystem I/O (a read plus a SHA-256 hash over the
    // whole backup) on the async runtime's current thread — `block_in_place`
    // hands this worker thread's other queued tasks off to the runtime's
    // other workers for the duration, the same reasoning `acquire.rs`'s own
    // `write_backup`/`insert_record` calls document (issue #1664 review
    // finding: this call and `mark_backup_restored`'s below were the two
    // synchronous calls in this module not already following that pattern).
    let backup_bytes = match tokio::task::block_in_place(|| {
        verify_and_read_backup(&backup_path, &backup_sha256)
    }) {
        Ok(bytes) => bytes,
        Err(detail) => return RestoreResult::Failed { detail },
    };

    // Restore is itself a write (ADR-0080 §10): mint a fresh lease on the
    // same file, unconditionally — never gated on `requires_lease`/
    // `require_lease: false` the way an ordinary write is, since restore's
    // whole purpose is the backup-then-lease mechanism itself, not a
    // policy an operator can opt this verb out of.
    let acquire_opts = AcquireOptions {
        file_id: file_id.clone(),
        backup_dir: opts.backup_dir.clone(),
        native_backup_folder_id: opts.native_backup_folder_id.clone(),
        expiry: opts.expiry,
        auth_policy: opts.auth_policy,
        ledger_path: opts.ledger_path.clone(),
    };
    let (new_token, expires_at, fresh_backup) =
        match acquire::acquire(client, &acquire_opts, authenticator).await {
            AcquireResult::Acquired {
                token,
                expires_at,
                backup,
            } => (token, expires_at, backup),
            AcquireResult::AlreadyLeased { token, expires_at } => {
                return RestoreResult::AlreadyLeased { token, expires_at }
            }
            AcquireResult::RefusedNativeDocument => return RestoreResult::RefusedNativeDocument,
            AcquireResult::Denied { detail } => return RestoreResult::Denied { detail },
            AcquireResult::Unavailable { detail } => return RestoreResult::Unavailable { detail },
            AcquireResult::Failed { detail } => return RestoreResult::Failed { detail },
        };
    // From here on, a fresh lease is real and live — every remaining
    // refusal must say so via `FreshLeaseButWriteFailed` rather than a
    // bare `Failed`/`Blocked`, or the token (Touch ID spent, a real backup
    // taken, a real ledger row) would be surfaced nowhere the caller could
    // ever find it again (issue #1664 review finding).
    let fresh_lease_but = |detail: String| RestoreResult::FreshLeaseButWriteFailed {
        token: new_token.clone(),
        expires_at,
        detail,
    };

    // The write-permission gate is re-checked against a *fresh* target
    // fetch here, immediately before the write — not reused from the
    // check above, before `acquire`'s internal Touch ID prompt: that
    // prompt can take up to two minutes to answer (ADR-0080 §7), and a
    // permission change landing in that window must not be silently
    // ignored just because it was already checked once (issue #1664
    // review finding, mirroring `content_edit.rs`'s own "re-fetched fresh
    // here rather than reusing the earlier snapshot" reasoning for its
    // staleness check).
    let target = match check_write_permission_gate(&files_api, &file_id, rules).await {
        GateCheck::Ok(target) => target,
        GateCheck::Failed(detail) => return fresh_lease_but(detail),
        GateCheck::NoVisibleParents | GateCheck::Denied(_) => {
            return fresh_lease_but(
                "the write-permission gate no longer allows this write, re-checked after the \
                 fresh lease's authentication prompt"
                    .to_string(),
            )
        }
    };

    // The target may also have changed *kind* during the same window: a
    // `Bytes` backup can only ever be restored into a still-binary file.
    // `acquire` above only refuses a native-document target when no
    // `native_backup_folder_id` is configured for this account (see
    // `RestoreResult::RefusedNativeDocument`'s doc comment) — when a backup
    // folder *is* configured, `acquire` instead succeeds by taking a native
    // Drive-copy backup of whatever the file now is, leaving nothing to
    // stop this write from PATCHing stale binary bytes into what is now a
    // Google-native document unless it is caught here (issue #1664 review
    // finding).
    if target.is_google_native() {
        return fresh_lease_but(
            "the target has become a Google-native document since this backup was taken; a \
             byte backup cannot be restored into a native document"
                .to_string(),
        );
    }

    // The restore write itself, through the exact same audited, fail-closed
    // path every other leased write in this codebase uses — the fresh
    // token was just minted above, so the check below is expected to
    // succeed, but routing through it anyway (rather than writing directly)
    // keeps "every content-mutating call goes through this one path" true
    // with no carve-out for restore.
    let leased = LeasedWrite {
        log_prefix: "drive lease restore",
        operation: "lease-restore",
        ledger_path: &opts.ledger_path,
        file_id: &file_id,
    };
    let lock = match gate_leased_write(leased, &files_api, Some(&new_token)).await {
        Ok(lock) => lock,
        Err(refusal) => return fresh_lease_but(leased_write_refusal_detail(refusal)),
    };
    let result = match files_api
        .edit_content(&file_id, &backup_bytes, &target.mime_type)
        .await
    {
        Ok(updated) => {
            finish_leased_write(
                leased,
                &lock,
                &new_token,
                updated.version,
                updated.modified_time,
            );
            RestoreResult::Restored {
                new_token: new_token.clone(),
                expires_at,
                backup: fresh_backup,
            }
        }
        Err(err) => {
            let detail = err.to_string();
            record_failed_leased_write(leased, &new_token, &detail);
            fresh_lease_but(detail)
        }
    };
    drop(lock);

    // Mark the backup lease's own row as consumed (ADR-0080 §4's
    // "transition"), best-effort — the restore itself already succeeded or
    // failed by this point, so a failure to stamp this is logged, not
    // surfaced as a failed restore.
    if matches!(result, RestoreResult::Restored { .. }) {
        // Synchronous ledger I/O (lock, load, save) on the async runtime's
        // current thread — `block_in_place` hands its other queued tasks
        // off to the runtime's other workers for the duration, the same
        // reasoning `acquire.rs`'s own ledger writes document (issue #1664
        // review finding).
        tokio::task::block_in_place(|| mark_backup_restored(&opts.ledger_path, &opts.token));
    }

    result
}

/// The outcome of fetching `file_id`'s current metadata and evaluating the
/// write-permission gate against it — shared by the pre-lease-mint check
/// and the post-authentication re-check immediately before the write
/// (issue #1664 review finding: hand-copying this three-step sequence
/// risked exactly the kind of drift `check.rs`'s own module doc warns a
/// shared function exists to prevent — see its "half-dozen independent
/// copies" reasoning).
enum GateCheck {
    /// Allowed. The freshly fetched target, for the caller's own further
    /// use (its `mime_type`, in particular) — boxed since it otherwise
    /// dwarfs every other variant here (`clippy::large_enum_variant`).
    Ok(Box<crate::drive::types::DriveFile>),
    /// The metadata fetch or the gate evaluation itself failed — an
    /// operational error, not a verdict.
    Failed(String),
    /// The target has no parents this account can see and no `file_id`
    /// rule named it.
    NoVisibleParents,
    /// The folder write-permission gate refused it.
    Denied(Option<DecidingRule>),
}

/// Fetches `file_id`'s current metadata and evaluates the write-permission
/// gate against it (ADR-0080 Consequences: a *third*, independent check
/// alongside OAuth scope and the lease, never substituted by either). A
/// `file_id` rule is consulted before the parents, so a file shared by link
/// or email can still be granted (issue #1612), mirroring
/// `content_edit.rs` exactly.
async fn check_write_permission_gate(
    files_api: &FilesApi<'_>,
    file_id: &str,
    rules: &[FolderPermissionRule],
) -> GateCheck {
    let target = match files_api.get_metadata(file_id).await {
        Ok(target) => target,
        Err(err) => return GateCheck::Failed(err.to_string()),
    };
    let evaluated = match folder_ancestry::resolve_decision_for_file_target(
        files_api,
        &target,
        DriveOperation::Edit,
        rules,
    )
    .await
    {
        Ok(evaluated) => evaluated,
        Err(err) => return GateCheck::Failed(err.to_string()),
    };
    if evaluated.source == folder_ancestry::DecisionSource::NoVisibleParents {
        return GateCheck::NoVisibleParents;
    }
    if evaluated.decision.verdict == write_gate::Verdict::Deny {
        return GateCheck::Denied(evaluated.decision.decided_by);
    }
    GateCheck::Ok(Box::new(target))
}

/// Reads a `LeaseBackup::Bytes` file and verifies its SHA-256 still matches
/// the recorded hash before it is trusted as restore content — a backup
/// that has been corrupted or tampered with on disk since it was taken must
/// never be silently written back to Drive.
fn verify_and_read_backup(path: &Path, expected_sha256: &str) -> Result<Vec<u8>, String> {
    let bytes = std::fs::read(path)
        .map_err(|err| format!("Failed to read backup at {}: {err}", path.display()))?;
    let actual = crate::cli::drive::read::to_hex_string(&Sha256::digest(&bytes));
    // Case-insensitive, matching `verify_sha256_checksum`'s own comparison
    // (`src/cli/drive/read.rs`) — both hash producers in this codebase
    // always emit lowercase hex today, so this has no live trigger, but a
    // case-sensitive compare here would be a latent bug the moment that
    // stops being true (issue #1664 review finding).
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        return Err(format!(
            "backup at {} no longer matches its recorded SHA-256 (expected {expected_sha256}, \
             got {actual}) — refusing to restore from what may be corrupted or tampered-with \
             content",
            path.display()
        ));
    }
    Ok(bytes)
}

/// Best-effort: stamps `token`'s row with `restored_at` (ADR-0080 §4).
///
/// Goes through [`LeaseLedger::mutate_locked`] — this runs after
/// `restore_inner` has already released its own lock (acquired via
/// `gate_leased_write`/`finish_leased_write` for the *fresh* lease's row),
/// so without a fresh lock here this load-then-save could race a
/// concurrent, unrelated `drive lease acquire`/write on a *different*
/// file: `LeaseLedger::save` rewrites the whole file, so whichever of the
/// two calls saves last would silently discard the other's change (issue
/// #1664 review finding) — exactly the class of bug the lock exists to
/// prevent everywhere else in this module.
fn mark_backup_restored(ledger_path: &Path, token: &str) {
    let result = LeaseLedger::mutate_locked(ledger_path, |ledger| {
        ledger.mark_restored(token, Utc::now());
    });
    if let Err(err) = result {
        tracing::debug!(
            "drive lease restore: failed to mark the backup lease as restored-from: {err}"
        );
    }
}

/// Renders a [`LeaseGateRefusal`] hit against the just-minted fresh token —
/// not expected in practice, but every branch still needs a message rather
/// than an `unreachable!()`, since a lease could in principle expire or be
/// released by something else in the narrow window between minting it and
/// this check.
fn leased_write_refusal_detail(refusal: LeaseGateRefusal) -> String {
    match refusal {
        LeaseGateRefusal::NoLease => {
            "the freshly minted lease was not presented to its own write check".to_string()
        }
        LeaseGateRefusal::Expired => {
            "the freshly minted lease was already expired or released by the time of the \
             restore write"
                .to_string()
        }
        LeaseGateRefusal::WrongFile => {
            "the freshly minted lease was bound to a different file than the restore write \
             targeted"
                .to_string()
        }
        LeaseGateRefusal::Stale => {
            "the file changed again between minting the fresh lease and the restore write"
                .to_string()
        }
        LeaseGateRefusal::Failed(detail) => detail,
    }
}

/// Builds and writes the top-level audit record for one restore attempt,
/// naming both the backup token read from and (once minted) the fresh
/// token written under (ADR-0080 §10/§11).
fn record_attempt(opts: &RestoreOptions, result: &RestoreResult) {
    let (lease_id, error) = match result {
        RestoreResult::Restored { new_token, .. } => (Some(new_token.clone()), None),
        RestoreResult::AlreadyLeased { token, .. } => (Some(token.clone()), None),
        // A fresh lease was minted even though the write itself didn't
        // land — its token belongs in `lease_id` here for the same reason
        // `drive lease acquire`'s own audit record does: it is real and
        // live, not merely attempted.
        RestoreResult::FreshLeaseButWriteFailed { token, detail, .. } => {
            (Some(token.clone()), Some(detail.clone()))
        }
        RestoreResult::Denied { detail }
        | RestoreResult::Unavailable { detail }
        | RestoreResult::Failed { detail } => (None, Some(detail.clone())),
        RestoreResult::NoSuchBackupToken
        | RestoreResult::NoTypedRestorePath { .. }
        | RestoreResult::BackupTooLargeForSimpleUpload { .. }
        | RestoreResult::RefusedNoVisibleParents
        | RestoreResult::Blocked { .. }
        | RestoreResult::RefusedNativeDocument => (None, None),
    };
    // Re-reads the backup token's own row for its `file_id` — cheap, and
    // avoids threading it through every `restore_inner` return path just
    // for this one best-effort record. Absent (and thus an empty
    // `file_id`) only for `NoSuchBackupToken`, which by definition has no
    // file to name.
    let file_id = LeaseLedger::load(&opts.ledger_path)
        .ok()
        .and_then(|ledger| ledger.get(&opts.token).map(|record| record.file_id.clone()))
        .unwrap_or_default();
    let outcome = AuditOutcome {
        command: vec!["drive".to_string(), "lease-restore".to_string()],
        integration: "drive",
        file_id,
        lease_id,
        verdict: result.verdict().to_string(),
        restored_from_lease_id: Some(opts.token.clone()),
        error,
        ..Default::default()
    };
    if let Err(err) = crate::request_log::record_audit_event(outcome) {
        tracing::warn!("drive lease restore: failed to write audit record: {err}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::lease::authenticate::{AuthOutcome, Unsupported};
    use crate::drive::lease::ledger::LeaseRecord;
    use crate::drive::types::{GOOGLE_FOLDER_MIME_TYPE, GOOGLE_SHEET_MIME_TYPE};
    use crate::test_support::AuditLogGuard;
    use crate::utils::secret::Secret;
    use std::path::PathBuf;

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

    struct FakeAuthenticator(AuthOutcome);
    impl Authenticator for FakeAuthenticator {
        fn authenticate(&self, _reason: &str, _policy: AuthPolicy) -> AuthOutcome {
            self.0.clone()
        }
    }

    struct PanicsIfCalled;
    impl Authenticator for PanicsIfCalled {
        fn authenticate(&self, _reason: &str, _policy: AuthPolicy) -> AuthOutcome {
            // omni-dev: coverage ignore reason="every test using this double refuses before authenticating; a hit here is a regression, not a coverage gap"
            panic!("must not authenticate: refused before minting a fresh lease")
            // omni-dev: coverage end
        }
    }

    fn mount_file(server_id: &str, mime_type: &str, parents: &[&str]) -> wiremock::Mock {
        let parents: Vec<&str> = parents.to_vec();
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!(
                "/drive/v3/files/{server_id}"
            )))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": server_id, "name": server_id, "mimeType": mime_type,
                    "parents": parents, "version": "1",
                })),
            )
    }

    fn mount_folder(id: &str) -> wiremock::Mock {
        mount_file(id, GOOGLE_FOLDER_MIME_TYPE, &[])
    }

    fn mount_download(file_id: &str, bytes: &'static [u8]) -> wiremock::Mock {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!(
                "/drive/v3/files/{file_id}"
            )))
            .and(wiremock::matchers::query_param("alt", "media"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(bytes))
    }

    fn allow_rule(folder: &str) -> FolderPermissionRule {
        FolderPermissionRule::folder(folder).allowing([DriveOperation::Edit])
    }

    /// Writes `bytes` under `dir` and returns a `LeaseBackup::Bytes` pointing
    /// at it with the matching SHA-256 — the shape [`verify_and_read_backup`]
    /// expects to find.
    fn write_backup_file(dir: &Path, bytes: &[u8]) -> LeaseBackup {
        let path = dir.join("backup.bin");
        std::fs::write(&path, bytes).unwrap();
        let sha256 = crate::cli::drive::read::to_hex_string(&Sha256::digest(bytes));
        LeaseBackup::Bytes {
            path,
            sha256,
            size: bytes.len() as u64,
        }
    }

    /// Seeds `ledger_path` with a lease for `file_id` whose backup is
    /// `backup`, returning its token.
    fn seed_backup_lease(ledger_path: &Path, file_id: &str, backup: LeaseBackup) -> String {
        let token = "backup-token".to_string();
        let mut ledger = LeaseLedger::default();
        ledger.insert(LeaseRecord {
            token: token.clone(),
            file_id: file_id.to_string(),
            version: "1".to_string(),
            modified_time: None,
            backup,
            acquired_at: Utc::now() - ChronoDuration::hours(2),
            expires_at: Utc::now() - ChronoDuration::hours(1),
            released_at: None,
            restored_at: None,
        });
        ledger.save(ledger_path).unwrap();
        token
    }

    fn opts(dir: &Path, token: &str) -> RestoreOptions {
        RestoreOptions {
            token: token.to_string(),
            backup_dir: dir.join("backups"),
            native_backup_folder_id: None,
            expiry: ChronoDuration::minutes(30),
            auth_policy: AuthPolicy::DeviceOwner,
            ledger_path: dir.join("lease-ledger.jsonl"),
        }
    }

    #[tokio::test]
    async fn no_such_backup_token_is_refused_before_any_network_call() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditLogGuard::redirect(dir.path());
        // No mocks at all: reaching any Drive call fails the test.

        let result = restore(
            &client,
            &opts(dir.path(), "no-such-token"),
            &PanicsIfCalled,
            &[],
        )
        .await;

        assert!(matches!(result, RestoreResult::NoSuchBackupToken));
    }

    #[tokio::test]
    async fn a_native_backup_token_reports_no_typed_restore_path_before_any_network_call() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditLogGuard::redirect(dir.path());
        let test_opts = opts(dir.path(), "");
        let token = seed_backup_lease(
            &test_opts.ledger_path,
            "sheet-1",
            LeaseBackup::DriveCopy {
                file_id: "copy-1".to_string(),
            },
        );
        // No mocks at all: reaching any Drive call fails the test.

        let result = restore(
            &client,
            &opts(dir.path(), &token),
            &PanicsIfCalled,
            &[allow_rule("parent-1")],
        )
        .await;

        assert!(matches!(
            result,
            RestoreResult::NoTypedRestorePath { backup_location } if backup_location == "copy-1"
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_corrupted_backup_is_refused_before_minting_a_fresh_lease() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditLogGuard::redirect(dir.path());
        std::fs::create_dir_all(dir.path().join("backups")).unwrap();
        let mut backup = write_backup_file(dir.path(), b"original content");
        // Tamper with the file on disk after computing the (now stale)
        // recorded hash.
        if let LeaseBackup::Bytes { path, .. } = &backup {
            std::fs::write(path, b"tampered content").unwrap();
        }
        if let LeaseBackup::Bytes { size, .. } = &mut backup {
            *size = b"tampered content".len() as u64;
        }
        let test_opts = opts(dir.path(), "");
        let token = seed_backup_lease(&test_opts.ledger_path, "file-1", backup);
        mount_file("file-1", "text/plain", &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;

        let result = restore(
            &client,
            &opts(dir.path(), &token),
            &PanicsIfCalled,
            &[allow_rule("parent-1")],
        )
        .await;

        let RestoreResult::Failed { detail } = result else {
            panic!("expected Failed, got {result:?}");
        };
        assert!(detail.contains("SHA-256"), "{detail}");
    }

    #[tokio::test]
    async fn a_denied_target_is_blocked_with_zero_mutating_calls() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditLogGuard::redirect(dir.path());
        std::fs::create_dir_all(dir.path().join("backups")).unwrap();
        let backup = write_backup_file(dir.path(), b"original content");
        let test_opts = opts(dir.path(), "");
        let token = seed_backup_lease(&test_opts.ledger_path, "file-1", backup);
        mount_file("file-1", "text/plain", &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        // No rules granting Edit on parent-1, and no PATCH mock: reaching
        // the fresh-lease step or a mutating call fails the test.

        let result = restore(&client, &opts(dir.path(), &token), &PanicsIfCalled, &[]).await;

        assert!(matches!(result, RestoreResult::Blocked { .. }));
    }

    #[tokio::test]
    async fn an_orphan_target_is_refused_as_having_no_visible_parents() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditLogGuard::redirect(dir.path());
        std::fs::create_dir_all(dir.path().join("backups")).unwrap();
        let backup = write_backup_file(dir.path(), b"original content");
        let test_opts = opts(dir.path(), "");
        let token = seed_backup_lease(&test_opts.ledger_path, "file-1", backup);
        mount_file("file-1", "text/plain", &[]).mount(&server).await;

        let result = restore(&client, &opts(dir.path(), &token), &PanicsIfCalled, &[]).await;

        assert!(matches!(result, RestoreResult::RefusedNoVisibleParents));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_successful_restore_re_uploads_the_backup_and_mints_a_fresh_lease() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditLogGuard::redirect(dir.path());
        std::fs::create_dir_all(dir.path().join("backups")).unwrap();
        let backup = write_backup_file(dir.path(), b"the original content");
        let test_opts = opts(dir.path(), "");
        let old_token = seed_backup_lease(&test_opts.ledger_path, "file-1", backup);

        // The gate check's own `files.get`, plus the fresh `acquire`'s two
        // (pre-auth and post-backup) `files.get`s, plus `gate_leased_write`'s
        // own live-version fetch — all the same mock, unbounded.
        mount_file("file-1", "text/plain", &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        // The fresh acquire's own byte backup of the file's *current*
        // (pre-restore) content.
        mount_download("file-1", b"the current, about-to-be-overwritten content")
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/upload/drive/v3/files/file-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "file-1", "name": "file-1", "version": "2",
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let result = restore(
            &client,
            &opts(dir.path(), &old_token),
            &FakeAuthenticator(AuthOutcome::Authorized),
            &[allow_rule("parent-1")],
        )
        .await;

        let RestoreResult::Restored { new_token, .. } = result else {
            panic!("expected Restored, got {result:?}");
        };
        assert_ne!(new_token, old_token);

        let ledger = LeaseLedger::load(&test_opts.ledger_path).unwrap();
        let old_record = ledger.get(&old_token).expect("old row must be kept");
        assert!(
            old_record.restored_at.is_some(),
            "the backup lease's row must be marked restored-from"
        );
        let new_record = ledger
            .get(&new_token)
            .expect("fresh lease must be recorded");
        assert!(new_record.is_live(Utc::now()));
    }

    #[test]
    fn mark_backup_restored_takes_its_own_lock_and_is_best_effort_when_unavailable() {
        // Regression test for issue #1664's review finding: marking the
        // backup lease as restored-from must take its own `LedgerLock`
        // (closing a concurrent-save race with an unrelated
        // acquire/write elsewhere in this module — `LeaseLedger::save`
        // rewrites the whole file). Tested directly against the private
        // function rather than through the full `restore()` flow: every
        // step of that flow (the fresh lease's own acquire, its
        // `gate_leased_write` check) shares the *same* lock path, so
        // pre-holding it for the whole flow would block those legitimate
        // acquisitions too, not just this one.
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        seed_backup_lease(&ledger_path, "file-1", write_backup_file(dir.path(), b"x"));
        let mut lock_path = ledger_path.clone().into_os_string();
        lock_path.push(".lock");
        std::fs::File::create(std::path::PathBuf::from(lock_path)).unwrap();

        // Must not panic despite the lock already being held.
        mark_backup_restored(&ledger_path, "backup-token");

        let ledger = LeaseLedger::load(&ledger_path).unwrap();
        assert!(
            ledger.get("backup-token").unwrap().restored_at.is_none(),
            "the lock was held, so the mark must not have landed"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_permission_revoked_during_the_prompt_reports_the_fresh_lease_but_refuses_the_write()
    {
        // Regression test for issue #1664's review finding: the
        // write-permission gate is checked once before minting the fresh
        // lease and again immediately before the write, since the
        // interactive authentication prompt in between can take up to two
        // minutes to answer (ADR-0080 §7). The first `files.get`/gate pair
        // allows; the second denies — simulating a permission change
        // landing during the wait.
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditLogGuard::redirect(dir.path());
        std::fs::create_dir_all(dir.path().join("backups")).unwrap();
        let backup = write_backup_file(dir.path(), b"the original content");
        let test_opts = opts(dir.path(), "");
        let old_token = seed_backup_lease(&test_opts.ledger_path, "file-1", backup);

        // First pass (the initial gate check, before minting a fresh
        // lease): parent-1 allows.
        mount_file("file-1", "text/plain", &["parent-1"])
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        // The fresh acquire's own two `files.get`s (pre-auth and
        // post-backup) still see `parent-1` too — only the *third* fetch,
        // the re-check right before the write, sees the new, unlisted
        // parent.
        mount_file("file-1", "text/plain", &["parent-1"])
            .up_to_n_times(2)
            .with_priority(2)
            .mount(&server)
            .await;
        mount_download("file-1", b"the current, about-to-be-overwritten content")
            .mount(&server)
            .await;
        // The re-check's own fetch: now parented under a folder no rule
        // grants.
        mount_file("file-1", "text/plain", &["now-unlisted-parent"])
            .with_priority(3)
            .mount(&server)
            .await;
        mount_folder("now-unlisted-parent").mount(&server).await;
        // Reaching the write at all would mean the re-check failed to catch
        // the revoked permission — an unmocked PATCH would 404 and *also*
        // produce a `FreshLeaseButWriteFailed` with a different token than
        // the backup's, which would let this test pass for the wrong
        // reason. Pin it down explicitly: expect zero PATCH requests.
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/upload/drive/v3/files/file-1"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let result = restore(
            &client,
            &opts(dir.path(), &old_token),
            &FakeAuthenticator(AuthOutcome::Authorized),
            &[allow_rule("parent-1")],
        )
        .await;

        let RestoreResult::FreshLeaseButWriteFailed { token, detail, .. } = result else {
            panic!("expected FreshLeaseButWriteFailed, got {result:?}");
        };
        assert_ne!(
            token, old_token,
            "the surfaced token must be the fresh one, not the backup token"
        );
        assert!(
            detail.contains("write-permission gate no longer allows this write"),
            "expected the permission re-check's own refusal detail, got: {detail}"
        );
        // The fresh lease must still be live and findable, even though the
        // write it authorised never happened.
        let ledger = LeaseLedger::load(&test_opts.ledger_path).unwrap();
        assert!(ledger.get(&token).unwrap().is_live(Utc::now()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_restore_write_still_surfaces_the_fresh_lease_token() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditLogGuard::redirect(dir.path());
        std::fs::create_dir_all(dir.path().join("backups")).unwrap();
        let backup = write_backup_file(dir.path(), b"the original content");
        let test_opts = opts(dir.path(), "");
        let old_token = seed_backup_lease(&test_opts.ledger_path, "file-1", backup);
        mount_file("file-1", "text/plain", &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_download("file-1", b"the current, about-to-be-overwritten content")
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/upload/drive/v3/files/file-1"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let result = restore(
            &client,
            &opts(dir.path(), &old_token),
            &FakeAuthenticator(AuthOutcome::Authorized),
            &[allow_rule("parent-1")],
        )
        .await;

        let RestoreResult::FreshLeaseButWriteFailed { token, .. } = result else {
            panic!("expected FreshLeaseButWriteFailed, got {result:?}");
        };
        assert_ne!(token, old_token);
        let ledger = LeaseLedger::load(&test_opts.ledger_path).unwrap();
        assert!(
            ledger.get(&old_token).unwrap().restored_at.is_none(),
            "a failed write must not mark the backup lease as restored-from"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_already_leased_target_is_reported_via_the_fresh_acquire_step() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditLogGuard::redirect(dir.path());
        std::fs::create_dir_all(dir.path().join("backups")).unwrap();
        let backup = write_backup_file(dir.path(), b"original content");
        let test_opts = opts(dir.path(), "");
        let old_token = seed_backup_lease(&test_opts.ledger_path, "file-1", backup);
        // A second, still-live lease already covers this file — simulates a
        // concurrent `drive lease acquire`/write in progress.
        let mut ledger = LeaseLedger::load(&test_opts.ledger_path).unwrap();
        ledger.insert(LeaseRecord {
            token: "already-live".to_string(),
            file_id: "file-1".to_string(),
            version: "1".to_string(),
            modified_time: None,
            backup: LeaseBackup::Bytes {
                path: std::path::PathBuf::from("/tmp/other-backup"),
                sha256: "deadbeef".to_string(),
                size: 0,
            },
            acquired_at: Utc::now(),
            expires_at: Utc::now() + ChronoDuration::minutes(30),
            released_at: None,
            restored_at: None,
        });
        ledger.save(&test_opts.ledger_path).unwrap();
        mount_file("file-1", "text/plain", &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        // The fresh acquire authenticates and takes its own backup *before*
        // discovering the already-live lease at the ledger-insert step
        // (`acquire.rs`'s own sequence) — that backup ends up orphaned, but
        // still needs a mock, or the acquire fails earlier than the
        // `AlreadyLeased` check this test means to exercise. No PATCH mock:
        // reaching the restore write itself would mean the short-circuit
        // failed to prevent it.
        mount_download("file-1", b"orphaned backup content")
            .mount(&server)
            .await;

        let result = restore(
            &client,
            &opts(dir.path(), &old_token),
            &FakeAuthenticator(AuthOutcome::Authorized),
            &[allow_rule("parent-1")],
        )
        .await;

        assert!(matches!(
            result,
            RestoreResult::AlreadyLeased { token, .. } if token == "already-live"
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unavailable_authentication_is_reported_and_takes_no_backup() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditLogGuard::redirect(dir.path());
        std::fs::create_dir_all(dir.path().join("backups")).unwrap();
        let backup = write_backup_file(dir.path(), b"original content");
        let test_opts = opts(dir.path(), "");
        let token = seed_backup_lease(&test_opts.ledger_path, "file-1", backup);
        mount_file("file-1", "text/plain", &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;

        let result = restore(
            &client,
            &opts(dir.path(), &token),
            &Unsupported,
            &[allow_rule("parent-1")],
        )
        .await;

        assert!(matches!(result, RestoreResult::Unavailable { .. }));
        assert!(dir
            .path()
            .join("backups")
            .read_dir()
            .unwrap()
            .next()
            .is_none());
    }

    #[test]
    fn leased_write_refusal_detail_covers_every_variant() {
        assert_eq!(
            leased_write_refusal_detail(LeaseGateRefusal::NoLease),
            "the freshly minted lease was not presented to its own write check"
        );
        assert_eq!(
            leased_write_refusal_detail(LeaseGateRefusal::Expired),
            "the freshly minted lease was already expired or released by the time of the \
             restore write"
        );
        assert_eq!(
            leased_write_refusal_detail(LeaseGateRefusal::WrongFile),
            "the freshly minted lease was bound to a different file than the restore write \
             targeted"
        );
        assert_eq!(
            leased_write_refusal_detail(LeaseGateRefusal::Stale),
            "the file changed again between minting the fresh lease and the restore write"
        );
        assert_eq!(
            leased_write_refusal_detail(LeaseGateRefusal::Failed("boom".to_string())),
            "boom"
        );
    }

    #[tokio::test]
    async fn a_corrupt_ledger_is_reported_as_failed() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditLogGuard::redirect(dir.path());
        let test_opts = opts(dir.path(), "backup-token");
        std::fs::write(&test_opts.ledger_path, "not json\n").unwrap();
        // No mocks at all: an unreadable ledger must be refused before any
        // network call.

        let result = restore(&client, &test_opts, &PanicsIfCalled, &[]).await;

        assert!(matches!(result, RestoreResult::Failed { .. }));
    }

    #[tokio::test]
    async fn a_target_metadata_fetch_failure_is_reported_as_failed() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditLogGuard::redirect(dir.path());
        std::fs::create_dir_all(dir.path().join("backups")).unwrap();
        let backup = write_backup_file(dir.path(), b"original content");
        let test_opts = opts(dir.path(), "");
        let token = seed_backup_lease(&test_opts.ledger_path, "file-1", backup);
        // No mock for file-1 at all: the gate check's own `files.get` 404s.

        let result = restore(&client, &opts(dir.path(), &token), &PanicsIfCalled, &[]).await;

        assert!(matches!(result, RestoreResult::Failed { .. }));
    }

    #[tokio::test]
    async fn a_parent_lookup_failure_is_reported_as_failed() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditLogGuard::redirect(dir.path());
        std::fs::create_dir_all(dir.path().join("backups")).unwrap();
        let backup = write_backup_file(dir.path(), b"original content");
        let test_opts = opts(dir.path(), "");
        let token = seed_backup_lease(&test_opts.ledger_path, "file-1", backup);
        mount_file("file-1", "text/plain", &["parent-1"])
            .mount(&server)
            .await;
        // No mock for parent-1: resolving the folder chain 404s.

        let result = restore(&client, &opts(dir.path(), &token), &PanicsIfCalled, &[]).await;

        assert!(matches!(result, RestoreResult::Failed { .. }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_native_document_target_is_refused_via_the_fresh_acquire_step() {
        // The initial gate check's own `files.get` never inspects the
        // target's mime type — only the internal, fresh `acquire` step
        // does, refusing a Google-native file before ever authenticating
        // (`acquire.rs`). A `Bytes` backup for a file that is, by the time
        // of restore, a native document exercises that refusal bubbling
        // through `restore_inner` unchanged.
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditLogGuard::redirect(dir.path());
        std::fs::create_dir_all(dir.path().join("backups")).unwrap();
        let backup = write_backup_file(dir.path(), b"original content");
        let test_opts = opts(dir.path(), "");
        let token = seed_backup_lease(&test_opts.ledger_path, "file-1", backup);
        mount_file("file-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        // No download/PATCH mock: reaching either would mean the
        // native-document refusal failed to short-circuit first.

        let result = restore(
            &client,
            &opts(dir.path(), &token),
            &PanicsIfCalled,
            &[allow_rule("parent-1")],
        )
        .await;

        assert!(matches!(result, RestoreResult::RefusedNativeDocument));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_target_that_became_native_is_refused_at_the_recheck_when_a_backup_folder_is_configured(
    ) {
        // Regression test for issue #1664's review finding: when this
        // account has a `native_backup_folder_id` configured, the internal
        // fresh `acquire` step no longer refuses a native-document target
        // (it takes a native Drive-copy backup instead, see
        // `acquire::tests::native_document_with_a_backup_folder_configured_copies_instead_of_refusing`)
        // — so restore's own mime-type re-check, immediately before the
        // write, is the only thing left to stop stale binary bytes from
        // being PATCHed into what is now a Google-native document.
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditLogGuard::redirect(dir.path());
        std::fs::create_dir_all(dir.path().join("backups")).unwrap();
        let backup = write_backup_file(dir.path(), b"the original content");
        let test_opts = opts(dir.path(), "");
        let old_token = seed_backup_lease(&test_opts.ledger_path, "file-1", backup);

        mount_file("file-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/drive/v3/files/file-1/copy"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "copy-1", "name": "backup", "mimeType": GOOGLE_SHEET_MIME_TYPE
                })),
            )
            .expect(1)
            .mount(&server)
            .await;
        // No PATCH mock: reaching the write would mean the mime-type
        // re-check failed to catch the target having become native.
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/upload/drive/v3/files/file-1"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let mut restore_opts = opts(dir.path(), &old_token);
        restore_opts.native_backup_folder_id = Some("backup-folder".to_string());

        let result = restore(
            &client,
            &restore_opts,
            &FakeAuthenticator(AuthOutcome::Authorized),
            &[allow_rule("parent-1")],
        )
        .await;

        let RestoreResult::FreshLeaseButWriteFailed { token, detail, .. } = result else {
            panic!("expected FreshLeaseButWriteFailed, got {result:?}");
        };
        assert_ne!(token, old_token);
        assert!(
            detail.contains("Google-native document"),
            "expected the mime-type re-check's own refusal detail, got: {detail}"
        );
    }

    #[tokio::test]
    async fn a_backup_over_the_upload_cap_is_refused_before_any_network_call() {
        // Regression test for issue #1664's review finding: `acquire` can
        // back up a binary file well past Drive's 5 MB simple-upload cap,
        // but the restore write goes through that same capped endpoint —
        // checked from the backup's own recorded size, before any network
        // call, so a restore that could never succeed does not spend a
        // real Touch ID prompt and a fresh backup finding that out.
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditLogGuard::redirect(dir.path());
        let test_opts = opts(dir.path(), "");
        let oversized = crate::drive::files_api::MAX_UPLOAD_BYTES + 1;
        let token = seed_backup_lease(
            &test_opts.ledger_path,
            "file-1",
            LeaseBackup::Bytes {
                path: PathBuf::from("/does/not/need/to/exist"),
                sha256: "deadbeef".to_string(),
                size: oversized,
            },
        );
        // No mocks at all: reaching any Drive call fails the test.

        let result = restore(&client, &opts(dir.path(), &token), &PanicsIfCalled, &[]).await;

        assert!(matches!(
            result,
            RestoreResult::BackupTooLargeForSimpleUpload { size } if size == oversized
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_denied_fresh_authentication_is_reported() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditLogGuard::redirect(dir.path());
        std::fs::create_dir_all(dir.path().join("backups")).unwrap();
        let backup = write_backup_file(dir.path(), b"original content");
        let test_opts = opts(dir.path(), "");
        let token = seed_backup_lease(&test_opts.ledger_path, "file-1", backup);
        mount_file("file-1", "text/plain", &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        // No download/PATCH mock: a denied prompt must stop before either.

        let result = restore(
            &client,
            &opts(dir.path(), &token),
            &FakeAuthenticator(AuthOutcome::Denied("no".to_string())),
            &[allow_rule("parent-1")],
        )
        .await;

        assert!(matches!(result, RestoreResult::Denied { .. }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_invalid_fresh_expiry_surfaces_as_failed() {
        // `RestoreOptions.expiry` is not itself range-checked — the CLI's
        // own `--expiry-minutes` parser is the usual gate — but the
        // internal fresh `acquire` call enforces the same range
        // independently, and its refusal must bubble through unchanged.
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditLogGuard::redirect(dir.path());
        std::fs::create_dir_all(dir.path().join("backups")).unwrap();
        let backup = write_backup_file(dir.path(), b"original content");
        let test_opts = opts(dir.path(), "");
        let token = seed_backup_lease(&test_opts.ledger_path, "file-1", backup);
        mount_file("file-1", "text/plain", &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;

        let mut restore_opts = opts(dir.path(), &token);
        restore_opts.expiry = ChronoDuration::minutes(0);

        let result = restore(
            &client,
            &restore_opts,
            &PanicsIfCalled,
            &[allow_rule("parent-1")],
        )
        .await;

        assert!(matches!(result, RestoreResult::Failed { .. }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_metadata_fetch_failure_during_the_recheck_reports_fresh_lease_but_write_failed() {
        // The write-permission gate is re-checked against a *fresh*
        // `files.get` immediately before the restore write (issue #1664
        // review finding) — the fresh lease is already real and live by
        // then, so a failure fetching that metadata must surface as
        // `FreshLeaseButWriteFailed`, never a bare `Failed` that would
        // orphan the token nowhere the caller could find it again.
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditLogGuard::redirect(dir.path());
        std::fs::create_dir_all(dir.path().join("backups")).unwrap();
        let backup = write_backup_file(dir.path(), b"the original content");
        let test_opts = opts(dir.path(), "");
        let old_token = seed_backup_lease(&test_opts.ledger_path, "file-1", backup);

        // The gate check's own fetch, plus the fresh `acquire`'s two
        // (pre-auth and post-backup) fetches — three total before the
        // re-check.
        mount_file("file-1", "text/plain", &["parent-1"])
            .up_to_n_times(3)
            .with_priority(1)
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_download("file-1", b"the current, about-to-be-overwritten content")
            .mount(&server)
            .await;
        // The re-check's own fetch — the fourth `files.get` — fails outright.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/file-1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .with_priority(2)
            .mount(&server)
            .await;

        let result = restore(
            &client,
            &opts(dir.path(), &old_token),
            &FakeAuthenticator(AuthOutcome::Authorized),
            &[allow_rule("parent-1")],
        )
        .await;

        let RestoreResult::FreshLeaseButWriteFailed { token, .. } = result else {
            panic!("expected FreshLeaseButWriteFailed, got {result:?}");
        };
        assert_ne!(token, old_token);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_parent_lookup_failure_during_the_recheck_reports_fresh_lease_but_write_failed() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditLogGuard::redirect(dir.path());
        std::fs::create_dir_all(dir.path().join("backups")).unwrap();
        let backup = write_backup_file(dir.path(), b"the original content");
        let test_opts = opts(dir.path(), "");
        let old_token = seed_backup_lease(&test_opts.ledger_path, "file-1", backup);

        mount_file("file-1", "text/plain", &["parent-1"])
            .up_to_n_times(3)
            .with_priority(1)
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_download("file-1", b"the current, about-to-be-overwritten content")
            .mount(&server)
            .await;
        // The re-check's own fetch sees a brand-new, unmounted parent — its
        // own lookup 404s, rather than merely being unlisted by any rule.
        mount_file("file-1", "text/plain", &["parent-2"])
            .with_priority(2)
            .mount(&server)
            .await;

        let result = restore(
            &client,
            &opts(dir.path(), &old_token),
            &FakeAuthenticator(AuthOutcome::Authorized),
            &[allow_rule("parent-1")],
        )
        .await;

        let RestoreResult::FreshLeaseButWriteFailed { token, .. } = result else {
            panic!("expected FreshLeaseButWriteFailed, got {result:?}");
        };
        assert_ne!(token, old_token);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stale_file_at_write_time_reports_fresh_lease_but_write_failed() {
        // `gate_leased_write` re-fetches the live version/`modifiedTime`
        // itself, immediately before the write — a *fifth* `files.get`,
        // distinct from the write-permission re-check's own fourth one
        // above. A version change caught only here (not by the permission
        // re-check, which never looks at `version`) must refuse via the
        // same `FreshLeaseButWriteFailed` path.
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditLogGuard::redirect(dir.path());
        std::fs::create_dir_all(dir.path().join("backups")).unwrap();
        let backup = write_backup_file(dir.path(), b"the original content");
        let test_opts = opts(dir.path(), "");
        let old_token = seed_backup_lease(&test_opts.ledger_path, "file-1", backup);

        // Covers the gate check, the fresh acquire's two fetches, and the
        // permission re-check — four calls, all still at version "1".
        mount_file("file-1", "text/plain", &["parent-1"])
            .up_to_n_times(4)
            .with_priority(1)
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_download("file-1", b"the current, about-to-be-overwritten content")
            .mount(&server)
            .await;
        // `gate_leased_write`'s own fetch — the fifth call — sees a new
        // version, moved by something else after the fresh lease recorded
        // version "1".
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/file-1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "file-1", "name": "file-1", "mimeType": "text/plain",
                    "parents": ["parent-1"], "version": "2",
                })),
            )
            .with_priority(2)
            .mount(&server)
            .await;

        let result = restore(
            &client,
            &opts(dir.path(), &old_token),
            &FakeAuthenticator(AuthOutcome::Authorized),
            &[allow_rule("parent-1")],
        )
        .await;

        let RestoreResult::FreshLeaseButWriteFailed { token, detail, .. } = result else {
            panic!("expected FreshLeaseButWriteFailed, got {result:?}");
        };
        assert_ne!(token, old_token);
        assert!(
            detail.contains("file changed again"),
            "expected the staleness refusal's own detail, got: {detail}"
        );
    }

    #[tokio::test]
    async fn a_best_effort_audit_write_failure_is_warned_and_swallowed() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditLogGuard::redirect(dir.path());
        std::fs::create_dir(dir.path().join("audit.jsonl")).unwrap();

        // Cheapest way to reach `record_attempt`: an unknown token, so no
        // network call is needed either.
        let result = restore(
            &client,
            &opts(dir.path(), "no-such-token"),
            &PanicsIfCalled,
            &[],
        )
        .await;

        assert!(matches!(result, RestoreResult::NoSuchBackupToken));
    }

    #[test]
    fn restore_result_serializes_to_jsonl() {
        let mut buf = Vec::new();
        RestoreResult::Restored {
            new_token: "tok-2".to_string(),
            expires_at: Utc::now(),
            backup: LeaseBackup::Bytes {
                path: PathBuf::from("/tmp/backup"),
                sha256: "deadbeef".to_string(),
                size: 0,
            },
        }
        .write_jsonl(&mut buf)
        .unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("\"status\":\"restored\""), "{text}");
        assert!(text.contains("tok-2"), "{text}");
    }
}
