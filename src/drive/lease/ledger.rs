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
//! Unlike `InsertLedger`, a row is not dropped on an ordinary rewrite once
//! expired or released — `drive lease restore` locates a backup by token,
//! and a restore is almost always wanted *after* the expiry window, once a
//! bad write has been noticed. [`super::prune`] is the one caller that does
//! drop rows, deliberately: it is the ADR-0080 Consequences fast-follow
//! (#1678) that drops a row together with the backup it points at, never
//! one without the other.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

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
pub enum LeaseBackup {
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
    /// Set once `drive lease restore <TOKEN>` successfully restores from
    /// this row's backup (ADR-0080 §4/§10) — the "transition" §4 says a
    /// restored-from row is marked with, kept rather than dropped so a
    /// second restore attempt (or an auditor) can still see this backup was
    /// already used. Does not affect [`Self::is_live`]: a row can be
    /// restored from and still separately expire/be released on its own
    /// schedule, and restoring from an already-expired backup is the
    /// expected common case (§4), not something this field forbids.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) restored_at: Option<DateTime<Utc>>,
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

    /// Every row currently in the ledger, in token order. Read-only —
    /// [`super::prune`] uses this to decide what's eligible, never to
    /// mutate a row directly (a row is only ever dropped wholesale, via
    /// [`Self::remove`]).
    pub(crate) fn iter(&self) -> impl Iterator<Item = &LeaseRecord> {
        self.0.values()
    }

    /// Drops `token`'s row entirely — unlike every other mutator here, this
    /// removes state rather than updating it. The sole caller is
    /// [`super::prune`], and only ever after that row's backup has already
    /// been deleted/trashed: a ledger row and the backup it points at must
    /// never be dropped one without the other (ADR-0080 Consequences,
    /// #1678). Returns the removed row, if any, so a caller can account for
    /// what it freed.
    pub(crate) fn remove(&mut self, token: &str) -> Option<LeaseRecord> {
        self.0.remove(token)
    }

    /// The live (unexpired, unreleased) lease already covering `file_id`,
    /// if any.
    ///
    /// `drive lease acquire` refuses to mint a second, independent lease
    /// while one is already live: [`check_and_lock_lease`]'s staleness
    /// check compares the *caller's own* pre-lock version snapshot against
    /// *that token's own* ledger row, so two leases acquired concurrently
    /// on the same file would each capture the same version and each pass
    /// their own check against it — the second write would then silently
    /// overwrite the first's, exactly what the staleness check exists to
    /// prevent (issue #1664 review finding). Refusing a second live lease
    /// on the same file closes that gap: only one token can ever be
    /// checked against the file's true current state at a time.
    ///
    /// [`check_and_lock_lease`]: super::check::check_and_lock_lease
    pub(crate) fn live_lease_for_file(
        &self,
        file_id: &str,
        now: DateTime<Utc>,
    ) -> Option<&LeaseRecord> {
        self.0
            .values()
            .find(|record| record.file_id == file_id && record.is_live(now))
    }

