//! `insert-ledger.jsonl`: the sole record of what `gmail insert` has already
//! inserted into a destination mailbox — the idempotency/resumability
//! mechanism a multi-thousand-message restore needs (issue #1655).
//!
//! Gmail assigns a **fresh** id to every inserted message, so
//! `manifest.jsonl`'s own `id` (the *source* mailbox's id) can't be used as
//! a presence check the way `gmail sync`/`extract-attachments` use it. The
//! dedupe key is instead [`dedupe_key`] — the archived `Message-ID`, or a
//! synthetic fallback — scoped by **destination address**, not just the
//! key: the archive's own `state.json` names the *source* mailbox, which
//! for the consolidation use case ([#1655]'s primary motivation) is
//! precisely not where mail is going. A ledger written for one destination
//! must yield zero hits against a different one, mechanically — so
//! `crate::cli::gmail::sync::state::validate_identity` is **deliberately
//! not called** anywhere in `gmail insert`; that check exists for `sync`'s
//! different job of keeping one archive tied to one source mailbox.
//!
//! Follows `sync::manifest`'s posture, not `sync::state`'s: a corrupt
//! ledger is a hard error, never a silent start-from-empty — discarding it
//! doesn't lose metadata, it duplicates every already-inserted message on
//! the very next run.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// `<archive-dir>/insert-ledger.jsonl`, a sibling of `manifest.jsonl`.
pub(crate) fn ledger_path(archive_dir: &Path) -> PathBuf {
    archive_dir.join("insert-ledger.jsonl")
}

/// `<archive-dir>/insert-ledger.lock`, an advisory marker held for the
/// lifetime of a non-dry-run insert (see [`LedgerLock`]).
fn ledger_lock_path(archive_dir: &Path) -> PathBuf {
    archive_dir.join("insert-ledger.lock")
}

/// How a ledger entry came to exist.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InsertOrigin {
    /// This run (or an earlier one) actually called `messages.insert`.
    Inserted,
    /// `--verify-remote` found a matching message already on the
    /// destination via an `rfc822msgid:` probe, without inserting anything.
    RemoteProbe,
}

/// One record of a source message already accounted for against a
/// destination mailbox.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct InsertLedgerRecord {
    /// The destination mailbox's email address — part of the dedupe
    /// identity, not metadata; see the module doc.
    pub(crate) destination: String,
    /// The dedupe key ([`dedupe_key`]) this record was stored under.
    pub(crate) key: String,
    /// The source archive's message id (`manifest.jsonl`'s key).
    pub(crate) source_id: String,
    /// The id Gmail assigned on insert. Absent for an
    /// [`InsertOrigin::RemoteProbe`] hit — no insert happened, so there is
    /// no new id to record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) inserted_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) inserted_thread_id: Option<String>,
    pub(crate) inserted_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) label_ids: Vec<String>,
    pub(crate) origin: InsertOrigin,
}

/// The whole ledger, keyed by `(destination, key)` — a `BTreeMap` for the
/// same deterministic-output reason `sync::manifest::Manifest` uses one.
///
/// Loaded entirely into memory, mutated, and rewritten wholesale on
/// [`Self::save`] — identical strategy to `sync::manifest::Manifest`, at a
/// scale (thousands, not millions, of entries per archive) where that's
/// cheap.
#[derive(Debug, Default)]
pub(crate) struct InsertLedger(BTreeMap<(String, String), InsertLedgerRecord>);

