//! The shared "does this presented `--lease` token authorise this write?"
//! check ([ADR-0080](../../../docs/adrs/adr-0080.md) §9), factored out of
//! `content_edit.rs` once every other content-mutating engine
//! (`sheets::write`, `sheets::structure`, `sheets::format`,
//! `sheets::validation`, `sheets::protection`, `docs::write`) needed the
//! identical fail-closed sequence: acquire the ledger lock, look up the
//! token, and refuse unless it is present, live, bound to the right file,
//! and not stale. A half-dozen independent copies of a security-critical
//! refusal path is exactly the kind of drift a shared function exists to
//! prevent — a bug fixed in one copy but not the others is worse than one
//! copy reviewed six times.
//!
//! What stays with each caller: mapping [`LeaseCheckOutcome`] onto that
//! engine's own `*Result` enum (`EditResult`/`WriteResult`/
//! `StructureResult`/`ProtectionResult`/`DocsWriteResult` and friends all
//! carry their own `RefusedNoLease`/`RefusedLeaseExpired`/
//! `RefusedLeaseWrongFile`/`RefusedLeaseStale` variants, since each is a
//! distinct wire type), and deciding *when* to call it and what
//! `live_version` to check against — that differs by surface (§6).

use std::path::Path;

use crate::drive::files_api::FilesApi;
use crate::drive::lease::ledger::{LeaseLedger, LedgerLock};

/// The result of checking a presented `--lease` token against the ledger.
pub(crate) enum LeaseCheckOutcome {
    /// The token is present, live, bound to `file_id`, and not stale. The
    /// still-held [`LedgerLock`] must be kept alive by the caller across
    /// the mutating call and into [`refresh_lease_after_write`] — see that
    /// function's doc comment for why releasing it early reopens the
    /// double-spend window this lock exists to close.
    Ok(LedgerLock),
    /// No `--lease` was presented at all.
    NoLease,
    /// The ledger could not be read, the token is not in it, or it has
    /// expired — folded into one outcome per [`check_and_lock_lease`]'s
    /// fail-closed reasoning.
    Expired,
    /// The presented lease is bound to a different file id.
    WrongFile,
    /// The file has moved since the lease's recorded `version`.
    Stale,
    /// Acquiring the ledger lock itself failed (another `drive lease`
    /// operation genuinely in progress). Says nothing about the token's
    /// validity — the caller should report this as an operational failure,
    /// not fold it into `Expired`.
    Failed(String),
}

