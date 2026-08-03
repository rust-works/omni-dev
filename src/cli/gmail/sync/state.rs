//! `state.json`: the `historyId` watermark and account identity.
//!
//! Deliberately disposable — presence-on-disk (the archived `.eml`s and
//! `manifest.jsonl`) is the real idempotence mechanism (#1467); the
//! watermark here is a pure optimisation over re-listing the whole mailbox.
//! Missing or corrupt state both fall back to full reconciliation rather
//! than erroring, mirroring `manifest.rs`'s opposite, deliberate asymmetry:
//! label/message metadata is *not* similarly disposable.

use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The watermark and account identity persisted between sync runs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ArchiveState {
    pub(crate) history_id: String,
    pub(crate) email_address: String,
    pub(crate) last_sync: DateTime<Utc>,
    /// The `--query` used for the most recent full sync/reconciliation, if
    /// any (`None` = whole mailbox). Informational only today — see
    /// `docs/gmail.md`'s Sync section for the known `--query` +
    /// incremental-sync scope limitation this does not solve.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) query: Option<String>,
}

/// The result of attempting to load `state.json`.
pub(crate) enum LoadOutcome {
    /// No `state.json` — first run.
    Absent,
    /// `state.json` exists but could not be parsed. Treated the same as
    /// [`Self::Absent`] by callers (full reconciliation), never as a hard
    /// error — this file is designed to be fully disposable.
    Corrupt(String),
    /// A successfully parsed prior state.
    Present(ArchiveState),
}

/// Loads `state.json`, never failing: an absent or corrupt file is a
/// [`LoadOutcome`] variant for the caller to act on, not an `Err`.
pub(crate) fn load(path: &Path) -> LoadOutcome {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return LoadOutcome::Absent,
        Err(e) => return LoadOutcome::Corrupt(e.to_string()),
    };
    match serde_json::from_str::<ArchiveState>(&text) {
        Ok(state) => LoadOutcome::Present(state),
        Err(e) => LoadOutcome::Corrupt(e.to_string()),
    }
}

/// Rejects a state whose `email_address` doesn't match the currently
/// authenticated account.
///
/// This is the one case that must be a loud, immediate failure rather than
/// a silent fallback — mixing two mailboxes' history into one archive is
/// exactly the bug class this guards against (the Facebook harvester writes
/// `user_id` into its own resume state but never compares it on reload).
pub(crate) fn validate_identity(state: &ArchiveState, authenticated_email: &str) -> Result<()> {
    if state.email_address != authenticated_email {
        anyhow::bail!(
            "state.json belongs to {} but the authenticated account is {authenticated_email}; \
             refusing to mix two mailboxes into one archive. Point --output-dir at a fresh \
             directory, or re-run against the correct account.",
            state.email_address
        );
    }
    Ok(())
}

/// Atomically writes `state.json` (temp file + rename) — a single,
/// infrequently-written, single-writer file, so the simple sibling-`.tmp`
/// approach suffices (contrast `manifest.rs`'s `tempfile`-crate version,
/// chosen there for its cleanup-on-drop-if-not-persisted behaviour).
pub(crate) fn save(state: &ArchiveState, path: &Path) -> Result<()> {
    let json = serde_json::to_string_pretty(state).context("Failed to serialise sync state")?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, json)
        .with_context(|| format!("Failed to write sync state to {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("Failed to finalise sync state at {}", path.display()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn sample_state() -> ArchiveState {
        ArchiveState {
            history_id: "1000".to_string(),
            email_address: "user@example.com".to_string(),
            last_sync: DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            query: None,
        }
    }

    // ── load ─────────────────────────────────────────────────────────

    #[test]
    fn load_absent_when_file_does_not_exist() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            load(&dir.path().join("state.json")),
            LoadOutcome::Absent
        ));
    }

    #[test]
    fn load_present_round_trips_a_saved_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let state = sample_state();
        save(&state, &path).unwrap();

        match load(&path) {
            LoadOutcome::Present(loaded) => assert_eq!(loaded, state),
            _ => panic!("expected Present"),
        }
    }

    #[test]
    fn load_corrupt_on_malformed_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(matches!(load(&path), LoadOutcome::Corrupt(_)));
    }

    #[test]
    fn load_present_with_query_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let state = ArchiveState {
            query: Some("label:finance".to_string()),
            ..sample_state()
        };
        save(&state, &path).unwrap();
        match load(&path) {
            LoadOutcome::Present(loaded) => {
                assert_eq!(loaded.query.as_deref(), Some("label:finance"));
            }
            _ => panic!("expected Present"),
        }
    }

    // ── validate_identity ──────────────────────────────────────────────

    #[test]
    fn validate_identity_accepts_matching_account() {
        let state = sample_state();
        assert!(validate_identity(&state, "user@example.com").is_ok());
    }

    #[test]
    fn validate_identity_rejects_mismatched_account() {
        let state = sample_state();
        let err = validate_identity(&state, "other@example.com").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("user@example.com"));
        assert!(msg.contains("other@example.com"));
    }

    // ── save ─────────────────────────────────────────────────────────

    #[test]
    fn save_is_atomic_and_leaves_no_tmp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        save(&sample_state(), &path).unwrap();
        assert!(path.exists());
        assert!(!path.with_extension("tmp").exists());
    }

    #[test]
    fn save_overwrites_a_previous_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        save(&sample_state(), &path).unwrap();

        let updated = ArchiveState {
            history_id: "2000".to_string(),
            ..sample_state()
        };
        save(&updated, &path).unwrap();

        match load(&path) {
            LoadOutcome::Present(loaded) => assert_eq!(loaded.history_id, "2000"),
            _ => panic!("expected Present"),
        }
    }
}
