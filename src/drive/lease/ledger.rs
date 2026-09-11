//! The lease ledger (`lease-ledger.jsonl`) — the operational state
//! [ADR-0080](../../../docs/adrs/adr-0080.md) §4 requires: read by every
//! leased write to decide whether a presented token is valid, unexpired,
//! bound to the right file, and not stale. Distinct from the audit log
//! (`crate::request_log`'s `audit.jsonl`, ADR-0080 §11): the ledger is a
//! live, queryable store a program's own logic depends on to make a
//! security decision; the audit log is never read to authorize anything.
//!
//! Follows `gmail insert`'s `InsertLedger` precedent exactly
//! (`src/cli/gmail/insert/ledger.rs`): an in-memory `BTreeMap` is the
//! source of truth, and every state change atomically rewrites the whole
//! file (`tempfile::NamedTempFile` + `persist`), never appends an event.
//! Unlike `InsertLedger`, a row is **never dropped** on rewrite once
//! expired or released — `drive lease restore` (a later phase) locates a
//! backup by token, and a restore is almost always wanted *after* the
//! expiry window, once a bad write has been noticed. A `lease prune` that
//! drops a row together with the backup it points at is the deliberate
//! fast-follow ADR-0080's Consequences name, not implemented here.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Where a lease's backup landed (ADR-0080 §3's fidelity split).
///
/// An enum, not a set of `Option` fields on [`LeaseRecord`] directly: the
/// two kinds are mutually exclusive by construction (a lease backs up
/// either bytes or a native document, never both or neither), which a
/// type-level split enforces and a handful of independently-optional
/// fields would only document.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum LeaseBackup {
    /// A binary file's raw bytes, copied to local disk (ADR-0080 §3,
    /// Phase 2).
    Bytes {
        /// Local path the bytes were written to.
        path: PathBuf,
        /// SHA-256 of the backed-up bytes, so a later restore (or an
        /// auditor) can verify the backup was not corrupted or tampered
        /// with.
        sha256: String,
        /// Size of the backed-up bytes.
        size: u64,
    },
    /// A native document (Sheet/Doc/Slide), copied Drive-side via
    /// `files.copy` into the account's configured backup folder
    /// (ADR-0080 §3, Phase 3) — lossless, and restorable by a human in the
    /// Drive UI even without this tool.
    DriveCopy {
        /// The copy's own Drive file id.
        file_id: String,
    },
}

/// One row of the ledger — a lease's full operational state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct LeaseRecord {
    /// The opaque token identifying this lease. An identifier, not a
    /// bearer credential (ADR-0080 §2) — safe to log in plaintext, since a
    /// write under it still needs this user's own ledger, OAuth
    /// credentials and folder-permission grant.
    pub(crate) token: String,
    /// The Drive file id this lease is bound to. A write presenting this
    /// token against a *different* file id is refused
    /// (`RefusedLeaseWrongFile`).
    pub(crate) file_id: String,
    /// The file's Drive `version` this lease was last checked against —
    /// recorded at acquire time, and updated after each successful write
    /// under this lease (ADR-0080 §5's multi-use semantics). The
    /// staleness check compares this to the file's *live* `version`.
    pub(crate) version: String,
    /// The file's `modifiedTime`, same update rule as `version` — carried
    /// for the audit trail, not itself compared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) modified_time: Option<String>,
    /// Where the backup landed.
    pub(crate) backup: LeaseBackup,
    /// When Touch ID (or the account password) authorised this lease.
    pub(crate) acquired_at: DateTime<Utc>,
    /// Absolute expiry, fixed at acquisition. Never extended by a write —
    /// ADR-0080 §5: the only way to get a fresh window is a fresh
    /// `drive lease acquire`, which means a fresh prompt.
    pub(crate) expires_at: DateTime<Utc>,
    /// Set by an explicit release (a later phase's verb, not yet
    /// implemented) — distinct from expiry, which needs no write to take
    /// effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) released_at: Option<DateTime<Utc>>,
}