/// Acquires the ledger lock and checks `lease_token` against it: present,
/// unexpired, bound to `file_id`, and not stale against `live_version`
/// (ADR-0080 §6/§9).
///
/// `log_prefix` names the calling command for the `tracing::warn!` an
/// unreadable ledger emits (e.g. `"drive edit"`, `"drive sheets write"`),
/// so an operator can tell which surface hit a systemic ledger problem.
///
/// On success, the returned [`LeaseCheckOutcome::Ok`] holds the still-live
/// [`LedgerLock`] — the caller must keep it alive across the mutating call
/// and into [`refresh_lease_after_write`], not drop it right away, or two
/// concurrent writes presenting the same token could each load the ledger
/// before either has recorded its write and both pass the staleness check
/// against the same now-stale `version` (a lease-token double-spend). On
/// every refusal, no lock is held (either none was ever taken, or it is
/// dropped before returning).
///
/// A ledger load failure reports [`LeaseCheckOutcome::Expired`] rather than
/// [`LeaseCheckOutcome::Failed`] — an unreadable ledger means "no token in
/// it can be verified," which is exactly what that refusal already
/// communicates, and it avoids a corrupt/missing ledger being mistaken for
/// an API or validation error. It is still logged at `warn` (distinct from
/// the plain "no such token" case, which is expected and not logged) so a
/// systemic ledger problem — as opposed to an ordinary expired/unknown
/// token — leaves an operator-visible trace rather than silently
/// masquerading as the latter.
pub(crate) fn check_and_lock_lease(
    log_prefix: &str,
    ledger_path: &Path,
    lease_token: Option<&str>,
    file_id: &str,
    live_version: Option<&str>,
) -> LeaseCheckOutcome {
    // Every refusal below is also an audit event (ADR-0080 §11): "refused"/
    // "expired"/"stale" for the ledger-level verdicts named in that section,
    // widened by "failed" for an operational error (the ledger lock itself
    // could not be acquired) the same way `drive lease acquire`'s own audit
    // trail widens its verdict set beyond the ADR's base four. Best-effort —
    // a write is already being refused regardless of whether this record
    // lands, so a logging failure here changes nothing about the refusal.
    let audit_refusal = |verdict: &str, lease_id: Option<&str>| {
        write_check_audit(log_prefix, file_id, lease_id, verdict, live_version);
    };

    let Some(token) = lease_token else {
        audit_refusal("refused", None);
        return LeaseCheckOutcome::NoLease;
    };
    let lock = match LedgerLock::acquire(ledger_path) {
        Ok(lock) => lock,
        Err(err) => {
            audit_refusal("failed", Some(token));
            return LeaseCheckOutcome::Failed(err.to_string());
        }
    };
    // Every exit below refuses unless the token is verified live, bound to
    // this file, and fresh — a `Result` failure anywhere in this lookup (an
    // unreadable ledger, an absent token) must refuse, never fall through
    // as "no refusal". `?` is deliberately not used on the ledger load: a
    // stray `?` here would turn a read error into silent approval — the
    // opposite of fail-closed.
    let ledger = match LeaseLedger::load(ledger_path) {
        Ok(ledger) => ledger,
        Err(err) => {
            // Bind the path so it is formatted whenever the branch runs —
            // not only when a subscriber happens to be installed — so
            // coverage sees it (the `daemon/services/worktrees.rs::
            // load_pr_cache` pattern).
            let ledger_path = ledger_path.display();
            tracing::warn!(
                "{log_prefix}: lease ledger at {ledger_path} could not be read ({err}); \
                 refusing the presented lease as expired rather than trusting an unreadable \
                 ledger"
            );
            audit_refusal("expired", Some(token));
            return LeaseCheckOutcome::Expired;
        }
    };
    let Some(record) = ledger.get(token) else {
        audit_refusal("expired", Some(token));
        return LeaseCheckOutcome::Expired;
    };
    if !record.is_live(chrono::Utc::now()) {
        audit_refusal("expired", Some(token));
        return LeaseCheckOutcome::Expired;
    }
    if record.file_id != file_id {
        audit_refusal("refused", Some(token));
        return LeaseCheckOutcome::WrongFile;
    }
    if live_version != Some(record.version.as_str()) {
        audit_refusal("stale", Some(token));
        return LeaseCheckOutcome::Stale;
    }

    // The write-ahead intent record (ADR-0080 §11): durably written before
    // the mutating API call this lease authorises, fail-closed. Unlike
    // every other audit write in this module and in `drive lease acquire`,
    // a failure here refuses the write outright — this is the one point in
    // the whole leased-write path the ADR requires it, since everything
    // before it (the permission gate, the lease lookup itself) refuses
    // without ever touching Drive content, and everything after it (the
    // mutating call) is the content-mutating act this record's whole
    // purpose is to make un-auditable-by-omission impossible.
    let intent = crate::request_log::AuditOutcome {
        command: vec![log_prefix.to_string()],
        integration: "drive",
        file_id: file_id.to_string(),
        lease_id: Some(token.to_string()),
        verdict: "pending".to_string(),
        version_before: live_version.map(str::to_string),
        ..Default::default()
    };
    if let Err(err) = crate::request_log::record_audit_event(intent) {
        return LeaseCheckOutcome::Failed(format!(
            "failed to write the write-ahead audit record: {err}"
        ));
    }

    LeaseCheckOutcome::Ok(lock)
}

