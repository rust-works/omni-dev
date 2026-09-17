//! `drive lease acquire` — the engine behind
//! [ADR-0080](../../../docs/adrs/adr-0080.md) §2: authenticate, then back
//! up, then record, then mint. Binary files back up as bytes to local disk;
//! a Google-native document (Sheet/Doc/Slide) backs up as a lossless
//! Drive-side `files.copy` into the account's configured backup folder
//! (§3's fidelity split) — refused outright, before authenticating at all,
//! when no backup folder is configured for this account, or when a live
//! lease already covers the target file (issue #1690). The latter refusal
//! is a fast path, not the authoritative gate — see [`acquire_inner`] — so
//! the narrow remaining race still spends a prompt and a backup; that
//! backup is then reclaimed (deleted/trashed) rather than left orphaned,
//! since `drive lease prune` can only ever see backups a ledger row
//! points at.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::cli::drive::format::JsonlSerialize;
use crate::cli::format::sanitize_for_terminal;
use crate::drive::client::DriveClient;
use crate::drive::files_api::FilesApi;
use crate::drive::lease::authenticate::{AuthOutcome, AuthPolicy, Authenticator};
#[cfg(test)]
use crate::drive::lease::ledger::LedgerLock;
use crate::drive::lease::ledger::{LeaseBackup, LeaseLedger, LeaseRecord};

/// Per-call options for `drive lease acquire`.
#[derive(Debug, Clone)]
pub struct AcquireOptions {
    /// The file id to lease.
    pub file_id: String,
    /// Local directory byte backups are written under. Unused for a
    /// native-document target.
    pub backup_dir: PathBuf,
    /// Destination folder for a native document's Drive-side backup copy
    /// (ADR-0080 §3/§13) — the account's configured
    /// `lease_backup_folder_id`. `None` means a native-document target is
    /// refused outright; unused for a binary target.
    pub native_backup_folder_id: Option<String>,
    /// How long the lease stays live from the moment it is authorised.
    pub expiry: ChronoDuration,
    /// Which authentication policy to present (ADR-0080 §7).
    pub auth_policy: AuthPolicy,
    /// Path to the lease ledger. Production callers pass
    /// [`crate::drive::lease::ledger::ledger_path`]'s own result; tests
    /// pass a path under a `tempdir` so a test run never touches the real
    /// ledger.
    pub ledger_path: PathBuf,
    /// The global headless/off-macOS opt-out (ADR-0080 §8/§13, issue
    /// #1677): when `true`, an [`AuthOutcome::Unavailable`] outcome — no
    /// authenticator exists in this context at all — is waived instead of
    /// refusing the lease, and the acquisition proceeds without a human
    /// ever having been prompted. Resolved by
    /// `crate::drive::lease::settings::resolve_allow_headless`.
    pub allow_headless: bool,
}

/// What happened.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum AcquireResult {
    /// Authenticated, backed up, and recorded. Present the token to a
    /// gated write via `--lease`.
    Acquired {
        /// The opaque lease token.
        token: String,
        /// When this lease stops authorising writes.
        expires_at: DateTime<Utc>,
        /// Where the backup landed.
        backup: LeaseBackup,
        /// `true` when this acquisition proceeded under the headless
        /// opt-out (ADR-0080 §8/§13) instead of a real device-owner
        /// prompt — no human presence was verified. Carried into the audit
        /// record ([`record_attempt`]) so a waived acquisition is durably
        /// distinguishable from a normally-authorised one.
        headless_waiver: bool,
    },
    /// A live lease already covers this file — its token is returned for
    /// reuse rather than minting a second, independent one. Two leases
    /// concurrently acquired on the same file could each pass their own
    /// staleness check against the same now-stale snapshot, letting the
    /// second writer silently clobber the first's write (issue #1664
    /// review finding) — see
    /// [`LeaseLedger::live_lease_for_file`](super::ledger::LeaseLedger::live_lease_for_file)'s
    /// doc comment. A lock-free pre-check in [`acquire_inner`] refuses most
    /// of these *before* authenticating at all (issue #1690); only the
    /// narrow remaining race — another process inserting its own lease
    /// between that pre-check and the authoritative, lock-held check —
    /// still spends a prompt and a backup first, and that backup is
    /// reclaimed (deleted/trashed) automatically rather than left orphaned.
    AlreadyLeased {
        /// The existing lease's token — present this to `--lease` instead.
        token: String,
        /// When the existing lease expires.
        expires_at: DateTime<Utc>,
    },
    /// The target is a Google-native document and no backup folder is
    /// configured for this account (`lease_backup_folder_id`) — nowhere to
    /// put the required Drive-side copy.
    RefusedNativeDocument,
    /// A human answered the prompt and refused, or it timed out.
    Denied {
        /// The platform's own message.
        detail: String,
    },
    /// No authenticator is available in this context (ADR-0080 §8) — no
    /// backup was taken and no ledger row was written.
    Unavailable {
        /// Why no authenticator is available.
        detail: String,
    },
    /// An API, filesystem, or ledger error.
    Failed {
        /// A human-readable summary of what failed.
        detail: String,
    },
}

impl JsonlSerialize for AcquireResult {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> Result<(), anyhow::Error> {
        crate::cli::drive::format::write_scalar_jsonl(self, out)
    }
}

/// Runs the four-step sequence ADR-0080 §2 specifies, in order, aborting at
/// the first failure with nothing further attempted, then records the
/// attempt to the audit sink regardless of outcome (ADR-0080 §11).
///
/// The audit record here is best-effort, not write-ahead/fail-closed the
/// way a leased *write*'s is: `drive lease acquire` mutates no Drive
/// content — it takes a backup and writes a ledger row, both already
/// durable by the time this function is about to return — so there is no
/// "mutating API call" for a write-ahead record to precede. A logging
/// failure here is warned, not surfaced as a failed acquisition: the
/// consent, the backup and the ledger row already happened, and turning a
/// genuine success into a reported failure because a follow-up log write
/// failed would make the tool's own output less trustworthy, not more.
pub async fn acquire(
    client: &DriveClient,
    opts: &AcquireOptions,
    authenticator: &dyn Authenticator,
) -> AcquireResult {
    let (result, disposition) = acquire_inner(client, opts, authenticator).await;
    record_attempt(opts, &result, disposition.as_ref());
    result
}

/// The accepted lease-expiry range (ADR-0080 §5): at least a minute, at
/// most 24 hours. Enforced here, in the engine, not only by the CLI's own
/// `--expiry-minutes` parser (`cli::drive::lease::parse_expiry_minutes`,
/// which shares these constants) — a non-CLI caller of this engine (a
/// future MCP tool, a test helper reused outside tests) could otherwise
/// mint a lease whose expiry silently violates the ADR's intent, or one
/// that overflows `chrono::Duration` (issue #1664 review finding).
pub(crate) const MIN_EXPIRY_MINUTES: i64 = 1;
/// See [`MIN_EXPIRY_MINUTES`].
pub(crate) const MAX_EXPIRY_MINUTES: i64 = 24 * 60;

