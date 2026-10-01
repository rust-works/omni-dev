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
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// Delay before retrying an id after its first failed fetch (#1790); doubles
/// with each further consecutive failure up to [`BACKOFF_CAP`].
const BACKOFF_BASE: Duration = Duration::minutes(5);

/// Longest an id is ever deferred. Also the bound on how far in the future a
/// stored `next_retry_at` is trusted, so a clock that moved backwards cannot
/// park an id indefinitely (see [`PendingFetch::is_due`]).
const BACKOFF_CAP: Duration = Duration::hours(24);

/// Largest doubling exponent applied to [`BACKOFF_BASE`] — well past the point
/// where the result already exceeds [`BACKOFF_CAP`], so the shift cannot
/// overflow for any failure count.
const BACKOFF_MAX_DOUBLINGS: u32 = 20;

/// Longest `last_error` kept in `state.json`, in characters: enough to
/// recognise the failure in a warning line without letting an API error body
/// bloat a file rewritten every run.
const LAST_ERROR_MAX_CHARS: usize = 200;

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
    /// Message ids whose fetch failed, to be retried by a later run (#1784),
    /// each with its own failure count and retry time (#1790). Carrying them
    /// here is what lets `history_id` advance on every run: the failed ids no
    /// longer need to be rediscovered by replaying a history window that
    /// would otherwise age out of Gmail's ~1-week retention. Still
    /// disposable — losing this file falls back to a full listing, which
    /// re-discovers them anyway.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) pending_fetch: Vec<PendingFetch>,
}

/// One message id awaiting a retried fetch, with the bookkeeping that spaces
/// the retries out (#1790).
///
/// Deserialises from either this object or the bare id string #1788 wrote, so
/// an existing `state.json` keeps loading; a bare id counts as one failure
/// with no `next_retry_at`, i.e. due immediately — exactly the pre-backoff
/// behaviour. Always serialises as the object.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(from = "PendingFetchRepr")]
pub(crate) struct PendingFetch {
    pub(crate) id: String,
    /// Consecutive failed fetch attempts.
    pub(crate) failures: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) first_failed_at: Option<DateTime<Utc>>,
    /// Not retried before this instant; `None` means due now.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) next_retry_at: Option<DateTime<Utc>>,
    /// The most recent failure's reason, truncated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_error: Option<String>,
}

/// The two on-disk shapes of a [`PendingFetch`]: the object, and the bare id
/// string written before #1790.
#[derive(Deserialize)]
#[serde(untagged)]
enum PendingFetchRepr {
    Id(String),
    Entry {
        id: String,
        #[serde(default)]
        failures: u32,
        #[serde(default)]
        first_failed_at: Option<DateTime<Utc>>,
        #[serde(default)]
        next_retry_at: Option<DateTime<Utc>>,
        #[serde(default)]
        last_error: Option<String>,
    },
}

impl From<PendingFetchRepr> for PendingFetch {
    fn from(repr: PendingFetchRepr) -> Self {
        match repr {
            PendingFetchRepr::Id(id) => Self {
                id,
                failures: 1,
                first_failed_at: None,
                next_retry_at: None,
                last_error: None,
            },
            PendingFetchRepr::Entry {
                id,
                failures,
                first_failed_at,
                next_retry_at,
                last_error,
            } => Self {
                id,
                // An entry that exists has failed at least once, whatever a
                // hand-edited or partial object says.
                failures: failures.max(1),
                first_failed_at,
                next_retry_at,
                last_error,
            },
        }
    }
}

impl PendingFetch {
    /// The entry for `id` after a fetch attempt failed at `now` with
    /// `reason`, continuing `previous`'s failure streak if there was one.
    pub(crate) fn failed(
        previous: Option<&Self>,
        id: &str,
        reason: &str,
        now: DateTime<Utc>,
    ) -> Self {
        let failures = previous.map_or(1, |p| p.failures.saturating_add(1));
        Self {
            id: id.to_string(),
            failures,
            first_failed_at: previous.and_then(|p| p.first_failed_at).or(Some(now)),
            next_retry_at: Some(now + backoff_delay(failures)),
            last_error: Some(reason.chars().take(LAST_ERROR_MAX_CHARS).collect()),
        }
    }

