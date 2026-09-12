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
//! The same function owns the leased write's audit trail (ADR-0080 §11):
//! a best-effort record for every refusal, the fail-closed write-ahead
//! `pending` intent record for a lease that checks out, and — through
//! [`finish_leased_write`]/[`finish_leased_native_write`] and
//! [`record_failed_leased_write`], which every engine calls after its own
//! mutating call — the `allowed`/`failed` outcome half of the pair. An
//! engine that took the lock and then reported neither outcome would leave
//! an intent record that reads as an interrupted write, so the two halves
//! are deliberately the *only* way to conclude a leased write.
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
use crate::request_log::AuditOutcome;

/// The `verdict` vocabulary of a leased write's audit records (ADR-0080
/// §11), kebab-case like every other `*Result::log_status` in the crate and
/// mirroring the engines' own `Refused*` result variants one-to-one, so a
/// `verdict:` query on `audit.jsonl` distinguishes exactly what the CLI
/// reported. `drive lease acquire`'s own vocabulary lives in
/// `acquire.rs`.
pub(crate) mod verdict {
    /// The write-ahead intent record: the lease checked out and the
    /// mutating call is about to be issued.
    pub const PENDING: &str = "pending";
    /// The mutating call succeeded.
    pub const ALLOWED: &str = "allowed";
    /// Either the mutating call failed, or (with no `pending` record
    /// preceding it) the ledger lock could not be acquired. Carries the
    /// error either way.
    pub const FAILED: &str = "failed";
    /// No `--lease` was presented at all.
    pub const REFUSED_NO_LEASE: &str = "refused-no-lease";
    /// The token is unknown, has expired, or the ledger was unreadable.
    pub const REFUSED_LEASE_EXPIRED: &str = "refused-lease-expired";
    /// The token is bound to a different file id.
    pub const REFUSED_LEASE_WRONG_FILE: &str = "refused-lease-wrong-file";
    /// The file has moved since the lease's recorded `version`.
    pub const REFUSED_LEASE_STALE: &str = "refused-lease-stale";
}

/// The result of checking a presented `--lease` token against the ledger.
pub(crate) enum LeaseCheckOutcome {
    /// The token is present, live, bound to `file_id`, and not stale. The
    /// still-held [`LedgerLock`] must be kept alive by the caller across
    /// the mutating call and into [`finish_leased_write`] — see that
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
    /// operation genuinely in progress), or the write-ahead audit record
    /// could not be written. Says nothing about the token's validity — the
    /// caller should report this as an operational failure, not fold it
    /// into `Expired`.
    Failed(String),
}

/// Identifies one leased write to [`check_and_lock_lease`] and the two
/// functions that conclude it, so the check and every record of its audit
/// pair name the same write the same way.
#[derive(Clone, Copy)]
pub(crate) struct LeasedWrite<'a> {
    /// The calling engine, for the `tracing` messages this module emits
    /// (e.g. `"drive edit"`, `"drive sheets structure"`) — an operator
    /// reading the log needs to know which surface hit a ledger problem.
    /// Not a CLI command: the Sheets engines each serve several verbs.
    pub log_prefix: &'a str,
    /// The verb's `log_operation()` value (`"edit"`,
    /// `"sheets-delete-sheet"`, `"docs-replace"`, …). Becomes the audit
    /// records' `command` as `["drive", <operation>]` — the exact shape
    /// `request_log::build_drive_mutation_record` gives the same write's
    /// `drivemutation` record in `log.jsonl`, so `command:` queries join
    /// the two files and the forensic record names the verb that ran, not
    /// merely the engine that ran it.
    pub operation: &'a str,
    /// The lease ledger the token is checked against and refreshed in.
    pub ledger_path: &'a Path,
    /// The Drive file id being written.
    pub file_id: &'a str,
}