/// What became of a backup this attempt took, once [`finish_acquisition`]
/// decided it is referenced by no ledger row — the belt-and-braces half of
/// issue #1690's fix, for the narrow race the lock-free pre-check in
/// [`acquire_inner`] cannot close. `None` everywhere in
/// [`acquire_inner`]'s return value means this attempt never took a
/// backup at all (refused/denied/unavailable before ever reaching step 2).
enum BackupDisposition {
    /// Deleted (bytes) or trashed (Drive copy) before returning.
    Reclaimed(LeaseBackup),
    /// Reclaiming it itself failed; the backup is still on disk/Drive,
    /// now truly orphaned — `drive lease prune` cannot see it either,
    /// since no ledger row points at it. Carried so the audit record can
    /// still name its location for a human to clean up.
    ReclaimFailed(LeaseBackup),
}

async fn acquire_inner(
    client: &DriveClient,
    opts: &AcquireOptions,
    authenticator: &dyn Authenticator,
) -> (AcquireResult, Option<BackupDisposition>) {
    let expiry_minutes = opts.expiry.num_minutes();
    if !(MIN_EXPIRY_MINUTES..=MAX_EXPIRY_MINUTES).contains(&expiry_minutes) {
        return (
            AcquireResult::Failed {
                detail: format!(
                    "expiry must be between {MIN_EXPIRY_MINUTES} and {MAX_EXPIRY_MINUTES} \
                     minutes, got {expiry_minutes}"
                ),
            },
            None,
        );
    }

    let files_api = FilesApi::new(client);
    let target = match files_api.get_metadata(&opts.file_id).await {
        Ok(target) => target,
        Err(err) => {
            return (
                AcquireResult::Failed {
                    detail: err.to_string(),
                },
                None,
            )
        }
    };

    let is_native = target.is_google_native();
    if is_native && opts.native_backup_folder_id.is_none() {
        return (AcquireResult::RefusedNativeDocument, None);
    }

    if target.version.is_none() {
        return (
            AcquireResult::Failed {
                detail: "Drive did not return a `version` for this file; refusing to lease it \
                         without a staleness check"
                    .to_string(),
            },
            None,
        );
    }

    // Lock-free live-lease pre-check (issue #1690): cheap, and
    // deliberately *not* the authoritative gate — `insert_record`, taken
    // under the ledger lock inside `finish_acquisition` below, remains
    // that (see its own doc comment). This one's only job is refusing
    // *before* spending the (up to 120s) Touch ID prompt and a real
    // backup on the common case this closes: a second `acquire` on a
    // file that already has a live lease — a "did I already lease this?"
    // retry, a lost token — not merely a rare race. A lease that goes
    // live or expires in the window between this check and the
    // authoritative one is still decided correctly there either way: this
    // fast path can only ever be *more* conservative than the real gate
    // (refusing a lease that in fact expires a moment later, which just
    // means the caller retries), never less, so the two can never
    // disagree in the direction that would matter.
    let pre_check = tokio::task::block_in_place(|| LeaseLedger::load(&opts.ledger_path));
    match pre_check {
        Ok(ledger) => {
            if let Some(existing) = ledger.live_lease_for_file(&opts.file_id, Utc::now()) {
                return (
                    AcquireResult::AlreadyLeased {
                        token: existing.token.clone(),
                        expires_at: existing.expires_at,
                    },
                    None,
                );
            }
        }
        Err(err) => {
            return (
                AcquireResult::Failed {
                    detail: err.to_string(),
                },
                None,
            )
        }
    }

    // 1. Authenticate — consent gates the action, not merely possession of
    // the resulting token (ADR-0080 §2). Nothing below runs on refusal.
    // `target.name` is Drive-controlled (renamable by anyone with edit
    // access to the file) and is shown verbatim inside the OS consent
    // prompt, so it is sanitized the same way any other server-supplied
    // string reaching a terminal or prompt is elsewhere in this CLI —
    // stripping control characters and bidi-override code points closes
    // off a spoofed/deceptive file name misleading what the operator is
    // authorising.
    let reason = format!(
        "back up and lease-write '{}'",
        sanitize_for_terminal(&target.name)
    );
    // `authenticate` blocks synchronously on the human's answer, up to
    // `PROMPT_TIMEOUT` (120s) — `block_in_place` hands this worker
    // thread's other queued tasks off to the runtime's other workers for
    // the duration, so a single Touch ID prompt cannot stall unrelated
    // concurrent work on a shared multi-thread runtime (the daemon and
    // the MCP server both run one). `acquire` itself stays a plain `async
    // fn` — `block_in_place` runs the closure on the *current* thread, so
    // it needs no `'static`/`Send` bound on `authenticator`, unlike
    // `spawn_blocking`.
    let auth_outcome =
        tokio::task::block_in_place(|| authenticator.authenticate(&reason, opts.auth_policy));
    let headless_waiver = match auth_outcome {
        AuthOutcome::Authorized => false,
        AuthOutcome::Denied(detail) => return (AcquireResult::Denied { detail }, None),
        AuthOutcome::Unavailable(detail) => {
            // ADR-0080 §8/§13: an explicit, per-installation opt-out lets
            // this proceed with no human ever having been prompted, rather
            // than refusing outright. `headless_waiver` on the eventual
            // `Acquired` result (and so the audit record) is what makes
            // this waiver durably visible.
            if !opts.allow_headless {
                return (AcquireResult::Unavailable { detail }, None);
            }
            true
        }
    };

    // 2. Backup — bytes for a binary file, a Drive-side copy for a native
    // document (ADR-0080 §3). `native_backup_folder_id` is guaranteed
    // `Some` here whenever `is_native`, by the refusal above.
    let backup = if is_native {
        let folder_id = opts.native_backup_folder_id.as_deref().unwrap_or_default();
        match native_backup(&files_api, &opts.file_id, folder_id, &target.name).await {
            Ok(backup) => backup,
            Err(err) => {
                return (
                    AcquireResult::Failed {
                        detail: err.to_string(),
                    },
                    None,
                )
            }
        }
    } else {
        match byte_backup(&files_api, &opts.file_id, &opts.backup_dir, &target.name).await {
            Ok(backup) => backup,
            Err(err) => {
                return (
                    AcquireResult::Failed {
                        detail: err.to_string(),
                    },
                    None,
                )
            }
        }
    };

    // From here on a backup exists, so every remaining exit must account
    // for it: `Acquired` because a ledger row now references it, every
    // other outcome by reclaiming it (issue #1690's belt-and-braces half,
    // for the pre-check's narrow remaining race).
    let result = finish_acquisition(&files_api, opts, backup.clone(), headless_waiver).await;
    if matches!(result, AcquireResult::Acquired { .. }) {
        return (result, None);
    }
    let disposition = reclaim_backup(&files_api, backup).await;
    (result, Some(disposition))
}