impl LeaseRecord {
    /// Whether this lease may still authorise a write: unexpired and not
    /// explicitly released. Does **not** check the file's live `version`
    /// against [`Self::version`] — that is the staleness check, a separate
    /// concern evaluated against a fresh `files.get` (ADR-0080 §6).
    pub(crate) fn is_live(&self, now: DateTime<Utc>) -> bool {
        self.released_at.is_none() && now < self.expires_at
    }
}

/// The whole ledger, keyed by token.
///
/// Loaded entirely into memory, mutated, and rewritten wholesale on
/// [`LeaseLedger::save`] — see the module doc for why, and for how this
/// differs from `InsertLedger`.
#[derive(Debug, Default)]
pub(crate) struct LeaseLedger(BTreeMap<String, LeaseRecord>);

impl LeaseLedger {
    /// Loads `lease-ledger.jsonl`. An absent file is an empty ledger (no
    /// lease has ever been acquired on this machine); a present-but-
    /// unparseable file is a hard error, mirroring `InsertLedger::load`'s
    /// posture — losing this file doesn't lose recoverable metadata, it
    /// loses the only record of which tokens are still valid.
    pub(crate) fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("Failed to read lease ledger at {}", path.display()))
            }
        };
        let mut map = BTreeMap::new();
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let record: LeaseRecord = serde_json::from_str(line).with_context(|| {
                format!(
                    "Failed to parse lease ledger line {} at {} — this ledger is the sole \
                     record of which lease tokens are still valid, against what version, and \
                     which backups they point at. Fix the line or restore the ledger from a \
                     backup before running any leased write",
                    i + 1,
                    path.display()
                )
            })?;
            map.insert(record.token.clone(), record);
        }
        Ok(Self(map))
    }

    /// Looks up a token's current row, if any.
    pub(crate) fn get(&self, token: &str) -> Option<&LeaseRecord> {
        self.0.get(token)
    }

    /// Records a freshly acquired lease.
    pub(crate) fn insert(&mut self, record: LeaseRecord) {
        self.0.insert(record.token.clone(), record);
    }

    /// Updates the recorded `version`/`modified_time` for `token` after a
    /// successful write under it (ADR-0080 §5's multi-use semantics). A
    /// no-op if the token is absent, which should not happen — the caller
    /// already validated it via [`Self::get`] before performing the write.
    pub(crate) fn record_write(
        &mut self,
        token: &str,
        version: String,
        modified_time: Option<String>,
    ) {
        if let Some(rec) = self.0.get_mut(token) {
            rec.version = version;
            rec.modified_time = modified_time;
        }
    }

    /// Atomically rewrites `lease-ledger.jsonl` in full — the same
    /// temp-file-in-the-same-directory-then-rename pattern
    /// `InsertLedger::save`/`sync::manifest::Manifest::save` use.
    pub(crate) fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            if !dir.exists() {
                crate::daemon::paths::ensure_dir_0700(dir)?;
            }
        }
        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut tmp = tempfile::NamedTempFile::new_in(dir)
            .with_context(|| format!("Failed to create a temp file in {}", dir.display()))?;
        for record in self.0.values() {
            serde_json::to_writer(&mut tmp, record)
                .context("Failed to serialise a lease ledger record")?;
            tmp.write_all(b"\n")
                .context("Failed to write a lease ledger record")?;
        }
        tmp.flush().context("Failed to flush the lease ledger")?;
        tmp.persist(path)
            .map_err(|e| e.error)
            .with_context(|| format!("Failed to publish lease ledger to {}", path.display()))?;
        Ok(())
    }
}

/// Resolves `lease-ledger.jsonl`'s path: `state_dir` (falling back to
/// `data_dir`) joined with `omni-dev/lease-ledger.jsonl` — the same
/// resolution `crate::request_log::log_file_path` uses, no env override
/// (unlike the log files, nothing about *which* ledger a write consults is
/// meant to be redirectable per-invocation).
pub(crate) fn ledger_path() -> Result<PathBuf> {
    let base = dirs::state_dir()
        .or_else(dirs::data_dir)
        .context("could not resolve the state/data directory for the lease ledger")?;
    Ok(base.join("omni-dev").join("lease-ledger.jsonl"))
}