/// Writes one best-effort `kind: "audit"` record for a lease-check outcome
/// that never reaches a mutating call — either a refusal (see
/// [`check_and_lock_lease`]'s own doc comment for why these stay
/// best-effort) or, via [`write_outcome_audit`], the outcome half of a
/// write that did.
fn write_check_audit(
    log_prefix: &str,
    file_id: &str,
    lease_id: Option<&str>,
    verdict: &str,
    version_before: Option<&str>,
) {
    let outcome = crate::request_log::AuditOutcome {
        command: vec![log_prefix.to_string()],
        integration: "drive",
        file_id: file_id.to_string(),
        lease_id: lease_id.map(str::to_string),
        verdict: verdict.to_string(),
        version_before: version_before.map(str::to_string),
        ..Default::default()
    };
    if let Err(err) = crate::request_log::record_audit_event(outcome) {
        tracing::warn!("{log_prefix}: failed to write audit record: {err}");
    }
}

/// Writes the outcome half of a leased write's audit pair (ADR-0080 §11),
/// after the mutating call this write's intent record (written inside
/// [`check_and_lock_lease`]) already authorised has returned — `"allowed"`
/// on success, `"failed"` on an API error. Best-effort, and deliberately
/// independent of the intent record's own success: the mutating call has
/// already happened either way by the time this runs, so there is nothing
/// left to refuse (mirrors [`refresh_lease_after_write`]'s identical
/// reasoning for the ledger side of a successful write).
///
/// `version_after`/`modified_time_after` are opportunistic, not another
/// round trip: pass them when the mutating call's own response already
/// carries them (content_edit.rs's `files.update`), `None` otherwise (every
/// Sheets/Docs call, whose response carries no Drive metadata at all) — the
/// refreshed ledger row already has that state for a surface willing to pay
/// [`refresh_lease_after_native_write`]'s extra fetch, so a third round
/// trip here just to duplicate it into the audit record would not be
/// buying anything the ledger doesn't already have.
pub(crate) fn write_outcome_audit(
    log_prefix: &str,
    file_id: &str,
    lease_id: &str,
    verdict: &str,
    version_after: Option<String>,
    modified_time_after: Option<String>,
) {
    let outcome = crate::request_log::AuditOutcome {
        command: vec![log_prefix.to_string()],
        integration: "drive",
        file_id: file_id.to_string(),
        lease_id: Some(lease_id.to_string()),
        verdict: verdict.to_string(),
        version_after,
        modified_time_after,
        ..Default::default()
    };
    if let Err(err) = crate::request_log::record_audit_event(outcome) {
        tracing::warn!("{log_prefix}: failed to write outcome audit record: {err}");
    }
}

/// Best-effort: updates the lease's recorded `version`/`modified_time`
/// after a successful write, so a second write under the same lease is
/// checked against the file's *new* state (ADR-0080 §5's multi-use
/// semantics). A failure here is logged, never surfaced as a failed write —
/// the write already succeeded — and its consequence is safe rather than
/// silent: the ledger keeps the *old* version, so the next write under this
/// lease sees a spurious staleness mismatch and refuses, never a missed one
/// (ADR-0080 §4).
///
/// Takes `_lock` (already held by the caller since
/// [`check_and_lock_lease`]) rather than acquiring its own — acquiring a
/// second time here, in the same process, on the same path, would fail
/// against the lock this call is still holding.
pub(crate) fn refresh_lease_after_write(
    log_prefix: &str,
    _lock: &LedgerLock,
    ledger_path: &Path,
    token: &str,
    version: Option<String>,
    modified_time: Option<String>,
) {
    let Some(version) = version else {
        tracing::debug!(
            "{log_prefix}: write response carried no `version`; lease ledger not refreshed"
        );
        return;
    };
    let result = (|| -> anyhow::Result<()> {
        let mut ledger = LeaseLedger::load(ledger_path)?;
        ledger.record_write(token, version, modified_time);
        ledger.save(ledger_path)
    })();
    if let Err(err) = result {
        tracing::debug!(
            "{log_prefix}: failed to refresh lease ledger after a successful write: {err}"
        );
    }
}