/// Steps 3–4: re-fetches metadata, checks the file's live lease under the
/// ledger lock, and mints the lease. Split out of `acquire_inner` so every
/// exit past the backup step shares that function's one reclaim wrapper —
/// this function only ever decides mint-or-refuse, never reclaims.
async fn finish_acquisition(
    files_api: &FilesApi<'_>,
    opts: &AcquireOptions,
    backup: LeaseBackup,
    headless_waiver: bool,
) -> AcquireResult {
    // 3. Ledger record. `version`/`modified_time` are re-fetched here
    // rather than reused from the `target` metadata read at the very top —
    // that read happened before the (up to 120s) Touch ID prompt and
    // before the backup itself, so the file could have moved in the
    // meantime. Recording that earlier, now-possibly-stale version would
    // break the token's own invariant of being "bound to a specific backup
    // and a specific Drive version" (ADR-0080 §2/§4): the backup reflects
    // whatever the file was at backup time, so the recorded version must
    // too, or the very next write under this lease could spuriously refuse
    // as stale (or, worse, pass a staleness check against a version that
    // doesn't match what was actually backed up).
    let post_backup = match files_api.get_metadata(&opts.file_id).await {
        Ok(post_backup) => post_backup,
        Err(err) => {
            return AcquireResult::Failed {
                detail: err.to_string(),
            }
        }
    };
    let Some(version) = post_backup.version else {
        return AcquireResult::Failed {
            detail: "Drive did not return a `version` for this file after the backup; \
                     refusing to record a lease without a staleness check"
                .to_string(),
        };
    };
    let token = crate::request_log::new_id();
    let now = Utc::now();
    let expires_at = now + opts.expiry;
    let record = LeaseRecord {
        token: token.clone(),
        file_id: opts.file_id.clone(),
        version,
        modified_time: post_backup.modified_time,
        backup: backup.clone(),
        acquired_at: now,
        expires_at,
        released_at: None,
        restored_at: None,
        restored_sheet_id: None,
    };
    // Synchronous ledger I/O (lock, load, save) on the async runtime's
    // current thread — `block_in_place` hands its other queued tasks off
    // to the runtime's other workers for the duration, the same reasoning
    // the `authenticate` call above documents.
    match tokio::task::block_in_place(|| insert_record(record, &opts.ledger_path)) {
        Ok(InsertOutcome::Inserted) => {}
        // Refuse rather than mint a second, independent lease — see
        // `AcquireResult::AlreadyLeased`'s and `insert_record`'s own doc
        // comments for why this stays the authoritative gate even with
        // the lock-free pre-check in `acquire_inner`. Reclaiming the
        // backup this attempt just took happens there, once it sees this
        // isn't `Acquired` — not here, since this function's only job is
        // deciding mint-or-refuse.
        Ok(InsertOutcome::AlreadyLeased(existing)) => {
            return AcquireResult::AlreadyLeased {
                token: existing.token,
                expires_at: existing.expires_at,
            };
        }
        Err(err) => {
            return AcquireResult::Failed {
                detail: err.to_string(),
            };
        }
    }

    // 4. Print the token (the caller's job) and exit 0.
    AcquireResult::Acquired {
        token,
        expires_at,
        backup,
        headless_waiver,
    }
}

/// Deletes/trashes a backup this attempt itself just took, once
/// `acquire_inner` has decided it ends up referenced by no ledger row —
/// the belt-and-braces half of issue #1690's fix, for the narrow race the
/// lock-free pre-check above cannot close by itself. Reuses
/// [`super::prune::clear_backup`], which already handles both
/// [`LeaseBackup`] variants and tolerates an already-absent backup. Safe
/// specifically because `backup` is always one this very call just
/// created moments ago under a fresh identity — `write_backup`'s
/// `create_new`/`O_EXCL` open, or `native_backup`'s brand-new `files.copy`
/// id — never one an existing ledger row (live, expired, or already
/// pruned) could still point at.
///
/// A reclamation failure is warned, not surfaced: the primary outcome
/// (`AlreadyLeased`/`Failed`) is unaffected either way, and the orphan's
/// location still ends up in the best-effort audit record
/// ([`record_attempt`]) for `drive lease prune` — or a human — to find.
async fn reclaim_backup(files_api: &FilesApi<'_>, backup: LeaseBackup) -> BackupDisposition {
    match super::prune::clear_backup(files_api, &backup).await {
        Ok(()) => BackupDisposition::Reclaimed(backup),
        Err(err) => {
            let (location, ..) = backup.audit_fields();
            tracing::warn!(
                "drive lease acquire: failed to reclaim an orphaned backup at {}: {err} — \
                 `drive lease prune` cannot see it either, since no ledger row references it; \
                 remove it manually",
                location.unwrap_or_default()
            );
            BackupDisposition::ReclaimFailed(backup)
        }
    }
}

/// What [`insert_record`] did.
enum InsertOutcome {
    /// The record was inserted; the lease is minted.
    Inserted,
    /// A live lease already covered this record's `file_id` — nothing was
    /// inserted or overwritten. Carries that existing record so the caller
    /// can return its token instead — boxed, since it otherwise dwarfs the
    /// dataless `Inserted` variant (`clippy::large_enum_variant`), the same
    /// reasoning `restore.rs`'s `GateCheck::Ok` documents.
    AlreadyLeased(Box<LeaseRecord>),
}

/// Downloads a binary file's bytes and writes them to `backup_dir`
/// (ADR-0080 §3's binary-file case).
async fn byte_backup(
    files_api: &FilesApi<'_>,
    file_id: &str,
    backup_dir: &Path,
    name: &str,
) -> anyhow::Result<LeaseBackup> {
    let bytes = files_api.download(file_id).await?;
    let sha256 = crate::cli::drive::read::to_hex_string(&Sha256::digest(&bytes));
    let path = backup_file_path(backup_dir, file_id, name);
    // `write_backup` is synchronous filesystem I/O for a backup of arbitrary
    // size — `block_in_place` hands this worker thread's other queued tasks
    // off to the runtime's other workers for the duration, the same
    // reasoning the `authenticate` call above documents.
    tokio::task::block_in_place(|| write_backup(&path, &bytes))?;
    Ok(LeaseBackup::Bytes {
        path,
        sha256,
        size: bytes.len() as u64,
    })
}

/// Copies a native document into `backup_folder_id` via `files.copy`
/// (ADR-0080 §3's native-document case) — lossless, and restorable by a
/// human in the Drive UI even without this tool.
async fn native_backup(
    files_api: &FilesApi<'_>,
    file_id: &str,
    backup_folder_id: &str,
    name: &str,
) -> anyhow::Result<LeaseBackup> {
    let copy_name = backup_name(file_id, name);
    let copy = files_api
        .copy(file_id, backup_folder_id, &copy_name)
        .await?;
    Ok(LeaseBackup::DriveCopy { file_id: copy.id })
}

/// `<dir>` joined with [`backup_name`]'s result.
fn backup_file_path(dir: &Path, file_id: &str, name: &str) -> PathBuf {
    dir.join(backup_name(file_id, name))
}

/// `<YYYYMMDDTHHMMSSZ>-<fileId>-<name>` (ADR-0080 §3) — UTC, seconds
/// precision, the file id first to survive a name containing `/`. Shared by
/// the local byte-backup path ([`backup_file_path`] joins it under a
/// directory) and the native Drive-copy path (used directly as the copy's
/// own `name`).
fn backup_name(file_id: &str, name: &str) -> String {
    let timestamp = Utc::now().format("%Y%m%dT%H%M%SZ");
    let safe_name = name.replace('/', "_");
    format!("{timestamp}-{file_id}-{safe_name}")
}

