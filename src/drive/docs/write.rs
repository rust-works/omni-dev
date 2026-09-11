//! `drive docs replace`/`append` engines — text mutation gated by the
//! ADR-0071 write-permission rules and leased against a document revision
//! (issue #1615, [ADR-0076](../../../docs/adrs/adr-0076.md)).
//!
//! Single-target, so this follows `sheets/write.rs`'s linear shape — a
//! public wrapper that logs, and an `_inner` that classifies then mutates —
//! rather than `file_move.rs`'s Plan/Execute split. `--dry-run` and a real
//! run therefore share the same gate classification *by construction* (same
//! function, same early return), and Docs adds one strengthening: the
//! preview values are computed from the snapshot **before** the branch and
//! are the same values on both paths, so the two cannot disagree about what
//! they saw.
//!
//! Two refusals happen **before** the gate, because they are not policy
//! decisions — the operation is simply nonsensical for that target: anything
//! that is not a Google Doc, and a shortcut (even one pointing at a Doc,
//! since we don't follow shortcuts). This is the mirror image of
//! `content_edit.rs`'s Google-native refusal.
//!
//! The ordering of the rest is deliberate and is ADR-0076 §3 and §8:
//!
//! 1. The **gate runs before `documents.get`**. The naive ordering — read
//!    first, so the preview is nicer — would hand the document body to a
//!    caller the gate is about to refuse.
//! 2. A missing `revisionId` is refused rather than written unleased. Google
//!    populates it only for callers with edit access, so its absence means
//!    the write would fail anyway; refusing keeps "there is no unleased
//!    path" true without exception.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::cli::drive::format::{write_scalar_jsonl, JsonlSerialize};
use crate::drive::client::DriveClient;
use crate::drive::docs::api::{is_stale_revision, DocsApi, SuggestionsViewMode};
use crate::drive::docs::client::DocsClient;
use crate::drive::docs::write_types::DocsRequest;
use crate::drive::files_api::FilesApi;
use crate::drive::folder_ancestry;
use crate::drive::lease::check::{
    check_and_lock_lease, refresh_lease_after_native_write, LeaseCheckOutcome,
};
use crate::drive::types::{GOOGLE_DOC_MIME_TYPE, GOOGLE_SHORTCUT_MIME_TYPE};
use crate::drive::write_gate::{self, DecidingRule, DriveOperation, FolderPermissionRule};
use crate::request_log::{self, DriveMutationOutcome};

/// What to mutate.
///
/// An enum carrying its own data rather than a verb discriminant plus a bag
/// of `Option`s, so "a replace with no search text" is unrepresentable. This
/// is the one place this module improves on `sheets/write.rs`'s template
/// rather than copying it: that module's `WriteVerb::Clear` sits beside a
/// `values` field which must be empty, and `describe` has to special-case
/// the pairing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WritePayload {
    /// Replace every occurrence of `search` with `replace`.
    Replace {
        /// The literal substring to find. Never a regex.
        search: String,
        /// What to put in its place.
        replace: String,
        /// Whether matching is case-sensitive.
        match_case: bool,
    },
    /// Append `text` to the end of the document body.
    Append {
        /// The text to insert.
        text: String,
    },
}

impl WritePayload {
    /// Which verb this payload is.
    #[must_use]
    pub const fn verb(&self) -> WriteVerb {
        match self {
            Self::Replace { .. } => WriteVerb::Replace,
            Self::Append { .. } => WriteVerb::Append,
        }
    }

    /// Rejects a payload that cannot produce a valid request, before any
    /// network call.
    ///
    /// The API rejects an empty `containsText.text`, and an empty append is
    /// a no-op worth naming rather than a round-trip worth spending.
    fn validate(&self) -> Result<(), String> {
        match self {
            Self::Replace { search, .. } if search.is_empty() => {
                Err("--search cannot be empty".to_string())
            }
            Self::Append { text } if text.is_empty() => {
                Err("nothing to append: the text is empty".to_string())
            }
            _ => Ok(()),
        }
    }

    /// The single `DocsRequest` this payload sends.
    fn to_request(&self) -> DocsRequest {
        match self {
            Self::Replace {
                search,
                replace,
                match_case,
            } => DocsRequest::replace_all_text(search, replace, *match_case),
            Self::Append { text } => DocsRequest::insert_text_at_end(text),
        }
    }
}

/// Which text mutation to perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteVerb {
    /// Replace every occurrence of some text.
    Replace,
    /// Append text to the end of the document.
    Append,
}

impl WriteVerb {
    /// The `operation` this verb records in the request log.
    ///
    /// `build_drive_mutation_record` shapes `command` as `["drive",
    /// <operation>]`, so these read as `drive docs-replace` in the log even
    /// though the CLI spells it `drive docs replace`.
    const fn log_operation(self) -> &'static str {
        match self {
            Self::Replace => "docs-replace",
            Self::Append => "docs-append",
        }
    }

    /// The CLI spelling, for messages.
    const fn label(self) -> &'static str {
        match self {
            Self::Replace => "replace",
            Self::Append => "append",
        }
    }
}

/// Per-call options.
#[derive(Debug, Clone)]
pub struct WriteOptions {
    /// The document to mutate.
    pub document_id: String,
    /// What to do to it.
    pub payload: WritePayload,
    /// Classify and preview, but send no mutation.
    pub dry_run: bool,
    /// The lease token presented via `--lease`. Checked only when the
    /// deciding rule requires one
    /// ([`write_gate::decided_rule_requires_lease`], ADR-0080 §1/§9);
    /// `None` is only ever valid when it does not.
    pub lease_token: Option<String>,
    /// Path to the lease ledger the token is checked against. Production
    /// callers pass `crate::drive::lease::ledger::ledger_path`'s own
    /// result; tests pass a path under a `tempdir`.
    pub ledger_path: PathBuf,
}

