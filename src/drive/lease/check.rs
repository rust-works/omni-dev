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
    let Some(token) = lease_token else {
        return LeaseCheckOutcome::NoLease;
    };
    let lock = match LedgerLock::acquire(ledger_path) {
        Ok(lock) => lock,
        Err(err) => return LeaseCheckOutcome::Failed(err.to_string()),
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
            tracing::warn!(
                "{log_prefix}: lease ledger at {} could not be read ({err}); refusing the \
                 presented lease as expired rather than trusting an unreadable ledger",
                ledger_path.display()
            );
            return LeaseCheckOutcome::Expired;
        }
    };
    let Some(record) = ledger.get(token) else {
        return LeaseCheckOutcome::Expired;
    };
    if !record.is_live(chrono::Utc::now()) {
        return LeaseCheckOutcome::Expired;
    }
    if record.file_id != file_id {
        return LeaseCheckOutcome::WrongFile;
    }
    if live_version != Some(record.version.as_str()) {
        return LeaseCheckOutcome::Stale;
    }
    LeaseCheckOutcome::Ok(lock)
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