    /// Marks `token`'s row as having been restored from (ADR-0080 §4/§10),
    /// stamped with `at`. A no-op if the token is absent. Idempotent: a
    /// second restore from the same backup just overwrites the timestamp,
    /// since the row already carries no count of how many times it has
    /// been used — `drive lease restore` is not rate-limited by this field.
    pub(crate) fn mark_restored(&mut self, token: &str, at: DateTime<Utc>) {
        if let Some(rec) = self.0.get_mut(token) {
            rec.restored_at = Some(at);
        }
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

    /// Loads the ledger at `path`, gives `f` mutable access, and saves the
    /// result back — the "load, mutate, save" step every ledger update in
    /// this module used to hand-roll independently (issue #1664 review
    /// finding: three near-identical copies of this sequence risked
    /// drifting from one another, ironically right after one of them —
    /// `drive lease restore`'s own `mark_backup_restored` — was fixed to
    /// close a lost-update race).
    ///
    /// Deliberately does **not** acquire [`LedgerLock`] itself: a leased
    /// write's own conclusion ([`super::check::finish_leased_write`])
    /// already holds the lock across its mutating Drive call and must not
    /// release it early by re-acquiring here — that caller uses this
    /// function directly. A caller that has not already taken the lock
    /// should use [`Self::mutate_locked`] instead.
    pub(crate) fn mutate<R>(path: &Path, f: impl FnOnce(&mut Self) -> R) -> Result<R> {
        let mut ledger = Self::load(path)?;
        let result = f(&mut ledger);
        ledger.save(path)?;
        Ok(result)
    }

    /// [`Self::mutate`], additionally acquiring [`LedgerLock`] first and
    /// holding it for the call's duration — the fully self-contained
    /// "acquire, load, mutate, save" sequence used by every ledger update
    /// that does not need to hold the lock across additional work beyond
    /// the mutation itself (e.g. `drive lease acquire`'s own
    /// check-then-insert, or `drive lease restore`'s best-effort
    /// mark-as-restored-from).
    pub(crate) fn mutate_locked<R>(path: &Path, f: impl FnOnce(&mut Self) -> R) -> Result<R> {
        let _lock = LedgerLock::acquire(path)?;
        Self::mutate(path, f)
    }

    /// Atomically rewrites `lease-ledger.jsonl` in full — the same
    /// temp-file-in-the-same-directory-then-rename pattern
    /// `InsertLedger::save`/`sync::manifest::Manifest::save` use.
    pub(crate) fn save(&self, path: &Path) -> Result<()> {
        crate::daemon::paths::ensure_parent_dir_0700(path)?;
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
    crate::request_log::omni_dev_state_subpath("lease-ledger.jsonl")
        .context("could not resolve the state/data directory for the lease ledger")
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

/// Env var overriding [`default_lock_wait_timeout`]. Value is whole
/// seconds; a missing, non-numeric, or non-positive value falls back to the
/// derived default.
const LEASE_LOCK_WAIT_ENV_VAR: &str = "OMNI_DEV_LEASE_LOCK_WAIT_SECS";

/// How long [`LedgerLock::acquire_waiting`] waits for a busy lock before
/// giving up, absent [`LEASE_LOCK_WAIT_ENV_VAR`].
///
/// Derived from [`crate::utils::http::read_timeout`] rather than a fixed
/// constant: a held lock can legitimately span several HTTP round trips
/// (`drive lease restore`'s `copyTo`, `edit_content`,
/// `rename_back_if_free` and a final `files.get`), and that timeout
/// resets on every successful read — a hardcoded wait budget here would
/// start producing spurious timeouts the moment the HTTP timeout is
/// tuned up. The ×4 headroom covers a restore's several sequential calls
/// against one read-timeout budget.
fn default_lock_wait_timeout() -> Duration {
    crate::utils::settings::get_env_var(LEASE_LOCK_WAIT_ENV_VAR)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&secs| secs > 0)
        .map_or_else(
            || crate::utils::http::read_timeout() * 4,
            Duration::from_secs,
        )
}

/// An advisory lock guarding against two overlapping `drive lease
/// acquire`/leased-write invocations against the same ledger:
/// [`LeaseLedger::save`] rewrites the whole file, so a second call's
/// rewrite racing the first would silently discard whichever lease state
/// lost the race.
///
/// Backed by [`crate::daemon::paths::FileLock`] (`flock(2)` on Unix):
/// kernel-released on process death, so — unlike the `create_new` marker
/// this replaced — a crashed or SIGKILLed holder never leaves a stale
/// lock, and `Drop` never unlinks the lock file. That matters: the old
/// Drop-unlinks-by-path shape let a second acquirer's "remove the lock
/// file and retry" (issued while the first holder was still live) create
/// a new marker that the first holder's own `Drop` would then delete by
/// path with no identity check, reopening a lease-token double-spend
/// (issue #1687). Nothing in this module ever tells an operator to delete
/// this file.
#[derive(Debug)]
pub(crate) struct LedgerLock {
    #[allow(dead_code)] // Held only for its Drop (releases the flock); never read.
    inner: crate::daemon::paths::FileLock,
}

impl LedgerLock {
    /// Acquires the lock guarding `ledger_path` without waiting. Production
    /// code passes [`ledger_path`]'s own result; tests pass a path under a
    /// `tempdir` so they never touch the real ledger or its lock. Used by
    /// the best-effort `mark_backup_restored` path and by `prune`'s
    /// candidate-selection scan, neither of which should block on another
    /// operation.
    pub(crate) fn acquire(ledger_path: &Path) -> Result<Self> {
        let path = lock_path_for(ledger_path);
        crate::daemon::paths::ensure_parent_dir_0700(&path)?;
        match crate::daemon::paths::try_lock_file_exclusive(&path) {
            Ok(inner) => Ok(Self { inner }),
            Err(crate::daemon::paths::FileLockError::Busy) => anyhow::bail!(
                "another `drive lease` operation appears to already be in progress ({} is \
                 locked) — concurrent access would clobber the ledger",
                path.display()
            ),
            Err(crate::daemon::paths::FileLockError::Io(err)) => Err(err.context(format!(
                "failed to lock the lease lock file at {}",
                path.display()
            ))),
        }
    }

    /// [`Self::acquire`], but waits for a busy lock instead of refusing
    /// immediately — used by every leased write via
    /// [`super::check::check_and_lock_lease`], where a concurrent write to
    /// an *unrelated* file should queue rather than hard-fail (issue
    /// #1687 point 1; the lock remains ledger-global, so this only
    /// changes hard-fail into wait, it does not add per-file scope).
    /// Polls with capped exponential backoff and emits a one-line notice
    /// on the first collision, so a human waiting on the CLI knows why
    /// nothing is happening yet.
    pub(crate) async fn acquire_waiting(ledger_path: &Path) -> Result<Self> {
        Self::acquire_waiting_with_timeout(ledger_path, default_lock_wait_timeout()).await
    }

    /// [`Self::acquire_waiting`] with an explicit wait budget — the seam
    /// that lets tests exercise the timeout in milliseconds without
    /// mutating the process environment.
    pub(crate) async fn acquire_waiting_with_timeout(
        ledger_path: &Path,
        max_wait: Duration,
    ) -> Result<Self> {
        let path = lock_path_for(ledger_path);
        crate::daemon::paths::ensure_parent_dir_0700(&path)?;

        let start = std::time::Instant::now();
        let mut delay = Duration::from_millis(50);
        let mut announced = false;
        loop {
            match crate::daemon::paths::try_lock_file_exclusive(&path) {
                Ok(inner) => return Ok(Self { inner }),
                Err(crate::daemon::paths::FileLockError::Busy) => {
                    if !announced {
                        tracing::info!(
                            "drive lease: waiting for another `drive lease` operation to \
                             finish ({} is locked)",
                            path.display()
                        );
                        announced = true;
                    }
                    if start.elapsed() >= max_wait {
                        anyhow::bail!(
                            "timed out after {max_wait:?} waiting for another `drive lease` \
                             operation to finish ({} is locked) — concurrent access would \
                             clobber the ledger",
                            path.display()
                        );
                    }
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_millis(500));
                }
                Err(crate::daemon::paths::FileLockError::Io(err)) => {
                    return Err(err.context(format!(
                        "failed to lock the lease lock file at {}",
                        path.display()
                    )))
                }
            }
        }
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
            restored_at: None,
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
    fn iter_yields_every_row() {
        let mut ledger = LeaseLedger::default();
        ledger.insert(sample_record("t1"));
        ledger.insert(sample_record("t2"));
        let tokens: Vec<&str> = ledger.iter().map(|r| r.token.as_str()).collect();
        assert_eq!(tokens.len(), 2);
        assert!(tokens.contains(&"t1"));
        assert!(tokens.contains(&"t2"));
    }

    #[test]
    fn remove_drops_a_present_row_and_returns_it() {
        let mut ledger = LeaseLedger::default();
        ledger.insert(sample_record("t1"));
        let removed = ledger.remove("t1").unwrap();
        assert_eq!(removed.token, "t1");
        assert!(ledger.get("t1").is_none());
    }

    #[test]
    fn remove_on_an_absent_token_is_a_noop() {
        let mut ledger = LeaseLedger::default();
        assert!(ledger.remove("missing").is_none());
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

    #[cfg(unix)]
    #[test]
    fn ledger_lock_acquire_reports_a_non_collision_failure_distinctly() {
        use std::os::unix::fs::PermissionsExt;

        // A permission failure only arises from the lock file's *first*
        // creation (a persistent lock file needs dir-write only then) —
        // exercised here against a lock path that does not yet exist, in a
        // directory with no write permission, so `open(..., O_CREAT)`
        // itself fails rather than the `flock` call. That makes it
        // structurally distinct from `FileLockError::Busy` (which only
        // ever comes from a contended `flock`), not merely a different
        // message.
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();

        let err = LedgerLock::acquire(&ledger_path).unwrap_err();

        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(err.to_string().contains("failed to lock"), "{err:?}");
        assert!(
            !err.to_string().contains("already be in progress"),
            "{err:?}"
        );
    }

    #[test]
    fn mutate_loads_applies_and_saves() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lease-ledger.jsonl");
        let mut ledger = LeaseLedger::default();
        ledger.insert(sample_record("t1"));
        ledger.save(&path).unwrap();

        let returned = LeaseLedger::mutate(&path, |ledger| {
            ledger.mark_restored("t1", Utc::now());
            "ok"
        })
        .unwrap();
        assert_eq!(returned, "ok");

        let reloaded = LeaseLedger::load(&path).unwrap();
        assert!(reloaded.get("t1").unwrap().restored_at.is_some());
    }

    #[test]
    fn mutate_locked_acquires_and_releases_its_own_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lease-ledger.jsonl");

        LeaseLedger::mutate_locked(&path, |ledger| {
            ledger.insert(sample_record("t1"));
        })
        .unwrap();

        // The lock must not outlive the call.
        assert!(LedgerLock::acquire(&path).is_ok());
        let reloaded = LeaseLedger::load(&path).unwrap();
        assert!(reloaded.get("t1").is_some());
    }

    #[test]
    fn mutate_locked_refuses_while_another_lock_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lease-ledger.jsonl");
        let _held = LedgerLock::acquire(&path).unwrap();

        // omni-dev: coverage ignore reason="mutate_locked refuses before ever calling the closure, so its body never runs — a hit here is a regression, not a coverage gap"
        let err = LeaseLedger::mutate_locked(&path, |ledger| {
            ledger.insert(sample_record("t1"));
        })
        // omni-dev: coverage end
        .unwrap_err();
        assert!(err.to_string().contains("already be in progress"));
    }