/// What happened (or, under `--dry-run`, would happen).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum WriteResult {
    /// A dry run of a replace: how many occurrences the *snapshot* held.
    WouldReplace {
        /// Counted client-side. Display-only — see [`count_occurrences`].
        occurrences: usize,
    },
    /// A dry run of an append.
    WouldAppend {
        /// The body's last `endIndex`, purely informational — nothing
        /// computes from it (ADR-0076 §5).
        #[serde(skip_serializing_if = "Option::is_none")]
        document_end_index: Option<i64>,
        /// Unicode scalar values being inserted.
        chars: usize,
        /// UTF-8 bytes being inserted.
        bytes: usize,
    },
    /// The target is not a Google Doc.
    RefusedNotADocument {
        /// Its actual mime type.
        mime_type: String,
    },
    /// The target is a shortcut, which is never followed.
    RefusedShortcut,
    /// The target has no parent folder this account can see, so no
    /// folder rule can apply and no `file_id` rule named it either.
    RefusedNoVisibleParents,
    /// `documents.get` returned no `revisionId`, which Google populates only
    /// for callers with edit access — so no lease can be taken, and there is
    /// no unleased path (ADR-0076 §3).
    RefusedNoRevisionId,
    /// The write-permission gate refused it.
    Blocked {
        /// The rule that decided, when one did.
        decided_by: Option<DecidingRule>,
    },
    /// No `--lease` was presented, and the deciding rule requires one
    /// (ADR-0080 §9).
    RefusedNoLease,
    /// The presented lease has expired, or was never a token this ledger
    /// knows about.
    RefusedLeaseExpired,
    /// The presented lease is bound to a different file id.
    RefusedLeaseWrongFile,
    /// The file has moved since the lease's recorded `version` — the
    /// staleness check (ADR-0080 §6). Distinct from [`Self::StaleRevision`],
    /// which is ADR-0076's own, unrelated Docs revision check.
    RefusedLeaseStale,
    /// A replace landed.
    Replaced {
        /// What the *server* reported changing.
        ///
        /// Not `Option`: a replace always has an answer, and "nothing
        /// matched" is a count rather than an absence of one. Docs omits
        /// `occurrencesChanged` entirely when it is zero (proto3), so the
        /// zero is resolved at the boundary — see
        /// [`crate::drive::docs::write_types::BatchUpdateDocumentResponse::occurrences_changed_for_replace`].
        occurrences_changed: i64,
    },
    /// An append landed.
    Appended {
        /// Unicode scalar values inserted.
        chars: usize,
        /// UTF-8 bytes inserted.
        bytes: usize,
    },
    /// The document changed between the read that computed this mutation
    /// and the `batchUpdate` that would have applied it. Nothing was
    /// written — a one-request batch is atomic.
    StaleRevision {
        /// The lease presented, for the log and for support.
        required_revision_id: String,
        /// The server's own message, unaltered.
        detail: String,
    },
    /// Anything else.
    Failed {
        /// The error, verbatim.
        detail: String,
    },
}

impl WriteResult {
    /// The kebab-case status for the request log.
    const fn log_status(&self) -> &'static str {
        match self {
            Self::WouldReplace { .. } => "would-replace",
            Self::WouldAppend { .. } => "would-append",
            Self::RefusedNotADocument { .. } => "refused-not-a-document",
            Self::RefusedShortcut => "refused-shortcut",
            Self::RefusedNoVisibleParents => "refused-no-visible-parents",
            Self::RefusedNoRevisionId => "refused-no-revision-id",
            Self::Blocked { .. } => "blocked",
            Self::RefusedNoLease => "refused-no-lease",
            Self::RefusedLeaseExpired => "refused-lease-expired",
            Self::RefusedLeaseWrongFile => "refused-lease-wrong-file",
            Self::RefusedLeaseStale => "refused-lease-stale",
            Self::Replaced { .. } => "replaced",
            Self::Appended { .. } => "appended",
            Self::StaleRevision { .. } => "stale-revision",
            Self::Failed { .. } => "failed",
        }
    }
}

/// The full outcome of one attempt.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WriteOutcome {
    /// The document acted on.
    pub document_id: String,
    /// Its name at the time of the attempt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    /// The folder the gate evaluated against, when a chain was walked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_folder_id: Option<String>,
    /// The lease presented, once one was minted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_revision_id: Option<String>,
    /// What happened.
    pub result: WriteResult,
}

impl JsonlSerialize for WriteOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> anyhow::Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Counts non-overlapping occurrences of `needle` in `haystack`.
///
/// **Display-only.** ADR-0076 §7 makes this an invariant rather than a
/// convention: the count never gates the mutation, and a count of zero still
/// sends the `batchUpdate`. It is an estimate over the body text this
/// command read, while the server matches over its own view — a match may
/// span a `textRun` boundary, or sit in a segment (a header, a footer, a
/// footnote) this crate does not fetch. Acting on it would let `omni-dev`
/// report "nothing to do" for a document that does have matches.
#[must_use]
pub fn count_occurrences(haystack: &str, needle: &str, match_case: bool) -> usize {
    if needle.is_empty() {
        return 0;
    }
    if match_case {
        return haystack.matches(needle).count();
    }
    // Lowercasing can change byte length (e.g. 'İ'), so this counts over
    // *both* sides lowercased rather than indexing back into the original.
    haystack
        .to_lowercase()
        .matches(&needle.to_lowercase())
        .count()
}

/// Runs one text mutation, logging every attempt that isn't a dry run.
///
/// Never returns `Err`: every failure is a [`WriteResult`] variant, so the
/// caller renders one shape and the log records one shape. Exit code stays 0
/// regardless, matching ADR-0076 §13.
pub async fn write(
    drive: &DriveClient,
    docs: &DocsClient,
    opts: &WriteOptions,
    rules: &[FolderPermissionRule],
) -> WriteOutcome {
    let started = Instant::now();
    let outcome = write_inner(drive, docs, opts, rules).await;
    if !opts.dry_run {
        record_attempt(&outcome, opts, started.elapsed());
    }
    outcome
}

