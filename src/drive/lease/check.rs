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
//! What stays with each caller: implementing [`FromLeaseRefusal`] once for
//! its own `*Result` enum (`EditResult`/`WriteResult`/`StructureResult`/
//! `ProtectionResult`/`DocsWriteResult` and friends all carry their own
//! `RefusedNoLease`/`RefusedLeaseExpired`/`RefusedLeaseWrongFile`/
//! `RefusedLeaseStale` variants, since each is a distinct wire type — the
//! mapping itself is [`LeaseGateRefusal::into_result`]'s job, not each
//! caller's), and deciding *when* to call it and what `live_version` to
//! check against — that differs by surface (§6).

use std::path::Path;
use std::time::Duration;

use crate::drive::files_api::FilesApi;
use crate::drive::lease::ledger::{
    default_lock_wait_timeout, offload_short_blocking_io, LeaseBackup, LeaseLedger, LedgerLock,
};
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

/// What a passed lease check hands the engine: the still-held ledger
/// lock, and where the lease's backup landed.
///
/// The backup rides along so a destructive verb's *outcome* can name the
/// copy to restore from — `sheets delete-sheet` and friends say "the lease
/// backed this up" in their real-run message, and a claim like that must
/// come from the ledger record the write was actually checked against, not
/// from the engine assuming one exists (a `require_lease: false` rule
/// reaches the mutating call with no lease and no backup at all).
pub(crate) struct LeaseGrant {
    /// See [`check_and_lock_lease`]'s "On success" doc for why this must
    /// outlive the write.
    pub(crate) lock: LedgerLock,
    /// The backup recorded at `drive lease acquire` time — bytes on disk
    /// for a binary file, a Drive copy for a native document (ADR-0080 §3).
    pub(crate) backup: LeaseBackup,
    /// The `--lease` token this grant validated — carried so a caller
    /// doesn't have to zip a separately-held `Option<&str>` token against
    /// `Option<&LeaseGrant>` to reconstruct what this grant already proved.
    pub(crate) token: String,
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
/// On success, the returned [`Ok`]'s [`LeaseGrant`] holds the still-live
/// [`LedgerLock`] — the caller must keep it alive across the mutating call
/// and into [`finish_leased_write`], not drop it right away, or two
/// concurrent writes presenting the same token could each load the ledger
/// before either has recorded its write and both pass the staleness check
/// against the same now-stale `version` (a lease-token double-spend). On
/// every refusal, no lock is held (either none was ever taken, or it is
/// dropped before returning).
///
/// A ledger load failure reports [`LeaseGateRefusal::Expired`] rather than
/// [`LeaseGateRefusal::Failed`] — an unreadable ledger means "no token in
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
/// [`LeaseGateRefusal::Failed`]. This is the one point in the whole
/// leased-write path the ADR requires it — everything before it refuses
/// without touching Drive content, and everything after it is the
/// content-mutating act the record exists to make un-auditable-by-omission
/// impossible. The record is `fsync`ed before this returns
/// (`request_log::record_audit`), so a process that dies mid-write still
/// leaves it behind.
pub(crate) async fn check_and_lock_lease(
    write: LeasedWrite<'_>,
    lease_token: Option<&str>,
    live_version: Option<&str>,
    live_modified_time: Option<&str>,
) -> Result<LeaseGrant, LeaseGateRefusal> {
    check_and_lock_lease_with_timeout(
        write,
        lease_token,
        live_version,
        live_modified_time,
        default_lock_wait_timeout(),
    )
    .await
}

/// [`check_and_lock_lease`] with an explicit ledger-lock wait budget — the
/// same test seam as [`LedgerLock::acquire_waiting_with_timeout`], so the
/// busy-lock timeout arm can be driven in milliseconds.
pub(crate) async fn check_and_lock_lease_with_timeout(
    write: LeasedWrite<'_>,
    lease_token: Option<&str>,
    live_version: Option<&str>,
    live_modified_time: Option<&str>,
    max_wait: Duration,
) -> Result<LeaseGrant, LeaseGateRefusal> {
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
        return Err(LeaseGateRefusal::NoLease);
    };
    // Waits rather than refusing outright: a concurrent leased write to an
    // *unrelated* file must not hard-fail just because the ledger lock is
    // ledger-global (issue #1687 point 1 — this narrows "hard-fail" to
    // "wait", it does not add per-file scope, so two writes to the *same*
    // file still serialize as before). Lock ordering: this lock is always
    // taken before the write-ahead audit record below, which takes its own
    // lock on `<log>.lock` (`request_log::record_audit_event`) — never the
    // reverse, or the two could deadlock against a caller doing the
    // opposite.
    let lock = match LedgerLock::acquire_waiting_with_timeout(ledger_path, max_wait).await {
        Ok(lock) => lock,
        Err(err) => {
            refuse(Some(token), verdict::FAILED, Some(err.to_string()));
            return Err(LeaseGateRefusal::Failed(err.to_string()));
        }
    };
    // Everything from here to the write-ahead record is straight-line
    // blocking file I/O with no `.await` in it — a ledger read, then either
    // one refusal record or the `fsync`ed intent write. One
    // `offload_short_blocking_io` region covers the lot, rather than one
    // per call: each region hands this worker's core away and takes it
    // back, so three wraps would pay that three times for what is a single
    // uninterrupted stretch of I/O (issue #1697).
    offload_short_blocking_io(|| {
        // Every exit below refuses unless the token is verified live, bound
        // to this file, and fresh — a `Result` failure anywhere in this
        // lookup (an unreadable ledger, an absent token) must refuse, never
        // fall through as "no refusal". `?` is deliberately not used on the
        // ledger load: a stray `?` here would turn a read error into silent
        // approval — the opposite of fail-closed.
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
                return Err(LeaseGateRefusal::Expired);
            }
        };
        let Some(record) = ledger.get(token) else {
            refuse(Some(token), verdict::REFUSED_LEASE_EXPIRED, None);
            return Err(LeaseGateRefusal::Expired);
        };
        if !record.is_live(chrono::Utc::now()) {
            refuse(Some(token), verdict::REFUSED_LEASE_EXPIRED, None);
            return Err(LeaseGateRefusal::Expired);
        }
        if record.file_id != file_id {
            refuse(Some(token), verdict::REFUSED_LEASE_WRONG_FILE, None);
            return Err(LeaseGateRefusal::WrongFile);
        }
        if live_version != Some(record.version.as_str()) {
            refuse(Some(token), verdict::REFUSED_LEASE_STALE, None);
            return Err(LeaseGateRefusal::Stale);
        }