/// `<ledger_path>.lock`, an advisory marker held for the lifetime of a
/// mutating ledger access (see [`LedgerLock`]). Derived from the ledger
/// path itself — never resolved independently — so a caller that redirects
/// the ledger (as every test does) automatically redirects its lock too.
fn lock_path_for(ledger_path: &Path) -> PathBuf {
    let mut name = ledger_path.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

/// An advisory, call-scoped lock (`create_new`, so two processes racing to
/// create it see exactly one winner) guarding against two overlapping
/// `drive lease acquire`/leased-write invocations against the same ledger:
/// [`LeaseLedger::save`] rewrites the whole file, so a second call's
/// rewrite racing the first would silently discard whichever lease state
/// lost the race. Held for the call's duration and removed on drop —
/// mirrors `gmail insert`'s `LedgerLock` exactly.
#[derive(Debug)]
pub(crate) struct LedgerLock {
    path: PathBuf,
}

impl LedgerLock {
    /// Acquires the lock guarding `ledger_path`. Production code passes
    /// [`ledger_path`]'s own result; tests pass a path under a `tempdir` so
    /// they never touch the real ledger or its lock.
    pub(crate) fn acquire(ledger_path: &Path) -> Result<Self> {
        let path = lock_path_for(ledger_path);
        if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            if !dir.exists() {
                crate::daemon::paths::ensure_dir_0700(dir)?;
            }
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path).with_context(|| {
            format!(
                "another `drive lease` operation appears to already be in progress ({} \
                 exists) — concurrent access would clobber the ledger. If you're sure no \
                 other operation is active (e.g. after a crash), remove the lock file and \
                 retry",
                path.display()
            )
        })?;
        crate::daemon::paths::ensure_handle_0600(&file)
            .with_context(|| format!("Failed to set 0600 on lock file {}", path.display()))?;
        Ok(Self { path })
    }
}