async fn write_inner(
    drive: &DriveClient,
    docs: &DocsClient,
    opts: &WriteOptions,
    rules: &[FolderPermissionRule],
) -> WriteOutcome {
    let bare = |result| WriteOutcome {
        document_id: opts.document_id.clone(),
        file_name: None,
        resolved_folder_id: None,
        required_revision_id: None,
        result,
    };

    // ── Pure validation, before any request ────────────────────────────
    if let Err(detail) = opts.payload.validate() {
        return bare(WriteResult::Failed { detail });
    }

    let files_api = FilesApi::new(drive);
    let target = match files_api.get_metadata(&opts.document_id).await {
        Ok(target) => target,
        Err(err) => {
            return bare(WriteResult::Failed {
                detail: err.to_string(),
            })
        }
    };

    let with_target = |result| WriteOutcome {
        document_id: opts.document_id.clone(),
        file_name: Some(target.name.clone()),
        resolved_folder_id: None,
        required_revision_id: None,
        result,
    };

    // ── Refusals that precede the gate ─────────────────────────────────
    if target.mime_type == GOOGLE_SHORTCUT_MIME_TYPE {
        return with_target(WriteResult::RefusedShortcut);
    }
    if target.mime_type != GOOGLE_DOC_MIME_TYPE {
        return with_target(WriteResult::RefusedNotADocument {
            mime_type: target.mime_type.clone(),
        });
    }

    // ── The gate, before any Docs call ─────────────────────────────────
    // A `file_id` rule is consulted before the parents are looked at, so a
    // Doc with no visible parent can still be granted (issue #1612).
    let evaluated = match folder_ancestry::resolve_decision_for_file_target(
        &files_api,
        &target,
        DriveOperation::DocsWrite,
        rules,
    )
    .await
    {
        Ok(evaluated) => evaluated,
        // A chain that could not be resolved is a refusal, never a silent
        // allow — ADR-0071 §3's highest-priority invariant.
        Err(err) => {
            return with_target(WriteResult::Failed {
                detail: err.to_string(),
            })
        }
    };

    // Only *after* the file-rule lookup has come up empty is "no visible
    // parents" the real story.
    if evaluated.source == folder_ancestry::DecisionSource::NoVisibleParents {
        return with_target(WriteResult::RefusedNoVisibleParents);
    }

    let folder_ancestry::FileTargetDecision {
        decision,
        resolved_folder_id,
        requires_lease,
        ..
    } = evaluated;

    let gated = |result, revision: Option<String>| WriteOutcome {
        document_id: opts.document_id.clone(),
        file_name: Some(target.name.clone()),
        resolved_folder_id: resolved_folder_id.clone(),
        required_revision_id: revision,
        result,
    };

    if decision.verdict == write_gate::Verdict::Deny {
        return gated(
            WriteResult::Blocked {
                decided_by: decision.decided_by,
            },
            None,
        );
    }

    // ── The read that mints the lease and computes the preview ─────────
    let api = DocsApi::new(docs);
    let document = match api
        .get_document(&opts.document_id, SuggestionsViewMode::default())
        .await
    {
        Ok(document) => document,
        Err(err) => {
            return gated(
                WriteResult::Failed {
                    detail: err.to_string(),
                },
                None,
            )
        }
    };

    let Some(revision_id) = document.revision_id.clone() else {
        return gated(WriteResult::RefusedNoRevisionId, None);
    };

    // Computed from *this* snapshot, before the dry-run branch, so a dry run
    // and a real run cannot disagree about what they saw (ADR-0076 §7).
    let preview = match &opts.payload {
        WritePayload::Replace {
            search, match_case, ..
        } => {
            // Across every tab, because a `replaceAllText` with no
            // `tabsCriteria` spans every tab (ADR-0076 §11).
            let corpus = document_text(&document);
            WriteResult::WouldReplace {
                occurrences: count_occurrences(&corpus, search, *match_case),
            }
        }
        WritePayload::Append { text } => WriteResult::WouldAppend {
            document_end_index: body_end_index(&document),
            chars: text.chars().count(),
            bytes: text.len(),
        },
    };

    if opts.dry_run {
        return gated(preview, Some(revision_id));
    }

    // The lease check (ADR-0080 §9) sits here: after the permission gate
    // and the `--dry-run` branch, before the mutating call — see
    // `content_edit.rs::edit_inner`'s doc comment for the full reasoning,
    // shared verbatim by every leased engine. This is a *separate*,
    // unrelated staleness check from ADR-0076's own `revisionId` lease
    // above: that one guards the Docs `batchUpdate` itself against a
    // concurrent edit, while this one guards the *lease ledger*'s recorded
    // Drive `version` (ADR-0080 §6). A fresh `files.get` immediately before
    // `batchUpdate`, not a reuse of the metadata fetched before the
    // (potentially slow) ancestor-chain walk and `documents.get` above.
    let lease_lock = if requires_lease {
        let live_version = match files_api.get_metadata(&opts.document_id).await {
            Ok(fresh) => fresh.version,
            Err(err) => {
                return gated(
                    WriteResult::Failed {
                        detail: err.to_string(),
                    },
                    Some(revision_id),
                )
            }
        };
        match check_and_lock_lease(
            "drive docs write",
            &opts.ledger_path,
            opts.lease_token.as_deref(),
            &opts.document_id,
            live_version.as_deref(),
        ) {
            LeaseCheckOutcome::Ok(lock) => Some(lock),
            LeaseCheckOutcome::NoLease => {
                return gated(WriteResult::RefusedNoLease, Some(revision_id))
            }
            LeaseCheckOutcome::Expired => {
                return gated(WriteResult::RefusedLeaseExpired, Some(revision_id))
            }
            LeaseCheckOutcome::WrongFile => {
                return gated(WriteResult::RefusedLeaseWrongFile, Some(revision_id))
            }
            LeaseCheckOutcome::Stale => {
                return gated(WriteResult::RefusedLeaseStale, Some(revision_id))
            }
            LeaseCheckOutcome::Failed(detail) => {
                return gated(WriteResult::Failed { detail }, Some(revision_id))
            }
        }
    } else {
        None
    };

    // ── The mutation ───────────────────────────────────────────────────
    let result = match api
        .batch_update(&opts.document_id, opts.payload.to_request(), &revision_id)
        .await
    {
        Ok(response) => {
            if let (Some(token), Some(lock)) = (&opts.lease_token, &lease_lock) {
                refresh_lease_after_native_write(
                    "drive docs write",
                    lock,
                    &opts.ledger_path,
                    token,
                    &files_api,
                    &opts.document_id,
                )
                .await;
            }
            match &opts.payload {
                WritePayload::Replace { .. } => WriteResult::Replaced {
                    occurrences_changed: response.occurrences_changed_for_replace(),
                },
                WritePayload::Append { text } => WriteResult::Appended {
                    chars: text.chars().count(),
                    bytes: text.len(),
                },
            }
        }
        Err(err) if is_stale_revision(&err) => WriteResult::StaleRevision {
            required_revision_id: revision_id.clone(),
            detail: err.to_string(),
        },
        Err(err) => WriteResult::Failed {
            detail: err.to_string(),
        },
    };
    drop(lease_lock);
    gated(result, Some(revision_id))
}

