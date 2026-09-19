//! `drive lease release <TOKEN>` — ends a lease's write window early
//! (issue #1685), the counterpart to the absolute expiry
//! [ADR-0080](../../../docs/adrs/adr-0080.md) §5 fixes at acquisition.
//!
//! **No authentication, and no Drive call at all.** Every other lease verb
//! either prompts for device-owner consent or touches Drive; this one only
//! ever *reduces* what a token can do, so gating it behind Touch ID would
//! spend a prompt to give up authority — and leave an operator who wants to
//! stand down a lease on a headless box with no way to do it. It is a pure
//! ledger mutation, which is also why it is the one `drive lease` subcommand
//! that takes no [`DriveClient`](crate::drive::client::DriveClient).
//!
//! **The row is kept, and stays restorable.** Release ends the lease's
//! authority to write, not its backup: `drive lease restore <TOKEN>` looks a
//! row up by token and never requires it to be live, so a released lease's
//! backup remains recoverable until [`super::prune`] drops the row and the
//! backup together.
//!
//! The immediate motivation is `drive lease restore`'s
//! `FreshLeaseButWriteFailed` state: the fresh lease that restore minted is
//! live and covers the file, so re-running the restore is refused until it is
//! either released or expires. Superseding (see
//! [`AcquireOptions::supersedes`](super::acquire::AcquireOptions::supersedes))
//! handles the backup token's own row automatically; this verb is what
//! handles every other lease an operator wants to stand down.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::cli::drive::format::JsonlSerialize;
use crate::drive::lease::ledger::{LeaseLedger, LedgerLock, ReleaseOutcome};

/// Per-call options for `drive lease release`.
#[derive(Debug, Clone)]
pub struct ReleaseOptions {
    /// The lease token to release.
    pub token: String,
    /// Path to the lease ledger. Production callers pass
    /// [`crate::drive::lease::ledger::ledger_path`]'s own result; tests pass
    /// a path under a `tempdir` so a test run never touches the real ledger.
    pub ledger_path: PathBuf,
}

/// What happened.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum ReleaseResult {
    /// The lease was live and its window is now closed. Its backup is
    /// untouched and still restorable.
    Released {
        /// The released token.
        token: String,
        /// The file it covered.
        file_id: String,
        /// When it would otherwise have expired on its own.
        expires_at: DateTime<Utc>,
    },
    /// The token names a real row that was already expired or already
    /// released — left exactly as it was, so an earlier release's timestamp
    /// is never overwritten. Reported rather than silently treated as a
    /// success: "already dead" and "just closed by you" are different
    /// answers to "is anything still holding this file?".
    NotLive {
        /// The token looked up.
        token: String,
        /// The file it covers.
        file_id: String,
        /// Its expiry, whether or not that is what ended it.
        expires_at: DateTime<Utc>,
        /// When an earlier release ended it, if that is what did.
        #[serde(skip_serializing_if = "Option::is_none")]
        released_at: Option<DateTime<Utc>>,
    },
    /// `<TOKEN>` names no row this ledger has ever recorded.
    NoSuchToken,
    /// The ledger could not be read, locked, or written.
    Failed {
        /// A human-readable summary of what failed.
        detail: String,
    },
}

impl JsonlSerialize for ReleaseResult {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> Result<(), anyhow::Error> {
        crate::cli::drive::format::write_scalar_jsonl(self, out)
    }
}

impl ReleaseResult {
    /// The kebab-case status for the audit record (ADR-0080 §11's free-form
    /// `verdict` vocabulary), matching this enum's own `#[serde(tag =
    /// "status")]` shape.
    fn verdict(&self) -> &'static str {
        match self {
            Self::Released { .. } => "released",
            Self::NotLive { .. } => "release-not-live",
            Self::NoSuchToken => "release-no-such-token",
            Self::Failed { .. } => "failed",
        }
    }

    /// The CLI's exit code for this outcome (issue #1775): `0` for
    /// `Released` and, since it's an idempotent no-op rather than a
    /// failure, `NotLive` too — the caller's goal, "this lease is not
    /// live", already holds. `NoSuchToken`/`Failed` are `1`.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Released { .. } | Self::NotLive { .. } => 0,
            Self::NoSuchToken | Self::Failed { .. } => 1,
        }
    }
}