/// Writes `bytes` to `path`, creating a missing `0700` parent directory
/// first — the same posture `crate::request_log`/the lease ledger use.
///
/// Opened `0600`-from-birth (never `std::fs::write`'s umask-derived mode)
/// — a backup is exactly the private, sensitive content this feature
/// exists to protect. Also `create_new` (`O_EXCL`) rather than a
/// truncating write: [`backup_file_path`]'s name has only whole-second
/// precision, so two acquisitions for the same file within one second
/// collide on the same path. Failing loudly here, instead of silently
/// overwriting, is what stops that rare collision from corrupting the
/// *earlier* lease's ledger row — its recorded `backup_sha256` would
/// otherwise no longer match the bytes actually on disk.
fn write_backup(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write as _;

    crate::daemon::paths::ensure_parent_dir_0700(path)?;
    let mut file = crate::daemon::paths::create_new_file_0600(path).map_err(|err| {
        // Distinguish a genuine same-second collision (the path already
        // existed) from the file being created fine but the follow-up
        // `fchmod` safety net failing — the latter is an unrelated
        // permissions/filesystem problem that "may already exist, retry"
        // would misdiagnose (issue #1664 review finding).
        if crate::daemon::paths::is_already_exists_error(&err) {
            err.context(format!(
                "Failed to create backup file at {} — it may already exist from a \
                 near-simultaneous `drive lease acquire` on the same file within the same \
                 second; retry",
                path.display()
            ))
        } else {
            err.context(format!(
                "Failed to create backup file at {} — not a same-second collision; check \
                 filesystem permissions",
                path.display()
            ))
        }
    })?;
    file.write_all(bytes)
        .with_context(|| format!("Failed to write backup to {}", path.display()))?;
    Ok(())
}

/// Inserts `record` into the ledger at `ledger_path` under
/// [`LeaseLedger::mutate_locked`]'s lock, loading and saving it in full —
/// the atomic-rewrite contract [`LeaseLedger`] documents. This lock-held
/// check-then-insert is the
/// authoritative gate against two leases ever being live on the same file
/// at once (see [`LeaseLedger::live_lease_for_file`]'s doc comment) — unlike
/// a check made before taking the lock, nothing can race it.
fn insert_record(record: LeaseRecord, ledger_path: &Path) -> anyhow::Result<InsertOutcome> {
    LeaseLedger::mutate_locked(ledger_path, |ledger| {
        if let Some(existing) = ledger.live_lease_for_file(&record.file_id, Utc::now()) {
            return InsertOutcome::AlreadyLeased(Box::new(existing.clone()));
        }
        ledger.insert(record);
        InsertOutcome::Inserted
    })
}

/// Whether `disposition` names a backup this attempt took and, if
/// reclaiming it failed, extends `base_verdict` to `<base>-backup-orphaned`
/// so an operator can grep for a leaked backup `drive lease prune` cannot
/// see (issue #1690) — see [`BackupDisposition`]. Returns `base_verdict`
/// unchanged, with no `backup_location`, when this attempt never took a
/// backup at all.
fn orphan_verdict(
    base_verdict: &str,
    disposition: Option<&BackupDisposition>,
) -> (String, Option<String>) {
    let backup = match disposition {
        None => return (base_verdict.to_string(), None),
        Some(BackupDisposition::Reclaimed(backup)) => backup,
        Some(BackupDisposition::ReclaimFailed(backup)) => {
            let (location, ..) = backup.audit_fields();
            return (format!("{base_verdict}-backup-orphaned"), location);
        }
    };
    let (location, ..) = backup.audit_fields();
    (base_verdict.to_string(), location)
}