        // The write-ahead intent record — fail-closed, see the doc comment.
        let intent = before(Some(token), verdict::PENDING);
        if let Err(err) = crate::request_log::record_audit_event(intent) {
            return Err(LeaseGateRefusal::Failed(format!(
                "failed to write the write-ahead audit record: {err}"
            )));
        }

        Ok(LeaseGrant {
            lock,
            backup: record.backup.clone(),
            token: token.to_string(),
        })
    })
}

/// The reason a leased write was refused: [`check_and_lock_lease`]'s own
/// four token-verdict variants, plus `Failed` for an operational failure —
/// including [`gate_leased_write`]'s own live-version fetch, the one step
/// every engine must take before it can even call `check_and_lock_lease`.
/// A caller maps exactly one enum onto its own `Refused*`/`Failed` result
/// variants, instead of two.
pub(crate) enum LeaseGateRefusal {
    /// No `--lease` was presented at all.
    NoLease,
    /// The token is unknown, has expired, or the ledger was unreadable.
    Expired,
    /// The token is bound to a different file id.
    WrongFile,
    /// The file has moved since the lease's recorded `version`.
    Stale,
    /// The live-version fetch failed, or acquiring/checking the lease did —
    /// an operational failure, not a verdict on the token.
    Failed(String),
}