impl InsertLedger {
    /// Loads `insert-ledger.jsonl`. An absent file is an empty ledger
    /// (first insert run against this archive); a present-but-unparseable
    /// file is a hard error — see the module doc.
    pub(crate) fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("Failed to read insert ledger at {}", path.display()))
            }
        };
        let mut map = BTreeMap::new();
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let record: InsertLedgerRecord = serde_json::from_str(line).with_context(|| {
                format!(
                    "Failed to parse insert ledger line {} at {} — this ledger is the sole \
                     record of what `gmail insert` has already inserted into a live mailbox; a \
                     silently-discarded ledger means the next run duplicates every message it \
                     covers. Fix the line or restore the ledger from a backup before re-running \
                     `gmail insert` (recovery from a lost ledger is `--verify-remote`)",
                    i + 1,
                    path.display()
                )
            })?;
            map.insert((record.destination.clone(), record.key.clone()), record);
        }
        Ok(Self(map))
    }

    /// Whether `destination`/`key` has already been accounted for (inserted
    /// or confirmed present via `--verify-remote`).
    pub(crate) fn contains(&self, destination: &str, key: &str) -> bool {
        self.0
            .contains_key(&(destination.to_string(), key.to_string()))
    }

    /// Records (or replaces) one entry.
    pub(crate) fn record(&mut self, record: InsertLedgerRecord) {
        self.0
            .insert((record.destination.clone(), record.key.clone()), record);
    }

    /// Atomically rewrites `insert-ledger.jsonl` in full — the same
    /// temp-file-in-the-same-directory-then-rename pattern as
    /// `sync::manifest::Manifest::save`.
    pub(crate) fn save(&self, path: &Path) -> Result<()> {
        let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
        let dir = dir.unwrap_or_else(|| Path::new("."));
        let mut tmp = tempfile::NamedTempFile::new_in(dir)
            .with_context(|| format!("Failed to create a temp file in {}", dir.display()))?;
        for record in self.0.values() {
            serde_json::to_writer(&mut tmp, record)
                .context("Failed to serialise an insert ledger record")?;
            tmp.write_all(b"\n")
                .context("Failed to write an insert ledger record")?;
        }
        tmp.flush().context("Failed to flush the insert ledger")?;
        tmp.persist(path)
            .map_err(|e| e.error)
            .with_context(|| format!("Failed to publish insert ledger to {}", path.display()))?;
        Ok(())
    }
}

/// Computes the dedupe key for one archived message: the archived
/// `Message-ID` with surrounding whitespace/angle-brackets stripped (it is
/// stored verbatim from the wire — see
/// `crate::cli::gmail::sync::manifest::ManifestRecord::rfc822_msgid` — so
/// still carries `<...>`), **not** case-folded, since a `Message-ID` local
/// part is case-sensitive and folding it risks a false merge that makes a
/// distinct message never get inserted. A message archived without a
/// `Message-ID` header falls back to an `archive-id:<source id>` sentinel,
/// scoped by the source archive's own id — collision-free within one
/// archive, though it can never be confirmed present via a remote
/// `rfc822msgid:` probe (there is no real Message-ID to search for).
pub(crate) fn dedupe_key(rfc822_msgid: Option<&str>, source_id: &str) -> String {
    match rfc822_msgid.map(str::trim).filter(|s| !s.is_empty()) {
        Some(msgid) => msgid
            .trim_start_matches('<')
            .trim_end_matches('>')
            .to_string(),
        None => format!("archive-id:{source_id}"),
    }
}

/// An advisory, run-scoped lock (`create_new`, so two processes racing to
/// create it see exactly one winner) guarding against two concurrent
/// `gmail insert` runs sharing an archive dir: [`InsertLedger::save`]
/// rewrites the whole file, so a second run's rewrite would silently
/// discard the first run's in-flight progress. Held for the run's duration
/// and removed on drop — best-effort, `--dry-run` never acquires it (it
/// never touches the ledger file at all).
#[derive(Debug)]
pub(crate) struct LedgerLock {
    path: PathBuf,
}

impl LedgerLock {
    pub(crate) fn acquire(archive_dir: &Path) -> Result<Self> {
        let path = ledger_lock_path(archive_dir);
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| {
                format!(
                    "another `gmail insert` run appears to already be in progress against this \
                     archive dir ({} exists) — concurrent runs would clobber each other's \
                     ledger. If you're sure no other run is active (e.g. after a crash), remove \
                     the lock file and re-run",
                    path.display()
                )
            })?;
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