impl Drop for LedgerLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;

    fn sample_record(token: &str) -> LeaseRecord {
        LeaseRecord {
            token: token.to_string(),
            file_id: "file1".to_string(),
            version: "1".to_string(),
            modified_time: Some("2026-09-11T00:00:00Z".to_string()),
            backup: LeaseBackup::Bytes {
                path: PathBuf::from("/tmp/backup1"),
                sha256: "abc123".to_string(),
                size: 42,
            },
            acquired_at: Utc::now(),
            expires_at: Utc::now() + ChronoDuration::minutes(30),
            released_at: None,
        }
    }

    #[test]
    fn lease_backup_drive_copy_round_trips_through_json() {
        let backup = LeaseBackup::DriveCopy {
            file_id: "copy-1".to_string(),
        };
        let json = serde_json::to_string(&backup).unwrap();
        assert!(json.contains("\"kind\":\"drive_copy\""), "{json}");
        let back: LeaseBackup = serde_json::from_str(&json).unwrap();
        assert_eq!(back, backup);
    }

    #[test]
    fn is_live_true_before_expiry_and_unreleased() {
        let rec = sample_record("t1");
        assert!(rec.is_live(Utc::now()));
    }

    #[test]
    fn is_live_false_after_expiry() {
        let rec = sample_record("t1");
        assert!(!rec.is_live(Utc::now() + ChronoDuration::hours(1)));
    }

    #[test]
    fn is_live_false_once_released() {
        let mut rec = sample_record("t1");
        rec.released_at = Some(Utc::now());
        assert!(!rec.is_live(Utc::now()));
    }

    #[test]
    fn load_absent_file_is_an_empty_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = LeaseLedger::load(&dir.path().join("nope.jsonl")).unwrap();
        assert!(ledger.get("t1").is_none());
    }

    #[test]
    fn load_surfaces_a_non_not_found_read_error() {
        // A directory at the ledger path makes `read_to_string` fail with
        // something other than `NotFound` (e.g. "Is a directory") — unlike
        // an absent file, that must be a hard error, not an empty ledger.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lease-ledger.jsonl");
        std::fs::create_dir(&path).unwrap();
        let err = LeaseLedger::load(&path).unwrap_err();
        assert!(err.to_string().contains("Failed to read lease ledger"));
    }

    #[test]
    fn load_rejects_a_corrupt_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lease-ledger.jsonl");
        std::fs::write(&path, "not json\n").unwrap();
        assert!(LeaseLedger::load(&path).is_err());
    }

    #[test]
    fn load_skips_blank_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lease-ledger.jsonl");
        std::fs::write(&path, "\n\n").unwrap();
        let ledger = LeaseLedger::load(&path).unwrap();
        assert!(ledger.get("t1").is_none());
    }

    #[test]
    fn save_then_load_round_trips_a_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lease-ledger.jsonl");
        let mut ledger = LeaseLedger::default();
        ledger.insert(sample_record("t1"));
        ledger.save(&path).unwrap();

        let reloaded = LeaseLedger::load(&path).unwrap();
        let rec = reloaded.get("t1").unwrap();
        assert_eq!(rec.file_id, "file1");
        assert_eq!(rec.version, "1");
    }

    #[test]
    fn record_write_updates_version_and_modified_time() {
        let mut ledger = LeaseLedger::default();
        ledger.insert(sample_record("t1"));
        ledger.record_write("t1", "2".to_string(), Some("later".to_string()));
        let rec = ledger.get("t1").unwrap();
        assert_eq!(rec.version, "2");
        assert_eq!(rec.modified_time.as_deref(), Some("later"));
    }

    #[test]
    fn record_write_on_an_absent_token_is_a_noop() {
        let mut ledger = LeaseLedger::default();
        ledger.record_write("missing", "2".to_string(), None);
        assert!(ledger.get("missing").is_none());
    }

    #[test]
    fn expired_row_is_kept_on_save_not_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lease-ledger.jsonl");
        let mut rec = sample_record("t1");
        rec.expires_at = Utc::now() - ChronoDuration::hours(1);
        let mut ledger = LeaseLedger::default();
        ledger.insert(rec);
        ledger.save(&path).unwrap();

        let reloaded = LeaseLedger::load(&path).unwrap();
        assert!(reloaded.get("t1").is_some());
    }

    #[test]
    fn save_creates_a_missing_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("lease-ledger.jsonl");
        let mut ledger = LeaseLedger::default();
        ledger.insert(sample_record("t1"));
        ledger.save(&path).unwrap();

        assert!(path.exists());
        let reloaded = LeaseLedger::load(&path).unwrap();
        assert!(reloaded.get("t1").is_some());
    }

    #[test]
    fn save_is_atomic_and_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lease-ledger.jsonl");
        let mut ledger = LeaseLedger::default();
        ledger.insert(sample_record("t1"));
        ledger.save(&path).unwrap();

        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            entries,
            vec![std::ffi::OsString::from("lease-ledger.jsonl")]
        );
    }

    #[test]
    fn ledger_lock_acquire_refuses_while_held() {
        // Exercises the lock against an explicit path rather than the real
        // state dir, by constructing the lock file directly — mirrors
        // `LedgerLock`'s own `create_new` semantics without depending on
        // `dirs::state_dir()` in a test.
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("held.lock");
        let _held = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
            .unwrap();
        let second = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path);
        assert!(second.is_err());
    }

    #[test]
    fn ledger_lock_acquire_creates_a_missing_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("nested").join("lease-ledger.jsonl");
        let lock = LedgerLock::acquire(&ledger_path).unwrap();
        assert!(dir
            .path()
            .join("nested")
            .join("lease-ledger.jsonl.lock")
            .exists());
        drop(lock);
    }

    #[test]
    fn ledger_lock_acquire_derives_its_path_from_the_ledger_path_and_refuses_a_second_caller() {
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("lease-ledger.jsonl");

        let first = LedgerLock::acquire(&ledger_path).unwrap();
        assert!(dir.path().join("lease-ledger.jsonl.lock").exists());
        assert!(LedgerLock::acquire(&ledger_path).is_err());

        drop(first);
        assert!(LedgerLock::acquire(&ledger_path).is_ok());
    }

    #[test]
    fn ledger_lock_removes_its_file_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("held.lock");
        {
            let lock = LedgerLock { path: path.clone() };
            assert!(!path.exists());
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .unwrap();
            drop(lock);
        }
        assert!(!path.exists());
    }
}