    #[test]
    fn ledger_lock_file_persists_after_drop_and_is_re_lockable() {
        // Unlike the old `create_new` marker, `Drop` must not unlink the
        // lock file — a holder unlinking-by-path is exactly what let a
        // second acquirer's marker be deleted out from under it (issue
        // #1687 point 2). The file staying put is what makes that
        // impossible; what actually releases the lock is the flock itself.
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        let lock_path = dir.path().join("lease-ledger.jsonl.lock");

        let lock = LedgerLock::acquire(&ledger_path).unwrap();
        assert!(lock_path.exists());
        drop(lock);
        assert!(
            lock_path.exists(),
            "the lock file must survive its own drop"
        );

        // And immediately re-lockable — no stale-lock error, no leftover
        // exclusion.
        LedgerLock::acquire(&ledger_path).unwrap();
    }

    #[test]
    fn ledger_lock_is_released_by_plain_fd_close_with_no_explicit_unlock() {
        // The direct regression test for issue #1687 point 3: a SIGKILLed
        // holder never runs `Flock`'s own `Drop` (which issues an explicit
        // `LOCK_UN`) — the kernel just closes its file descriptors. Locking
        // via the deprecated free `flock()` function (rather than the
        // `Flock<T>` RAII wrapper) and then dropping the plain `File`
        // reproduces exactly that: the fd closes via ordinary `File::drop`,
        // with no explicit unlock call ever issued, and the lock must still
        // be gone afterwards.
        use std::os::fd::AsRawFd;

        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        let lock_path = dir.path().join("lease-ledger.jsonl.lock");

        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        #[allow(deprecated)]
        nix::fcntl::flock(file.as_raw_fd(), nix::fcntl::FlockArg::LockExclusive).unwrap();
        drop(file);

        LedgerLock::acquire(&ledger_path).unwrap();
    }

    #[tokio::test]
    async fn ledger_lock_acquire_waiting_waits_for_a_concurrent_holder_then_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        let held = LedgerLock::acquire(&ledger_path).unwrap();

        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(held);
        });

        LedgerLock::acquire_waiting_with_timeout(&ledger_path, Duration::from_secs(5))
            .await
            .unwrap();
        releaser.join().unwrap();
    }

    #[tokio::test]
    async fn ledger_lock_acquire_waiting_times_out_when_never_released() {
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        let _held = LedgerLock::acquire(&ledger_path).unwrap();

        let err =
            LedgerLock::acquire_waiting_with_timeout(&ledger_path, Duration::from_millis(150))
                .await
                .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err:?}");
    }
}
