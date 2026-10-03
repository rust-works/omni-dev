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

use super::table::{self, TableEdit, TableError, TablePreview, TableVerb};
use crate::cli::drive::format::{write_scalar_jsonl, JsonlSerialize};
use crate::drive::client::DriveClient;
use crate::drive::docs::anchor::{self, AnchorError, EditPreview, ListPreview, Side};
use crate::drive::docs::api::{is_stale_revision, DocsApi, SuggestionsViewMode};
use crate::drive::docs::client::DocsClient;
use crate::drive::docs::write_types::{BulletPreset, DocsRequest};
use crate::drive::files_api::FilesApi;
use crate::drive::folder_ancestry;
use crate::drive::lease::check::{
    conclude_native_leased_write, gate_optional_leased_write, FromLeaseRefusal, LeaseGateRefusal,
    LeasedWrite,
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
    /// Typed table structural mutation.
    Table(TableEdit),
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
    /// Insert next to a unique anchor in tab bodies or an existing segment.
    Insert {
        /// Literal text to locate in the snapshot.
        anchor: String,
        /// Which edge of the anchor to use.
        side: Side,
        /// Text to insert.
        text: String,
        /// Case-sensitive by default.
        match_case: bool,
        /// Existing non-body segment selector; default addresses tab bodies.
        selection: anchor::SegmentSelection,
    },
    /// Apply a bounded style patch to a unique anchored range.
    Format {
        /// First anchor or complete match.
        from: String,
        /// Inclusive final anchor.
        to: Option<String>,
        /// Literal matching case sensitivity.
        match_case: bool,
        /// Explicit style values.
        style: super::style::StylePatch,
    },
    /// Delete a unique match or an inclusive pair of anchors.
    Delete {
        /// First anchor (or the entire single match).
        from: String,
        /// Last anchor, when deleting an inclusive range.
        to: Option<String>,
        /// Case-sensitive by default.
        match_case: bool,
        /// Existing non-body segment selector; default addresses tab bodies.
        selection: anchor::SegmentSelection,
    },
    /// Apply or remove list formatting on whole anchor-selected paragraphs.
    List {
        /// First anchor or single-paragraph match.
        from: String,
        /// Last anchor, inclusive of its containing paragraph.
        to: Option<String>,
        /// Creation uses a concrete preset; None removes bullets.
        preset: Option<BulletPreset>,
        /// Case-sensitive by default.
        match_case: bool,
    },
}

impl WritePayload {
    /// Which verb this payload is.
    #[must_use]
    pub const fn verb(&self) -> WriteVerb {
        match self {
            Self::Replace { .. } => WriteVerb::Replace,
            Self::Append { .. } => WriteVerb::Append,
            Self::Insert { .. } => WriteVerb::Insert,
            Self::Delete { .. } => WriteVerb::Delete,
            Self::List {
                preset: Some(_), ..
            } => WriteVerb::CreateBullets,
            Self::List { preset: None, .. } => WriteVerb::DeleteBullets,
            Self::Format {
                style: super::style::StylePatch::Text(_),
                ..
            } => WriteVerb::TextStyle,
            Self::Format { .. } => WriteVerb::ParagraphStyle,
            Self::Table(edit) => WriteVerb::Table(edit.verb()),
        }
    }

    /// Permission vocabulary is independent of the request's wire operation.
    /// List creation/removal uses DocsWrite: removal preserves prose, while
    /// creation's leading-tab removal is an explicit part of formatting consent.
    #[must_use]
    pub const fn gate_operation(&self) -> DriveOperation {
        match self {
            Self::Delete { .. } => DriveOperation::DocsDelete,
            Self::Format { .. } => DriveOperation::DocsFormat,
            Self::Table(edit) => edit.verb().gate_operation(),
            _ => DriveOperation::DocsWrite,
        }
    }

    const fn index_addressed(&self) -> bool {
        matches!(
            self,
            Self::Insert { .. }
                | Self::Delete { .. }
                | Self::List { .. }
                | Self::Format { .. }
                | Self::Table(_)
        )
    }

    /// Rejects a payload that cannot produce a valid request, before any
    /// network call.
    ///
    /// The API rejects an empty `containsText.text`, and an empty append is
    /// a no-op worth naming rather than a round-trip worth spending.
    fn validate(&self) -> Result<(), String> {
        if let Self::Insert { selection, .. } | Self::Delete { selection, .. } = self {
            selection
                .validate()
                .map_err(|_| "invalid segment selection".to_owned())?;
        }
        match self {
            Self::Table(edit) => edit.validate(),
            Self::Replace { search, .. } if search.is_empty() => {
                Err("--search cannot be empty".to_string())
            }
            Self::Append { text } if text.is_empty() => {
                Err("nothing to append: the text is empty".to_string())
            }
            Self::Insert { text, .. } if anchor::insertion_counts(text).0 == 0 => Err(
                "nothing to insert: no text remains after Docs strips unsupported characters"
                    .to_string(),
            ),
            Self::Insert { anchor, .. } if anchor.is_empty() || anchor.contains(['\n', '\r']) => {
                Err("anchor must be nonempty and within one paragraph".to_owned())
            }
            Self::Format { style, .. } if style.fields().is_empty() => {
                Err("at least one style property is required".to_owned())
            }
            Self::Delete { from, to, .. }
            | Self::List { from, to, .. }
            | Self::Format { from, to, .. }
                if from.is_empty()
                    || from.contains(['\n', '\r'])
                    || to
                        .as_ref()
                        .is_some_and(|to| to.is_empty() || to.contains(['\n', '\r'])) =>
            {
                Err("anchors must be nonempty and within one paragraph".to_owned())
            }
            _ => Ok(()),
        }
    }
}