    /// Whether a fetch of this id may be attempted at `now`. A `next_retry_at`
    /// more than [`BACKOFF_CAP`] ahead can only come from a clock that has
    /// since moved backwards, so it is treated as due rather than trusted.
    pub(crate) fn is_due(&self, now: DateTime<Utc>) -> bool {
        self.next_retry_at
            .is_none_or(|at| at <= now || at - now > BACKOFF_CAP)
    }
}

/// How long to defer an id after its `failures`-th consecutive failure:
/// `5 min * 2^(failures - 1)`, capped at 24 h.
pub(crate) fn backoff_delay(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(BACKOFF_MAX_DOUBLINGS);
    (BACKOFF_BASE * 2_i32.pow(doublings)).min(BACKOFF_CAP)
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
            pending_fetch: Vec::new(),
        }
    }

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    /// What a bare id string from #1788's `state.json` loads as.
    fn legacy(id: &str) -> PendingFetch {
        PendingFetch {
            id: id.to_string(),
            failures: 1,
            first_failed_at: None,
            next_retry_at: None,
            last_error: None,
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

    #[test]
    fn load_present_with_pending_fetch_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let state = ArchiveState {
            pending_fetch: vec![
                PendingFetch::failed(None, "m1", "boom", at("2026-01-01T00:00:00Z")),
                legacy("m2"),
            ],
            ..sample_state()
        };
        save(&state, &path).unwrap();
        assert!(matches!(load(&path), LoadOutcome::Present(loaded) if loaded == state));
    }

    #[test]
    fn load_reads_the_bare_id_strings_written_before_backoff_existed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(
            &path,
            r#"{"history_id":"1000","email_address":"user@example.com","last_sync":"2026-01-01T00:00:00Z","pending_fetch":["m1","m2"]}"#,
        )
        .unwrap();
        assert!(matches!(
            load(&path),
            LoadOutcome::Present(loaded)
                if loaded.pending_fetch == [legacy("m1"), legacy("m2")]
                    // A legacy id has no `next_retry_at`, so it is retried at once.
                    && loaded.pending_fetch[0].is_due(at("2026-01-01T00:00:00Z"))
        ));
    }

    #[test]
    fn load_reads_a_pending_entry_with_only_an_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(
            &path,
            r#"{"history_id":"1000","email_address":"user@example.com","last_sync":"2026-01-01T00:00:00Z","pending_fetch":[{"id":"m1"}]}"#,
        )
        .unwrap();
        match load(&path) {
            LoadOutcome::Present(loaded) => {
                assert_eq!(loaded.pending_fetch[0].id, "m1");
                assert_eq!(loaded.pending_fetch[0].failures, 1);
                assert!(loaded.pending_fetch[0].is_due(at("2026-01-01T00:00:00Z")));
            }
            _ => panic!("expected Present"),
        }
    }

    #[test]
    fn load_defaults_pending_fetch_for_a_state_written_before_it_existed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(
            &path,
            r#"{"history_id":"1000","email_address":"user@example.com","last_sync":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        match load(&path) {
            LoadOutcome::Present(loaded) => assert!(loaded.pending_fetch.is_empty()),
            _ => panic!("expected Present"),
        }
    }

    #[test]
    fn save_omits_an_empty_pending_fetch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        save(&sample_state(), &path).unwrap();
        assert!(!std::fs::read_to_string(&path)
            .unwrap()
            .contains("pending_fetch"));
    }

    // ── backoff (#1790) ──────────────────────────────────────────────

    #[test]
    fn backoff_delay_doubles_from_five_minutes() {
        assert_eq!(backoff_delay(1), Duration::minutes(5));
        assert_eq!(backoff_delay(2), Duration::minutes(10));
        assert_eq!(backoff_delay(3), Duration::minutes(20));
        assert_eq!(backoff_delay(4), Duration::minutes(40));
    }

    #[test]
    fn backoff_delay_is_capped_at_a_day_for_any_failure_count() {
        assert_eq!(backoff_delay(10), Duration::hours(24));
        assert_eq!(backoff_delay(1_000), Duration::hours(24));
        assert_eq!(backoff_delay(u32::MAX), Duration::hours(24));
    }

    #[test]
    fn backoff_delay_treats_zero_failures_like_one() {
        assert_eq!(backoff_delay(0), Duration::minutes(5));
    }

    #[test]
    fn failed_starts_a_streak_and_schedules_the_first_retry() {
        let now = at("2026-01-01T00:00:00Z");
        let entry = PendingFetch::failed(None, "m1", "rate limited", now);
        assert_eq!(entry.id, "m1");
        assert_eq!(entry.failures, 1);
        assert_eq!(entry.first_failed_at, Some(now));
        assert_eq!(entry.next_retry_at, Some(at("2026-01-01T00:05:00Z")));
        assert_eq!(entry.last_error.as_deref(), Some("rate limited"));
    }

    #[test]
    fn failed_continues_a_streak_and_keeps_the_first_failure_time() {
        let first = at("2026-01-01T00:00:00Z");
        let later = at("2026-01-01T01:00:00Z");
        let previous = PendingFetch::failed(None, "m1", "old", first);
        let entry = PendingFetch::failed(Some(&previous), "m1", "new", later);
        assert_eq!(entry.failures, 2);
        assert_eq!(entry.first_failed_at, Some(first));
        assert_eq!(entry.next_retry_at, Some(at("2026-01-01T01:10:00Z")));
        assert_eq!(entry.last_error.as_deref(), Some("new"));
    }

    #[test]
    fn failed_continues_a_legacy_entry_that_has_no_first_failure_time() {
        let now = at("2026-01-01T00:00:00Z");
        let entry = PendingFetch::failed(Some(&legacy("m1")), "m1", "boom", now);
        assert_eq!(entry.failures, 2);
        assert_eq!(entry.first_failed_at, Some(now));
    }

    #[test]
    fn failed_truncates_a_long_reason() {
        let now = at("2026-01-01T00:00:00Z");
        let entry = PendingFetch::failed(None, "m1", &"x".repeat(5_000), now);
        assert_eq!(entry.last_error.unwrap().chars().count(), 200);
    }

    #[test]
    fn is_due_without_a_retry_time() {
        assert!(legacy("m1").is_due(at("2026-01-01T00:00:00Z")));
    }

    #[test]
    fn is_due_only_once_the_retry_time_has_passed() {
        let entry = PendingFetch::failed(None, "m1", "boom", at("2026-01-01T00:00:00Z"));
        assert!(!entry.is_due(at("2026-01-01T00:04:59Z")));
        assert!(entry.is_due(at("2026-01-01T00:05:00Z")));
        assert!(entry.is_due(at("2026-01-02T00:00:00Z")));
    }

    #[test]
    fn is_due_when_the_retry_time_is_implausibly_far_ahead() {
        // A clock that jumped backwards would otherwise park the id for
        // however long the jump was.
        let entry = PendingFetch {
            next_retry_at: Some(at("2030-01-01T00:00:00Z")),
            ..legacy("m1")
        };
        assert!(entry.is_due(at("2026-01-01T00:00:00Z")));
        let within_cap = PendingFetch {
            next_retry_at: Some(at("2026-01-01T23:00:00Z")),
            ..legacy("m1")
        };
        assert!(!within_cap.is_due(at("2026-01-01T00:00:00Z")));
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