/// Acquires the ledger lock and checks `lease_token` against it: present,
/// unexpired, bound to `file_id`, and not stale against `live_version`
/// (ADR-0080 §6/§9).
///
/// `write` names the engine (for `tracing`), the verb (for the audit
/// records' `command`), the ledger and the file — see [`LeasedWrite`].
/// `live_modified_time` is the `modifiedTime` paired with `live_version`,
/// recorded alongside it as `modified_time_before`; it takes no part in
/// the check itself.
///
/// On success, the returned [`LeaseCheckOutcome::Ok`] holds the still-live
/// [`LedgerLock`] — the caller must keep it alive across the mutating call
/// and into [`finish_leased_write`], not drop it right away, or two
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
///
/// **Audit trail (ADR-0080 §11).** Every refusal writes one best-effort
/// record (`verdict`: one of the `refused-*` constants in [`verdict`], or
/// `failed` with the error when the ledger lock could not be taken) — a
/// write is being refused regardless of whether the record lands, so a
/// logging failure changes nothing about the refusal. A lease that checks
/// out writes the write-ahead `pending` intent record instead, and that
/// one is fail-closed: if it cannot be written the write is refused as
/// [`LeaseCheckOutcome::Failed`]. This is the one point in the whole
/// leased-write path the ADR requires it — everything before it refuses
/// without touching Drive content, and everything after it is the
/// content-mutating act the record exists to make un-auditable-by-omission
/// impossible. The record is `fsync`ed before this returns
/// (`request_log::record_audit`), so a process that dies mid-write still
/// leaves it behind.
pub(crate) fn check_and_lock_lease(
    write: LeasedWrite<'_>,
    lease_token: Option<&str>,
    live_version: Option<&str>,
    live_modified_time: Option<&str>,
) -> LeaseCheckOutcome {
    let LeasedWrite {
        log_prefix,
        ledger_path,
        file_id,
        ..
    } = write;
    let before = |lease_id: Option<&str>, verdict: &str| {
        let mut outcome = audit_outcome(write, lease_id, verdict);
        outcome.version_before = live_version.map(str::to_string);
        outcome.modified_time_before = live_modified_time.map(str::to_string);
        outcome
    };
    let refuse = |lease_id: Option<&str>, verdict: &str, error: Option<String>| {
        let mut outcome = before(lease_id, verdict);
        outcome.error = error;
        write_audit_best_effort(log_prefix, outcome);
    };

    let Some(token) = lease_token else {
        refuse(None, verdict::REFUSED_NO_LEASE, None);
        return LeaseCheckOutcome::NoLease;
    };
    let lock = match LedgerLock::acquire(ledger_path) {
        Ok(lock) => lock,
        Err(err) => {
            refuse(Some(token), verdict::FAILED, Some(err.to_string()));
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
            // Refused as expired like an unknown token, but the record
            // keeps the reason: an unreadable ledger is a systemic problem
            // an auditor should be able to tell apart from a stale token.
            refuse(
                Some(token),
                verdict::REFUSED_LEASE_EXPIRED,
                Some(format!(
                    "lease ledger at {ledger_path} could not be read: {err}"
                )),
            );
            return LeaseCheckOutcome::Expired;
        }
    };
    let Some(record) = ledger.get(token) else {
        refuse(Some(token), verdict::REFUSED_LEASE_EXPIRED, None);
        return LeaseCheckOutcome::Expired;
    };
    if !record.is_live(chrono::Utc::now()) {
        refuse(Some(token), verdict::REFUSED_LEASE_EXPIRED, None);
        return LeaseCheckOutcome::Expired;
    }
    if record.file_id != file_id {
        refuse(Some(token), verdict::REFUSED_LEASE_WRONG_FILE, None);
        return LeaseCheckOutcome::WrongFile;
    }
    if live_version != Some(record.version.as_str()) {
        refuse(Some(token), verdict::REFUSED_LEASE_STALE, None);
        return LeaseCheckOutcome::Stale;
    }

    // The write-ahead intent record — fail-closed, see the doc comment.
    let intent = before(Some(token), verdict::PENDING);
    if let Err(err) = crate::request_log::record_audit_event(intent) {
        return LeaseCheckOutcome::Failed(format!(
            "failed to write the write-ahead audit record: {err}"
        ));
    }

    LeaseCheckOutcome::Ok(lock)
}

/// The fields every one of this module's audit records shares. `command`
/// is `["drive", <operation>]`, byte-for-byte what
/// `request_log::build_drive_mutation_record` writes for the same write.
fn audit_outcome(write: LeasedWrite<'_>, lease_id: Option<&str>, verdict: &str) -> AuditOutcome {
    AuditOutcome {
        command: vec!["drive".to_string(), write.operation.to_string()],
        integration: "drive",
        file_id: write.file_id.to_string(),
        lease_id: lease_id.map(str::to_string),
        verdict: verdict.to_string(),
        ..Default::default()
    }
}