    fn sample_record(destination: &str, key: &str) -> InsertLedgerRecord {
        InsertLedgerRecord {
            destination: destination.to_string(),
            key: key.to_string(),
            source_id: "src1".to_string(),
            inserted_id: Some("new1".to_string()),
            inserted_thread_id: Some("thread1".to_string()),
            inserted_at: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            label_ids: vec!["INBOX".to_string()],
            origin: InsertOrigin::Inserted,
        }
    }

    // ── dedupe_key ────────────────────────────────────────────────────

    #[test]
    fn dedupe_key_strips_angle_brackets_and_whitespace() {
        assert_eq!(
            dedupe_key(Some("  <abc123@example.com>  "), "src1"),
            "abc123@example.com"
        );
    }

    #[test]
    fn dedupe_key_preserves_case() {
        assert_eq!(
            dedupe_key(Some("<AbC@Example.com>"), "src1"),
            "AbC@Example.com"
        );
    }

    #[test]
    fn dedupe_key_falls_back_to_archive_id_sentinel_when_absent() {
        assert_eq!(dedupe_key(None, "src1"), "archive-id:src1");
    }

    #[test]
    fn dedupe_key_falls_back_when_msgid_is_blank() {
        assert_eq!(dedupe_key(Some("   "), "src1"), "archive-id:src1");
    }

    // ── load / save round-trip ───────────────────────────────────────

    #[test]
    fn load_absent_file_is_an_empty_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = InsertLedger::load(&dir.path().join("insert-ledger.jsonl")).unwrap();
        assert!(!ledger.contains("a@example.com", "k1"));
    }

    #[test]
    fn load_rejects_a_corrupt_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("insert-ledger.jsonl");
        std::fs::write(&path, "not json at all\n").unwrap();
        let err = InsertLedger::load(&path).unwrap_err();
        assert!(err.to_string().contains("Failed to parse insert ledger"));
    }

    #[test]
    fn save_then_load_round_trips_a_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("insert-ledger.jsonl");
        let mut ledger = InsertLedger::default();
        ledger.record(sample_record("a@example.com", "k1"));
        ledger.save(&path).unwrap();

        let loaded = InsertLedger::load(&path).unwrap();
        assert!(loaded.contains("a@example.com", "k1"));
    }

    #[test]
    fn save_is_atomic_and_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("insert-ledger.jsonl");
        let mut ledger = InsertLedger::default();
        ledger.record(sample_record("a@example.com", "k1"));
        ledger.save(&path).unwrap();

        let leftover: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.path() != path)
            .collect();
        assert!(leftover.is_empty(), "expected no leftover temp files");
    }

    // ── destination scoping (the identity guard) ────────────────────

    #[test]
    fn contains_is_scoped_to_the_destination_address() {
        let mut ledger = InsertLedger::default();
        ledger.record(sample_record("a@example.com", "k1"));
        assert!(ledger.contains("a@example.com", "k1"));
        // Same key, different destination: no hit. A ledger written while
        // restoring into account A must not suppress an insert into B.
        assert!(!ledger.contains("b@example.com", "k1"));
    }

    #[test]
    fn record_replaces_an_existing_entry_for_the_same_key() {
        let mut ledger = InsertLedger::default();
        ledger.record(sample_record("a@example.com", "k1"));
        let mut updated = sample_record("a@example.com", "k1");
        updated.inserted_id = Some("different-id".to_string());
        ledger.record(updated);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("insert-ledger.jsonl");
        ledger.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 1, "must not duplicate the entry");
        assert!(text.contains("different-id"));
    }

    // ── LedgerLock ────────────────────────────────────────────────────

    #[test]
    fn ledger_lock_acquire_then_drop_removes_the_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = ledger_lock_path(dir.path());
        {
            let _lock = LedgerLock::acquire(dir.path()).unwrap();
            assert!(lock_path.exists());
        }
        assert!(!lock_path.exists());
    }

    #[test]
    fn ledger_lock_acquire_fails_while_another_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let _lock = LedgerLock::acquire(dir.path()).unwrap();
        let err = LedgerLock::acquire(dir.path()).unwrap_err();
        assert!(err.to_string().contains("already be in progress"));
    }
}