/// Every tab's text, concatenated — the corpus the occurrence preview counts
/// over.
///
/// Spans every tab because the mutation does (ADR-0076 §11); counting only
/// the first would make the preview a lower bound on a tabbed document.
fn document_text(document: &crate::drive::docs::types::Document) -> String {
    document
        .resolved_tabs()
        .iter()
        .filter_map(|tab| tab.body)
        .flat_map(crate::drive::docs::structure::flatten)
        .map(|element| element.text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The first tab's body end index, for the append preview.
///
/// Informational only — nothing computes an insertion point from it, because
/// `endOfSegmentLocation` lets the server do that (ADR-0076 §5).
fn body_end_index(document: &crate::drive::docs::types::Document) -> Option<i64> {
    document
        .resolved_tabs()
        .first()
        .and_then(|tab| tab.body)
        .and_then(|body| body.content.last())
        .map(crate::drive::docs::types::StructuralElement::end_index)
}

/// Emits the `kind: "drivemutation"` record.
///
/// Inside the engine, never the CLI layer, so a future MCP caller cannot
/// bypass it — and so a `Blocked` outcome, which makes zero API calls, still
/// leaves a trace. Same reasoning as `sheets/write.rs::record_attempt`.
///
/// **The searched, replacement and appended text are never recorded.** They
/// are user prose, and often the most sensitive thing in the invocation —
/// sharper than the Sheets case, where an A1 range is metadata rather than
/// content. Only counts and the opaque revision id go in.
fn record_attempt(outcome: &WriteOutcome, opts: &WriteOptions, duration: Duration) {
    let error = match &outcome.result {
        // A lost lease is recorded as an error alongside its own
        // `stale-revision` status: the status says what happened, the
        // detail carries the server's own message for support.
        WriteResult::Failed { detail } | WriteResult::StaleRevision { detail, .. } => {
            Some(detail.clone())
        }
        _ => None,
    };
    let decided_by = match &outcome.result {
        WriteResult::Blocked { decided_by } => decided_by.as_ref(),
        _ => None,
    };
    let decided_by = write_gate::decided_by_log_fields(decided_by);
    let occurrences_changed = match &outcome.result {
        WriteResult::Replaced {
            occurrences_changed,
        } => Some(*occurrences_changed),
        _ => None,
    };
    let inserted_chars = match &outcome.result {
        WriteResult::Appended { chars, .. } => Some(*chars as i64),
        _ => None,
    };

    request_log::record_drive_mutation(DriveMutationOutcome {
        operation: opts.payload.verb().log_operation(),
        file_id: outcome.document_id.clone(),
        file_name: outcome.file_name.clone().unwrap_or_default(),
        status: outcome.result.log_status().to_string(),
        resolved_folder_id: outcome.resolved_folder_id.clone(),
        decided_by_folder_id: decided_by.folder_id,
        decided_by_depth: decided_by.depth,
        decided_by_file_id: decided_by.file_id,
        occurrences_changed,
        inserted_chars,
        required_revision_id: outcome.required_revision_id.clone(),
        error,
        duration,
        ..Default::default()
    });
}

/// Renders an outcome as a single human-readable line.
///
/// Lives here rather than in the CLI layer so the CLI and a future MCP
/// caller describe an outcome identically.
#[must_use]
pub fn describe(outcome: &WriteOutcome, verb: WriteVerb) -> String {
    let name = outcome.file_name.as_deref().unwrap_or(&outcome.document_id);
    match &outcome.result {
        WriteResult::WouldReplace { occurrences } => format!(
            "Would replace: {occurrences} occurrence(s) in '{name}' \
             (counted from the copy just read)"
        ),
        WriteResult::WouldAppend {
            document_end_index,
            chars,
            bytes,
        } => {
            let where_ = document_end_index
                .map(|index| format!(" (body ends at index {index})"))
                .unwrap_or_default();
            format!("Would append: {chars} char(s) / {bytes} byte(s) to '{name}'{where_}")
        }
        WriteResult::RefusedNotADocument { mime_type } => format!(
            "Refused: '{name}' is not a Google Doc (mimeType: {mime_type}); \
             `drive docs {}` only works on Google Docs",
            verb.label()
        ),
        WriteResult::RefusedShortcut => format!(
            "Refused: '{name}' is a shortcut; `drive docs {}` doesn't follow shortcuts — \
             resolve the target document's id and use that instead",
            verb.label()
        ),
        WriteResult::RefusedNoVisibleParents => format!(
            "Refused: '{name}' has no parent folder visible to this account, so no folder \
             rule can apply to it. This is normal for a Doc shared by link or email. \
             Grant it by id instead: add {{\"file_id\": \"<document id>\", \"allow\": \
             [\"docs-write\"]}} to write_permissions.rules. (Adding it to a folder in your \
             own Drive and granting that folder `docs-write` also works.)"
        ),
        WriteResult::RefusedNoRevisionId => format!(
            "Refused: '{name}' returned no revision id, which Google sends only to callers \
             with edit access — so this write cannot be leased against a known version. \
             Request edit access, or check the account in use."
        ),
        WriteResult::Blocked { decided_by } => match decided_by {
            // The same one-line shape the other five `describe`/report
            // sites use, so the wording cannot drift between them.
            Some(rule) => format!(
                "Blocked: '{name}' — refused by rule on {} {}{}",
                rule.kind_label(),
                rule.id(),
                rule.depth_suffix()
            ),
            None => format!("Blocked: '{name}' — refused by default policy (no matching rule)"),
        },
        WriteResult::RefusedNoLease => format!(
            "Refused: '{name}' requires a Drive write lease — run `omni-dev drive lease \
             acquire {}` and pass the printed token via `--lease`.",
            outcome.document_id
        ),
        WriteResult::RefusedLeaseExpired => format!(
            "Refused: the presented lease is expired, released, or unknown to this ledger — \
             run `omni-dev drive lease acquire {}` again.",
            outcome.document_id
        ),
        WriteResult::RefusedLeaseWrongFile => format!(
            "Refused: the presented lease was acquired for a different file — run `omni-dev \
             drive lease acquire {}` for this one.",
            outcome.document_id
        ),
        WriteResult::RefusedLeaseStale => format!(
            "Refused: '{name}' changed since the lease was acquired (or last written under) \
             — re-run `omni-dev drive lease acquire {}` to lease the current version.",
            outcome.document_id
        ),
        WriteResult::Replaced {
            occurrences_changed,
        } => format!("Replaced: {occurrences_changed} occurrence(s) in '{name}'"),
        WriteResult::Appended { chars, bytes } => {
            format!("Appended: {chars} char(s) / {bytes} byte(s) to '{name}'")
        }
        WriteResult::StaleRevision {
            required_revision_id,
            ..
        } => format!(
            "Refused: '{name}' changed since it was read (revision lease \
             {required_revision_id} no longer current) — nothing was written. \
             Re-run to apply against the current version."
        ),
        WriteResult::Failed { detail } => format!("Failed: '{name}': {detail}"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::docs::client::DOCS_API_URL;
    use crate::drive::types::GOOGLE_SHEET_MIME_TYPE;
    use crate::test_support::env::MapEnv;
    use crate::utils::secret::Secret;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_credentials() -> DriveCredentials {
        DriveCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: DriveGrantedScopes::READONLY,
        }
    }

    /// Both hosts point at one wiremock server.
    ///
    /// `replace_session` swaps the Drive client's whole transport, so it
    /// must run **before** the derive — deriving first would leave the Docs
    /// client holding the original session, pointed at the real
    /// `oauth2.googleapis.com`, and make a live network call from a test.
    async fn clients(server: &MockServer) -> (DriveClient, DocsClient) {
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "test-token", "expires_in": 3600,
            })))
            .mount(server)
            .await;
        let mut drive = DriveClient::new(&server.uri(), &test_credentials()).unwrap();
        crate::drive::client::test_support::replace_session(
            &mut drive,
            &test_credentials(),
            &format!("{}/token", server.uri()),
        );
        let env = MapEnv::new().with(DOCS_API_URL, &server.uri());
        let docs = DocsClient::from_drive_client_with(&env, &drive).unwrap();
        (drive, docs)
    }

    /// `version: "1"` throughout — matches [`replace_opts`]'s/
    /// [`append_opts`]'s default seeded lease, so any test reaching the
    /// mutating call has a live, non-stale lease by construction (ADR-0080
    /// §9).
    fn mount_file(id: &str, mime_type: &str, parents: &[&str]) -> Mock {
        let parents: Vec<&str> = parents.to_vec();
        Mock::given(method("GET"))
            .and(path(format!("/drive/v3/files/{id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": id, "name": id, "mimeType": mime_type, "parents": parents,
                "version": "1",
            })))
    }

    /// Seeds `ledger_path` with a fresh, live lease for `document_id` at
    /// `version`, returning its token.
    fn seed_lease(ledger_path: &std::path::Path, document_id: &str, version: &str) -> String {
        // A fixed token, not a random one: every call gets its own isolated
        // ledger (a fresh tempdir), so uniqueness across tests is never a
        // concern.
        let token = "test-lease-token".to_string();
        let mut ledger = crate::drive::lease::ledger::LeaseLedger::default();
        ledger.insert(crate::drive::lease::ledger::LeaseRecord {
            token: token.clone(),
            file_id: document_id.to_string(),
            version: version.to_string(),
            modified_time: None,
            backup: crate::drive::lease::ledger::LeaseBackup::Bytes {
                path: std::path::PathBuf::from("/tmp/test-backup"),
                sha256: "deadbeef".to_string(),
                size: 0,
            },
            acquired_at: chrono::Utc::now(),
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(30),
            released_at: None,
        });
        ledger.save(ledger_path).unwrap();
        token
    }

    /// A fresh, isolated ledger path holding a live lease for `document_id`
    /// at version `"1"` (matching [`mount_file`]'s default).
    fn leased_opts_for(document_id: &str) -> (Option<String>, std::path::PathBuf) {
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        let token = seed_lease(&ledger_path, document_id, "1");
        (Some(token), ledger_path)
    }

    fn mount_folder(id: &str) -> Mock {
        mount_file(id, "application/vnd.google-apps.folder", &[])
    }

    fn mount_document(revision: Option<&str>, text: &str) -> Mock {
        let mut body = serde_json::json!({
            "documentId": "doc-1",
            "body": {"content": [{
                "startIndex": 1, "endIndex": 1 + text.encode_utf16().count() as i64,
                "paragraph": {"elements": [{"textRun": {"content": format!("{text}\n")}}]},
            }]},
        });
        if let Some(revision) = revision {
            body["revisionId"] = serde_json::json!(revision);
        }
        Mock::given(method("GET"))
            .and(path("/v1/documents/doc-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
    }

    fn mount_batch_update(response: serde_json::Value) -> Mock {
        Mock::given(method("POST"))
            .and(path("/v1/documents/doc-1:batchUpdate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
    }

    fn allow_rule(folder: &str) -> FolderPermissionRule {
        FolderPermissionRule {
            folder_id: Some(folder.to_string()),
            file_id: None,
            recursive: true,
            allow: std::iter::once(DriveOperation::DocsWrite).collect(),
            deny: HashSet::default(),
            require_lease: true,
        }
    }

    /// Seeds a fresh, isolated ledger with a live lease for `"doc-1"` at
    /// version `"1"` (matching [`mount_file`]'s default) and returns options
    /// carrying it. Every existing test built before the lease (ADR-0080
    /// §9) reaches its mutating call this way by construction — see
    /// `sheets/structure.rs::opts_for`'s doc comment for why this doesn't
    /// need touching each test individually. A test exercising the lease
    /// *refusal* paths builds `WriteOptions` directly instead (see the "the
    /// Drive write lease" test section below).
    fn replace_opts(dry_run: bool) -> WriteOptions {
        let (lease_token, ledger_path) = leased_opts_for("doc-1");
        WriteOptions {
            document_id: "doc-1".to_string(),
            payload: WritePayload::Replace {
                search: "Q3".to_string(),
                replace: "Q4".to_string(),
                match_case: true,
            },
            dry_run,
            lease_token,
            ledger_path,
        }
    }

    fn append_opts(dry_run: bool) -> WriteOptions {
        let (lease_token, ledger_path) = leased_opts_for("doc-1");
        WriteOptions {
            document_id: "doc-1".to_string(),
            payload: WritePayload::Append {
                text: "hello".to_string(),
            },
            dry_run,
            lease_token,
            ledger_path,
        }
    }

    // ── count_occurrences (pure) ───────────────────────────────────────

    #[test]
    fn count_occurrences_counts_non_overlapping_matches() {
        assert_eq!(count_occurrences("Q3 and Q3 and Q3", "Q3", true), 3);
        assert_eq!(count_occurrences("aaaa", "aa", true), 2);
    }

    #[test]
    fn count_occurrences_honours_case_sensitivity_both_ways() {
        assert_eq!(count_occurrences("it It IT", "it", true), 1);
        assert_eq!(count_occurrences("it It IT", "it", false), 3);
    }

    #[test]
    fn count_occurrences_of_an_empty_needle_is_zero() {
        assert_eq!(count_occurrences("anything", "", true), 0);
        assert_eq!(count_occurrences("anything", "", false), 0);
    }

    #[test]
    fn count_occurrences_handles_multibyte_text() {
        assert_eq!(count_occurrences("😀 x 😀", "😀", true), 2);
        assert_eq!(count_occurrences("ÉCOLE école", "école", false), 2);
    }

    // ── Refusals that precede the gate and every network call ──────────

    #[tokio::test]
    async fn a_non_document_is_refused_before_the_gate_or_any_docs_call() {
        let server = MockServer::start().await;
        let (drive, docs) = clients(&server).await;
        // A spreadsheet with a parent, and permissive rules — yet neither
        // the parent lookup nor any Docs call is mounted, so the refusal
        // must happen before both.
        mount_file("doc-1", GOOGLE_SHEET_MIME_TYPE, &["folder-1"])
            .mount(&server)
            .await;

        let outcome = write(
            &drive,
            &docs,
            &replace_opts(false),
            &[allow_rule("folder-1")],
        )
        .await;
        assert!(matches!(
            outcome.result,
            WriteResult::RefusedNotADocument { .. }
        ));
    }

    #[tokio::test]
    async fn a_shortcut_gets_its_own_refusal_not_the_generic_one() {
        let server = MockServer::start().await;
        let (drive, docs) = clients(&server).await;
        mount_file("doc-1", GOOGLE_SHORTCUT_MIME_TYPE, &["folder-1"])
            .mount(&server)
            .await;

        let outcome = write(
            &drive,
            &docs,
            &replace_opts(false),
            &[allow_rule("folder-1")],
        )
        .await;
        assert_eq!(outcome.result, WriteResult::RefusedShortcut);
        let text = describe(&outcome, WriteVerb::Replace);
        assert!(text.contains("is a shortcut"), "{text}");
        assert!(!text.contains("not a Google Doc"), "{text}");
    }

    #[tokio::test]
    async fn an_empty_search_is_refused_before_any_network_call() {
        let server = MockServer::start().await;
        let (drive, docs) = clients(&server).await;
        // No mocks at all: nothing may be requested.
        let opts = WriteOptions {
            payload: WritePayload::Replace {
                search: String::new(),
                replace: "x".to_string(),
                match_case: true,
            },
            ..replace_opts(false)
        };
        let outcome = write(&drive, &docs, &opts, &[]).await;
        match outcome.result {
            WriteResult::Failed { detail } => assert!(detail.contains("--search"), "{detail}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    // ── The gate ───────────────────────────────────────────────────────

    /// ADR-0076 §3: a blocked write must make **zero** Docs API calls, so
    /// the document body never reaches a caller the gate is refusing.
    #[tokio::test]
    async fn a_denied_target_is_blocked_with_zero_docs_calls() {
        let server = MockServer::start().await;
        let (drive, docs) = clients(&server).await;
        mount_file("doc-1", GOOGLE_DOC_MIME_TYPE, &["folder-1"])
            .mount(&server)
            .await;
        mount_folder("folder-1").mount(&server).await;
        // No `documents.get` and no batchUpdate mounted: reaching either
        // fails the test.

        let outcome = write(&drive, &docs, &replace_opts(false), &[]).await;
        assert!(matches!(outcome.result, WriteResult::Blocked { .. }));
    }

    #[tokio::test]
    async fn a_parentless_document_is_refused_distinctly_not_blocked() {
        let server = MockServer::start().await;
        let (drive, docs) = clients(&server).await;
        mount_file("doc-1", GOOGLE_DOC_MIME_TYPE, &[])
            .mount(&server)
            .await;

        let outcome = write(&drive, &docs, &replace_opts(false), &[]).await;
        assert_eq!(outcome.result, WriteResult::RefusedNoVisibleParents);
        let text = describe(&outcome, WriteVerb::Replace);
        assert!(text.contains("file_id"), "names the actual fix: {text}");
    }

    /// A `file_id` rule reaches a link-shared Doc that no folder rule could
    /// (issue #1612), so the parentless refusal must not pre-empt it.
    #[tokio::test]
    async fn a_file_rule_grants_a_parentless_document() {
        let server = MockServer::start().await;
        let (drive, docs) = clients(&server).await;
        mount_file("doc-1", GOOGLE_DOC_MIME_TYPE, &[])
            .mount(&server)
            .await;
        mount_document(Some("rev-1"), "Q3 report")
            .mount(&server)
            .await;

        let rules = [FolderPermissionRule {
            folder_id: None,
            file_id: Some("doc-1".to_string()),
            recursive: false,
            allow: std::iter::once(DriveOperation::DocsWrite).collect(),
            deny: HashSet::default(),
            require_lease: true,
        }];
        let outcome = write(&drive, &docs, &replace_opts(true), &rules).await;
        assert!(matches!(outcome.result, WriteResult::WouldReplace { .. }));
    }

    #[tokio::test]
    async fn an_ancestor_fetch_failure_produces_failed_not_allow() {
        let server = MockServer::start().await;
        let (drive, docs) = clients(&server).await;
        mount_file("doc-1", GOOGLE_DOC_MIME_TYPE, &["folder-1"])
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/drive/v3/files/folder-1"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let outcome = write(
            &drive,
            &docs,
            &replace_opts(false),
            &[allow_rule("folder-1")],
        )
        .await;
        assert!(
            matches!(outcome.result, WriteResult::Failed { .. }),
            "a chain that could not be resolved must never become an allow: {:?}",
            outcome.result
        );
    }

    // ── The lease ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn the_batch_update_presents_the_revision_from_the_get_it_just_made() {
        let server = MockServer::start().await;
        let (drive, docs) = clients(&server).await;
        mount_file("doc-1", GOOGLE_DOC_MIME_TYPE, &["folder-1"])
            .mount(&server)
            .await;
        mount_folder("folder-1").mount(&server).await;
        mount_document(Some("rev-abc"), "Q3 report")
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/documents/doc-1:batchUpdate"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "writeControl": {"requiredRevisionId": "rev-abc"},
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "documentId": "doc-1",
                "replies": [{"replaceAllText": {"occurrencesChanged": 1}}],
            })))
            .expect(1)
            .mount(&server)
            .await;

        let outcome = write(
            &drive,
            &docs,
            &replace_opts(false),
            &[allow_rule("folder-1")],
        )
        .await;
        assert_eq!(
            outcome.result,
            WriteResult::Replaced {
                occurrences_changed: 1
            }
        );
        assert_eq!(outcome.required_revision_id.as_deref(), Some("rev-abc"));
    }

    /// ADR-0076 §3: rather than fall back to an unleased write, a document
    /// Google withheld the revision id for is refused — and no batchUpdate
    /// is mounted, so reaching one fails the test.
    #[tokio::test]
    async fn a_document_with_no_revision_id_is_refused_rather_than_written_unleased() {
        let server = MockServer::start().await;
        let (drive, docs) = clients(&server).await;
        mount_file("doc-1", GOOGLE_DOC_MIME_TYPE, &["folder-1"])
            .mount(&server)
            .await;
        mount_folder("folder-1").mount(&server).await;
        mount_document(None, "Q3 report").mount(&server).await;

        let outcome = write(
            &drive,
            &docs,
            &replace_opts(false),
            &[allow_rule("folder-1")],
        )
        .await;
        assert_eq!(outcome.result, WriteResult::RefusedNoRevisionId);
        let text = describe(&outcome, WriteVerb::Replace);
        assert!(text.contains("edit access"), "{text}");
    }

    /// The exact 400 confirmed live (ADR-0076 §6).
    #[tokio::test]
    async fn a_stale_revision_400_is_its_own_outcome_not_a_generic_failure() {
        let server = MockServer::start().await;
        let (drive, docs) = clients(&server).await;
        mount_file("doc-1", GOOGLE_DOC_MIME_TYPE, &["folder-1"])
            .mount(&server)
            .await;
        mount_folder("folder-1").mount(&server).await;
        mount_document(Some("rev-old"), "Q3 report")
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/documents/doc-1:batchUpdate"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": {
                    "code": 400,
                    "message": "The required revision ID 'rev-old' does not match the latest revision.",
                    "status": "INVALID_ARGUMENT",
                },
            })))
            .mount(&server)
            .await;

        let outcome = write(
            &drive,
            &docs,
            &replace_opts(false),
            &[allow_rule("folder-1")],
        )
        .await;
        match &outcome.result {
            WriteResult::StaleRevision {
                required_revision_id,
                ..
            } => assert_eq!(required_revision_id, "rev-old"),
            other => panic!("expected StaleRevision, got {other:?}"),
        }
        let text = describe(&outcome, WriteVerb::Replace);
        assert!(text.contains("nothing was written"), "{text}");
        assert!(text.contains("Re-run"), "{text}");
    }

    /// `INVALID_ARGUMENT` is also what a malformed request carries, so an
    /// unrelated 400 must stay a generic failure — telling the user to
    /// re-run it would be advice that can never work.
    #[tokio::test]
    async fn an_unrelated_400_stays_a_generic_failure() {
        let server = MockServer::start().await;
        let (drive, docs) = clients(&server).await;
        mount_file("doc-1", GOOGLE_DOC_MIME_TYPE, &["folder-1"])
            .mount(&server)
            .await;
        mount_folder("folder-1").mount(&server).await;
        mount_document(Some("rev-1"), "Q3 report")
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/documents/doc-1:batchUpdate"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": {"code": 400, "message": "Invalid requests[0]", "status": "INVALID_ARGUMENT"},
            })))
            .mount(&server)
            .await;

        let outcome = write(
            &drive,
            &docs,
            &replace_opts(false),
            &[allow_rule("folder-1")],
        )
        .await;
        assert!(matches!(outcome.result, WriteResult::Failed { .. }));
    }

    // ── --dry-run ──────────────────────────────────────────────────────

    /// Mirrors `content_edit.rs::dry_run_never_calls_edit_endpoint`.
    #[tokio::test]
    async fn dry_run_never_calls_batch_update() {
        let server = MockServer::start().await;
        let (drive, docs) = clients(&server).await;
        mount_file("doc-1", GOOGLE_DOC_MIME_TYPE, &["folder-1"])
            .mount(&server)
            .await;
        mount_folder("folder-1").mount(&server).await;
        mount_document(Some("rev-1"), "Q3 report")
            .mount(&server)
            .await;
        // No batchUpdate mock: reaching it fails the test.

        let outcome = write(
            &drive,
            &docs,
            &replace_opts(true),
            &[allow_rule("folder-1")],
        )
        .await;
        assert!(matches!(outcome.result, WriteResult::WouldReplace { .. }));
    }

    /// Mirrors
    /// `content_edit.rs::dry_run_surfaces_the_same_blocked_reasoning_as_a_real_denied_run`.
    #[tokio::test]
    async fn dry_run_surfaces_the_same_blocked_reasoning_as_a_real_denied_run() {
        let server = MockServer::start().await;
        let (drive, docs) = clients(&server).await;
        mount_file("doc-1", GOOGLE_DOC_MIME_TYPE, &["folder-1"])
            .expect(2)
            .mount(&server)
            .await;
        mount_folder("folder-1").expect(2).mount(&server).await;

        let dry = write(&drive, &docs, &replace_opts(true), &[]).await;
        let real = write(&drive, &docs, &replace_opts(false), &[]).await;
        assert_eq!(dry.result, real.result);
        assert!(matches!(dry.result, WriteResult::Blocked { .. }));
    }

    #[tokio::test]
    async fn dry_run_replace_reports_the_occurrence_count_from_the_snapshot() {
        let server = MockServer::start().await;
        let (drive, docs) = clients(&server).await;
        mount_file("doc-1", GOOGLE_DOC_MIME_TYPE, &["folder-1"])
            .mount(&server)
            .await;
        mount_folder("folder-1").mount(&server).await;
        mount_document(Some("rev-1"), "Q3 then Q3 then Q3")
            .mount(&server)
            .await;

        let outcome = write(
            &drive,
            &docs,
            &replace_opts(true),
            &[allow_rule("folder-1")],
        )
        .await;
        assert_eq!(outcome.result, WriteResult::WouldReplace { occurrences: 3 });
        let text = describe(&outcome, WriteVerb::Replace);
        assert!(text.contains("3 occurrence(s)"), "{text}");
    }

    #[tokio::test]
    async fn dry_run_append_reports_the_end_index_and_the_insert_size() {
        let server = MockServer::start().await;
        let (drive, docs) = clients(&server).await;
        mount_file("doc-1", GOOGLE_DOC_MIME_TYPE, &["folder-1"])
            .mount(&server)
            .await;
        mount_folder("folder-1").mount(&server).await;
        mount_document(Some("rev-1"), "body").mount(&server).await;

        let outcome = write(&drive, &docs, &append_opts(true), &[allow_rule("folder-1")]).await;
        match outcome.result {
            WriteResult::WouldAppend {
                document_end_index,
                chars,
                bytes,
            } => {
                assert_eq!(chars, 5);
                assert_eq!(bytes, 5);
                assert!(document_end_index.is_some());
            }
            other => panic!("expected WouldAppend, got {other:?}"),
        }
    }

    /// ADR-0076 §7: the client-side count is display-only and **never**
    /// gates the mutation, so a zero count still sends the batch. Reporting
    /// "nothing to do" from an estimate would be wrong whenever the server
    /// can see a match this crate cannot.
    #[tokio::test]
    async fn a_zero_occurrence_replace_still_sends_the_batch_update() {
        let server = MockServer::start().await;
        let (drive, docs) = clients(&server).await;
        mount_file("doc-1", GOOGLE_DOC_MIME_TYPE, &["folder-1"])
            .mount(&server)
            .await;
        mount_folder("folder-1").mount(&server).await;
        mount_document(Some("rev-1"), "nothing matching here")
            .mount(&server)
            .await;
        mount_batch_update(serde_json::json!({
            "documentId": "doc-1",
            "replies": [{"replaceAllText": {"occurrencesChanged": 0}}],
        }))
        .expect(1)
        .mount(&server)
        .await;

        let outcome = write(
            &drive,
            &docs,
            &replace_opts(false),
            &[allow_rule("folder-1")],
        )
        .await;
        assert_eq!(
            outcome.result,
            WriteResult::Replaced {
                occurrences_changed: 0
            }
        );
    }

    /// An `insertText` replies with an empty object, which is normal.
    #[tokio::test]
    async fn an_append_reports_its_own_size_since_the_api_reports_no_count() {
        let server = MockServer::start().await;
        let (drive, docs) = clients(&server).await;
        mount_file("doc-1", GOOGLE_DOC_MIME_TYPE, &["folder-1"])
            .mount(&server)
            .await;
        mount_folder("folder-1").mount(&server).await;
        mount_document(Some("rev-1"), "body").mount(&server).await;
        mount_batch_update(serde_json::json!({"documentId": "doc-1", "replies": [{}]}))
            .mount(&server)
            .await;

        let outcome = write(
            &drive,
            &docs,
            &append_opts(false),
            &[allow_rule("folder-1")],
        )
        .await;
        assert_eq!(outcome.result, WriteResult::Appended { chars: 5, bytes: 5 });
    }

    // ── the Drive write lease (ADR-0080 §9) ────────────────────────────

    #[tokio::test]
    async fn refuses_without_a_lease_when_the_rule_requires_one() {
        let server = MockServer::start().await;
        let (drive, docs) = clients(&server).await;
        mount_file("doc-1", GOOGLE_DOC_MIME_TYPE, &["folder-1"])
            .mount(&server)
            .await;
        mount_folder("folder-1").mount(&server).await;
        mount_document(Some("rev-1"), "Q3 report")
            .mount(&server)
            .await;
        // No batchUpdate mock mounted — a refusal must make zero mutating
        // calls.

        let opts = WriteOptions {
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
            ..replace_opts(false)
        };
        let outcome = write(&drive, &docs, &opts, &[allow_rule("folder-1")]).await;
        assert!(matches!(outcome.result, WriteResult::RefusedNoLease));
        assert_eq!(outcome.result.log_status(), "refused-no-lease");
    }

    #[tokio::test]
    async fn refuses_an_unknown_lease_token() {
        let server = MockServer::start().await;
        let (drive, docs) = clients(&server).await;
        mount_file("doc-1", GOOGLE_DOC_MIME_TYPE, &["folder-1"])
            .mount(&server)
            .await;
        mount_folder("folder-1").mount(&server).await;
        mount_document(Some("rev-1"), "Q3 report")
            .mount(&server)
            .await;
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        // Never seeded — the ledger exists nowhere near this token.

        let opts = WriteOptions {
            lease_token: Some("bogus-token".to_string()),
            ledger_path,
            ..replace_opts(false)
        };
        let outcome = write(&drive, &docs, &opts, &[allow_rule("folder-1")]).await;
        assert!(matches!(outcome.result, WriteResult::RefusedLeaseExpired));
    }

    #[tokio::test]
    async fn refuses_a_lease_bound_to_a_different_file() {
        let server = MockServer::start().await;
        let (drive, docs) = clients(&server).await;
        mount_file("doc-1", GOOGLE_DOC_MIME_TYPE, &["folder-1"])
            .mount(&server)
            .await;
        mount_folder("folder-1").mount(&server).await;
        mount_document(Some("rev-1"), "Q3 report")
            .mount(&server)
            .await;
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        // Seeded for a *different* document id.
        let token = seed_lease(&ledger_path, "some-other-doc", "1");

        let opts = WriteOptions {
            lease_token: Some(token),
            ledger_path,
            ..replace_opts(false)
        };
        let outcome = write(&drive, &docs, &opts, &[allow_rule("folder-1")]).await;
        assert!(matches!(outcome.result, WriteResult::RefusedLeaseWrongFile));
    }

    #[tokio::test]
    async fn refuses_a_stale_lease_when_the_file_has_moved() {
        let server = MockServer::start().await;
        let (drive, docs) = clients(&server).await;
        // `mount_file` always returns version "1"; the lease below was
        // acquired against version "0" — a foreign edit landed since.
        mount_file("doc-1", GOOGLE_DOC_MIME_TYPE, &["folder-1"])
            .mount(&server)
            .await;
        mount_folder("folder-1").mount(&server).await;
        mount_document(Some("rev-1"), "Q3 report")
            .mount(&server)
            .await;
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        let token = seed_lease(&ledger_path, "doc-1", "0");

        let opts = WriteOptions {
            lease_token: Some(token),
            ledger_path,
            ..replace_opts(false)
        };
        let outcome = write(&drive, &docs, &opts, &[allow_rule("folder-1")]).await;
        assert!(matches!(outcome.result, WriteResult::RefusedLeaseStale));
    }

    #[tokio::test]
    async fn require_lease_false_skips_the_lease_check_entirely() {
        let server = MockServer::start().await;
        let (drive, docs) = clients(&server).await;
        mount_file("doc-1", GOOGLE_DOC_MIME_TYPE, &["folder-1"])
            .mount(&server)
            .await;
        mount_folder("folder-1").mount(&server).await;
        mount_document(Some("rev-1"), "Q3 report")
            .mount(&server)
            .await;
        mount_batch_update(serde_json::json!({
            "documentId": "doc-1",
            "replies": [{"replaceAllText": {"occurrencesChanged": 1}}],
        }))
        .mount(&server)
        .await;
        let rule = FolderPermissionRule {
            folder_id: Some("folder-1".to_string()),
            file_id: None,
            recursive: true,
            allow: std::iter::once(DriveOperation::DocsWrite).collect(),
            deny: HashSet::default(),
            require_lease: false,
        };

        // No lease token presented at all, and no ledger exists.
        let opts = WriteOptions {
            lease_token: None,
            ledger_path: std::path::PathBuf::from("/nonexistent/lease-ledger.jsonl"),
            ..replace_opts(false)
        };
        let outcome = write(&drive, &docs, &opts, &[rule]).await;
        assert!(matches!(outcome.result, WriteResult::Replaced { .. }));
    }

    #[tokio::test]
    async fn a_dry_run_never_needs_a_lease() {
        let server = MockServer::start().await;
        let (drive, docs) = clients(&server).await;
        mount_file("doc-1", GOOGLE_DOC_MIME_TYPE, &["folder-1"])
            .mount(&server)
            .await;
        mount_folder("folder-1").mount(&server).await;
        mount_document(Some("rev-1"), "Q3 report")
            .mount(&server)
            .await;
        // No batchUpdate mock, no lease token, no ledger — a dry run must
        // not need any of them.

        let opts = WriteOptions {
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
            ..replace_opts(true)
        };
        let outcome = write(&drive, &docs, &opts, &[allow_rule("folder-1")]).await;
        assert!(matches!(outcome.result, WriteResult::WouldReplace { .. }));
    }
}