/// Which text mutation to perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteVerb {
    /// Table structural edit.
    Table(TableVerb),
    /// Replace every occurrence of some text.
    Replace,
    /// Append text to the end of the document.
    Append,
    /// Anchor-addressed insertion.
    Insert,
    /// Anchor-addressed deletion.
    Delete,
    /// Create paragraph bullets or numbering.
    CreateBullets,
    /// Remove paragraph bullets or numbering.
    DeleteBullets,
    /// Character formatting.
    TextStyle,
    /// Paragraph formatting.
    ParagraphStyle,
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
            Self::Insert => "docs-insert",
            Self::Delete => "docs-delete",
            Self::CreateBullets => "docs-create-bullets",
            Self::DeleteBullets => "docs-delete-bullets",
            Self::TextStyle => "docs-text-style",
            Self::ParagraphStyle => "docs-paragraph-style",
            Self::Table(verb) => verb.log_operation(),
        }
    }

    /// The CLI spelling, for messages.
    const fn label(self) -> &'static str {
        match self {
            Self::Replace => "replace",
            Self::Append => "append",
            Self::Insert => "insert",
            Self::Delete => "delete",
            Self::CreateBullets => "create-bullets",
            Self::DeleteBullets => "delete-bullets",
            Self::TextStyle => "text-style",
            Self::ParagraphStyle => "paragraph-style",
            Self::Table(verb) => verb.label(),
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
    /// Snapshot-resolved table preview.
    WouldEditTable {
        /// The exact structural effect.
        edit: TablePreview,
    },
    /// A table mutation applied.
    EditedTable {
        /// Same effect metadata as the preview.
        edit: TablePreview,
    },
    /// Table addressing failed closed.
    RefusedTable {
        /// Typed refusal, without user prose.
        error: TableError,
    },
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
    /// Dry-run metadata for insertion.
    WouldInsert {
        /// Resolved effect, in UTF-16 units.
        edit: EditPreview,
    },
    /// Dry-run metadata for deletion.
    WouldDelete {
        /// Resolved effect, in UTF-16 units.
        edit: EditPreview,
    },
    /// An insertion landed at the resolved position.
    Inserted {
        /// The same metadata as the preview.
        edit: EditPreview,
    },
    /// A deletion landed over the resolved range.
    Deleted {
        /// The same metadata as the preview.
        edit: EditPreview,
    },
    /// Metadata-only preview of whole-paragraph list formatting.
    WouldFormatList {
        /// Range before any leading tabs are removed.
        edit: ListPreview,
        /// Some for creation, None for removal.
        preset: Option<BulletPreset>,
    },
    /// List formatting applied; indices still describe the pre-write snapshot.
    ListFormatted {
        /// The same metadata as the preview.
        edit: ListPreview,
        /// Some for creation, None for removal.
        preset: Option<BulletPreset>,
    },
    /// Exact requested formatting effect before mutation.
    WouldFormat {
        /// Affected range and character/paragraph counts (no text mutation).
        edit: EditPreview,
        /// Explicit style properties.
        style: super::style::StylePatch,
        /// Derived property mask.
        fields: String,
    },
    /// The same requested formatting effect after a successful mutation.
    Formatted {
        /// Affected range and counts.
        edit: EditPreview,
        /// Explicit style properties.
        style: super::style::StylePatch,
        /// Derived property mask.
        fields: String,
    },
    /// An anchor could not safely identify a unique effect.
    RefusedAnchor {
        /// Typed reason containing no anchor text.
        error: AnchorError,
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

impl FromLeaseRefusal for WriteResult {
    fn from_no_lease() -> Self {
        Self::RefusedNoLease
    }
    fn from_lease_expired() -> Self {
        Self::RefusedLeaseExpired
    }
    fn from_lease_wrong_file() -> Self {
        Self::RefusedLeaseWrongFile
    }
    fn from_lease_stale() -> Self {
        Self::RefusedLeaseStale
    }
    fn from_lease_failed(detail: String) -> Self {
        Self::Failed { detail }
    }
}

impl WriteResult {
    /// The kebab-case status for the request log.
    fn log_status(&self) -> &'static str {
        match self {
            Self::WouldFormat { .. } => "would-format",
            Self::Formatted { .. } => "formatted",
            Self::WouldReplace { .. } => "would-replace",
            Self::WouldAppend { .. } => "would-append",
            Self::WouldEditTable { .. } => "would-edit-table",
            Self::EditedTable { .. } => "edited-table",
            Self::RefusedTable { .. } => "refused-table",
            Self::WouldInsert { .. } => "would-insert",
            Self::WouldDelete { .. } => "would-delete",
            Self::Inserted { .. } => "inserted",
            Self::Deleted { .. } => "deleted",
            Self::WouldFormatList { .. } => "would-format-list",
            Self::ListFormatted { .. } => "list-formatted",
            Self::RefusedAnchor { .. } => "refused-anchor",
            Self::RefusedNotADocument { .. } => "refused-not-a-document",
            Self::RefusedShortcut => "refused-shortcut",
            Self::RefusedNoVisibleParents => "refused-no-visible-parents",
            Self::RefusedNoRevisionId => "refused-no-revision-id",
            Self::Blocked { .. } => "blocked",
            Self::RefusedNoLease => LeaseGateRefusal::NoLease.log_status(),
            Self::RefusedLeaseExpired => LeaseGateRefusal::Expired.log_status(),
            Self::RefusedLeaseWrongFile => LeaseGateRefusal::WrongFile.log_status(),
            Self::RefusedLeaseStale => LeaseGateRefusal::Stale.log_status(),
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
/// footnote) that `document_text` does not include in this count (issue
/// #1799 made those segments fetchable; folding them into this estimate is
/// separate follow-up work). Acting on the count would let `omni-dev` report
/// "nothing to do" for a document that does have matches.
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
        opts.payload.gate_operation(),
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
        .get_document(
            &opts.document_id,
            if opts.payload.index_addressed() {
                SuggestionsViewMode::Inline
            } else {
                SuggestionsViewMode::default()
            },
        )
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

    // Request and preview are built together from *this* snapshot, before the
    // dry-run branch, so a dry run and a real run cannot disagree about what
    // they saw (ADR-0076 §7). Building the request here, before the ledger
    // gate, also means no fallible work can orphan a pending intent.
    let (request, preview) = match &opts.payload {
        WritePayload::Table(edit) => match table::resolve(&document, edit) {
            Ok((request, edit)) => (request, WriteResult::WouldEditTable { edit }),
            Err(error) => return gated(WriteResult::RefusedTable { error }, Some(revision_id)),
        },
        WritePayload::Replace {
            search,
            replace,
            match_case,
        } => {
            // Across every tab, because a `replaceAllText` with no
            // `tabsCriteria` spans every tab (ADR-0076 §11).
            let corpus = document_text(&document);
            (
                DocsRequest::replace_all_text(search, replace, *match_case),
                WriteResult::WouldReplace {
                    occurrences: count_occurrences(&corpus, search, *match_case),
                },
            )
        }
        WritePayload::Append { text } => (
            DocsRequest::insert_text_at_end(text),
            WriteResult::WouldAppend {
                document_end_index: body_end_index(&document),
                chars: text.chars().count(),
                bytes: text.len(),
            },
        ),
        WritePayload::Insert {
            anchor,
            side,
            text,
            match_case,
            selection,
        } => {
            match anchor::resolve_insert_in(&document, anchor, *side, text, *match_case, selection)
            {
                Ok(edit) => (
                    DocsRequest::insert_text_at(text, &edit),
                    WriteResult::WouldInsert { edit },
                ),
                Err(error) => {
                    return gated(WriteResult::RefusedAnchor { error }, Some(revision_id))
                }
            }
        }
        WritePayload::Format {
            from,
            to,
            match_case,
            style,
        } => match anchor::resolve_format(
            &document,
            from,
            to.as_deref(),
            *match_case,
            matches!(style, super::style::StylePatch::Paragraph(_)),
        ) {
            Ok(edit) => (
                DocsRequest::format_range(&edit, style),
                WriteResult::WouldFormat {
                    edit,
                    style: style.clone(),
                    fields: style.fields(),
                },
            ),
            Err(error) => return gated(WriteResult::RefusedAnchor { error }, Some(revision_id)),
        },
        WritePayload::Delete {
            from,
            to,
            match_case,
            selection,
        } => {
            match anchor::resolve_delete_in(&document, from, to.as_deref(), *match_case, selection)
            {
                Ok(edit) => (
                    DocsRequest::delete_range(&edit),
                    WriteResult::WouldDelete { edit },
                ),
                Err(error) => {
                    return gated(WriteResult::RefusedAnchor { error }, Some(revision_id))
                }
            }
        }
        WritePayload::List {
            from,
            to,
            preset,
            match_case,
        } => match anchor::resolve_list(
            &document,
            from,
            to.as_deref(),
            *match_case,
            preset.is_some(),
        ) {
            Ok(edit) => (
                DocsRequest::list_bullets(&edit, *preset),
                WriteResult::WouldFormatList {
                    edit,
                    preset: *preset,
                },
            ),
            Err(error) => return gated(WriteResult::RefusedAnchor { error }, Some(revision_id)),
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
    //
    let leased = LeasedWrite {
        log_prefix: "drive docs write",
        operation: opts.payload.verb().log_operation(),
        ledger_path: &opts.ledger_path,
        file_id: &opts.document_id,
    };
    let lease_grant = match gate_optional_leased_write(
        leased,
        &files_api,
        requires_lease,
        opts.lease_token.as_deref(),
    )
    .await
    {
        Ok(grant) => grant,
        Err(err) => return gated(err.into_result(), Some(revision_id)),
    };

    // ── The mutation ───────────────────────────────────────────────────
    let result = match conclude_native_leased_write(
        leased,
        &lease_grant,
        &files_api,
        api.batch_update(&opts.document_id, request, &revision_id)
            .await,
        ToString::to_string,
    )
    .await
    {
        Ok(response) => match &opts.payload {
            WritePayload::Table(_) => match preview {
                WriteResult::WouldEditTable { edit } => WriteResult::EditedTable { edit },
                _ => WriteResult::Failed {
                    detail: "missing resolved table preview".into(),
                },
            },
            WritePayload::Replace { .. } => WriteResult::Replaced {
                occurrences_changed: response.occurrences_changed_for_replace(),
            },
            WritePayload::Append { text } => WriteResult::Appended {
                chars: text.chars().count(),
                bytes: text.len(),
            },
            WritePayload::Insert { .. }
            | WritePayload::Delete { .. }
            | WritePayload::List { .. }
            | WritePayload::Format { .. } => match preview {
                WriteResult::WouldFormatList { edit, preset } => {
                    WriteResult::ListFormatted { edit, preset }
                }
                WriteResult::WouldFormat {
                    edit,
                    style,
                    fields,
                } => WriteResult::Formatted {
                    edit,
                    style,
                    fields,
                },
                WriteResult::WouldInsert { edit } => WriteResult::Inserted { edit },
                WriteResult::WouldDelete { edit } => WriteResult::Deleted { edit },
                // omni-dev: coverage ignore reason="`preview` is built by the payload match above, so an Insert/Delete/List payload always carries its matching edit preview; this arm exists solely for exhaustiveness over the shared WriteResult enum"
                _ => WriteResult::Failed {
                    detail: "missing resolved edit preview".to_owned(),
                },
                // omni-dev: coverage end
            },
        },
        Err(err) => {
            // Either way the mutation did not happen — a `412` on
            // `writeControl.requiredRevisionId` included — so the intent
            // record gets a `failed` outcome carrying the reason.
            let detail = err.to_string();
            if is_stale_revision(&err) {
                WriteResult::StaleRevision {
                    required_revision_id: revision_id.clone(),
                    detail,
                }
            } else {
                WriteResult::Failed { detail }
            }
        }
    };
    drop(lease_grant);
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
        WriteResult::Inserted { edit } => Some(edit.chars as i64),
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
        WriteResult::WouldEditTable { edit } | WriteResult::EditedTable { edit } => {
            let action = if matches!(outcome.result, WriteResult::WouldEditTable { .. }) {
                "Would apply"
            } else {
                "Applied"
            };
            format!("{action} {} in '{name}': {}x{} -> {}x{}, UTF-16 index {}, tab {}, reference row {:?}, column {:?}, insert after {}, preceding newline {}",
                edit.operation.label(), edit.rows_before, edit.columns_before, edit.rows_after, edit.columns_after,
                edit.location.index, edit.location.tab_id.as_deref().unwrap_or("first"), edit.row_index, edit.column_index,
                edit.insert_after, edit.preceding_newline)
        }
        WriteResult::RefusedTable { error } => {
            format!("Refused: unsafe or unresolved table in '{name}': {error:?}")
        }
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
        WriteResult::WouldFormat {
            edit,
            style,
            fields,
        }
        | WriteResult::Formatted {
            edit,
            style,
            fields,
        } => {
            let action = if matches!(outcome.result, WriteResult::WouldFormat { .. }) {
                "Would format"
            } else {
                "Formatted"
            };
            format!("{action}: '{}' tab {:?} UTF-16 [{}, {}) / {} paragraph(s) / {} char(s) / {} byte(s); fields={fields}; style={style:?}", name, edit.tab_id, edit.start_index, edit.end_index, edit.paragraphs, edit.chars, edit.bytes)
        }
        WriteResult::WouldInsert { edit }
        | WriteResult::Inserted { edit }
        | WriteResult::WouldDelete { edit }
        | WriteResult::Deleted { edit } => {
            let action = match &outcome.result {
                WriteResult::WouldInsert { .. } => "Would insert",
                WriteResult::Inserted { .. } => "Inserted",
                WriteResult::WouldDelete { .. } => "Would delete",
                _ => "Deleted",
            };
            let segment = edit.segment_id.as_ref().map_or_else(String::new, |id| {
                let kind = match edit.segment_kind {
                    Some(anchor::SegmentKind::Header) => "header",
                    Some(anchor::SegmentKind::Footer) => "footer",
                    Some(anchor::SegmentKind::Footnote) => "footnote",
                    None => "segment",
                };
                format!(", {kind} {id}")
            });
            format!("{action}: {} char(s) / {} byte(s) in '{name}' at [{}, {}) UTF-16 code units, tab {}{}, {} paragraph(s)",
                edit.chars, edit.bytes, edit.start_index, edit.end_index,
                edit.tab_id.as_deref().unwrap_or("first"), segment, edit.paragraphs)
        }
        WriteResult::WouldFormatList { edit, preset }
        | WriteResult::ListFormatted { edit, preset } => {
            let action = if matches!(outcome.result, WriteResult::WouldFormatList { .. }) {
                "Would format list"
            } else {
                "Formatted list"
            };
            let preset = preset.map_or_else(|| "remove bullets".to_owned(), |p| format!("{p:?}"));
            format!("{action}: {preset} in '{name}', {} paragraph(s), pre-write [{}, {}) UTF-16 code units, tab {}, {} leading tab(s) removed",
                edit.paragraphs, edit.start_index, edit.end_index,
                edit.tab_id.as_deref().unwrap_or("first"), edit.leading_tabs_removed)
        }
        WriteResult::RefusedAnchor { error } => {
            format!("Refused: unsafe or unresolved anchor in '{name}': {error:?}")
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
             [\"{}\"]}} to write_permissions.rules. (Adding it to a folder in your \
             own Drive and granting that folder `{}` also works.)",
            match verb {
                WriteVerb::Table(v) =>
                    if v.gate_operation() == DriveOperation::DocsStructure {
                        "docs-structure"
                    } else {
                        "docs-table-delete"
                    },
                WriteVerb::Delete => "docs-delete",
                WriteVerb::TextStyle | WriteVerb::ParagraphStyle => "docs-format",
                _ => "docs-write",
            },
            match verb {
                WriteVerb::Table(v) =>
                    if v.gate_operation() == DriveOperation::DocsStructure {
                        "docs-structure"
                    } else {
                        "docs-table-delete"
                    },
                WriteVerb::Delete => "docs-delete",
                WriteVerb::TextStyle | WriteVerb::ParagraphStyle => "docs-format",
                _ => "docs-write",
            }
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
        WriteResult::RefusedNoLease => LeaseGateRefusal::NoLease
            .describe_line(&outcome.document_id, &format!("'{name}'"))
            .unwrap_or_default(),
        WriteResult::RefusedLeaseExpired => LeaseGateRefusal::Expired
            .describe_line(&outcome.document_id, &format!("'{name}'"))
            .unwrap_or_default(),
        WriteResult::RefusedLeaseWrongFile => LeaseGateRefusal::WrongFile
            .describe_line(&outcome.document_id, &format!("'{name}'"))
            .unwrap_or_default(),
        WriteResult::RefusedLeaseStale => LeaseGateRefusal::Stale
            .describe_line(&outcome.document_id, &format!("'{name}'"))
            .unwrap_or_default(),
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

    use crate::drive::test_support::seed_lease;

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
                "startIndex": 1, "endIndex": 2 + text.encode_utf16().count() as i64,
                "paragraph": {"elements": [{"startIndex": 1, "endIndex": 2 + text.encode_utf16().count() as i64, "textRun": {"content": format!("{text}\n")}}]},
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
    async fn reports_a_lock_acquisition_failure_as_failed() {
        // A pre-existing lock file simulates another `drive lease`
        // operation genuinely in progress — reported as an operational
        // failure, not folded into `RefusedLeaseExpired`.
        let server = MockServer::start().await;
        let (drive, docs) = clients(&server).await;
        mount_file("doc-1", GOOGLE_DOC_MIME_TYPE, &["folder-1"])
            .mount(&server)
            .await;
        mount_folder("folder-1").mount(&server).await;
        mount_document(Some("rev-1"), "Q3 report")
            .mount(&server)
            .await;

        let opts = replace_opts(false);
        // Under `flock` (issue #1687), a busy lock now waits rather than
        // hard-failing (`check_and_lock_lease` -> `acquire_waiting`), so a
        // held `LedgerLock` no longer reproduces an immediate failure here.
        // A directory at the lock path does: opening it for write fails
        // outright with an I/O error, which is never retried.
        let mut lock_path = opts.ledger_path.clone().into_os_string();
        lock_path.push(".lock");
        std::fs::create_dir(std::path::PathBuf::from(lock_path)).unwrap();

        let outcome = write(&drive, &docs, &opts, &[allow_rule("folder-1")]).await;
        assert!(matches!(outcome.result, WriteResult::Failed { .. }));
    }

    #[tokio::test]
    async fn a_failed_pre_lease_refetch_is_reported_as_failed_with_no_batch_update_call() {
        // The gate's own resolve step succeeds off the first `files.get`,
        // but the fresh re-fetch feeding the staleness check (ADR-0080 §6)
        // fails — the write must report `Failed` and never reach
        // `batchUpdate`.
        let server = MockServer::start().await;
        let (drive, docs) = clients(&server).await;
        mount_file("doc-1", GOOGLE_DOC_MIME_TYPE, &["folder-1"])
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/drive/v3/files/doc-1"))
            .respond_with(ResponseTemplate::new(500))
            .with_priority(2)
            .mount(&server)
            .await;
        mount_folder("folder-1").mount(&server).await;
        mount_document(Some("rev-1"), "Q3 report")
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/documents/doc-1:batchUpdate"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
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
    async fn require_lease_false_writes_without_a_lease() {
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
    async fn require_lease_false_still_refuses_a_stale_lease_if_one_is_presented() {
        // ADR-0080 §13: `require_lease: false` relaxes the *requirement*,
        // not the *meaning* — a token volunteered anyway is checked exactly
        // like a required one, including staleness.
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
        // No batchUpdate mock mounted — a refusal must make zero mutating
        // calls.
        let rule = FolderPermissionRule {
            folder_id: Some("folder-1".to_string()),
            file_id: None,
            recursive: true,
            allow: std::iter::once(DriveOperation::DocsWrite).collect(),
            deny: HashSet::default(),
            require_lease: false,
        };
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
        let outcome = write(&drive, &docs, &opts, &[rule]).await;
        assert!(matches!(outcome.result, WriteResult::RefusedLeaseStale));
    }

    #[test]
    fn describe_renders_every_lease_refusal_with_the_lease_acquire_hint() {
        let outcome_with = |result: WriteResult| WriteOutcome {
            document_id: "doc-1".to_string(),
            file_name: Some("Budget".to_string()),
            resolved_folder_id: None,
            required_revision_id: None,
            result,
        };

        let text = describe(
            &outcome_with(WriteResult::RefusedNoLease),
            WriteVerb::Replace,
        );
        assert!(text.contains("requires a Drive write lease"), "{text}");
        assert!(text.contains("drive lease acquire doc-1"), "{text}");

        let text = describe(
            &outcome_with(WriteResult::RefusedLeaseExpired),
            WriteVerb::Replace,
        );
        assert!(text.contains("expired, released, or unknown"), "{text}");
        assert!(text.contains("drive lease acquire doc-1"), "{text}");

        let text = describe(
            &outcome_with(WriteResult::RefusedLeaseWrongFile),
            WriteVerb::Replace,
        );
        assert!(text.contains("acquired for a different file"), "{text}");
        assert!(text.contains("drive lease acquire doc-1"), "{text}");

        let text = describe(
            &outcome_with(WriteResult::RefusedLeaseStale),
            WriteVerb::Replace,
        );
        assert!(
            text.contains("changed since the lease was acquired"),
            "{text}"
        );
        assert!(text.contains("drive lease acquire doc-1"), "{text}");
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

    // ── the write's own audit trail (ADR-0080 §11) ─────────────────────

    #[tokio::test]
    async fn a_leased_docs_write_concludes_its_audit_pair_with_allowed() {
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
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "documentId": "doc-1",
                "replies": [{"replaceAllText": {"occurrencesChanged": 1}}],
            })))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let audit = crate::test_support::AuditLogGuard::redirect(dir.path());

        let outcome = write(
            &drive,
            &docs,
            &replace_opts(false),
            &[allow_rule("folder-1")],
        )
        .await;
        assert!(matches!(outcome.result, WriteResult::Replaced { .. }));

        let records = audit.records();
        assert_eq!(audit.verdicts(), ["pending", "allowed"], "{records:?}");
        // The verb, not the engine — the same `["drive", <log_operation>]`
        // this write's `drivemutation` record carries, so an auditor can
        // see which verb ran without joining back to `log.jsonl`.
        assert_eq!(records[0].command, ["drive", "docs-replace"]);
    }

    #[tokio::test]
    async fn a_leased_docs_write_that_fails_concludes_its_audit_pair_with_the_error() {
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
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let audit = crate::test_support::AuditLogGuard::redirect(dir.path());

        let outcome = write(
            &drive,
            &docs,
            &replace_opts(false),
            &[allow_rule("folder-1")],
        )
        .await;
        let WriteResult::Failed { detail } = &outcome.result else {
            panic!("expected Failed, got {:?}", outcome.result);
        };

        let records = audit.records();
        assert_eq!(audit.verdicts(), ["pending", "failed"], "{records:?}");
        assert_eq!(records[1].error.as_deref(), Some(detail.as_str()));
    }
    fn anchored_payloads() -> Vec<WritePayload> {
        vec![
            WritePayload::Insert {
                anchor: "Q3".into(),
                side: Side::After,
                text: "😀".into(),
                match_case: true,
                selection: anchor::SegmentSelection::default(),
            },
            WritePayload::Delete {
                from: "Q3".into(),
                to: None,
                match_case: true,
                selection: anchor::SegmentSelection::default(),
            },
        ]
    }

    fn segment_payloads() -> Vec<WritePayload> {
        let mut out = Vec::new();
        for id in ["header", "footer", "footnote"] {
            for mut payload in anchored_payloads() {
                match &mut payload {
                    WritePayload::Insert { selection, .. }
                    | WritePayload::Delete { selection, .. } => {
                        selection.segment_id = Some(id.into());
                        selection.tab_id = Some("child".into());
                    }
                    _ => unreachable!(), // omni-dev: coverage ignore-line reason="anchored_payloads yields only Insert and Delete"
                }
                out.push(payload);
            }
        }
        out
    }

    async fn mount_segments(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/v1/documents/doc-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(segment_document()))
            .with_priority(1)
            .mount(server)
            .await;
    }

    fn segment_document() -> serde_json::Value {
        let content = serde_json::json!([{"endIndex": 13, "paragraph": {"elements": [{"endIndex": 13, "textRun": {"content": "😀 Q3 report\n"}}]}}]);
        serde_json::json!({"revisionId": "rev-anchor", "body": {}, "tabs": [{
            "tabProperties": {"tabId": "parent"}, "childTabs": [{
                "tabProperties": {"tabId": "child"}, "documentTab": {
                    "headers": {"header": {"content": content}},
                    "footers": {"footer": {"content": content}},
                    "footnotes": {"footnote": {"content": content}}
                }
            }]
        }]})
    }

    #[tokio::test]
    async fn missing_segments_refuse_without_mutation() {
        let server = MockServer::start().await;
        let (drive, docs) = anchored_setup(&server).await;
        let payload = segment_payloads().remove(0);
        let rule = rule_for(&payload);
        let mut opts = replace_opts(false);
        opts.payload = payload;
        assert_eq!(
            write(&drive, &docs, &opts, &[rule]).await.result,
            WriteResult::RefusedAnchor {
                error: AnchorError::SegmentNotFound
            }
        );
        assert!(!server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|r| r.url.path().ends_with(":batchUpdate")));
    }

    #[tokio::test]
    async fn selected_segment_previews_and_writes_share_identity_indices_and_revision() {
        for payload in segment_payloads() {
            let server = MockServer::start().await;
            let (drive, docs) = anchored_setup(&server).await;
            mount_segments(&server).await;
            let mut opts = replace_opts(true);
            opts.payload = payload.clone();
            let rule = rule_for(&payload);
            let edit = match write(&drive, &docs, &opts, std::slice::from_ref(&rule))
                .await
                .result
            {
                WriteResult::WouldInsert { edit } | WriteResult::WouldDelete { edit } => edit,
                other => panic!("{other:?}"),
            };
            assert_eq!(edit.tab_id.as_deref(), Some("child"));
            assert!(edit.segment_kind.is_some());
            mount_batch_update(serde_json::json!({"replies": [{}]}))
                .expect(1)
                .mount(&server)
                .await;
            opts.dry_run = false;
            let applied = match write(&drive, &docs, &opts, &[rule]).await.result {
                WriteResult::Inserted { edit } | WriteResult::Deleted { edit } => edit,
                other => panic!("{other:?}"),
            };
            assert_eq!(edit, applied);
            let requests = server.received_requests().await.unwrap();
            let batch = requests
                .iter()
                .find(|r| r.url.path().ends_with(":batchUpdate"))
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&batch.body).unwrap();
            assert_eq!(
                body["writeControl"],
                serde_json::json!({"requiredRevisionId": "rev-anchor"})
            );
            assert_eq!(body["requests"].as_array().unwrap().len(), 1);
            let location = if payload.verb() == WriteVerb::Insert {
                assert_eq!((edit.start_index, edit.end_index), (5, 5));
                &body["requests"][0]["insertText"]["location"]
            } else {
                assert_eq!((edit.start_index, edit.end_index), (3, 5));
                &body["requests"][0]["deleteContentRange"]["range"]
            };
            assert_eq!(location["segmentId"], edit.segment_id.unwrap());
            assert_eq!(location["tabId"], "child");
        }
    }

    fn list_payloads() -> Vec<WritePayload> {
        [Some(BulletPreset::BulletDiscCircleSquare), None]
            .into_iter()
            .map(|preset| WritePayload::List {
                from: "Q3".into(),
                to: None,
                preset,
                match_case: true,
            })
            .collect()
    }

    #[tokio::test]
    async fn list_edits_preview_and_apply_the_same_snapshot_in_one_inline_leased_request() {
        for payload in list_payloads() {
            assert_eq!(payload.gate_operation(), DriveOperation::DocsWrite);
            let server = MockServer::start().await;
            let (drive, docs) = clients(&server).await;
            mount_file("doc-1", GOOGLE_DOC_MIME_TYPE, &["folder-1"])
                .mount(&server)
                .await;
            mount_folder("folder-1").mount(&server).await;
            mount_document(Some("rev-list"), "\t\t😀 Q3 report")
                .mount(&server)
                .await;
            let mut opts = replace_opts(true);
            opts.payload = payload.clone();
            let rule = rule_for(&payload);
            let outcome = write(&drive, &docs, &opts, std::slice::from_ref(&rule)).await;
            let (expected, preset) = match outcome.result {
                WriteResult::WouldFormatList { edit, preset } => (edit, preset),
                other => panic!("{other:?}"),
            };
            assert_eq!(
                (
                    expected.start_index,
                    expected.end_index,
                    expected.paragraphs
                ),
                (1, 16, 1)
            );
            assert_eq!(
                expected.leading_tabs_removed,
                if preset.is_some() { 2 } else { 0 }
            );
            mount_batch_update(serde_json::json!({"replies": [{}]}))
                .expect(1)
                .mount(&server)
                .await;
            opts.dry_run = false;
            let outcome = write(&drive, &docs, &opts, &[rule]).await;
            assert_eq!(
                outcome.result,
                WriteResult::ListFormatted {
                    edit: expected,
                    preset
                }
            );
            let requests = server.received_requests().await.unwrap();
            let reads: Vec<_> = requests
                .iter()
                .filter(|r| r.url.path() == "/v1/documents/doc-1")
                .collect();
            assert_eq!(reads.len(), 2);
            assert!(reads.iter().all(|r| r
                .url
                .query_pairs()
                .any(|(k, v)| k == "suggestionsViewMode" && v == "SUGGESTIONS_INLINE")));
            let batch = requests
                .iter()
                .find(|r| r.url.path().ends_with(":batchUpdate"))
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&batch.body).unwrap();
            let range = serde_json::json!({"startIndex": 1, "endIndex": 16});
            let request = if preset.is_some() {
                serde_json::json!({"createParagraphBullets": {"range": range, "bulletPreset": "BULLET_DISC_CIRCLE_SQUARE"}})
            } else {
                serde_json::json!({"deleteParagraphBullets": {"range": range}})
            };
            assert_eq!(
                body,
                serde_json::json!({"requests": [request], "writeControl": {"requiredRevisionId": "rev-list"}})
            );
            let serialized = serde_json::to_string(&outcome).unwrap();
            assert!(!serialized.contains("Q3") && !serialized.contains("report"));
        }
    }

    async fn anchored_setup(server: &MockServer) -> (DriveClient, DocsClient) {
        let clients = clients(server).await;
        mount_file("doc-1", GOOGLE_DOC_MIME_TYPE, &["folder-1"])
            .mount(server)
            .await;
        mount_folder("folder-1").mount(server).await;
        mount_document(Some("rev-anchor"), "😀 Q3 report")
            .mount(server)
            .await;

        clients
    }

    fn rule_for(payload: &WritePayload) -> FolderPermissionRule {
        let mut rule = allow_rule("folder-1");
        rule.allow = std::iter::once(payload.gate_operation()).collect();
        rule
    }

    #[test]
    fn every_docs_verb_maps_to_its_gate_operation() {
        for payload in [
            replace_opts(true).payload,
            append_opts(true).payload,
            anchored_payloads().remove(0),
        ] {
            assert_eq!(payload.gate_operation(), DriveOperation::DocsWrite);
        }
        assert_eq!(
            anchored_payloads().remove(1).gate_operation(),
            DriveOperation::DocsDelete
        );
    }

    #[tokio::test]
    async fn anchored_edits_read_inline_and_send_one_request_from_the_leased_snapshot() {
        for payload in anchored_payloads() {
            let server = MockServer::start().await;
            let (drive, docs) = anchored_setup(&server).await;
            let rule = rule_for(&payload);
            let mut opts = replace_opts(true);
            opts.payload = payload;
            let preview = write(&drive, &docs, &opts, std::slice::from_ref(&rule)).await;
            let expected = match &preview.result {
                WriteResult::WouldInsert { edit } | WriteResult::WouldDelete { edit } => {
                    edit.clone()
                }
                other => panic!("{other:?}"),
            };
            assert_eq!(
                expected.start_index,
                if opts.payload.verb() == WriteVerb::Insert {
                    6
                } else {
                    4
                }
            );
            mount_batch_update(serde_json::json!({"replies": [{}]}))
                .expect(1)
                .mount(&server)
                .await;
            opts.dry_run = false;
            let result = write(&drive, &docs, &opts, &[rule]).await;
            let actual = match result.result {
                WriteResult::Inserted { edit } | WriteResult::Deleted { edit } => edit,
                other => panic!("{other:?}"),
            };
            assert_eq!(actual, expected);
            let requests = server.received_requests().await.unwrap();
            let reads: Vec<_> = requests
                .iter()
                .filter(|r| r.url.path() == "/v1/documents/doc-1")
                .collect();
            assert_eq!(reads.len(), 2);
            for read in reads {
                assert!(read
                    .url
                    .query_pairs()
                    .any(|(k, v)| k == "suggestionsViewMode" && v == "SUGGESTIONS_INLINE"));
            }
            let batch = requests
                .iter()
                .find(|r| r.url.path().ends_with(":batchUpdate"))
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&batch.body).unwrap();
            assert_eq!(
                body["writeControl"],
                serde_json::json!({"requiredRevisionId": "rev-anchor"})
            );
            assert_eq!(body["requests"].as_array().unwrap().len(), 1);
            if opts.payload.verb() == WriteVerb::Insert {
                assert_eq!(
                    body["requests"][0]["insertText"],
                    serde_json::json!({"text": "😀", "location": {"index": 6}})
                );
            } else {
                assert_eq!(
                    body["requests"][0]["deleteContentRange"]["range"],
                    serde_json::json!({"startIndex": 4, "endIndex": 6})
                );
            }
        }
    }

    #[tokio::test]
    async fn a_docs_write_grant_does_not_allow_delete_and_delete_does_not_allow_other_verbs() {
        let mut payloads = anchored_payloads();
        payloads.extend(segment_payloads());
        payloads.extend(list_payloads());
        payloads.push(replace_opts(false).payload);
        payloads.push(append_opts(false).payload);
        for payload in payloads {
            let server = MockServer::start().await;
            let (drive, docs) = clients(&server).await;
            mount_file("doc-1", GOOGLE_DOC_MIME_TYPE, &["folder-1"])
                .mount(&server)
                .await;
            mount_folder("folder-1").mount(&server).await;
            let mut rule = allow_rule("folder-1");
            if payload.verb() != WriteVerb::Delete {
                rule.allow = std::iter::once(DriveOperation::DocsDelete).collect();
            }
            let mut opts = replace_opts(false);
            opts.payload = payload;
            assert!(matches!(
                write(&drive, &docs, &opts, &[rule]).await.result,
                WriteResult::Blocked { .. }
            ));
            assert!(!server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|r| r.url.path().starts_with("/v1/documents")));
        }
    }

    #[tokio::test]
    async fn unresolved_anchors_and_missing_revisions_never_mutate() {
        for payload in anchored_payloads()
            .into_iter()
            .chain(list_payloads())
            .chain(formatting_payloads())
        {
            for (text, revision, expected) in [
                ("nothing", Some("r"), "missing"),
                ("Q3 Q3", Some("r"), "ambiguous"),
                ("Q3", None, "revision"),
            ] {
                let server = MockServer::start().await;
                let (drive, docs) = clients(&server).await;
                mount_file("doc-1", GOOGLE_DOC_MIME_TYPE, &["folder-1"])
                    .mount(&server)
                    .await;
                mount_folder("folder-1").mount(&server).await;
                mount_document(revision, text).mount(&server).await;
                let mut opts = replace_opts(false);
                opts.payload = payload.clone();
                let outcome = write(&drive, &docs, &opts, &[rule_for(&payload)]).await;
                match expected {
                    "missing" => assert!(matches!(
                        outcome.result,
                        WriteResult::RefusedAnchor {
                            error: AnchorError::NotFound
                        }
                    )),
                    "ambiguous" => assert!(matches!(
                        outcome.result,
                        WriteResult::RefusedAnchor {
                            error: AnchorError::Ambiguous { count: 2 }
                        }
                    )),
                    _ => assert_eq!(outcome.result, WriteResult::RefusedNoRevisionId),
                }
                assert!(!server
                    .received_requests()
                    .await
                    .unwrap()
                    .iter()
                    .any(|r| r.url.path().ends_with(":batchUpdate")));
            }
        }
    }

    #[tokio::test]
    async fn anchored_edits_preserve_every_lease_refusal() {
        for payload in anchored_payloads()
            .into_iter()
            .chain(segment_payloads())
            .chain(list_payloads())
            .chain(formatting_payloads())
        {
            for expected in [
                WriteResult::RefusedNoLease,
                WriteResult::RefusedLeaseExpired,
                WriteResult::RefusedLeaseWrongFile,
                WriteResult::RefusedLeaseStale,
            ] {
                let server = MockServer::start().await;
                let (drive, docs) = anchored_setup(&server).await;
                if matches!(&payload, WritePayload::Insert { selection, .. } | WritePayload::Delete { selection, .. } if selection.segment_id.is_some())
                {
                    mount_segments(&server).await;
                }
                let mut opts = replace_opts(false);
                opts.payload = payload.clone();
                opts.lease_token = match expected {
                    WriteResult::RefusedNoLease => None,
                    WriteResult::RefusedLeaseExpired => Some("unknown".into()),
                    WriteResult::RefusedLeaseWrongFile => {
                        Some(seed_lease(&opts.ledger_path, "different-doc", "1"))
                    }
                    _ => Some(seed_lease(&opts.ledger_path, "doc-1", "0")),
                };
                assert_eq!(
                    write(&drive, &docs, &opts, &[rule_for(&payload)])
                        .await
                        .result,
                    expected
                );
                assert!(!server
                    .received_requests()
                    .await
                    .unwrap()
                    .iter()
                    .any(|r| r.url.path().ends_with(":batchUpdate")));
            }
        }
    }

    #[tokio::test]
    async fn anchored_edits_refuse_stale_docs_revisions() {
        for payload in anchored_payloads()
            .into_iter()
            .chain(segment_payloads())
            .chain(list_payloads())
            .chain(formatting_payloads())
        {
            let server = MockServer::start().await;
            let (drive, docs) = anchored_setup(&server).await;
            if matches!(&payload, WritePayload::Insert { selection, .. } | WritePayload::Delete { selection, .. } if selection.segment_id.is_some())
            {
                mount_segments(&server).await;
            }
            Mock::given(method("POST")).and(path("/v1/documents/doc-1:batchUpdate"))
                .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({"error": {"code": 400, "message": "The required revision ID 'rev-anchor' does not match the latest revision."}})))
                .expect(1).mount(&server).await;
            let mut opts = replace_opts(false);
            opts.payload = payload.clone();
            assert!(matches!(
                write(&drive, &docs, &opts, &[rule_for(&payload)])
                    .await
                    .result,
                WriteResult::StaleRevision { .. }
            ));
        }
    }
    #[tokio::test]
    async fn anchored_edits_send_the_child_tab_and_cross_paragraph_range() {
        for payload in [
            WritePayload::Insert {
                anchor: "needle".into(),
                side: Side::After,
                text: "text".into(),
                match_case: true,
                selection: anchor::SegmentSelection::default(),
            },
            WritePayload::Delete {
                from: "first".into(),
                to: Some("needle".into()),
                match_case: true,
                selection: anchor::SegmentSelection::default(),
            },
        ] {
            let server = MockServer::start().await;
            let (drive, docs) = clients(&server).await;
            mount_file("doc-1", GOOGLE_DOC_MIME_TYPE, &["folder-1"])
                .mount(&server)
                .await;
            mount_folder("folder-1").mount(&server).await;
            Mock::given(method("GET")).and(path("/v1/documents/doc-1"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "revisionId": "tab-revision", "tabs": [{
                        "tabProperties": {"tabId": "parent"}, "childTabs": [{
                            "tabProperties": {"tabId": "child"}, "documentTab": {"body": {"content": [
                                {"startIndex": 1, "endIndex": 7, "paragraph": {"elements": [{"startIndex": 1, "endIndex": 7, "textRun": {"content": "first\n"}}]}},
                                {"startIndex": 7, "endIndex": 17, "paragraph": {"elements": [{"startIndex": 7, "endIndex": 17, "textRun": {"content": "😀 needle\n"}}]}}
                            ]}}
                        }]
                    }]
                }))).mount(&server).await;
            mount_batch_update(serde_json::json!({"replies": [{}]}))
                .expect(1)
                .mount(&server)
                .await;
            let mut opts = replace_opts(false);
            opts.payload = payload.clone();
            let outcome = write(&drive, &docs, &opts, &[rule_for(&payload)]).await;
            let edit = match outcome.result {
                WriteResult::Inserted { edit } | WriteResult::Deleted { edit } => edit,
                other => panic!("{other:?}"),
            };
            assert_eq!(edit.tab_id.as_deref(), Some("child"));
            let requests = server.received_requests().await.unwrap();
            let batch = requests
                .iter()
                .find(|r| r.url.path().ends_with(":batchUpdate"))
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&batch.body).unwrap();
            if payload.verb() == WriteVerb::Insert {
                assert_eq!(
                    body["requests"][0]["insertText"]["location"],
                    serde_json::json!({"index": 16, "tabId": "child"})
                );
            } else {
                assert_eq!((edit.chars, edit.bytes, edit.paragraphs), (14, 17, 2));
                assert_eq!(
                    body["requests"][0]["deleteContentRange"]["range"],
                    serde_json::json!({"startIndex": 1, "endIndex": 16, "tabId": "child"})
                );
            }
        }
    }

    // ── Anchored payload validation, rendering and logging ─────────────

    fn insert_payload(anchor: &str, text: &str) -> WritePayload {
        WritePayload::Insert {
            anchor: anchor.into(),
            side: Side::After,
            text: text.into(),
            match_case: true,
            selection: anchor::SegmentSelection::default(),
        }
    }

    fn delete_payload(from: &str, to: Option<&str>) -> WritePayload {
        WritePayload::Delete {
            from: from.into(),
            to: to.map(Into::into),
            match_case: true,
            selection: anchor::SegmentSelection::default(),
        }
    }

    #[test]
    fn validate_refuses_anchored_payloads_that_cannot_address_an_edit() {
        let nothing_left = insert_payload("a", "\0\r\u{e000}").validate().unwrap_err();
        assert!(nothing_left.contains("nothing to insert"), "{nothing_left}");

        for anchor in ["", "two\nlines", "carriage\rreturn"] {
            let err = insert_payload(anchor, "x").validate().unwrap_err();
            assert!(err.contains("anchor must be nonempty"), "{anchor:?}: {err}");
        }
        for (from, to) in [
            ("", None),
            ("two\nlines", None),
            ("a", Some("")),
            ("a", Some("two\nlines")),
            ("a", Some("carriage\rreturn")),
        ] {
            let err = delete_payload(from, to).validate().unwrap_err();
            assert!(
                err.contains("anchors must be nonempty"),
                "{from:?}..{to:?}: {err}"
            );
        }

        assert!(insert_payload("a", "x").validate().is_ok());
        assert!(delete_payload("a", None).validate().is_ok());
        assert!(delete_payload("a", Some("b")).validate().is_ok());
    }

    /// An unaddressable payload is refused before any request, so a typo in an
    /// anchor never costs a round-trip or touches the lease.
    #[tokio::test]
    async fn an_unaddressable_anchored_payload_fails_before_any_network_call() {
        let mut invalid_segment = insert_payload("a", "x");
        if let WritePayload::Insert { selection, .. } = &mut invalid_segment {
            selection.segment_id = Some(String::new());
        }
        let mut invalid_tab = delete_payload("a", None);
        if let WritePayload::Delete { selection, .. } = &mut invalid_tab {
            selection.tab_id = Some("child".into());
        }
        for payload in [
            invalid_segment,
            invalid_tab,
            insert_payload("", "x"),
            insert_payload("a", "\0"),
            delete_payload("", None),
            delete_payload("a", Some("two\nlines")),
            WritePayload::List {
                from: String::new(),
                to: None,
                preset: Some(BulletPreset::BulletCheckbox),
                match_case: true,
            },
            WritePayload::List {
                from: "a".into(),
                to: Some("two\nlines".into()),
                preset: None,
                match_case: true,
            },
        ] {
            let server = MockServer::start().await;
            let (drive, docs) = clients(&server).await;
            let mut opts = replace_opts(false);
            opts.payload = payload;
            let outcome = write(&drive, &docs, &opts, &[]).await;
            assert!(
                matches!(outcome.result, WriteResult::Failed { .. }),
                "{:?}",
                outcome.result
            );
            assert!(server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|r| r.url.path() == "/token"));
        }
    }

    fn preview_edit() -> EditPreview {
        EditPreview {
            segment_id: None,
            segment_kind: None,
            start_index: 4,
            end_index: 6,
            tab_id: None,
            paragraphs: 1,
            chars: 1,
            bytes: 4,
        }
    }

    fn outcome_with(result: WriteResult) -> WriteOutcome {
        WriteOutcome {
            document_id: "doc-1".to_string(),
            file_name: Some("Budget".to_string()),
            resolved_folder_id: None,
            required_revision_id: None,
            result,
        }
    }

    #[test]
    fn describe_reports_selected_segment_identity_without_debug_types() {
        let mut edit = preview_edit();
        edit.segment_id = Some("h1".into());
        edit.segment_kind = Some(anchor::SegmentKind::Header);
        let rendered = describe(
            &outcome_with(WriteResult::WouldInsert { edit }),
            WriteVerb::Insert,
        );
        assert!(rendered.contains(", header h1,"), "{rendered}");
        assert!(!rendered.contains("Some("), "{rendered}");
    }

    #[test]
    fn describe_labels_each_segment_kind() {
        for (kind, label) in [
            (Some(anchor::SegmentKind::Footer), ", footer s1,"),
            (Some(anchor::SegmentKind::Footnote), ", footnote s1,"),
            (None, ", segment s1,"),
        ] {
            let mut edit = preview_edit();
            edit.segment_id = Some("s1".into());
            edit.segment_kind = kind;
            let rendered = describe(
                &outcome_with(WriteResult::WouldDelete { edit }),
                WriteVerb::Delete,
            );
            assert!(rendered.contains(label), "{rendered}");
        }
    }

    #[test]
    fn describe_renders_each_anchored_outcome_with_its_utf16_range() {
        for (result, verb, expected) in [
            (
                WriteResult::WouldInsert {
                    edit: preview_edit(),
                },
                WriteVerb::Insert,
                "Would insert",
            ),
            (
                WriteResult::Inserted {
                    edit: preview_edit(),
                },
                WriteVerb::Insert,
                "Inserted",
            ),
            (
                WriteResult::WouldDelete {
                    edit: preview_edit(),
                },
                WriteVerb::Delete,
                "Would delete",
            ),
            (
                WriteResult::Deleted {
                    edit: preview_edit(),
                },
                WriteVerb::Delete,
                "Deleted",
            ),
        ] {
            let text = describe(&outcome_with(result), verb);
            assert_eq!(
                text,
                format!(
                    "{expected}: 1 char(s) / 4 byte(s) in 'Budget' at [4, 6) UTF-16 code units, tab first, 1 paragraph(s)"
                )
            );
        }

        let mut edit = preview_edit();
        edit.tab_id = Some("t.1".to_string());
        let text = describe(
            &outcome_with(WriteResult::Deleted { edit }),
            WriteVerb::Delete,
        );
        assert!(text.contains("tab t.1,"), "{text}");
    }

    #[test]
    fn list_descriptions_expose_pre_write_indices_and_tab_removal_without_prose() {
        let edit = ListPreview {
            start_index: 1,
            end_index: 20,
            tab_id: Some("child".into()),
            paragraphs: 2,
            leading_tabs_removed: 3,
        };
        let preview = WriteResult::WouldFormatList {
            edit: edit.clone(),
            preset: Some(BulletPreset::BulletCheckbox),
        };
        let applied = WriteResult::ListFormatted {
            edit: ListPreview {
                leading_tabs_removed: 0,
                ..edit
            },
            preset: None,
        };
        assert_eq!(preview.log_status(), "would-format-list");
        assert_eq!(applied.log_status(), "list-formatted");
        for (result, verb, action) in [
            (preview, WriteVerb::CreateBullets, "Would format list"),
            (applied, WriteVerb::DeleteBullets, "Formatted list"),
        ] {
            let text = describe(&outcome_with(result), verb);
            assert!(text.starts_with(action), "{text}");
            assert!(
                text.contains("pre-write [1, 20)")
                    && text.contains("tab child")
                    && text.contains(if verb == WriteVerb::CreateBullets {
                        "3 leading tab(s) removed"
                    } else {
                        "0 leading tab(s) removed"
                    }),
                "{text}"
            );
        }
    }

    #[test]
    fn describe_names_the_unresolved_anchor_without_quoting_document_text() {
        let text = describe(
            &outcome_with(WriteResult::RefusedAnchor {
                error: AnchorError::Ambiguous { count: 2 },
            }),
            WriteVerb::Insert,
        );
        assert!(text.starts_with("Refused: unsafe or unresolved anchor in 'Budget'"));
        assert!(text.contains("Ambiguous"), "{text}");
    }

    #[test]
    fn refusals_name_the_verb_and_the_grant_that_would_allow_it() {
        let not_a_doc = WriteResult::RefusedNotADocument {
            mime_type: GOOGLE_SHEET_MIME_TYPE.to_string(),
        };
        for (verb, label) in [
            (WriteVerb::Replace, "replace"),
            (WriteVerb::Append, "append"),
            (WriteVerb::Insert, "insert"),
            (WriteVerb::Delete, "delete"),
            (WriteVerb::CreateBullets, "create-bullets"),
            (WriteVerb::DeleteBullets, "delete-bullets"),
            (WriteVerb::TextStyle, "text-style"),
            (WriteVerb::ParagraphStyle, "paragraph-style"),
        ] {
            let text = describe(&outcome_with(not_a_doc.clone()), verb);
            assert!(text.contains(&format!("`drive docs {label}`")), "{text}");
        }

        for (verb, grant) in [
            (WriteVerb::Insert, "docs-write"),
            (WriteVerb::Delete, "docs-delete"),
            (WriteVerb::CreateBullets, "docs-write"),
            (WriteVerb::DeleteBullets, "docs-write"),
            (WriteVerb::TextStyle, "docs-format"),
            (WriteVerb::ParagraphStyle, "docs-format"),
        ] {
            let text = describe(&outcome_with(WriteResult::RefusedNoVisibleParents), verb);
            assert!(text.contains(&format!("[\"{grant}\"]")), "{text}");
            assert!(
                text.contains(&format!("granting that folder `{grant}`")),
                "{text}"
            );
        }
    }

    #[test]
    fn describe_names_the_rule_that_blocked_a_write_or_the_default_policy() {
        let blocked = |decided_by| {
            describe(
                &outcome_with(WriteResult::Blocked { decided_by }),
                WriteVerb::Delete,
            )
        };
        assert_eq!(
            blocked(Some(DecidingRule::Folder {
                folder_id: "folder-1".to_string(),
                depth: 2,
            })),
            "Blocked: 'Budget' — refused by rule on folder folder-1 (depth 2)"
        );
        assert_eq!(
            blocked(Some(DecidingRule::File {
                file_id: "doc-1".to_string(),
            })),
            "Blocked: 'Budget' — refused by rule on file doc-1"
        );
        assert_eq!(
            blocked(None),
            "Blocked: 'Budget' — refused by default policy (no matching rule)"
        );
    }

    #[test]
    fn dry_run_statuses_are_named_for_the_log_even_though_only_writes_record() {
        for (result, status) in [
            (
                WriteResult::WouldReplace { occurrences: 1 },
                "would-replace",
            ),
            (
                WriteResult::WouldInsert {
                    edit: preview_edit(),
                },
                "would-insert",
            ),
            (
                WriteResult::WouldDelete {
                    edit: preview_edit(),
                },
                "would-delete",
            ),
            (
                WriteResult::Inserted {
                    edit: preview_edit(),
                },
                "inserted",
            ),
            (
                WriteResult::Deleted {
                    edit: preview_edit(),
                },
                "deleted",
            ),
            (
                WriteResult::WouldFormat {
                    edit: preview_edit(),
                    style: formatting_style(),
                    fields: "bold".to_string(),
                },
                "would-format",
            ),
            (
                WriteResult::Formatted {
                    edit: preview_edit(),
                    style: formatting_style(),
                    fields: "bold".to_string(),
                },
                "formatted",
            ),
        ] {
            assert_eq!(result.log_status(), status);
        }
    }

    fn formatting_style() -> super::super::style::StylePatch {
        super::super::style::StylePatch::Text(super::super::style::TextStyle {
            bold: Some(true),
            ..super::super::style::TextStyle::default()
        })
    }

    /// A preview and the write it predicts differ only in the leading verb;
    /// the range, counts, property mask and explicit style are reported
    /// identically so the two can be diffed by eye.
    #[test]
    fn describe_renders_a_formatting_outcome_with_its_range_mask_and_style() {
        for (result, expected) in [
            (
                WriteResult::WouldFormat {
                    edit: preview_edit(),
                    style: formatting_style(),
                    fields: "bold".to_string(),
                },
                "Would format",
            ),
            (
                WriteResult::Formatted {
                    edit: preview_edit(),
                    style: formatting_style(),
                    fields: "bold".to_string(),
                },
                "Formatted",
            ),
        ] {
            let text = describe(&outcome_with(result), WriteVerb::TextStyle);
            assert!(
                text.starts_with(&format!(
                    "{expected}: 'Budget' tab None UTF-16 [4, 6) / 1 paragraph(s) / 1 char(s) / 4 byte(s); fields=bold; style="
                )),
                "{text}"
            );
            assert!(text.contains("bold: Some(true)"), "{text}");
        }

        let mut edit = preview_edit();
        edit.tab_id = Some("t.1".to_string());
        let text = describe(
            &outcome_with(WriteResult::Formatted {
                edit,
                style: formatting_style(),
                fields: "bold".to_string(),
            }),
            WriteVerb::TextStyle,
        );
        assert!(text.contains("tab Some(\"t.1\")"), "{text}");
    }

    fn formatting_payloads() -> Vec<WritePayload> {
        use super::super::style::*;
        [
            StylePatch::Text(TextStyle {
                bold: Some(false),
                italic: Some(true),
                ..TextStyle::default()
            }),
            StylePatch::Paragraph(ParagraphStyle {
                alignment: Some(Alignment::Center),
                named_style_type: Some(NamedStyle::Heading1),
            }),
        ]
        .into_iter()
        .map(|style| WritePayload::Format {
            from: "Q3".into(),
            to: None,
            match_case: true,
            style,
        })
        .collect()
    }

    #[tokio::test]
    async fn formatting_previews_and_writes_share_exact_wire_effects() {
        for payload in formatting_payloads() {
            let server = MockServer::start().await;
            let (drive, docs) = anchored_setup(&server).await;
            let rule = rule_for(&payload);
            let mut opts = replace_opts(true);
            opts.payload = payload;
            let preview = write(&drive, &docs, &opts, std::slice::from_ref(&rule)).await;
            let (edit, style, fields) = match preview.result {
                WriteResult::WouldFormat {
                    edit,
                    style,
                    fields,
                } => (edit, style, fields),
                other => panic!("{other:?}"),
            };
            assert!(!server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|r| r.url.path().ends_with(":batchUpdate")));
            mount_batch_update(serde_json::json!({"replies": [{}]}))
                .expect(1)
                .mount(&server)
                .await;
            opts.dry_run = false;
            let actual = write(&drive, &docs, &opts, &[rule]).await;
            assert_eq!(
                actual.result,
                WriteResult::Formatted {
                    edit: edit.clone(),
                    style: style.clone(),
                    fields: fields.clone()
                }
            );
            let requests = server.received_requests().await.unwrap();
            for read in requests
                .iter()
                .filter(|r| r.url.path() == "/v1/documents/doc-1")
            {
                assert!(read
                    .url
                    .query_pairs()
                    .any(|(k, v)| k == "suggestionsViewMode" && v == "SUGGESTIONS_INLINE"));
            }
            let batch = requests
                .iter()
                .find(|r| r.url.path().ends_with(":batchUpdate"))
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&batch.body).unwrap();
            assert_eq!(
                body["writeControl"],
                serde_json::json!({"requiredRevisionId": "rev-anchor"})
            );
            assert_eq!(body["requests"].as_array().unwrap().len(), 1);
            let (verb, properties, expected_style, expected_range) = match style {
                super::super::style::StylePatch::Text(_) => (
                    "updateTextStyle",
                    "textStyle",
                    serde_json::json!({"bold": false, "italic": true}),
                    (4, 6),
                ),
                super::super::style::StylePatch::Paragraph(_) => (
                    "updateParagraphStyle",
                    "paragraphStyle",
                    serde_json::json!({"alignment": "CENTER", "namedStyleType": "HEADING_1"}),
                    (1, 14),
                ),
            };
            assert_eq!((edit.start_index, edit.end_index), expected_range);
            assert_eq!(
                body["requests"][0][verb],
                serde_json::json!({"range": {"startIndex": edit.start_index, "endIndex": edit.end_index}, properties: expected_style, "fields": fields})
            );
        }
    }

    #[tokio::test]
    async fn formatting_is_blocked_before_content_reads_by_other_grants() {
        for payload in formatting_payloads() {
            for grant in [
                DriveOperation::DocsWrite,
                DriveOperation::DocsDelete,
                DriveOperation::Edit,
                DriveOperation::SheetsWrite,
                DriveOperation::SlidesWrite,
            ] {
                let server = MockServer::start().await;
                let (drive, docs) = clients(&server).await;
                mount_file("doc-1", GOOGLE_DOC_MIME_TYPE, &["folder-1"])
                    .mount(&server)
                    .await;
                mount_folder("folder-1").mount(&server).await;
                let mut rule = rule_for(&payload);
                rule.allow = std::iter::once(grant).collect();
                let mut opts = replace_opts(true);
                opts.payload = payload.clone();
                assert!(matches!(
                    write(&drive, &docs, &opts, &[rule]).await.result,
                    WriteResult::Blocked { .. }
                ));
                assert!(!server
                    .received_requests()
                    .await
                    .unwrap()
                    .iter()
                    .any(|r| r.url.path().starts_with("/v1/documents")));
            }
        }
    }

    #[test]
    fn empty_style_and_invalid_anchors_fail_before_network() {
        let mut payload = formatting_payloads().remove(0);
        if let WritePayload::Format { style, .. } = &mut payload {
            *style =
                super::super::style::StylePatch::Text(super::super::style::TextStyle::default());
        }
        assert!(payload.validate().is_err());
        for anchor in ["", "a\nb"] {
            let mut payload = formatting_payloads().remove(0);
            if let WritePayload::Format { from, .. } = &mut payload {
                *from = anchor.into();
            }
            assert!(payload.validate().is_err());
        }
    }

    fn table_payloads() -> Vec<WritePayload> {
        use super::super::table::{TableEdit, TableVerb};
        let mut payloads = vec![WritePayload::Table(TableEdit::Insert {
            anchor: "Intro".into(),
            side: Side::After,
            rows: 2,
            columns: 3,
            match_case: true,
        })];
        for verb in [
            TableVerb::InsertRow,
            TableVerb::InsertColumn,
            TableVerb::DeleteRow,
            TableVerb::DeleteColumn,
        ] {
            payloads.push(WritePayload::Table(TableEdit::Dimension {
                verb,
                cell: "Alpha".into(),
                after: true,
                match_case: true,
            }));
        }
        payloads
    }

    async fn table_setup(server: &MockServer) -> (DriveClient, DocsClient) {
        let clients = clients(server).await;
        mount_file("doc-1", GOOGLE_DOC_MIME_TYPE, &["folder-1"])
            .mount(server)
            .await;
        mount_folder("folder-1").mount(server).await;
        Mock::given(method("GET"))
            .and(path("/v1/documents/doc-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(super::super::table::fixture()))
            .mount(server)
            .await;
        clients
    }

    #[tokio::test]
    async fn table_gate_isolation_blocks_before_content_fetch_in_preview_and_real_runs() {
        for payload in table_payloads() {
            for dry_run in [false, true] {
                for grant in [
                    DriveOperation::Edit,
                    DriveOperation::DocsWrite,
                    DriveOperation::DocsDelete,
                    DriveOperation::SheetsStructure,
                    DriveOperation::SheetsDelete,
                    if payload.gate_operation() == DriveOperation::DocsStructure {
                        DriveOperation::DocsTableDelete
                    } else {
                        DriveOperation::DocsStructure
                    },
                ] {
                    let server = MockServer::start().await;
                    let (drive, docs) = clients(&server).await;
                    mount_file("doc-1", GOOGLE_DOC_MIME_TYPE, &["folder-1"])
                        .mount(&server)
                        .await;
                    mount_folder("folder-1").mount(&server).await;
                    let mut rule = rule_for(&payload);
                    rule.allow = [grant].into_iter().collect();
                    let mut opts = replace_opts(dry_run);
                    opts.payload = payload.clone();
                    assert!(matches!(
                        write(&drive, &docs, &opts, &[rule]).await.result,
                        WriteResult::Blocked { .. }
                    ));
                    assert!(server
                        .received_requests()
                        .await
                        .unwrap()
                        .iter()
                        .all(|r| !r.url.path().starts_with("/v1/documents")));
                }
            }
        }
    }

    #[tokio::test]
    async fn table_preview_and_leased_write_share_effects_and_one_revision_pinned_request() {
        for payload in table_payloads() {
            let server = MockServer::start().await;
            let (drive, docs) = table_setup(&server).await;
            let mut opts = replace_opts(true);
            opts.payload = payload;
            let rule = rule_for(&opts.payload);
            opts.lease_token = None;
            let preview = write(&drive, &docs, &opts, std::slice::from_ref(&rule)).await;
            let WriteResult::WouldEditTable { edit: expected } = preview.result else {
                panic!("{preview:?}");
            };
            assert!(server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|r| !r.url.path().ends_with(":batchUpdate")));
            mount_batch_update(serde_json::json!({"replies":[{}]}))
                .expect(1)
                .mount(&server)
                .await;
            let (token, path) = leased_opts_for("doc-1");
            opts.lease_token = token;
            opts.ledger_path = path;
            opts.dry_run = false;
            let applied = write(&drive, &docs, &opts, &[rule]).await;
            assert_eq!(
                applied.result,
                WriteResult::EditedTable {
                    edit: expected.clone()
                }
            );
            let requests = server.received_requests().await.unwrap();
            for read in requests
                .iter()
                .filter(|r| r.url.path() == "/v1/documents/doc-1")
            {
                assert!(read
                    .url
                    .query_pairs()
                    .any(|(k, v)| k == "suggestionsViewMode" && v == "SUGGESTIONS_INLINE"));
            }
            let batch = requests
                .iter()
                .find(|r| r.url.path().ends_with(":batchUpdate"))
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&batch.body).unwrap();
            let WritePayload::Table(edit) = &opts.payload else {
                unreachable!()
            };
            let doc = serde_json::from_value(super::super::table::fixture()).unwrap();
            let (request, _) = super::super::table::resolve(&doc, edit).unwrap();
            assert_eq!(
                body,
                serde_json::to_value(super::super::write_types::BatchUpdateDocumentRequest::new(
                    request,
                    "rev-table"
                ))
                .unwrap()
            );
        }
    }

    #[tokio::test]
    async fn table_writes_refuse_missing_and_stale_backup_leases_and_surface_stale_docs_revisions()
    {
        for payload in table_payloads() {
            for stage in 0..3 {
                let server = MockServer::start().await;
                let (drive, docs) = table_setup(&server).await;
                let mut opts = replace_opts(false);
                opts.payload = payload.clone();
                match stage {
                    0 => opts.lease_token = None,
                    1 => opts.lease_token = Some(seed_lease(&opts.ledger_path, "doc-1", "0")),
                    _ => {
                        Mock::given(method("POST")).and(path("/v1/documents/doc-1:batchUpdate"))
                        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({"error":{"code":400,"message":"The required revision ID does not match the latest revision.","status":"INVALID_ARGUMENT"}})))
                        .expect(1).mount(&server).await;
                    }
                }
                let result = write(&drive, &docs, &opts, &[rule_for(&payload)])
                    .await
                    .result;
                match stage {
                    0 => assert!(matches!(result, WriteResult::RefusedNoLease)),
                    1 => assert!(matches!(result, WriteResult::RefusedLeaseStale { .. })),
                    _ => assert!(matches!(result, WriteResult::StaleRevision { .. })),
                }
                if stage < 2 {
                    assert!(server
                        .received_requests()
                        .await
                        .unwrap()
                        .iter()
                        .all(|r| !r.url.path().ends_with(":batchUpdate")));
                }
            }
        }
    }
}