/// Builds and writes the `kind: "audit"` record for one acquire attempt.
/// See [`acquire`]'s own doc comment for why this is best-effort rather than
/// write-ahead/fail-closed. `disposition` is `Some` only when this attempt
/// took a backup that turned out to be reclaimed or orphaned rather than
/// referenced by a ledger row (issue #1690) — see [`BackupDisposition`].
fn record_attempt(
    opts: &AcquireOptions,
    result: &AcquireResult,
    disposition: Option<&BackupDisposition>,
) {
    let auth_policy = Some(
        match opts.auth_policy {
            AuthPolicy::DeviceOwner => "device-owner",
            AuthPolicy::BiometricsOnly => "biometrics-only",
        }
        .to_string(),
    );
    let outcome = match result {
        AcquireResult::Acquired {
            token,
            backup,
            expires_at: _,
            headless_waiver,
        } => {
            let (backup_location, backup_sha256, backup_size) = backup.audit_fields();
            // Re-read the just-written ledger row for the version/
            // modified_time actually recorded, rather than widening
            // `AcquireResult::Acquired` (a public, `--output json` wire
            // shape) with fields that exist only for this best-effort audit
            // record. A read failure here just omits them — the acquisition
            // itself already fully succeeded.
            let (version_after, modified_time_after) = LeaseLedger::load(&opts.ledger_path)
                .ok()
                .and_then(|ledger| ledger.get(token).cloned())
                .map_or((None, None), |record| {
                    (Some(record.version), record.modified_time)
                });
            crate::request_log::AuditOutcome {
                command: vec!["drive".to_string(), "lease-acquire".to_string()],
                integration: "drive",
                file_id: opts.file_id.clone(),
                lease_id: Some(token.clone()),
                // ADR-0080 §8/§13, issue #1677: a distinct verdict, rather
                // than a separate field on this shared, free-form-vocabulary
                // struct (ADR-0080 §11), durably distinguishes an
                // acquisition that waived the human-presence guarantee from
                // a normally-authorised one.
                verdict: if *headless_waiver {
                    "acquired-headless-waiver".to_string()
                } else {
                    "acquired".to_string()
                },
                version_after,
                modified_time_after,
                backup_location,
                backup_sha256,
                backup_size,
                auth_policy,
                ..Default::default()
            }
        }
        AcquireResult::AlreadyLeased { token, .. } => {
            let (verdict, backup_location) = orphan_verdict("already-leased", disposition);
            crate::request_log::AuditOutcome {
                command: vec!["drive".to_string(), "lease-acquire".to_string()],
                integration: "drive",
                file_id: opts.file_id.clone(),
                lease_id: Some(token.clone()),
                verdict,
                backup_location,
                auth_policy,
                ..Default::default()
            }
        }
        AcquireResult::RefusedNativeDocument => crate::request_log::AuditOutcome {
            command: vec!["drive".to_string(), "lease-acquire".to_string()],
            integration: "drive",
            file_id: opts.file_id.clone(),
            verdict: "refused-native-document".to_string(),
            auth_policy,
            ..Default::default()
        },
        AcquireResult::Denied { detail } => crate::request_log::AuditOutcome {
            command: vec!["drive".to_string(), "lease-acquire".to_string()],
            integration: "drive",
            file_id: opts.file_id.clone(),
            verdict: "denied".to_string(),
            error: Some(detail.clone()),
            auth_policy,
            ..Default::default()
        },
        AcquireResult::Unavailable { detail } => crate::request_log::AuditOutcome {
            command: vec!["drive".to_string(), "lease-acquire".to_string()],
            integration: "drive",
            file_id: opts.file_id.clone(),
            verdict: "unavailable".to_string(),
            error: Some(detail.clone()),
            auth_policy,
            ..Default::default()
        },
        AcquireResult::Failed { detail } => {
            let (verdict, backup_location) = orphan_verdict("failed", disposition);
            crate::request_log::AuditOutcome {
                command: vec!["drive".to_string(), "lease-acquire".to_string()],
                integration: "drive",
                file_id: opts.file_id.clone(),
                verdict,
                error: Some(detail.clone()),
                backup_location,
                auth_policy,
                ..Default::default()
            }
        }
    };
    if let Err(err) = crate::request_log::record_audit_event(outcome) {
        tracing::warn!("drive lease acquire: failed to write audit record: {err}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::lease::authenticate::Unsupported;
    use crate::test_support::AuditLogGuard as AuditGuard;
    use crate::utils::secret::Secret;

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

    /// Builds options rooted at `dir` (a `tempdir`), so a test never
    /// touches the real backup directory or the real lease ledger.
    fn opts(dir: &Path) -> AcquireOptions {
        AcquireOptions {
            file_id: "f1".to_string(),
            backup_dir: dir.join("backups"),
            native_backup_folder_id: None,
            expiry: ChronoDuration::minutes(30),
            auth_policy: AuthPolicy::DeviceOwner,
            ledger_path: dir.join("lease-ledger.jsonl"),
            allow_headless: false,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn denied_authentication_takes_no_backup_and_writes_no_ledger_row() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "n", "mimeType": "application/pdf", "version": "1"
                })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());

        let result = acquire(
            &client,
            &opts(root.path()),
            &FakeAuthenticator(AuthOutcome::Denied("no".to_string())),
        )
        .await;

        assert!(matches!(result, AcquireResult::Denied { .. }));
        assert!(!root.path().join("backups").exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unavailable_authentication_takes_no_backup() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "n", "mimeType": "application/pdf", "version": "1"
                })),
            )
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());

        let result = acquire(&client, &opts(root.path()), &Unsupported).await;

        assert!(matches!(result, AcquireResult::Unavailable { .. }));
        assert!(!root.path().join("backups").exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn headless_opt_out_proceeds_without_an_authenticator() {
        // ADR-0080 §8/§13, issue #1677: the same `Unsupported` authenticator
        // as `unavailable_authentication_takes_no_backup` above, but with
        // `allow_headless: true` — this must now proceed to a real backup
        // and ledger row instead of refusing, with `headless_waiver` set so
        // the waiver is durably visible.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "n", "mimeType": "application/pdf", "version": "1"
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param("alt", "media"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"bytes".to_vec()))
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());

        let result = acquire(
            &client,
            &AcquireOptions {
                allow_headless: true,
                ..opts(root.path())
            },
            &Unsupported,
        )
        .await;

        let AcquireResult::Acquired {
            headless_waiver, ..
        } = result
        else {
            panic!("expected Acquired, got {result:?}");
        };
        assert!(headless_waiver);
        assert!(root.path().join("backups").exists());
    }

    #[tokio::test]
    async fn get_metadata_failure_is_reported_as_failed_before_authenticating() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(
                wiremock::ResponseTemplate::new(404).set_body_json(serde_json::json!({
                    "error": {"code": 404, "message": "File not found"}
                })),
            )
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());

        struct PanicsIfCalled;
        impl Authenticator for PanicsIfCalled {
            fn authenticate(&self, _reason: &str, _policy: AuthPolicy) -> AuthOutcome {
                panic!("must not authenticate when metadata lookup already failed");
            }
        }

        let result = acquire(&client, &opts(root.path()), &PanicsIfCalled).await;
        assert!(matches!(result, AcquireResult::Failed { .. }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn download_failure_after_authorization_is_reported_as_failed() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "n", "mimeType": "application/pdf", "version": "1"
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param("alt", "media"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());

        let result = acquire(
            &client,
            &opts(root.path()),
            &FakeAuthenticator(AuthOutcome::Authorized),
        )
        .await;

        assert!(matches!(result, AcquireResult::Failed { .. }));
        assert!(!root.path().join("backups").exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_backup_write_collision_is_reported_as_failed() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "report.pdf", "mimeType": "application/pdf", "version": "1"
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param("alt", "media"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()))
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());
        let test_opts = opts(root.path());
        // Force `write_backup`'s `create_new` `open()` to fail deterministically
        // by putting a plain file where the backup directory should be, so the
        // `with_context` closure (otherwise dead in every other test here)
        // actually runs. Pre-creating the *exact* colliding path instead (the
        // same-second collision the doc comment describes) is racy under CI
        // load: the backup filename has only whole-second precision, and
        // enough time can pass between computing it here and `acquire()`
        // computing its own for the two to land in different seconds.
        std::fs::write(&test_opts.backup_dir, b"not a directory").unwrap();

        let result = acquire(
            &client,
            &test_opts,
            &FakeAuthenticator(AuthOutcome::Authorized),
        )
        .await;

        let AcquireResult::Failed { detail } = result else {
            panic!("expected Failed, got {result:?}");
        };
        assert!(detail.contains("Failed to create backup file"), "{detail}");
    }

    #[test]
    fn write_backup_reports_a_genuine_same_second_collision_distinctly() {
        // Exercises `write_backup` directly against a literal path, rather
        // than racing `acquire()`'s whole-second-precision timestamp — see
        // `a_backup_write_collision_is_reported_as_failed`'s doc comment for
        // why that race is avoided everywhere else.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("backup-file");
        std::fs::write(&path, b"already here").unwrap();

        let err = write_backup(&path, b"hello").unwrap_err();

        assert!(err.to_string().contains("may already exist"), "{err:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_ledger_insert_failure_reclaims_the_backup_just_taken() {
        // A concurrent lease op (or a leftover lock from a crash) holds the
        // ledger lock across this attempt's own `insert_record` — the
        // routine, non-race way `Failed` still follows a full backup
        // (issue #1690): the lock-free pre-check can't see this, since it
        // takes no lock itself. A held `LedgerLock`, not a bare
        // `File::create` on the lock path (issue #1687): under `flock`,
        // the file's mere existence holds nothing — only an actual lock
        // does, and `flock` conflicts against a second `open()` even from
        // this same process.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "report.pdf", "mimeType": "application/pdf", "version": "1"
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param("alt", "media"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()))
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());
        let test_opts = opts(root.path());
        let _held = LedgerLock::acquire(&test_opts.ledger_path).unwrap();

        let result = acquire(
            &client,
            &test_opts,
            &FakeAuthenticator(AuthOutcome::Authorized),
        )
        .await;

        assert!(matches!(result, AcquireResult::Failed { .. }));
        assert!(
            std::fs::read_dir(&test_opts.backup_dir)
                .unwrap()
                .next()
                .is_none(),
            "the backup this attempt took must be reclaimed, not left orphaned"
        );
        let contents = std::fs::read_to_string(root.path().join("audit.jsonl")).unwrap();
        let rec: crate::request_log::LogRecord = serde_json::from_str(contents.trim_end()).unwrap();
        assert_eq!(
            rec.context.get("verdict").map(String::as_str),
            Some("failed"),
            "a successfully reclaimed backup takes no verdict suffix"
        );
        assert!(
            rec.context.contains_key("backup_location"),
            "a reclaimed backup's location must still be audited"
        );
    }

    #[test]
    fn acquire_result_serializes_to_jsonl() {
        use crate::cli::drive::format::JsonlSerialize;

        let mut buf = Vec::new();
        AcquireResult::Acquired {
            token: "tok-1".to_string(),
            expires_at: Utc::now(),
            backup: LeaseBackup::Bytes {
                path: PathBuf::from("/tmp/backup"),
                sha256: "deadbeef".to_string(),
                size: 0,
            },
            headless_waiver: false,
        }
        .write_jsonl(&mut buf)
        .unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("\"status\":\"acquired\""), "{text}");
        assert!(text.contains("tok-1"), "{text}");
    }

    #[tokio::test]
    async fn native_document_is_refused_before_authenticating() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "n", "mimeType": "application/vnd.google-apps.document"
                })),
            )
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());

        // An authenticator that panics if called at all — proves the
        // native-document refusal happens before authentication, per the
        // module doc ("mirroring drive edit's identical refusal").
        struct PanicsIfCalled;
        impl Authenticator for PanicsIfCalled {
            fn authenticate(&self, _reason: &str, _policy: AuthPolicy) -> AuthOutcome {
                panic!("must not authenticate for a Google-native document");
            }
        }

        let result = acquire(&client, &opts(root.path()), &PanicsIfCalled).await;
        assert!(matches!(result, AcquireResult::RefusedNativeDocument));
    }

    #[tokio::test]
    async fn missing_version_is_refused_before_authenticating() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "n", "mimeType": "application/pdf"
                })),
            )
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());

        struct PanicsIfCalled;
        impl Authenticator for PanicsIfCalled {
            fn authenticate(&self, _reason: &str, _policy: AuthPolicy) -> AuthOutcome {
                panic!("must not authenticate without a version to lease against");
            }
        }

        let result = acquire(&client, &opts(root.path()), &PanicsIfCalled).await;
        assert!(matches!(result, AcquireResult::Failed { .. }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn authorized_acquisition_backs_up_bytes_and_records_a_ledger_row() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "report.pdf", "mimeType": "application/pdf",
                    "version": "42", "modifiedTime": "2026-09-11T00:00:00Z"
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param("alt", "media"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()))
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());

        let result = acquire(
            &client,
            &opts(root.path()),
            &FakeAuthenticator(AuthOutcome::Authorized),
        )
        .await;

        let AcquireResult::Acquired { token, backup, .. } = result else {
            panic!("expected Acquired, got {result:?}");
        };
        let LeaseBackup::Bytes { path, .. } = backup else {
            panic!("expected a Bytes backup for a binary file, got {backup:?}");
        };
        assert!(!token.is_empty());
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        assert!(path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains("f1-report.pdf"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn recorded_version_reflects_a_post_backup_fetch_not_the_pre_auth_snapshot() {
        // Regression test for the lease-record TOCTOU (issue #1664): the
        // metadata read at the very top of `acquire` (before the
        // authenticator prompt and the backup itself) returns version
        // "0". A foreign edit lands during that window, so the fetch
        // taken right after the backup returns "1" — and "1" is what must
        // end up in the ledger record, not the pre-auth "0" (which would
        // no longer match what was actually backed up).
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "report.pdf", "mimeType": "application/pdf",
                    "version": "0", "modifiedTime": "2026-09-11T00:00:00Z"
                })),
            )
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "report.pdf", "mimeType": "application/pdf",
                    "version": "1", "modifiedTime": "2026-09-11T00:05:00Z"
                })),
            )
            .with_priority(2)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param("alt", "media"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()))
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());
        let test_opts = opts(root.path());

        let result = acquire(
            &client,
            &test_opts,
            &FakeAuthenticator(AuthOutcome::Authorized),
        )
        .await;

        let AcquireResult::Acquired { token, .. } = result else {
            panic!("expected Acquired, got {result:?}");
        };
        let ledger = LeaseLedger::load(&test_opts.ledger_path).unwrap();
        let record = ledger.get(&token).expect("token must be in the ledger");
        assert_eq!(
            record.version, "1",
            "the recorded version must come from the post-backup fetch, not the pre-auth one"
        );
        assert_eq!(
            record.modified_time.as_deref(),
            Some("2026-09-11T00:05:00Z")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn native_document_with_a_backup_folder_configured_copies_instead_of_refusing() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "f1", "name": "Budget", "mimeType": "application/vnd.google-apps.spreadsheet",
                "version": "7"
            })))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/drive/v3/files/f1/copy"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "copy-1", "name": "backup", "mimeType": "application/vnd.google-apps.spreadsheet"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());
        let mut opts = opts(root.path());
        opts.native_backup_folder_id = Some("backup-folder".to_string());

        let result = acquire(&client, &opts, &FakeAuthenticator(AuthOutcome::Authorized)).await;

        let AcquireResult::Acquired { backup, .. } = result else {
            panic!("expected Acquired, got {result:?}");
        };
        assert_eq!(
            backup,
            LeaseBackup::DriveCopy {
                file_id: "copy-1".to_string()
            }
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_native_copy_after_authorization_is_reported_as_failed() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "f1", "name": "Budget", "mimeType": "application/vnd.google-apps.spreadsheet",
                "version": "7"
            })))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/drive/v3/files/f1/copy"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());
        let mut opts = opts(root.path());
        opts.native_backup_folder_id = Some("backup-folder".to_string());

        let result = acquire(&client, &opts, &FakeAuthenticator(AuthOutcome::Authorized)).await;

        assert!(
            matches!(result, AcquireResult::Failed { .. }),
            "expected Failed, got {result:?}"
        );
        assert!(!opts.ledger_path.exists(), "no ledger row must be written");
    }

    /// Mounts the pre-auth `files.get` (version "1", consumed once) plus
    /// the byte download, leaving the post-backup `files.get` to the
    /// caller — for tests exercising a failure in that second fetch.
    async fn mount_pre_auth_metadata_and_download(server: &wiremock::MockServer) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "report.pdf", "mimeType": "application/pdf",
                    "version": "1"
                })),
            )
            .up_to_n_times(1)
            .with_priority(1)
            .mount(server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param("alt", "media"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()))
            .mount(server)
            .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_post_backup_metadata_fetch_is_reported_as_failed() {
        let server = wiremock::MockServer::start().await;
        mount_pre_auth_metadata_and_download(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .with_priority(2)
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());
        let test_opts = opts(root.path());

        let result = acquire(
            &client,
            &test_opts,
            &FakeAuthenticator(AuthOutcome::Authorized),
        )
        .await;

        assert!(
            matches!(result, AcquireResult::Failed { .. }),
            "expected Failed, got {result:?}"
        );
        assert!(
            !test_opts.ledger_path.exists(),
            "no ledger row must be written"
        );
        assert!(
            std::fs::read_dir(&test_opts.backup_dir)
                .unwrap()
                .next()
                .is_none(),
            "the backup this attempt took must be reclaimed, not left orphaned (issue #1690)"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_post_backup_fetch_without_a_version_is_reported_as_failed() {
        // The pre-auth fetch carried a `version`, so the early refusal
        // does not fire; the post-backup fetch — the one the ledger row is
        // actually recorded from — omits it, and a lease with no staleness
        // check must not be minted.
        let server = wiremock::MockServer::start().await;
        mount_pre_auth_metadata_and_download(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "report.pdf", "mimeType": "application/pdf"
                })),
            )
            .with_priority(2)
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());
        let test_opts = opts(root.path());

        let result = acquire(
            &client,
            &test_opts,
            &FakeAuthenticator(AuthOutcome::Authorized),
        )
        .await;

        let AcquireResult::Failed { detail } = result else {
            panic!("expected Failed, got {result:?}");
        };
        assert!(detail.contains("after the backup"), "{detail}");
        assert!(
            !test_opts.ledger_path.exists(),
            "no ledger row must be written"
        );
        assert!(
            std::fs::read_dir(&test_opts.backup_dir)
                .unwrap()
                .next()
                .is_none(),
            "the backup this attempt took must be reclaimed, not left orphaned (issue #1690)"
        );
    }

    // ── the audit sink (ADR-0080 §11) ──────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn an_acquired_lease_writes_an_audit_record_carrying_the_backup_and_version() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "report.pdf", "mimeType": "application/pdf",
                    "version": "42", "modifiedTime": "2026-09-11T00:00:00Z"
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param("alt", "media"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()))
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());

        let result = acquire(
            &client,
            &opts(root.path()),
            &FakeAuthenticator(AuthOutcome::Authorized),
        )
        .await;

        let AcquireResult::Acquired { token, .. } = result else {
            panic!("expected Acquired, got {result:?}");
        };

        let contents = std::fs::read_to_string(root.path().join("audit.jsonl")).unwrap();
        let rec: crate::request_log::LogRecord = serde_json::from_str(contents.trim_end()).unwrap();
        assert_eq!(rec.kind, crate::request_log::RecordKind::Audit);
        assert_eq!(rec.context.get("lease_id"), Some(&token));
        assert_eq!(
            rec.context.get("verdict").map(String::as_str),
            Some("acquired")
        );
        assert_eq!(
            rec.context.get("version_after").map(String::as_str),
            Some("42")
        );
        assert_eq!(
            rec.context.get("auth_policy").map(String::as_str),
            Some("device-owner")
        );
        assert!(rec.context.contains_key("backup_sha256"));
    }

    #[tokio::test]
    async fn a_refused_acquisition_still_writes_an_audit_record() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "f1", "name": "Budget", "mimeType": "application/vnd.google-apps.spreadsheet",
                "version": "7"
            })))
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());

        struct PanicsIfCalled;
        impl Authenticator for PanicsIfCalled {
            fn authenticate(&self, _reason: &str, _policy: AuthPolicy) -> AuthOutcome {
                panic!("must not authenticate with no backup folder configured");
            }
        }

        let result = acquire(&client, &opts(root.path()), &PanicsIfCalled).await;

        assert!(matches!(result, AcquireResult::RefusedNativeDocument));

        let contents = std::fs::read_to_string(root.path().join("audit.jsonl")).unwrap();
        let rec: crate::request_log::LogRecord = serde_json::from_str(contents.trim_end()).unwrap();
        assert_eq!(
            rec.context.get("verdict").map(String::as_str),
            Some("refused-native-document")
        );
        assert_eq!(rec.context.get("lease_id"), None);
    }

    #[tokio::test]
    async fn native_document_without_a_backup_folder_is_refused_before_authenticating() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "f1", "name": "Budget", "mimeType": "application/vnd.google-apps.spreadsheet",
                "version": "7"
            })))
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());

        struct PanicsIfCalled;
        impl Authenticator for PanicsIfCalled {
            fn authenticate(&self, _reason: &str, _policy: AuthPolicy) -> AuthOutcome {
                panic!("must not authenticate with no backup folder configured");
            }
        }

        let result = acquire(&client, &opts(root.path()), &PanicsIfCalled).await;
        assert!(matches!(result, AcquireResult::RefusedNativeDocument));
    }

    // ── expiry bound enforced in the engine, not only the CLI (#1664) ───

    #[tokio::test]
    async fn expiry_below_the_minimum_is_refused_before_authenticating() {
        // No `/drive/v3/files/f1` mock is mounted — if the check below did
        // not gate before the metadata fetch, this would fail differently
        // (a connection/parse error, not this message).
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());
        let mut test_opts = opts(root.path());
        test_opts.expiry = ChronoDuration::minutes(0);

        struct PanicsIfCalled;
        impl Authenticator for PanicsIfCalled {
            fn authenticate(&self, _reason: &str, _policy: AuthPolicy) -> AuthOutcome {
                panic!("must not authenticate with an out-of-range expiry");
            }
        }

        let result = acquire(&client, &test_opts, &PanicsIfCalled).await;
        let AcquireResult::Failed { detail } = result else {
            panic!("expected Failed, got {result:?}");
        };
        assert!(detail.contains("expiry must be between"), "{detail}");
    }

    #[tokio::test]
    async fn expiry_above_the_maximum_is_refused_before_authenticating() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());
        let mut test_opts = opts(root.path());
        test_opts.expiry = ChronoDuration::minutes(MAX_EXPIRY_MINUTES + 1);

        struct PanicsIfCalled;
        impl Authenticator for PanicsIfCalled {
            fn authenticate(&self, _reason: &str, _policy: AuthPolicy) -> AuthOutcome {
                panic!("must not authenticate with an out-of-range expiry");
            }
        }

        let result = acquire(&client, &test_opts, &PanicsIfCalled).await;
        let AcquireResult::Failed { detail } = result else {
            panic!("expected Failed, got {result:?}");
        };
        assert!(detail.contains("expiry must be between"), "{detail}");
    }

    // ── refusing a second live lease on the same file (#1664) ───────────

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_acquisition_while_one_is_still_live_reuses_the_existing_token_instead_of_minting_a_second(
    ) {
        // Regression test for the TOCTOU this closes: two independently
        // acquired leases on the same file would otherwise each capture the
        // same version and each pass their own staleness check against it,
        // letting the second writer's write silently clobber the first's.
        // Refusing to mint the second lease at all closes that gap. The
        // second attempt's authenticator panics if called at all — proving
        // the lock-free pre-check (issue #1690) refuses it before ever
        // spending a Touch ID prompt, not merely before minting.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "report.pdf", "mimeType": "application/pdf",
                    "version": "1"
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param("alt", "media"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()))
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());
        let test_opts = opts(root.path());

        let first = acquire(
            &client,
            &test_opts,
            &FakeAuthenticator(AuthOutcome::Authorized),
        )
        .await;
        let AcquireResult::Acquired {
            token: first_token, ..
        } = first
        else {
            panic!("expected Acquired, got {first:?}");
        };

        let mut second_opts = test_opts.clone();
        second_opts.backup_dir = root.path().join("backups2");

        struct PanicsIfCalled;
        impl Authenticator for PanicsIfCalled {
            fn authenticate(&self, _reason: &str, _policy: AuthPolicy) -> AuthOutcome {
                panic!(
                    "must not authenticate: the lock-free pre-check must refuse a second \
                     live lease before spending a prompt"
                );
            }
        }

        let second = acquire(&client, &second_opts, &PanicsIfCalled).await;
        let AcquireResult::AlreadyLeased { token, .. } = second else {
            panic!("expected AlreadyLeased, got {second:?}");
        };
        assert_eq!(token, first_token);
        assert!(
            !second_opts.backup_dir.exists(),
            "the pre-check must refuse before ever taking a backup"
        );

        // Only the first lease is live in the ledger.
        let ledger = LeaseLedger::load(&test_opts.ledger_path).unwrap();
        assert_eq!(
            ledger
                .live_lease_for_file("f1", Utc::now())
                .map(|r| &r.token),
            Some(&first_token)
        );
    }

    // ── the belt-and-braces reclaim for the pre-check's narrow race (#1690) ──

    /// An authenticator that, as a side effect of authenticating, inserts a
    /// live lease for `file_id` into the ledger at `ledger_path` — used to
    /// simulate another process's `acquire` winning the race between this
    /// attempt's lock-free pre-check and its own `insert_record`, which the
    /// pre-check is not designed to close (only to make rare).
    struct InsertsALiveLeaseDuringAuth {
        ledger_path: PathBuf,
        file_id: String,
    }
    impl Authenticator for InsertsALiveLeaseDuringAuth {
        fn authenticate(&self, _reason: &str, _policy: AuthPolicy) -> AuthOutcome {
            LeaseLedger::mutate_locked(&self.ledger_path, |ledger| {
                ledger.insert(LeaseRecord {
                    token: "racer".to_string(),
                    file_id: self.file_id.clone(),
                    version: "1".to_string(),
                    modified_time: None,
                    backup: LeaseBackup::DriveCopy {
                        file_id: "racer-backup".to_string(),
                    },
                    acquired_at: Utc::now(),
                    expires_at: Utc::now() + ChronoDuration::minutes(30),
                    released_at: None,
                    restored_at: None,
                });
            })
            .unwrap();
            AuthOutcome::Authorized
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_lease_inserted_during_the_prompt_reclaims_this_attempts_bytes_backup() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "report.pdf", "mimeType": "application/pdf", "version": "1"
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param("alt", "media"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()))
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());
        let test_opts = opts(root.path());

        let result = acquire(
            &client,
            &test_opts,
            &InsertsALiveLeaseDuringAuth {
                ledger_path: test_opts.ledger_path.clone(),
                file_id: test_opts.file_id.clone(),
            },
        )
        .await;

        let AcquireResult::AlreadyLeased { token, .. } = result else {
            panic!("expected AlreadyLeased, got {result:?}");
        };
        assert_eq!(token, "racer");
        assert!(
            std::fs::read_dir(&test_opts.backup_dir)
                .unwrap()
                .next()
                .is_none(),
            "this attempt's own now-orphaned backup must be reclaimed"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_lease_inserted_during_the_prompt_reclaims_this_attempts_native_backup() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "f1", "name": "Budget", "mimeType": "application/vnd.google-apps.spreadsheet",
                "version": "7"
            })))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/drive/v3/files/f1/copy"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "copy-1", "name": "backup", "mimeType": "application/vnd.google-apps.spreadsheet"
            })))
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/drive/v3/files/copy-1"))
            .and(wiremock::matchers::body_json(
                serde_json::json!({"trashed": true}),
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "copy-1", "name": "backup", "trashed": true,
                })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());
        let mut test_opts = opts(root.path());
        test_opts.native_backup_folder_id = Some("backup-folder".to_string());

        let result = acquire(
            &client,
            &test_opts,
            &InsertsALiveLeaseDuringAuth {
                ledger_path: test_opts.ledger_path.clone(),
                file_id: test_opts.file_id.clone(),
            },
        )
        .await;

        let AcquireResult::AlreadyLeased { token, .. } = result else {
            panic!("expected AlreadyLeased, got {result:?}");
        };
        assert_eq!(token, "racer");
        // The PATCH mock's `.expect(1)` above is the real assertion:
        // reclaiming this attempt's own orphaned Drive-copy backup means
        // trashing it.
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_reclamation_failure_is_audited_as_backup_orphaned_but_still_reports_already_leased()
    {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "f1", "name": "Budget", "mimeType": "application/vnd.google-apps.spreadsheet",
                "version": "7"
            })))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/drive/v3/files/f1/copy"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "copy-1", "name": "backup", "mimeType": "application/vnd.google-apps.spreadsheet"
            })))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/drive/v3/files/copy-1"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());
        let mut test_opts = opts(root.path());
        test_opts.native_backup_folder_id = Some("backup-folder".to_string());

        let result = acquire(
            &client,
            &test_opts,
            &InsertsALiveLeaseDuringAuth {
                ledger_path: test_opts.ledger_path.clone(),
                file_id: test_opts.file_id.clone(),
            },
        )
        .await;

        assert!(matches!(result, AcquireResult::AlreadyLeased { .. }));
        let contents = std::fs::read_to_string(root.path().join("audit.jsonl")).unwrap();
        let rec: crate::request_log::LogRecord = serde_json::from_str(contents.trim_end()).unwrap();
        assert_eq!(
            rec.context.get("verdict").map(String::as_str),
            Some("already-leased-backup-orphaned")
        );
        assert_eq!(
            rec.context.get("backup_location").map(String::as_str),
            Some("copy-1")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_acquisition_after_the_first_expires_mints_a_fresh_lease() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "report.pdf", "mimeType": "application/pdf",
                    "version": "1"
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param("alt", "media"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()))
            .mount(&server)
            .await;
        let client = client_with_bootstrapped_token(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _audit = AuditGuard::redirect(root.path());
        let test_opts = opts(root.path());

        let first = acquire(
            &client,
            &test_opts,
            &FakeAuthenticator(AuthOutcome::Authorized),
        )
        .await;
        let AcquireResult::Acquired {
            token: first_token, ..
        } = first
        else {
            panic!("expected Acquired, got {first:?}");
        };
        // Backdate the first lease's expiry directly rather than waiting out
        // a real one — `MIN_EXPIRY_MINUTES` now rules out a sub-minute
        // `--expiry-minutes` that a real sleep-then-retry could use instead.
        let mut ledger = LeaseLedger::load(&test_opts.ledger_path).unwrap();
        let mut expired = ledger.get(&first_token).unwrap().clone();
        expired.expires_at = Utc::now() - ChronoDuration::minutes(1);
        ledger.insert(expired);
        ledger.save(&test_opts.ledger_path).unwrap();

        // A distinct `backup_dir`: `backup_name`'s timestamp has only
        // whole-second precision, so a second backup of the same file
        // within one test's runtime would otherwise collide on the same
        // path (see `write_backup`'s own doc comment).
        let mut second_opts = test_opts.clone();
        second_opts.backup_dir = root.path().join("backups2");

        let second = acquire(
            &client,
            &second_opts,
            &FakeAuthenticator(AuthOutcome::Authorized),
        )
        .await;
        let AcquireResult::Acquired {
            token: second_token,
            ..
        } = second
        else {
            panic!("expected Acquired, got {second:?}");
        };
        assert_ne!(second_token, first_token);
    }
}