/// Releases `opts.token`, then records the attempt to the audit sink
/// regardless of outcome.
///
/// Best-effort logging, for the same reason [`super::acquire::acquire`]'s own
/// record is rather than write-ahead/fail-closed: this verb mutates no Drive
/// content, so there is no mutating call for a write-ahead record to precede,
/// and the ledger write is already durable by the time this returns. Turning
/// a genuine release into a reported failure because a follow-up log write
/// failed would make the tool's own output less trustworthy, not more.
pub async fn release(opts: &ReleaseOptions) -> ReleaseResult {
    let result = release_inner(opts).await;
    record_attempt(opts, &result);
    result
}

/// Waits for a busy ledger lock rather than failing (issue #1738): a
/// release mutates no Drive content and is always safe to queue, and it is
/// the verb a failed restore tells the user to run next, so a hard-fail
/// there leaves them stuck. Async so it can share the leased-write gate's
/// [`LedgerLock::acquire_waiting`], which runs on any runtime flavor.
async fn release_inner(opts: &ReleaseOptions) -> ReleaseResult {
    let outcome = async {
        let lock = LedgerLock::acquire_waiting(&opts.ledger_path).await?;
        LeaseLedger::mutate(&lock, &opts.ledger_path, |ledger| {
            ledger.release(&opts.token, Utc::now(), None)
        })
    }
    .await;
    match outcome {
        Ok(ReleaseOutcome::Released {
            file_id,
            expires_at,
        }) => ReleaseResult::Released {
            token: opts.token.clone(),
            file_id,
            expires_at,
        },
        Ok(ReleaseOutcome::NotLive {
            file_id,
            expires_at,
            released_at,
        }) => ReleaseResult::NotLive {
            token: opts.token.clone(),
            file_id,
            expires_at,
            released_at,
        },
        Ok(ReleaseOutcome::NotFound) => ReleaseResult::NoSuchToken,
        Err(err) => ReleaseResult::Failed {
            detail: err.to_string(),
        },
    }
}

/// Builds and writes the `kind: "audit"` record for one release attempt.
///
/// `file_id` is carried whenever a row was found, released or not, so a
/// `--query 'file_id:<id>'` over the audit log sees a presented-but-dead
/// token too; only an unknown token has no file to name, and then the token
/// itself is all an auditor has to go on.
///
/// A `Failed` release (typically a lock wait that timed out) never read the
/// ledger under the lock, so its file is looked up here by a lock-free read
/// instead (issue #1738) — safe because [`LeaseLedger::save`] replaces the
/// file atomically, and a guess is all it is: an unreadable ledger or a
/// token it doesn't hold leaves `file_id` empty, as before.
fn record_attempt(opts: &ReleaseOptions, result: &ReleaseResult) {
    let (file_id, error) = match result {
        ReleaseResult::Released { file_id, .. } | ReleaseResult::NotLive { file_id, .. } => {
            (file_id.clone(), None)
        }
        ReleaseResult::NoSuchToken => (String::new(), None),
        ReleaseResult::Failed { detail } => (
            file_id_for_token(&opts.ledger_path, &opts.token).unwrap_or_default(),
            Some(detail.clone()),
        ),
    };
    let outcome = crate::request_log::AuditOutcome {
        command: vec!["drive".to_string(), "lease-release".to_string()],
        integration: "drive",
        file_id,
        // Named even on a refusal: "this token was presented to release and
        // did not resolve to a live lease" is itself worth being able to
        // grep for by token.
        lease_id: Some(opts.token.clone()),
        verdict: result.verdict().to_string(),
        error,
        ..Default::default()
    };
    if let Err(err) = crate::request_log::record_audit_event(outcome) {
        tracing::warn!("drive lease release: failed to write audit record: {err}");
    }
}