impl LeaseGateRefusal {
    /// The kebab-case `log_status`/audit `verdict` string for this refusal,
    /// one-to-one with the `verdict::REFUSED_*`/[`verdict::FAILED`]
    /// constants above — the single home for a string every leased engine
    /// previously retyped by hand in its own `log_status()`.
    pub(crate) fn log_status(&self) -> &'static str {
        match self {
            Self::NoLease => verdict::REFUSED_NO_LEASE,
            Self::Expired => verdict::REFUSED_LEASE_EXPIRED,
            Self::WrongFile => verdict::REFUSED_LEASE_WRONG_FILE,
            Self::Stale => verdict::REFUSED_LEASE_STALE,
            Self::Failed(_) => verdict::FAILED,
        }
    }

    /// Renders this refusal's message as a single line (never containing a
    /// newline) — the single home for prose every leased engine's
    /// `describe`/`describe_lines` previously retyped by hand.
    ///
    /// `id` is the id printed in the `drive lease acquire {id}` hint every
    /// message carries. `target` is how the caller wants the file named in
    /// the two variants (`NoLease`/`Stale`) that name it at all — already
    /// rendered exactly as the caller wants it to read (e.g. `'Budget'` or
    /// a bare, pre-sanitized id), since callers disagree on whether that
    /// name is quoted. `Expired`/`WrongFile` reference no target at all, so
    /// they ignore it.
    ///
    /// Returns [`None`] for `Failed`, whose detail is deliberately not
    /// rendered here: every caller already has its own `Failed { detail }`
    /// arm for every non-lease operational failure too, so folding it in
    /// here would just be a second place that formats it. Never more than
    /// one line, so `Option<String>` rather than `Vec<String>` — a caller
    /// wanting a `Vec` (an engine's own multi-line `describe_lines`) gets
    /// one back via `Option`'s own `IntoIterator`.
    pub(crate) fn describe_line(&self, id: &str, target: &str) -> Option<String> {
        match self {
            Self::NoLease => Some(format!(
                "Refused: {target} requires a Drive write lease — run `omni-dev drive lease \
                 acquire {id}` and pass the printed token via `--lease`."
            )),
            Self::Expired => Some(format!(
                "Refused: the presented lease is expired, released, or unknown to this ledger \
                 — run `omni-dev drive lease acquire {id}` again."
            )),
            Self::WrongFile => Some(format!(
                "Refused: the presented lease was acquired for a different file — run \
                 `omni-dev drive lease acquire {id}` for this one."
            )),
            Self::Stale => Some(format!(
                "Refused: {target} changed since the lease was acquired (or last written \
                 under) — re-run `omni-dev drive lease acquire {id}` to lease the current \
                 version."
            )),
            Self::Failed(_) => None,
        }
    }
}

/// Converts a [`LeaseGateRefusal`] into an engine's own `*Result` enum —
/// implemented once per engine (`EditResult`/`WriteResult`/
/// `StructureResult`/`ProtectionResult`/`FormatResult`/`ValidationResult`/
/// `DocsWriteResult`, one `RefusedNoLease`/`RefusedLeaseExpired`/
/// `RefusedLeaseWrongFile`/`RefusedLeaseStale`/`Failed { detail }` arm each)
/// so [`LeaseGateRefusal::into_result`] collapses the five-arm match every
/// engine previously repeated verbatim at its own call site into one call.
pub(crate) trait FromLeaseRefusal {
    /// No `--lease` was presented at all.
    fn from_no_lease() -> Self;
    /// The token is unknown, has expired, or the ledger was unreadable.
    fn from_lease_expired() -> Self;
    /// The token is bound to a different file id.
    fn from_lease_wrong_file() -> Self;
    /// The file has moved since the lease's recorded `version`.
    fn from_lease_stale() -> Self;
    /// An operational failure, not a verdict on the token.
    fn from_lease_failed(detail: String) -> Self;
}

impl LeaseGateRefusal {
    /// Maps this refusal onto `T`'s own equivalent variant — see
    /// [`FromLeaseRefusal`].
    pub(crate) fn into_result<T: FromLeaseRefusal>(self) -> T {
        match self {
            Self::NoLease => T::from_no_lease(),
            Self::Expired => T::from_lease_expired(),
            Self::WrongFile => T::from_lease_wrong_file(),
            Self::Stale => T::from_lease_stale(),
            Self::Failed(detail) => T::from_lease_failed(detail),
        }
    }
}

/// Fetches `write.file_id`'s live version/`modifiedTime` and checks
/// `lease_token` against it via [`check_and_lock_lease`] — the "fetch, then
/// check" sequence every leased-write engine repeats verbatim (ADR-0080
/// §9). Returns the still-held [`LedgerLock`] on success (see
/// [`check_and_lock_lease`]'s own doc comment for why the caller must keep
/// it alive across the mutating call and into [`finish_leased_write`]/
/// [`finish_leased_native_write`]) inside its [`LeaseGrant`], or the
/// reason for refusal.
///
/// A caller must build its mutating request *before* calling this — this
/// must be the last fallible step before the mutating call itself. A
/// success here fsyncs the write-ahead `pending` audit record, and nothing
/// else concludes it except the mutating call's own `allowed`/`failed`
/// outcome; a fallible step still ahead of the caller (building the
/// request, resolving its target) can refuse *after* that record is
/// written, leaving it orphaned as though the process had died mid-write
/// (#1688).
pub(crate) async fn gate_leased_write(
    write: LeasedWrite<'_>,
    files_api: &FilesApi<'_>,
    lease_token: Option<&str>,
) -> Result<LeaseGrant, LeaseGateRefusal> {
    let (live_version, live_modified_time) = files_api
        .get_metadata(write.file_id)
        .await
        .map(|fresh| (fresh.version, fresh.modified_time))
        .map_err(|err| LeaseGateRefusal::Failed(err.to_string()))?;
    check_and_lock_lease(
        write,
        lease_token,
        live_version.as_deref(),
        live_modified_time.as_deref(),
    )
    .await
}