/// Writes one audit record best-effort: a failure is warned, never
/// surfaced. Every record in this module goes through here except the
/// write-ahead intent record, whose failure [`check_and_lock_lease`]
/// refuses the write on.
fn write_audit_best_effort(log_prefix: &str, outcome: AuditOutcome) {
    let verdict = outcome.verdict.clone();
    if let Err(err) = crate::request_log::record_audit_event(outcome) {
        tracing::warn!("{log_prefix}: failed to write the `{verdict}` audit record: {err}");
    }
}

/// Writes the `failed` outcome half of a leased write's audit pair
/// (ADR-0080 §11) after the mutating call this write's intent record
/// (written inside [`check_and_lock_lease`]) authorised has returned an
/// error, carrying that error. Best-effort, and deliberately independent
/// of the intent record's own success: the mutating call has already been
/// attempted by the time this runs, so there is nothing left to refuse.
/// The `allowed` half is written by [`finish_leased_write`] /
/// [`finish_leased_native_write`], so a successful write cannot refresh
/// its lease without also concluding its audit pair.
pub(crate) fn record_failed_leased_write(write: LeasedWrite<'_>, token: &str, error: &str) {
    let mut outcome = audit_outcome(write, Some(token), verdict::FAILED);
    outcome.error = Some(error.to_string());
    write_audit_best_effort(write.log_prefix, outcome);
}