/// [`refresh_lease_after_write`] for a surface whose mutating call itself
/// returns no Drive metadata — Sheets `values.*`/`batchUpdate` and Docs
/// `batchUpdate` all reply with their own API's response shape, not a
/// [`crate::drive::types::DriveFile`] — so the new `version` has to come
/// from a **second** `files.get` issued after the write succeeds (ADR-0080
/// §6: "two extra round-trips per leased write... accepted as the price of
/// the property"). A collaborator's edit landing in the gap between the
/// write and this read is absorbed into the lease as if it were ours; that
/// is the same named, accepted limitation, not a hidden one.
///
/// A failure to even re-fetch is logged and swallowed exactly like a save
/// failure would be — never surfaced as a failed write, for the same reason
/// [`refresh_lease_after_write`] itself never is.
pub(crate) async fn refresh_lease_after_native_write(
    log_prefix: &str,
    lock: &LedgerLock,
    ledger_path: &Path,
    token: &str,
    files_api: &FilesApi<'_>,
    file_id: &str,
) {
    match files_api.get_metadata(file_id).await {
        Ok(fresh) => refresh_lease_after_write(
            log_prefix,
            lock,
            ledger_path,
            token,
            fresh.version,
            fresh.modified_time,
        ),
        Err(err) => {
            tracing::debug!(
                "{log_prefix}: failed to re-fetch version after a successful write; lease \
                 ledger not refreshed: {err}"
            );
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::client::DriveClient;
    use crate::drive::lease::ledger::{LeaseBackup, LeaseLedger, LeaseRecord};
    use crate::test_support::AuditLogGuard;
    use crate::utils::secret::Secret;

    fn seed_lease(ledger_path: &Path, token: &str, file_id: &str, version: &str) {
        let mut ledger = LeaseLedger::default();
        ledger.insert(LeaseRecord {
            token: token.to_string(),
            file_id: file_id.to_string(),
            version: version.to_string(),
            modified_time: None,
            backup: LeaseBackup::Bytes {
                path: std::path::PathBuf::from("/tmp/test-backup"),
                sha256: "deadbeef".to_string(),
                size: 0,
            },
            acquired_at: chrono::Utc::now(),
            expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
            released_at: None,
        });
        ledger.save(ledger_path).unwrap();
    }

    fn read_audit_lines(audit_path: &Path) -> Vec<crate::request_log::LogRecord> {
        std::fs::read_to_string(audit_path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

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

    #[test]
    fn refuses_as_expired_and_logs_the_path_when_the_ledger_is_unreadable() {
        // A directory in place of the ledger file makes `LeaseLedger::load`
        // fail with something other than a missing-file error — refused as
        // expired, and the `warn!` names the unreadable path (the field
        // expression this test exercises).
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditLogGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        std::fs::create_dir(&ledger_path).unwrap();

        let outcome =
            check_and_lock_lease("test", &ledger_path, Some("any-token"), "file-1", Some("1"));
        assert!(matches!(outcome, LeaseCheckOutcome::Expired));
    }

    #[tokio::test]
    async fn native_refresh_logs_and_swallows_a_failed_refetch() {
        // The second `files.get` (needed because Sheets/Docs write calls
        // carry no Drive metadata of their own) fails here — the refresh is
        // best-effort, so this must log and return without panicking or
        // touching the ledger's recorded version.
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/file-1"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let files_api = FilesApi::new(&client);

        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        let lock = LedgerLock::acquire(&ledger_path).unwrap();
        let mut ledger = LeaseLedger::default();
        ledger.insert(LeaseRecord {
            token: "tok".to_string(),
            file_id: "file-1".to_string(),
            version: "1".to_string(),
            modified_time: None,
            backup: LeaseBackup::Bytes {
                path: std::path::PathBuf::from("/tmp/test-backup"),
                sha256: "deadbeef".to_string(),
                size: 0,
            },
            acquired_at: chrono::Utc::now(),
            expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
            released_at: None,
        });
        ledger.save(&ledger_path).unwrap();

        refresh_lease_after_native_write("test", &lock, &ledger_path, "tok", &files_api, "file-1")
            .await;

        let reloaded = LeaseLedger::load(&ledger_path).unwrap();
        assert_eq!(reloaded.get("tok").unwrap().version, "1");
    }

    // ── the write's own audit trail (ADR-0080 §11) ─────────────────────

    #[test]
    fn a_successful_check_writes_a_pending_intent_record() {
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditLogGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        seed_lease(&ledger_path, "tok-1", "file-1", "1");

        let outcome = check_and_lock_lease(
            "drive edit",
            &ledger_path,
            Some("tok-1"),
            "file-1",
            Some("1"),
        );
        assert!(matches!(outcome, LeaseCheckOutcome::Ok(_)));

        let records = read_audit_lines(&dir.path().join("audit.jsonl"));
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(
            records[0].context.get("verdict").map(String::as_str),
            Some("pending")
        );
        assert_eq!(
            records[0].context.get("lease_id").map(String::as_str),
            Some("tok-1")
        );
        assert_eq!(
            records[0].context.get("version_before").map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn the_write_is_refused_when_the_intent_record_cannot_be_written() {
        // Fail-closed (ADR-0080 §11): unlike every refusal above, a failure
        // to write the write-ahead record must refuse the write outright —
        // this is the one point in the lease check the ADR requires it.
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditLogGuard::redirect(dir.path());
        // A directory in place of the audit file makes the write fail the
        // same way an unwritable/missing-permission path would.
        std::fs::create_dir(dir.path().join("audit.jsonl")).unwrap();
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        seed_lease(&ledger_path, "tok-1", "file-1", "1");

        let outcome = check_and_lock_lease(
            "drive edit",
            &ledger_path,
            Some("tok-1"),
            "file-1",
            Some("1"),
        );
        let LeaseCheckOutcome::Failed(detail) = outcome else {
            panic!("expected Failed, got a lock/refusal instead");
        };
        assert!(detail.contains("write-ahead"), "{detail}");
    }

    #[test]
    fn each_refusal_writes_its_own_verdict_to_the_audit_log() {
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditLogGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        seed_lease(&ledger_path, "tok-1", "file-1", "1");

        // No token presented at all.
        assert!(matches!(
            check_and_lock_lease("drive edit", &ledger_path, None, "file-1", Some("1")),
            LeaseCheckOutcome::NoLease
        ));
        // Unknown token.
        assert!(matches!(
            check_and_lock_lease(
                "drive edit",
                &ledger_path,
                Some("bogus"),
                "file-1",
                Some("1")
            ),
            LeaseCheckOutcome::Expired
        ));
        // Bound to a different file.
        assert!(matches!(
            check_and_lock_lease(
                "drive edit",
                &ledger_path,
                Some("tok-1"),
                "some-other-file",
                Some("1")
            ),
            LeaseCheckOutcome::WrongFile
        ));
        // Stale version.
        assert!(matches!(
            check_and_lock_lease(
                "drive edit",
                &ledger_path,
                Some("tok-1"),
                "file-1",
                Some("2")
            ),
            LeaseCheckOutcome::Stale
        ));

        let records = read_audit_lines(&dir.path().join("audit.jsonl"));
        let verdicts: Vec<Option<&String>> =
            records.iter().map(|r| r.context.get("verdict")).collect();
        assert_eq!(
            verdicts,
            vec![
                Some(&"refused".to_string()),
                Some(&"expired".to_string()),
                Some(&"refused".to_string()),
                Some(&"stale".to_string()),
            ]
        );
        // The no-token refusal carries no lease id; every other one does,
        // even though the token turned out invalid — it is an identifier,
        // not a bearer credential (docs/drive.md), safe to record.
        assert_eq!(records[0].context.get("lease_id"), None);
        assert_eq!(
            records[1].context.get("lease_id").map(String::as_str),
            Some("bogus")
        );
    }

    #[test]
    fn write_outcome_audit_records_the_final_verdict_and_post_write_version() {
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditLogGuard::redirect(dir.path());

        write_outcome_audit(
            "drive edit",
            "file-1",
            "tok-1",
            "allowed",
            Some("2".to_string()),
            Some("2026-09-12T00:00:00Z".to_string()),
        );

        let records = read_audit_lines(&dir.path().join("audit.jsonl"));
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(
            records[0].context.get("verdict").map(String::as_str),
            Some("allowed")
        );
        assert_eq!(
            records[0].context.get("version_after").map(String::as_str),
            Some("2")
        );
    }
}