/// [`gate_leased_write`], but honouring `requires_lease` (ADR-0080 §13/§9):
/// the deciding folder rule's `require_lease` governs whether a lease is
/// *mandatory*, not whether a presented one is *honoured*. When the rule
/// does not require a lease and the caller presented none either, this
/// short-circuits to `Ok(None)` before issuing `files.get` — an unleased
/// write under a `require_lease: false` rule costs exactly the zero extra
/// round-trips it costs today. In every other case — the rule requires a
/// lease, or the caller presented a token anyway — this defers to
/// [`gate_leased_write`] unchanged, so a volunteered token under a
/// non-requiring rule is validated, consumed and audited exactly like a
/// required one, including refusing a stale/wrong-file/expired token
/// outright rather than silently dropping it.
///
/// Centralised here rather than duplicated as an `if requires_lease` guard
/// in each of the seven engines, for the same reason [`gate_leased_write`]
/// itself is centralised: a bug in this policy fixed in one copy but not
/// the others is worse than one copy reviewed six times.
///
/// `--dry-run` callers never reach this function at all (every engine's
/// `dry_run` early return precedes the lease check), so `--dry-run --lease`
/// still validates nothing — nothing is mutated, so there is nothing to
/// consume.
pub(crate) async fn gate_optional_leased_write(
    write: LeasedWrite<'_>,
    files_api: &FilesApi<'_>,
    requires_lease: bool,
    lease_token: Option<&str>,
) -> Result<Option<LeaseGrant>, LeaseGateRefusal> {
    if !requires_lease && lease_token.is_none() {
        return Ok(None);
    }
    gate_leased_write(write, files_api, lease_token)
        .await
        .map(Some)
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
///
/// The append is `fsync`ed, so it goes through
/// [`offload_short_blocking_io`] here rather than at each call site — this
/// is the module's single audit-write choke point, so a refusal path added
/// later cannot forget it. Nested inside a caller that already opened its
/// own region the helper is a no-op, which is why the two big regions
/// below can still cover their whole stretch of I/O in one hand-off.
fn write_audit_best_effort(log_prefix: &str, outcome: AuditOutcome) {
    let verdict = outcome.verdict.clone();
    if let Err(err) = offload_short_blocking_io(|| crate::request_log::record_audit_event(outcome))
    {
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
/// Takes `lock` (already held by the caller since
/// [`check_and_lock_lease`]) rather than acquiring its own — acquiring a
/// second time here, in the same process, on the same path, would fail
/// against the lock this call is still holding.
///
/// It takes a bare [`LedgerLock`], not the [`LeaseGrant`] the lock came
/// from, and `LeaseLedger::mutate` re-reads the ledger [`check_and_lock_lease`]
/// already read under that same lock. Both are deliberate (issue #1697):
/// `mutate`'s own doc comment has the reasoning, the short version being
/// that the rewrite it performs covers every row, so building it on a
/// caller's older snapshot would trade one small read for two views of one
/// ledger under a single lock. [`finish_leased_native_write`] and this
/// module's tests also call in with a lock and no grant, so the narrower
/// parameter is what they need.
pub(crate) fn finish_leased_write(
    write: LeasedWrite<'_>,
    lock: &LedgerLock,
    token: &str,
    version: Option<String>,
    modified_time: Option<String>,
) {
    let LeasedWrite {
        log_prefix,
        ledger_path,
        ..
    } = write;
    // One region for both halves — an `fsync`ed audit append and a full
    // ledger rewrite — for the same reason the gate itself takes one (issue
    // #1697). This is a sync fn reachable with no runtime at all, which
    // `offload_short_blocking_io` handles by just running the closure.
    offload_short_blocking_io(|| {
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
        let result = LeaseLedger::mutate(lock, ledger_path, |ledger| {
            ledger.record_write(token, version, modified_time);
        });
        if let Err(err) = result {
            tracing::debug!(
                "{log_prefix}: failed to refresh lease ledger after a successful write: {err}"
            );
        }
    });
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

/// Concludes a leased write against a surface whose mutating call itself
/// returns the file's post-write version/modified time (Drive's
/// `files.update` does) — calls [`finish_leased_write`] on `Ok`,
/// [`record_failed_leased_write`] on `Err`, and returns `outcome` unchanged
/// either way. `version_and_modified_time` extracts the pair from the
/// success value; `detail` renders the error for the audit record —
/// callers disagree on `Display` (`{err}`) vs. the alternate, full-chain
/// form (`{err:#}`), so this takes their existing formatting rather than
/// picking one for them.
///
/// Every leased-write engine previously repeated this "finish on success,
/// record on failure" bookkeeping by hand around its own mutating call —
/// this collapses it to one call, leaving only the per-engine construction
/// of its own success/failure result variant at the call site. The caller
/// still owns `lease_grant`'s lifetime (see [`check_and_lock_lease`]'s doc
/// comment for why it must outlive the mutating call) and must still drop
/// it once its own result is built.
pub(crate) fn conclude_leased_write<T, E>(
    write: LeasedWrite<'_>,
    lease_grant: &Option<LeaseGrant>,
    outcome: Result<T, E>,
    version_and_modified_time: impl FnOnce(&T) -> (Option<String>, Option<String>),
    detail: impl FnOnce(&E) -> String,
) -> Result<T, E> {
    match &outcome {
        Ok(value) => {
            if let Some(grant) = lease_grant {
                let (version, modified_time) = version_and_modified_time(value);
                finish_leased_write(write, &grant.lock, &grant.token, version, modified_time);
            }
        }
        Err(err) => {
            if let Some(grant) = lease_grant {
                record_failed_leased_write(write, &grant.token, &detail(err));
            }
        }
    }
    outcome
}

/// [`conclude_leased_write`] for a surface whose mutating call returns no
/// Drive metadata of its own (Sheets/Docs `batchUpdate`) — calls
/// [`finish_leased_native_write`] on `Ok`, [`record_failed_leased_write`] on
/// `Err`, and returns `outcome` unchanged either way.
pub(crate) async fn conclude_native_leased_write<T, E>(
    write: LeasedWrite<'_>,
    lease_grant: &Option<LeaseGrant>,
    files_api: &FilesApi<'_>,
    outcome: Result<T, E>,
    detail: impl FnOnce(&E) -> String,
) -> Result<T, E> {
    match &outcome {
        Ok(_) => {
            if let Some(grant) = lease_grant {
                finish_leased_native_write(write, &grant.lock, &grant.token, files_api).await;
            }
        }
        Err(err) => {
            if let Some(grant) = lease_grant {
                record_failed_leased_write(write, &grant.token, &detail(err));
            }
        }
    }
    outcome
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::client::DriveClient;
    use crate::drive::lease::ledger::LeaseLedger;
    use crate::test_support::AuditLogGuard;
    use crate::utils::secret::Secret;

    /// This module's own seeding needs a caller-supplied token (never
    /// [`crate::drive::test_support::seed_lease`]'s fixed one — several
    /// tests here seed several tokens in the same ledger) and a 1-hour
    /// expiry (this module's own tests don't care about the 30-minute one
    /// the shared helper's other callers assume), so it delegates to
    /// [`crate::drive::test_support::seed_lease_full`] rather than the
    /// simpler shared wrapper.
    fn seed_lease(ledger_path: &Path, token: &str, file_id: &str, version: &str) {
        crate::drive::test_support::seed_lease_full(
            ledger_path,
            token,
            file_id,
            version,
            chrono::Duration::hours(1),
            LeaseBackup::Bytes {
                path: std::path::PathBuf::from("/tmp/test-backup"),
                sha256: "deadbeef".to_string(),
                size: 0,
            },
        );
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

    #[tokio::test]
    async fn refuses_as_expired_and_logs_the_path_when_the_ledger_is_unreadable() {
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
        )
        .await;
        assert!(matches!(outcome, Err(LeaseGateRefusal::Expired)));

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

    // ── `gate_optional_leased_write` (ADR-0080 §13) ─────────────────────

    #[tokio::test]
    async fn an_unrequired_write_with_no_token_skips_the_gate_entirely() {
        // No `files.get` mock is mounted at all — a `require_lease: false`
        // write presenting no token must never touch the network, exactly
        // as costly as the old `if requires_lease { .. } else { None }`
        // branch it replaces.
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let files_api = FilesApi::new(&client);
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditLogGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");

        let outcome = gate_optional_leased_write(
            leased("edit", &ledger_path, "file-1"),
            &files_api,
            false,
            None,
        )
        .await;
        assert!(matches!(outcome, Ok(None)));
    }

    #[tokio::test]
    async fn an_unrequired_write_with_a_valid_token_is_gated_like_a_required_one() {
        // ADR-0080 §13: `require_lease: false` relaxes the *requirement*,
        // not the *meaning* — a presented token is validated and granted.
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/file-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "file-1", "name": "file-1", "version": "1",
                })),
            )
            .mount(&server)
            .await;
        let files_api = FilesApi::new(&client);
        let dir = tempfile::tempdir().unwrap();
        let audit = AuditLogGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        seed_lease(&ledger_path, "tok-1", "file-1", "1");

        let outcome = gate_optional_leased_write(
            leased("edit", &ledger_path, "file-1"),
            &files_api,
            false,
            Some("tok-1"),
        )
        .await;
        assert!(matches!(outcome, Ok(Some(_))));
        assert_eq!(audit.verdicts(), [verdict::PENDING]);
    }

    #[tokio::test]
    async fn an_unrequired_write_still_refuses_a_stale_presented_token() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/file-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "file-1", "name": "file-1", "version": "1",
                })),
            )
            .mount(&server)
            .await;
        let files_api = FilesApi::new(&client);
        let dir = tempfile::tempdir().unwrap();
        let audit = AuditLogGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        // Seeded at version "0"; the live file (above) is at "1".
        seed_lease(&ledger_path, "tok-1", "file-1", "0");

        let outcome = gate_optional_leased_write(
            leased("edit", &ledger_path, "file-1"),
            &files_api,
            false,
            Some("tok-1"),
        )
        .await;
        assert!(matches!(outcome, Err(LeaseGateRefusal::Stale)));
        assert_eq!(audit.verdicts(), [verdict::REFUSED_LEASE_STALE]);
    }

    // ── the write's own audit trail (ADR-0080 §11) ─────────────────────

    #[tokio::test]
    async fn a_successful_check_writes_a_pending_intent_record() {
        let dir = tempfile::tempdir().unwrap();
        let audit = AuditLogGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        seed_lease(&ledger_path, "tok-1", "file-1", "1");

        let outcome = check_and_lock_lease(
            leased("edit", &ledger_path, "file-1"),
            Some("tok-1"),
            Some("1"),
            Some("2026-09-12T00:00:00Z"),
        )
        .await;
        assert!(outcome.is_ok());

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

    #[tokio::test]
    async fn the_write_is_refused_when_the_intent_record_cannot_be_written() {
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
        )
        .await;
        let Err(LeaseGateRefusal::Failed(detail)) = outcome else {
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

    #[tokio::test]
    async fn each_refusal_writes_its_own_verdict_to_the_audit_log() {
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
            )
            .await,
            Err(LeaseGateRefusal::NoLease)
        ));
        // Unknown token.
        assert!(matches!(
            check_and_lock_lease(
                leased("edit", &ledger_path, "file-1"),
                Some("bogus"),
                Some("1"),
                None
            )
            .await,
            Err(LeaseGateRefusal::Expired)
        ));
        // Bound to a different file.
        assert!(matches!(
            check_and_lock_lease(
                leased("edit", &ledger_path, "some-other-file"),
                Some("tok-1"),
                Some("1"),
                None
            )
            .await,
            Err(LeaseGateRefusal::WrongFile)
        ));
        // Stale version.
        assert!(matches!(
            check_and_lock_lease(
                leased("edit", &ledger_path, "file-1"),
                Some("tok-1"),
                Some("2"),
                None
            )
            .await,
            Err(LeaseGateRefusal::Stale)
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

    #[tokio::test]
    async fn a_best_effort_audit_failure_is_warned_and_swallowed() {
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
            )
            .await,
            Err(LeaseGateRefusal::NoLease)
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

    #[cfg(unix)]
    #[tokio::test]
    async fn a_ledger_lock_failure_writes_a_failed_record_carrying_the_error() {
        // A held lock now makes the check *wait*, not fail (issue #1687
        // point 1 — see `check_and_lock_lease_waits_for_a_concurrent_holder`
        // below), so this exercises the `Failed` branch via a permission
        // failure instead: a 0o500 ledger dir makes the *first* lock
        // creation fail outright, with no waiting loop entered at all. The
        // ledger lives in its own subdirectory, restricted independently of
        // `dir.path()` itself, so the *audit log* (a sibling of the ledger
        // dir, not inside it) stays writable — otherwise this would also
        // block the very `failed` record the test asserts on.
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let audit = AuditLogGuard::redirect(dir.path());
        let ledger_dir = dir.path().join("locked");
        std::fs::create_dir(&ledger_dir).unwrap();
        let ledger_path = ledger_dir.join("lease-ledger.jsonl");
        seed_lease(&ledger_path, "tok-1", "file-1", "1");
        std::fs::set_permissions(&ledger_dir, std::fs::Permissions::from_mode(0o500)).unwrap();

        let outcome = check_and_lock_lease(
            leased("edit", &ledger_path, "file-1"),
            Some("tok-1"),
            Some("1"),
            None,
        )
        .await;

        std::fs::set_permissions(&ledger_dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(matches!(outcome, Err(LeaseGateRefusal::Failed(_))));

        // An operational failure, not a verdict about the token: `failed`,
        // with the reason, and no `pending` record since no mutating call
        // was ever authorised.
        let records = audit.records();
        assert_eq!(audit.verdicts(), [verdict::FAILED], "{records:?}");
        let error = records[0].error.as_deref().unwrap_or_default();
        assert!(error.contains("failed to lock"), "{error}");
    }

    #[tokio::test]
    async fn a_ledger_lock_wait_timeout_writes_a_failed_record_carrying_the_error() {
        // The Busy -> wait -> timeout arm (issue #1738): the lock is held
        // for longer than the budget, so the waiting loop runs to
        // exhaustion — unlike the test above, which never enters it.
        let dir = tempfile::tempdir().unwrap();
        let audit = AuditLogGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        seed_lease(&ledger_path, "tok-1", "file-1", "1");
        let _held = LedgerLock::acquire(&ledger_path).unwrap();

        let outcome = check_and_lock_lease_with_timeout(
            leased("edit", &ledger_path, "file-1"),
            Some("tok-1"),
            Some("1"),
            None,
            Duration::from_millis(150),
        )
        .await;

        assert!(
            matches!(&outcome, Err(LeaseGateRefusal::Failed(detail)) if detail.contains("timed out")),
            "expected a timed-out Failed refusal"
        );
        let records = audit.records();
        assert_eq!(audit.verdicts(), [verdict::FAILED], "{records:?}");
        let error = records[0].error.as_deref().unwrap_or_default();
        assert!(error.contains("timed out"), "{error}");
    }

    #[tokio::test]
    async fn check_and_lock_lease_waits_for_a_concurrent_holder_then_succeeds() {
        // The direct regression test for issue #1687 point 1: a write to
        // this ledger that is already locked by another (unrelated) write
        // must queue behind it, not hard-fail.
        let dir = tempfile::tempdir().unwrap();
        let audit = AuditLogGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        seed_lease(&ledger_path, "tok-1", "file-1", "1");
        let held = LedgerLock::acquire(&ledger_path).unwrap();

        let releaser = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            drop(held);
        });

        let outcome = check_and_lock_lease(
            leased("edit", &ledger_path, "file-1"),
            Some("tok-1"),
            Some("1"),
            None,
        )
        .await;
        releaser.join().unwrap();

        assert!(
            outcome.is_ok(),
            "waited holder should have released the lock"
        );
        assert_eq!(audit.verdicts(), [verdict::PENDING]);
    }

    // ── `LeaseGateRefusal::log_status`/`describe_line` ──────────────────

    #[test]
    fn log_status_matches_the_verdict_constants() {
        assert_eq!(
            LeaseGateRefusal::NoLease.log_status(),
            verdict::REFUSED_NO_LEASE
        );
        assert_eq!(
            LeaseGateRefusal::Expired.log_status(),
            verdict::REFUSED_LEASE_EXPIRED
        );
        assert_eq!(
            LeaseGateRefusal::WrongFile.log_status(),
            verdict::REFUSED_LEASE_WRONG_FILE
        );
        assert_eq!(
            LeaseGateRefusal::Stale.log_status(),
            verdict::REFUSED_LEASE_STALE
        );
        assert_eq!(
            LeaseGateRefusal::Failed("boom".to_string()).log_status(),
            verdict::FAILED
        );
    }

    #[test]
    fn describe_line_renders_the_target_and_id_where_each_variant_names_one() {
        assert_eq!(
            LeaseGateRefusal::NoLease.describe_line("file-1", "'Budget'"),
            Some(
                "Refused: 'Budget' requires a Drive write lease — run `omni-dev drive lease \
                 acquire file-1` and pass the printed token via `--lease`."
                    .to_string()
            )
        );
        assert_eq!(
            LeaseGateRefusal::Expired.describe_line("file-1", "'Budget'"),
            Some(
                "Refused: the presented lease is expired, released, or unknown to this ledger \
                 — run `omni-dev drive lease acquire file-1` again."
                    .to_string()
            )
        );
        assert_eq!(
            LeaseGateRefusal::WrongFile.describe_line("file-1", "'Budget'"),
            Some(
                "Refused: the presented lease was acquired for a different file — run \
                 `omni-dev drive lease acquire file-1` for this one."
                    .to_string()
            )
        );
        assert_eq!(
            LeaseGateRefusal::Stale.describe_line("file-1", "'Budget'"),
            Some(
                "Refused: 'Budget' changed since the lease was acquired (or last written \
                 under) — re-run `omni-dev drive lease acquire file-1` to lease the current \
                 version."
                    .to_string()
            )
        );
        assert_eq!(
            LeaseGateRefusal::Failed("boom".to_string()).describe_line("file-1", "'Budget'"),
            None
        );
    }

    /// `multi_thread` on purpose — the only flavor on which the gate's
    /// `offload_short_blocking_io` regions actually reach
    /// `tokio::task::block_in_place` (issue #1697). Two things are pinned
    /// at once: the gate still works there (a bare `block_in_place` would
    /// have been fine here and panicked in every other test in this file),
    /// and its audit records still land in *this* test's redirected route.
    /// The second is the load-bearing half: `request_log` routes the audit
    /// file through a thread-local, so an offload that moved the write to
    /// another thread — `spawn_blocking` would — writes the forensic record
    /// somewhere else entirely, which no assertion on the refusal itself
    /// would catch.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_gate_writes_its_audit_records_on_the_calling_thread_when_it_offloads() {
        let dir = tempfile::tempdir().unwrap();
        let audit = AuditLogGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        seed_lease(&ledger_path, "tok-1", "file-1", "1");

        // `LeaseGateRefusal` is deliberately not `Debug`, so no `expect`.
        let Ok(grant) = check_and_lock_lease(
            leased("edit", &ledger_path, "file-1"),
            Some("tok-1"),
            Some("1"),
            Some("2026-09-11T00:00:00Z"),
        )
        .await
        else {
            panic!("a live, correctly-bound, fresh lease must be granted");
        };

        finish_leased_write(
            leased("edit", &ledger_path, "file-1"),
            &grant.lock,
            &grant.token,
            Some("2".to_string()),
            None,
        );
        drop(grant);

        assert_eq!(
            audit.verdicts(),
            [verdict::PENDING, verdict::ALLOWED],
            "{:?}",
            audit.records()
        );
        let reloaded = LeaseLedger::load(&ledger_path).unwrap();
        assert_eq!(reloaded.get("tok-1").unwrap().version, "2");
    }

    /// Pins the reload issue #1697 proposed removing. `finish_leased_write`
    /// goes through `LeaseLedger::mutate`, which re-reads the ledger rather
    /// than rewriting a copy loaded earlier under the same lock — so a row
    /// changed in between survives. `drive lease restore` depends on exactly
    /// this shape, stamping its backup row through the grant's own lock; it
    /// stamps *after* `finish_leased_write` today, but nothing in the type
    /// system says it must, and a cached ledger would silently drop the
    /// stamp if that order ever flipped.
    #[test]
    fn finish_leased_write_preserves_a_ledger_change_made_under_the_same_lock() {
        let dir = tempfile::tempdir().unwrap();
        let _audit = AuditLogGuard::redirect(dir.path());
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        let lock = LedgerLock::acquire(&ledger_path).unwrap();
        seed_lease(&ledger_path, "tok-1", "file-1", "1");

        // What `restore::stamp_backup_restored` does: another row's update,
        // through the lock this write is already holding.
        LeaseLedger::mutate(&lock, &ledger_path, |ledger| {
            ledger.mark_restored("tok-1", chrono::Utc::now(), Some(42));
        })
        .unwrap();

        finish_leased_write(
            leased("edit", &ledger_path, "file-1"),
            &lock,
            "tok-1",
            Some("2".to_string()),
            None,
        );

        let reloaded = LeaseLedger::load(&ledger_path).unwrap();
        let record = reloaded.get("tok-1").unwrap();
        assert_eq!(record.version, "2", "the write's own refresh must land");
        assert_eq!(
            record.restored_sheet_id,
            Some(42),
            "the change made under the same lock must survive the refresh"
        );
        assert!(record.restored_at.is_some());
    }
}