/// The file `token`'s row covers, read without the ledger lock — see
/// [`record_attempt`].
fn file_id_for_token(ledger_path: &std::path::Path, token: &str) -> Option<String> {
    LeaseLedger::load(ledger_path)
        .ok()?
        .get(token)
        .map(|record| record.file_id.clone())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::lease::ledger::{LeaseBackup, LeaseRecord};
    use crate::test_support::AuditLogGuard;
    use chrono::Duration as ChronoDuration;
    use std::path::Path;

    fn backup() -> LeaseBackup {
        LeaseBackup::Bytes {
            path: std::path::PathBuf::from("/tmp/backup.bin"),
            sha256: "deadbeef".to_string(),
            size: 4,
        }
    }

    /// Seeds one row whose window ends at `expires_at`, already released at
    /// `released_at`.
    fn seed(
        ledger_path: &Path,
        token: &str,
        expires_at: DateTime<Utc>,
        released_at: Option<DateTime<Utc>>,
    ) {
        let mut ledger = LeaseLedger::load(ledger_path).unwrap_or_default();
        ledger.insert(LeaseRecord {
            token: token.to_string(),
            file_id: "file-1".to_string(),
            version: "1".to_string(),
            modified_time: None,
            backup: backup(),
            acquired_at: Utc::now() - ChronoDuration::minutes(5),
            expires_at,
            released_at,
            superseded_by: None,
            restored_at: None,
            restored_sheet_id: None,
        });
        ledger.save(ledger_path).unwrap();
    }

    fn opts(dir: &Path, token: &str) -> ReleaseOptions {
        ReleaseOptions {
            token: token.to_string(),
            ledger_path: dir.join("lease-ledger.jsonl"),
        }
    }

    #[tokio::test]
    async fn a_live_lease_is_released_and_its_row_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditLogGuard::redirect(dir.path());
        let test_opts = opts(dir.path(), "live-token");
        let expires_at = Utc::now() + ChronoDuration::minutes(30);
        seed(&test_opts.ledger_path, "live-token", expires_at, None);

        let result = release(&test_opts).await;

        assert!(matches!(
            &result,
            ReleaseResult::Released { token, file_id, .. }
                if token == "live-token" && file_id == "file-1"
        ));
        // The row survives, so `drive lease restore` can still find the
        // backup — release ends the lease's authority, not its usefulness.
        let ledger = LeaseLedger::load(&test_opts.ledger_path).unwrap();
        let record = ledger.get("live-token").unwrap();
        assert!(record.released_at.is_some());
        assert!(!record.is_live(Utc::now()));
    }

    #[tokio::test]
    async fn an_already_released_lease_keeps_its_original_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditLogGuard::redirect(dir.path());
        let test_opts = opts(dir.path(), "released-token");
        let first_release = Utc::now() - ChronoDuration::minutes(1);
        seed(
            &test_opts.ledger_path,
            "released-token",
            Utc::now() + ChronoDuration::minutes(30),
            Some(first_release),
        );

        let result = release(&test_opts).await;

        assert!(matches!(
            &result,
            ReleaseResult::NotLive { released_at: Some(at), .. } if *at == first_release
        ));
        let ledger = LeaseLedger::load(&test_opts.ledger_path).unwrap();
        assert_eq!(
            ledger.get("released-token").unwrap().released_at,
            Some(first_release),
            "re-releasing must not overwrite the original release's timestamp"
        );
    }

    #[tokio::test]
    async fn an_expired_lease_is_reported_as_not_live_without_being_stamped() {
        let dir = tempfile::tempdir().unwrap();
        let audit = AuditLogGuard::redirect(dir.path());
        let test_opts = opts(dir.path(), "expired-token");
        seed(
            &test_opts.ledger_path,
            "expired-token",
            Utc::now() - ChronoDuration::minutes(1),
            None,
        );

        let result = release(&test_opts).await;

        assert!(matches!(
            &result,
            ReleaseResult::NotLive {
                released_at: None,
                ..
            }
        ));
        let ledger = LeaseLedger::load(&test_opts.ledger_path).unwrap();
        assert!(
            ledger.get("expired-token").unwrap().released_at.is_none(),
            "expiry already ended this lease; release must not claim credit for it"
        );
        // The row was found, so its file is named even though nothing
        // changed — a `file_id:` query over the audit log must see this
        // attempt too.
        let record = audit.records().pop().unwrap();
        assert_eq!(
            record.context.get("verdict").map(String::as_str),
            Some("release-not-live")
        );
        assert_eq!(
            record.context.get("file_id").map(String::as_str),
            Some("file-1")
        );
    }

    #[tokio::test]
    async fn an_unknown_token_is_reported_rather_than_silently_succeeding() {
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditLogGuard::redirect(dir.path());

        let result = release(&opts(dir.path(), "no-such-token")).await;

        assert!(matches!(result, ReleaseResult::NoSuchToken));
    }

    #[tokio::test]
    async fn an_unreadable_ledger_is_reported_as_failed() {
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditLogGuard::redirect(dir.path());
        let test_opts = opts(dir.path(), "any-token");
        std::fs::write(&test_opts.ledger_path, "{not json}\n").unwrap();

        let result = release(&test_opts).await;

        assert!(matches!(result, ReleaseResult::Failed { .. }));
    }

    // Current-thread on purpose: `release` is dispatched from one in
    // `cli::drive`'s tests, so it has to work on one. Note this no longer
    // *pins* that `release` keeps off `block_in_place` — since issue #1697
    // the lease module offloads through `ledger::offload_short_blocking_io`,
    // which degrades to a plain call here instead of panicking.
    #[tokio::test]
    async fn a_release_waits_for_a_concurrent_holder_then_releases() {
        // Issue #1738: an unrelated lease operation holding the ledger lock
        // must delay a release, not fail it.
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditLogGuard::redirect(dir.path());
        let test_opts = opts(dir.path(), "live-token");
        seed(
            &test_opts.ledger_path,
            "live-token",
            Utc::now() + ChronoDuration::minutes(30),
            None,
        );
        let held = LedgerLock::acquire(&test_opts.ledger_path).unwrap();

        let releaser = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            drop(held);
        });

        let result = release(&test_opts).await;
        releaser.join().unwrap();

        assert!(
            matches!(result, ReleaseResult::Released { .. }),
            "{result:?}"
        );
    }

    #[test]
    fn a_failed_release_audits_the_file_its_token_covers() {
        // Issue #1738: the lock wait timed out, so the release never read
        // the ledger under the lock — but the audit record must still name
        // the file, or a `file_id:` query misses the attempt.
        let dir = tempfile::tempdir().unwrap();
        let audit = AuditLogGuard::redirect(dir.path());
        let test_opts = opts(dir.path(), "live-token");
        seed(
            &test_opts.ledger_path,
            "live-token",
            Utc::now() + ChronoDuration::minutes(30),
            None,
        );

        record_attempt(
            &test_opts,
            &ReleaseResult::Failed {
                detail: "timed out".to_string(),
            },
        );

        let records = audit.records();
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(
            records[0].context.get("file_id").map(String::as_str),
            Some("file-1"),
            "{records:?}"
        );
    }

    #[tokio::test]
    async fn a_release_writes_an_audit_record_naming_the_token() {
        let dir = tempfile::tempdir().unwrap();
        let audit = AuditLogGuard::redirect(dir.path());
        let test_opts = opts(dir.path(), "live-token");
        seed(
            &test_opts.ledger_path,
            "live-token",
            Utc::now() + ChronoDuration::minutes(30),
            None,
        );

        let result = release(&test_opts).await;
        assert!(matches!(result, ReleaseResult::Released { .. }));

        let records = audit.records();
        let record = records
            .iter()
            .find(|r| r.command == vec!["drive".to_string(), "lease-release".to_string()])
            .expect("a release writes one audit record");
        assert_eq!(
            record.context.get("verdict").map(String::as_str),
            Some("released")
        );
        assert_eq!(
            record.context.get("lease_id").map(String::as_str),
            Some("live-token")
        );
    }

    #[test]
    fn result_serializes_to_jsonl() {
        let mut out = Vec::new();
        ReleaseResult::NoSuchToken.write_jsonl(&mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\"status\":\"no-such-token\""), "{text}");
    }

    /// Pins the CLI exit-code classification (issue #1775): `NotLive` is
    /// `0`, the one outcome the issue left open — it's an idempotent
    /// no-op, not a failure.
    #[test]
    fn exit_code_matches_the_documented_classification() {
        let cases: Vec<(ReleaseResult, i32)> = vec![
            (
                ReleaseResult::Released {
                    token: "tok".to_string(),
                    file_id: "file-1".to_string(),
                    expires_at: Utc::now(),
                },
                0,
            ),
            (
                ReleaseResult::NotLive {
                    token: "tok".to_string(),
                    file_id: "file-1".to_string(),
                    expires_at: Utc::now(),
                    released_at: None,
                },
                0,
            ),
            (ReleaseResult::NoSuchToken, 1),
            (
                ReleaseResult::Failed {
                    detail: "boom".to_string(),
                },
                1,
            ),
        ];
        for (result, expected) in cases {
            assert_eq!(result.exit_code(), expected, "{result:?}");
        }
    }
}