/// Concludes a *successful* leased write: writes the `allowed` outcome
/// half of its audit pair (ADR-0080 §11), then updates the lease's
/// recorded `version`/`modified_time` so a second write under the same
/// lease is checked against the file's *new* state (§5's multi-use
/// semantics). Both are best-effort — a failure here is logged, never
/// surfaced as a failed write, since the write already succeeded — and
/// the ledger half's consequence is safe rather than silent: the ledger
/// keeps the *old* version, so the next write under this lease sees a
/// spurious staleness mismatch and refuses, never a missed one (§4).
///
/// `version`/`modified_time` are the file's post-write state when the
/// mutating call's own response carried it (`drive edit`'s `files.update`
/// does); the audit record is written even when it did not, so the
/// `pending` record always gets its outcome.
///
/// Takes `_lock` (already held by the caller since
/// [`check_and_lock_lease`]) rather than acquiring its own — acquiring a
/// second time here, in the same process, on the same path, would fail
/// against the lock this call is still holding.
pub(crate) fn finish_leased_write(
    write: LeasedWrite<'_>,
    _lock: &LedgerLock,
    token: &str,
    version: Option<String>,
    modified_time: Option<String>,
) {
    let LeasedWrite {
        log_prefix,
        ledger_path,
        ..
    } = write;
    let mut outcome = audit_outcome(write, Some(token), verdict::ALLOWED);
    outcome.version_after.clone_from(&version);
    outcome.modified_time_after.clone_from(&modified_time);
    write_audit_best_effort(log_prefix, outcome);

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

/// [`finish_leased_write`] for a surface whose mutating call itself
/// returns no Drive metadata — Sheets `values.*`/`batchUpdate` and Docs
/// `batchUpdate` all reply with their own API's response shape, not a
/// [`crate::drive::types::DriveFile`] — so the new `version` has to come
/// from a **second** `files.get` issued after the write succeeds (ADR-0080
/// §6: "two extra round-trips per leased write... accepted as the price of
/// the property"). A collaborator's edit landing in the gap between the
/// write and this read is absorbed into the lease as if it were ours; that
/// is the same named, accepted limitation, not a hidden one. The same
/// fetch feeds the `allowed` audit record's `version_after`, so it costs
/// no extra round trip.
///
/// A failure to even re-fetch is logged and swallowed exactly like a save
/// failure would be — never surfaced as a failed write, for the same reason
/// [`finish_leased_write`] itself never is — and the `allowed` record is
/// still written, without the post-write version.
pub(crate) async fn finish_leased_native_write(
    write: LeasedWrite<'_>,
    lock: &LedgerLock,
    token: &str,
    files_api: &FilesApi<'_>,
) {
    let (version, modified_time) = match files_api.get_metadata(write.file_id).await {
        Ok(fresh) => (fresh.version, fresh.modified_time),
        Err(err) => {
            let log_prefix = write.log_prefix;
            tracing::debug!(
                "{log_prefix}: failed to re-fetch version after a successful write; lease \
                 ledger not refreshed: {err}"
            );
            (None, None)
        }
    };
    finish_leased_write(write, lock, token, version, modified_time);
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

    /// A [`LeasedWrite`] for one test; `operation` is the verb's
    /// `log_operation()` value, exactly as an engine would pass it.
    fn leased<'a>(operation: &'a str, ledger_path: &'a Path, file_id: &'a str) -> LeasedWrite<'a> {
        LeasedWrite {
            log_prefix: "test",
            operation,
            ledger_path,
            file_id,
        }
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
        let audit = AuditLogGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        std::fs::create_dir(&ledger_path).unwrap();

        let outcome = check_and_lock_lease(
            leased("edit", &ledger_path, "file-1"),
            Some("any-token"),
            Some("1"),
            None,
        );
        assert!(matches!(outcome, LeaseCheckOutcome::Expired));

        // Refused as expired like an unknown token, but the audit record
        // keeps the systemic reason an auditor needs to tell them apart.
        let records = audit.records();
        assert_eq!(audit.verdicts(), [verdict::REFUSED_LEASE_EXPIRED]);
        let error = records[0].error.as_deref().unwrap_or_default();
        assert!(error.contains("could not be read"), "{error}");
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
        let audit = AuditLogGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        let lock = LedgerLock::acquire(&ledger_path).unwrap();
        seed_lease(&ledger_path, "tok", "file-1", "1");

        finish_leased_native_write(
            leased("edit", &ledger_path, "file-1"),
            &lock,
            "tok",
            &files_api,
        )
        .await;

        let reloaded = LeaseLedger::load(&ledger_path).unwrap();
        assert_eq!(reloaded.get("tok").unwrap().version, "1");
        // The write did succeed, so its intent record still gets its
        // `allowed` outcome — just without the post-write version.
        let records = audit.records();
        assert_eq!(audit.verdicts(), [verdict::ALLOWED]);
        assert_eq!(records[0].context.get("version_after"), None);
    }

    #[tokio::test]
    async fn native_finish_records_the_refetched_version_in_both_ledger_and_audit() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/file-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "file-1", "name": "Sheet", "version": "7",
                    "modifiedTime": "2026-09-12T00:00:00Z",
                })),
            )
            .mount(&server)
            .await;
        let files_api = FilesApi::new(&client);

        let dir = tempfile::tempdir().unwrap();
        let audit = AuditLogGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        let lock = LedgerLock::acquire(&ledger_path).unwrap();
        seed_lease(&ledger_path, "tok", "file-1", "1");

        finish_leased_native_write(
            leased("sheets-write", &ledger_path, "file-1"),
            &lock,
            "tok",
            &files_api,
        )
        .await;

        let reloaded = LeaseLedger::load(&ledger_path).unwrap();
        assert_eq!(reloaded.get("tok").unwrap().version, "7");
        let records = audit.records();
        assert_eq!(audit.verdicts(), [verdict::ALLOWED]);
        assert_eq!(records[0].command, ["drive", "sheets-write"]);
        assert_eq!(
            records[0].context.get("version_after").map(String::as_str),
            Some("7")
        );
        assert_eq!(
            records[0]
                .context
                .get("modified_time_after")
                .map(String::as_str),
            Some("2026-09-12T00:00:00Z")
        );
    }

    // ── the write's own audit trail (ADR-0080 §11) ─────────────────────

    #[test]
    fn a_successful_check_writes_a_pending_intent_record() {
        let dir = tempfile::tempdir().unwrap();
        let audit = AuditLogGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        seed_lease(&ledger_path, "tok-1", "file-1", "1");

        let outcome = check_and_lock_lease(
            leased("edit", &ledger_path, "file-1"),
            Some("tok-1"),
            Some("1"),
            Some("2026-09-12T00:00:00Z"),
        );
        assert!(matches!(outcome, LeaseCheckOutcome::Ok(_)));

        let records = audit.records();
        assert_eq!(audit.verdicts(), [verdict::PENDING], "{records:?}");
        let record = &records[0];
        // `["drive", <log_operation>]` — the same `command` this write's
        // `drivemutation` record gets, so a `command:` query joins the
        // two files.
        assert_eq!(record.command, ["drive", "edit"]);
        assert_eq!(
            record.context.get("lease_id").map(String::as_str),
            Some("tok-1")
        );
        assert_eq!(
            record.context.get("file_id").map(String::as_str),
            Some("file-1")
        );
        assert_eq!(
            record.context.get("version_before").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            record
                .context
                .get("modified_time_before")
                .map(String::as_str),
            Some("2026-09-12T00:00:00Z")
        );
        assert_eq!(record.error, None);
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
            leased("edit", &ledger_path, "file-1"),
            Some("tok-1"),
            Some("1"),
            None,
        );
        let LeaseCheckOutcome::Failed(detail) = outcome else {
            panic!("expected Failed, got a lock/refusal instead");
        };
        assert!(detail.contains("write-ahead"), "{detail}");
        // And the ledger lock taken on the way in was released with the
        // refusal — a leaked lock would fail every later `drive lease`
        // operation on this ledger as "already in progress".
        assert!(
            LedgerLock::acquire(&ledger_path).is_ok(),
            "the ledger lock must not outlive a refused check"
        );
    }

    #[test]
    fn each_refusal_writes_its_own_verdict_to_the_audit_log() {
        let dir = tempfile::tempdir().unwrap();
        let audit = AuditLogGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        seed_lease(&ledger_path, "tok-1", "file-1", "1");

        // No token presented at all.
        assert!(matches!(
            check_and_lock_lease(
                leased("edit", &ledger_path, "file-1"),
                None,
                Some("1"),
                None
            ),
            LeaseCheckOutcome::NoLease
        ));
        // Unknown token.
        assert!(matches!(
            check_and_lock_lease(
                leased("edit", &ledger_path, "file-1"),
                Some("bogus"),
                Some("1"),
                None
            ),
            LeaseCheckOutcome::Expired
        ));
        // Bound to a different file.
        assert!(matches!(
            check_and_lock_lease(
                leased("edit", &ledger_path, "some-other-file"),
                Some("tok-1"),
                Some("1"),
                None
            ),
            LeaseCheckOutcome::WrongFile
        ));
        // Stale version.
        assert!(matches!(
            check_and_lock_lease(
                leased("edit", &ledger_path, "file-1"),
                Some("tok-1"),
                Some("2"),
                None
            ),
            LeaseCheckOutcome::Stale
        ));

        // One verdict per engine-reported refusal, so an auditor can tell a
        // wrong-file refusal from a missing lease without inferring it from
        // which fields happen to be present.
        let records = audit.records();
        assert_eq!(
            audit.verdicts(),
            [
                verdict::REFUSED_NO_LEASE,
                verdict::REFUSED_LEASE_EXPIRED,
                verdict::REFUSED_LEASE_WRONG_FILE,
                verdict::REFUSED_LEASE_STALE,
            ],
            "{records:?}"
        );
        // Ordinary refusals are verdicts, not errors.
        assert!(
            records.iter().all(|record| record.error.is_none()),
            "{records:?}"
        );
        // The no-token refusal carries no lease id; every other one does,
        // even though the token turned out invalid — it is an identifier,
        // not a bearer credential (docs/drive.md), safe to record.
        assert_eq!(records[0].context.get("lease_id"), None);
        assert_eq!(
            records[1].context.get("lease_id").map(String::as_str),
            Some("bogus")
        );
        // Every refusal records the live state it was checked against.
        assert!(records
            .iter()
            .all(|record| record.context.contains_key("version_before")));
    }

    #[test]
    fn finish_leased_write_records_the_allowed_outcome_and_refreshes_the_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let audit = AuditLogGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        let lock = LedgerLock::acquire(&ledger_path).unwrap();
        seed_lease(&ledger_path, "tok-1", "file-1", "1");

        finish_leased_write(
            leased("edit", &ledger_path, "file-1"),
            &lock,
            "tok-1",
            Some("2".to_string()),
            Some("2026-09-12T00:00:00Z".to_string()),
        );

        let records = audit.records();
        assert_eq!(audit.verdicts(), [verdict::ALLOWED], "{records:?}");
        assert_eq!(
            records[0].context.get("lease_id").map(String::as_str),
            Some("tok-1")
        );
        assert_eq!(
            records[0].context.get("version_after").map(String::as_str),
            Some("2")
        );
        assert_eq!(
            records[0]
                .context
                .get("modified_time_after")
                .map(String::as_str),
            Some("2026-09-12T00:00:00Z")
        );
        let reloaded = LeaseLedger::load(&ledger_path).unwrap();
        assert_eq!(reloaded.get("tok-1").unwrap().version, "2");
    }

    #[test]
    fn finish_leased_write_still_records_allowed_when_the_response_had_no_version() {
        let dir = tempfile::tempdir().unwrap();
        let audit = AuditLogGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        let lock = LedgerLock::acquire(&ledger_path).unwrap();
        seed_lease(&ledger_path, "tok-1", "file-1", "1");

        finish_leased_write(
            leased("edit", &ledger_path, "file-1"),
            &lock,
            "tok-1",
            None,
            None,
        );

        let records = audit.records();
        assert_eq!(audit.verdicts(), [verdict::ALLOWED], "{records:?}");
        assert_eq!(records[0].context.get("version_after"), None);
        // ...and the ledger keeps the old version (a spurious-staleness
        // refusal next time, never a missed one).
        let reloaded = LeaseLedger::load(&ledger_path).unwrap();
        assert_eq!(reloaded.get("tok-1").unwrap().version, "1");
    }

    #[test]
    fn record_failed_leased_write_carries_the_error() {
        let dir = tempfile::tempdir().unwrap();
        let audit = AuditLogGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");

        record_failed_leased_write(
            leased("sheets-format-cells", &ledger_path, "file-1"),
            "tok-1",
            "HTTP 500 from Sheets",
        );

        let records = audit.records();
        assert_eq!(audit.verdicts(), [verdict::FAILED], "{records:?}");
        assert_eq!(records[0].command, ["drive", "sheets-format-cells"]);
        assert_eq!(
            records[0].context.get("lease_id").map(String::as_str),
            Some("tok-1")
        );
        assert_eq!(records[0].error.as_deref(), Some("HTTP 500 from Sheets"));
    }

    #[test]
    fn a_best_effort_audit_failure_is_warned_and_swallowed() {
        // Only the intent record is fail-closed; every other record in the
        // module must never turn a refusal or a succeeded write into a
        // panic or a different outcome when the sink is unwritable.
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditLogGuard::redirect(dir.path());
        std::fs::create_dir(dir.path().join("audit.jsonl")).unwrap();
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        seed_lease(&ledger_path, "tok-1", "file-1", "1");

        assert!(matches!(
            check_and_lock_lease(
                leased("edit", &ledger_path, "file-1"),
                None,
                Some("1"),
                None
            ),
            LeaseCheckOutcome::NoLease
        ));
        record_failed_leased_write(leased("edit", &ledger_path, "file-1"), "tok-1", "boom");
        let lock = LedgerLock::acquire(&ledger_path).unwrap();
        finish_leased_write(
            leased("edit", &ledger_path, "file-1"),
            &lock,
            "tok-1",
            Some("2".to_string()),
            None,
        );
        // The ledger half of `finish_leased_write` is independent of the
        // audit half.
        drop(lock);
        let reloaded = LeaseLedger::load(&ledger_path).unwrap();
        assert_eq!(reloaded.get("tok-1").unwrap().version, "2");
    }

    #[test]
    fn a_ledger_lock_failure_writes_a_failed_record_carrying_the_error() {
        let dir = tempfile::tempdir().unwrap();
        let audit = AuditLogGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        seed_lease(&ledger_path, "tok-1", "file-1", "1");
        // Another `drive lease` operation is mid-flight.
        let _held = LedgerLock::acquire(&ledger_path).unwrap();

        let outcome = check_and_lock_lease(
            leased("edit", &ledger_path, "file-1"),
            Some("tok-1"),
            Some("1"),
            None,
        );
        assert!(matches!(outcome, LeaseCheckOutcome::Failed(_)));

        // An operational failure, not a verdict about the token: `failed`,
        // with the reason, and no `pending` record since no mutating call
        // was ever authorised.
        let records = audit.records();
        assert_eq!(audit.verdicts(), [verdict::FAILED], "{records:?}");
        let error = records[0].error.as_deref().unwrap_or_default();
        assert!(error.contains("already be in progress"), "{error}");
    }
}
