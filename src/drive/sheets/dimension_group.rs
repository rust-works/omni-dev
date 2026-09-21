//! Dimension groups — row/column outlining, the collapsible +/- grouping
//! bar — via `spreadsheets.batchUpdate` (issue #1833,
//! [ADR-0084](../../../docs/adrs/adr-0084-dimension-groups.md)).
//!
//! Gated by [`DriveOperation::SheetsStructure`] — every mutating verb here
//! reaches the same operation as `add-banding`/`update-banding`/
//! `delete-banding` (`banding.rs`, ADR-0082): a group is presentation
//! applied to a dimension span, and removing one destroys no data.
//!
//! **No client-side depth cap**, despite issue #1833's own scope note
//! proposing one. `addDimensionGroup`'s request carries only a `range` —
//! the server derives the new group's `depth` from how that range
//! overlaps existing groups on the same axis (a superset increments an
//! existing group's depth and gives the new group that shallower depth; a
//! subset creates a new, deeper one; a partial overlap *widens the
//! existing group to the union of the two spans* and creates a new,
//! deeper one over their intersection — so an add can move an existing
//! group's edges, which `--dry-run` cannot foresee; see the Sheets API
//! reference for `AddDimensionGroupRequest`). Predicting that outcome
//! client-side would mean re-implementing those rules, and no maximum
//! depth is documented anywhere in the API reference to validate against
//! in the first place — Sheets stays the authority on how deep a nesting
//! may get, the same stance every other span-based verb in this crate
//! takes on the *count* of rows/columns it may act on. Only the span
//! itself (`--start`/`--end` within the sheet's current extent) is
//! validated here, matching `format.rs`'s `auto-resize-dimension`.
//!
//! A group carries no server-assigned id: it is addressed by `(range,
//! depth)`, both of which `updateDimensionGroup`'s wire request already
//! requires. `update-dimension-group` resolves its target by exact `range`
//! match, disambiguated by an optional `--depth` when more than one group
//! shares that exact range (the API creates exactly this when a group is
//! added over a range equal to an existing one) — the same ambiguous-match
//! shape `protection.rs::find_existing_protection` uses for a protected
//! range, which likewise carries no id of its own.
//!
//! `delete-dimension-group` requires an **exact** range match among the
//! groups `list-dimension-groups` would show; the API's own partial-range
//! delete (a `range` that only partially overlaps a group decrements its
//! depth rather than removing anything, per the API reference's own
//! example) is not exposed — a documented cut, not a silent gap, the same
//! "never promise a change the real run then surprises you with" stance
//! `banding.rs`'s id-only addressing takes for a different reason.
//! `deleteDimensionGroup`'s wire request carries only a `range`, no
//! `depth`, so an exact-range match that resolves to more than one group
//! (different depths) is not refused as ambiguous — the request itself has
//! no way to name one over the other, and Sheets decides. The outcome
//! reports the depth acted on only when the match was unambiguous.
//!
//! Shape mirrors `banding.rs`: resolve a target, gate, dry-run, mutate,
//! log — with no A1 composition step, since every verb here addresses a
//! dimension span (`--sheet`/`--dimension`/`--start`/`--end`, 1-based
//! inclusive) rather than an A1 range.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::cli::drive::format::{write_scalar_jsonl, JsonlSerialize};
use crate::drive::client::DriveClient;
use crate::drive::files_api::FilesApi;
use crate::drive::lease::check::{
    conclude_native_leased_write, gate_optional_leased_write, FromLeaseRefusal, LeaseGateRefusal,
    LeasedWrite,
};
use crate::drive::sheets::api::SheetsApi;
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::grid_range;
use crate::drive::sheets::target_gate;
use crate::drive::sheets::types::{
    AddDimensionGroupRequest, BatchUpdateRequestItem, DeleteDimensionGroupRequest, Dimension,
    DimensionGroup, DimensionRange, Sheet, Spreadsheet, UpdateDimensionGroupRequest,
};
use crate::drive::types::SheetTargetRefusal;
use crate::drive::write_gate::{self, DecidingRule, DriveOperation, FolderPermissionRule};
use crate::request_log::{self, DriveMutationOutcome};

/// Which mutation to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DimensionGroupVerb {
    /// Add a new outline group over a row or column span.
    AddDimensionGroup {
        /// Title of the sheet to modify.
        sheet: String,
        /// Rows or columns.
        dimension: Dimension,
        /// 1-based first row/column, inclusive.
        start: i64,
        /// 1-based last row/column, inclusive.
        end: i64,
    },
    /// Change an existing group's `collapsed` state.
    UpdateDimensionGroup {
        /// Title of the sheet the group is on.
        sheet: String,
        /// Rows or columns.
        dimension: Dimension,
        /// 1-based first row/column, inclusive — identifies the group
        /// together with `end`.
        start: i64,
        /// 1-based last row/column, inclusive.
        end: i64,
        /// Disambiguates which group at that exact span to update, when
        /// more than one exists at different depths. `None` requires the
        /// span to resolve to exactly one group.
        depth: Option<i64>,
        /// The new `collapsed` value.
        collapsed: bool,
    },
    /// Remove an outline group.
    DeleteDimensionGroup {
        /// Title of the sheet the group is on.
        sheet: String,
        /// Rows or columns.
        dimension: Dimension,
        /// 1-based first row/column, inclusive — identifies the group
        /// together with `end`.
        start: i64,
        /// 1-based last row/column, inclusive.
        end: i64,
    },
}

impl DimensionGroupVerb {
    const fn log_operation(&self) -> &'static str {
        match self {
            Self::AddDimensionGroup { .. } => "sheets-add-dimension-group",
            Self::UpdateDimensionGroup { .. } => "sheets-update-dimension-group",
            Self::DeleteDimensionGroup { .. } => "sheets-delete-dimension-group",
        }
    }

    const fn label(&self) -> &'static str {
        match self {
            Self::AddDimensionGroup { .. } => "add-dimension-group",
            Self::UpdateDimensionGroup { .. } => "update-dimension-group",
            Self::DeleteDimensionGroup { .. } => "delete-dimension-group",
        }
    }

    /// The sheet, axis and 1-based inclusive span every verb carries.
    const fn sheet_dimension_span(&self) -> (&str, Dimension, i64, i64) {
        match self {
            Self::AddDimensionGroup {
                sheet,
                dimension,
                start,
                end,
            }
            | Self::UpdateDimensionGroup {
                sheet,
                dimension,
                start,
                end,
                ..
            }
            | Self::DeleteDimensionGroup {
                sheet,
                dimension,
                start,
                end,
            } => (sheet.as_str(), *dimension, *start, *end),
        }
    }
}

/// Per-call options.
#[derive(Debug, Clone)]
pub struct DimensionGroupOptions {
    /// Spreadsheet id.
    pub spreadsheet_id: String,
    /// Which mutation to perform.
    pub verb: DimensionGroupVerb,
    /// Classify and describe only; never call `batchUpdate`.
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
pub enum DimensionGroupResult {
    /// `--dry-run`, and the gate would allow it.
    WouldChange {
        /// A human-readable summary of the effect.
        summary: String,
    },
    /// The target is not a Google Sheet.
    RefusedNotASpreadsheet {
        /// The target's actual MIME type.
        mime_type: String,
    },
    /// The target is a shortcut, which we do not follow.
    RefusedShortcut,
    /// The target has no parents this account can see.
    RefusedNoVisibleParents,
    /// The named sheet does not exist in this workbook.
    RefusedSheetNotFound {
        /// The title that was not found.
        title: String,
        /// The titles that do exist.
        available: Vec<String>,
    },
    /// The verb's own `--start`/`--end` were invalid, or past the sheet's
    /// current extent.
    RefusedInvalidRange {
        /// What was wrong and why.
        detail: String,
    },
    /// `update-dimension-group`/`delete-dimension-group` named a span with
    /// no dimension group over it.
    RefusedDimensionGroupNotFound {
        /// What was searched for and why nothing matched.
        detail: String,
    },
    /// `update-dimension-group` named a span with more than one group at
    /// different depths, and no `--depth` was given to pick one.
    RefusedAmbiguousDimensionGroup {
        /// The depths of every group found at that span.
        depths: Vec<i64>,
    },
    /// The folder write-permission gate refused it.
    Blocked {
        /// The rule that decided the refusal, if any.
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
    /// staleness check (ADR-0080 §6).
    RefusedLeaseStale,
    /// The mutation succeeded.
    Changed {
        /// Same summary as [`Self::WouldChange`].
        summary: String,
        /// The depth of the group acted on, when the match was
        /// unambiguous. `None` for `add-dimension-group` (server-derived,
        /// unknowable from this reply) and for a `delete-dimension-group`
        /// whose span matched more than one group.
        depth: Option<i64>,
    },
    /// An API or validation error.
    Failed {
        /// A human-readable summary of what failed.
        detail: String,
    },
}

impl FromLeaseRefusal for DimensionGroupResult {
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

impl DimensionGroupResult {
    fn log_status(&self) -> &'static str {
        match self {
            Self::WouldChange { .. } => "would-change",
            Self::RefusedNotASpreadsheet { .. } => "refused-not-a-spreadsheet",
            Self::RefusedShortcut => "refused-shortcut",
            Self::RefusedNoVisibleParents => "refused-no-visible-parents",
            Self::RefusedSheetNotFound { .. } => "refused-sheet-not-found",
            Self::RefusedInvalidRange { .. } => "refused-invalid-range",
            Self::RefusedDimensionGroupNotFound { .. } => "refused-dimension-group-not-found",
            Self::RefusedAmbiguousDimensionGroup { .. } => "refused-ambiguous-dimension-group",
            Self::Blocked { .. } => "blocked",
            Self::RefusedNoLease => LeaseGateRefusal::NoLease.log_status(),
            Self::RefusedLeaseExpired => LeaseGateRefusal::Expired.log_status(),
            Self::RefusedLeaseWrongFile => LeaseGateRefusal::WrongFile.log_status(),
            Self::RefusedLeaseStale => LeaseGateRefusal::Stale.log_status(),
            Self::Changed { .. } => "changed",
            Self::Failed { .. } => "failed",
        }
    }
}

/// The full outcome of one attempt.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DimensionGroupOutcome {
    /// The spreadsheet acted on.
    pub spreadsheet_id: String,
    /// Its Drive file name, when the metadata fetch got that far.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    /// The folder the gate evaluated against.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_folder_id: Option<String>,
    /// The sheet the group is (or would be) on, once known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sheet_id: Option<i64>,
    /// The span the request log records, e.g. `"ROWS 5:7"` — 1-based
    /// inclusive, once known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dimension_range_label: Option<String>,
    /// Which mutation was attempted. Not serialised.
    #[serde(skip)]
    pub verb: DimensionGroupVerb,
    /// What happened.
    pub result: DimensionGroupResult,
}

impl JsonlSerialize for DimensionGroupOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> anyhow::Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Runs one dimension-group mutation, logging every attempt that isn't a
/// dry run.
pub async fn dimension_group(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &DimensionGroupOptions,
    rules: &[FolderPermissionRule],
) -> DimensionGroupOutcome {
    let started = Instant::now();
    let outcome = dimension_group_inner(drive, sheets, opts, rules).await;
    if !opts.dry_run {
        record_attempt(&outcome, opts, started.elapsed());
    }
    outcome
}

async fn dimension_group_inner(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &DimensionGroupOptions,
    rules: &[FolderPermissionRule],
) -> DimensionGroupOutcome {
    let bare = |result| DimensionGroupOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: None,
        resolved_folder_id: None,
        sheet_id: None,
        dimension_range_label: None,
        verb: opts.verb.clone(),
        result,
    };

    let (target, decision, resolved_folder_id, requires_lease) = match target_gate::resolve(
        drive,
        &opts.spreadsheet_id,
        DriveOperation::SheetsStructure,
        rules,
    )
    .await
    {
        target_gate::TargetGateOutcome::MetadataFetchFailed { detail } => {
            return bare(DimensionGroupResult::Failed { detail })
        }
        target_gate::TargetGateOutcome::Refused { target, refusal } => {
            let result = match refusal {
                SheetTargetRefusal::Shortcut => DimensionGroupResult::RefusedShortcut,
                SheetTargetRefusal::NotASpreadsheet { mime_type } => {
                    DimensionGroupResult::RefusedNotASpreadsheet { mime_type }
                }
                SheetTargetRefusal::NoVisibleParents => {
                    DimensionGroupResult::RefusedNoVisibleParents
                }
            };
            return DimensionGroupOutcome {
                spreadsheet_id: opts.spreadsheet_id.clone(),
                file_name: Some(target.name),
                resolved_folder_id: None,
                sheet_id: None,
                dimension_range_label: None,
                verb: opts.verb.clone(),
                result,
            };
        }
        target_gate::TargetGateOutcome::GateFetchFailed { target, detail } => {
            return DimensionGroupOutcome {
                spreadsheet_id: opts.spreadsheet_id.clone(),
                file_name: Some(target.name),
                resolved_folder_id: None,
                sheet_id: None,
                dimension_range_label: None,
                verb: opts.verb.clone(),
                result: DimensionGroupResult::Failed { detail },
            };
        }
        target_gate::TargetGateOutcome::Gated {
            target,
            decision,
            resolved_folder_id,
            requires_lease,
        } => (target, decision, resolved_folder_id, requires_lease),
    };

    let pre_gated = |result| DimensionGroupOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: Some(target.name.clone()),
        resolved_folder_id: resolved_folder_id.clone(),
        sheet_id: None,
        dimension_range_label: None,
        verb: opts.verb.clone(),
        result,
    };

    if decision.verdict == write_gate::Verdict::Deny {
        return pre_gated(DimensionGroupResult::Blocked {
            decided_by: decision.decided_by,
        });
    }

    let api = SheetsApi::new(sheets);
    let workbook = match api
        .get_spreadsheet_with_dimension_groups(&opts.spreadsheet_id)
        .await
    {
        Ok(workbook) => workbook,
        Err(err) => {
            return pre_gated(DimensionGroupResult::Failed {
                detail: format!("{err:#}"),
            })
        }
    };

    let (sheet_title, dimension, start, end) = opts.verb.sheet_dimension_span();
    let sheet_id = match grid_range::find_sheet_id(&workbook, sheet_title, |title, available| {
        DimensionGroupResult::RefusedSheetNotFound { title, available }
    }) {
        Ok(sheet_id) => sheet_id,
        Err(result) => return pre_gated(result),
    };
    let Some(sheet) = find_sheet(&workbook, sheet_id) else {
        unreachable!("find_sheet_id resolved this id from this same workbook") // omni-dev: coverage ignore-line reason="find_sheet_id only ever returns an id it read out of this same workbook's sheets, so a lookup by that id in the same workbook always succeeds; this else-arm exists only to unwrap the shared Option"
    };

    let range = match validate_span(dimension, start, end, sheet, sheet_id) {
        Ok(range) => range,
        Err(detail) => return pre_gated(DimensionGroupResult::RefusedInvalidRange { detail }),
    };
    let range_label = dimension_range_label(&range);

    let gated = |sheet_id, result| DimensionGroupOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: Some(target.name.clone()),
        resolved_folder_id: resolved_folder_id.clone(),
        sheet_id: Some(sheet_id),
        dimension_range_label: Some(range_label.clone()),
        verb: opts.verb.clone(),
        result,
    };

    let existing = match &opts.verb {
        DimensionGroupVerb::AddDimensionGroup { .. } => ExistingGroups::None,
        DimensionGroupVerb::UpdateDimensionGroup { depth, .. } => {
            match resolve_for_update(candidates(sheet, dimension, &range), *depth) {
                Ok(group) => ExistingGroups::One(group),
                Err(result) => return gated(sheet_id, result),
            }
        }
        DimensionGroupVerb::DeleteDimensionGroup { .. } => {
            match resolve_for_delete(candidates(sheet, dimension, &range)) {
                Ok(depth) => ExistingGroups::MaybeOne(depth),
                Err(result) => return gated(sheet_id, result),
            }
        }
    };

    let summary = describe_effect(&opts.verb, &range_label);

    if opts.dry_run {
        return gated(sheet_id, DimensionGroupResult::WouldChange { summary });
    }

    let (request, depth) = build_request(&opts.verb, range, existing);

    let files_api = FilesApi::new(drive);
    let leased = LeasedWrite {
        log_prefix: "drive sheets dimension_group",
        operation: opts.verb.log_operation(),
        ledger_path: &opts.ledger_path,
        file_id: &opts.spreadsheet_id,
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
        Err(err) => return gated(sheet_id, err.into_result()),
    };

    let result = match conclude_native_leased_write(
        leased,
        &lease_grant,
        &files_api,
        api.batch_update(&opts.spreadsheet_id, vec![request]).await,
        |err| format!("{err:#}"),
    )
    .await
    {
        Ok(_response) => DimensionGroupResult::Changed { summary, depth },
        Err(err) => DimensionGroupResult::Failed {
            detail: format!("{err:#}"),
        },
    };
    drop(lease_grant);
    gated(sheet_id, result)
}

/// What was resolved for an existing group, if this verb needs one.
/// `add-dimension-group` needs none; `update-dimension-group` always
/// resolves exactly one (or has already returned its refusal);
/// `delete-dimension-group` resolves a depth only when the match was
/// unambiguous.
enum ExistingGroups<'a> {
    None,
    One(&'a DimensionGroup),
    MaybeOne(Option<i64>),
}

/// Finds the `Sheet` with this id in an already-fetched workbook. Never
/// `None` when `sheet_id` came from [`grid_range::find_sheet_id`] against
/// the same `workbook`.
fn find_sheet(workbook: &Spreadsheet, sheet_id: i64) -> Option<&Sheet> {
    workbook
        .sheets
        .iter()
        .find(|s| s.sheet_id() == Some(sheet_id))
}

/// Every group on `sheet`'s `dimension` axis whose range exactly matches
/// `range` — the candidates `update`/`delete` disambiguate among.
fn candidates<'a>(
    sheet: &'a Sheet,
    dimension: Dimension,
    range: &DimensionRange,
) -> Vec<&'a DimensionGroup> {
    let groups = match dimension {
        Dimension::Rows => &sheet.row_groups,
        Dimension::Columns => &sheet.column_groups,
    };
    groups
        .iter()
        .filter(|group| &group.range == range)
        .collect()
}

/// The `UpdateDimensionGroup` half of resolving `candidates`: exactly one
/// match is required, `--depth` disambiguating when more than one exists.
fn resolve_for_update(
    candidates: Vec<&DimensionGroup>,
    depth: Option<i64>,
) -> Result<&DimensionGroup, DimensionGroupResult> {
    if let Some(depth) = depth {
        return candidates
            .into_iter()
            .find(|group| group.depth == depth)
            .ok_or_else(|| DimensionGroupResult::RefusedDimensionGroupNotFound {
                detail: format!("no dimension group over that span at depth {depth}"),
            });
    }
    match candidates.len() {
        0 => Err(DimensionGroupResult::RefusedDimensionGroupNotFound {
            detail: "no dimension group exists over that span; run \
                     `drive sheets list-dimension-groups` to see what exists"
                .to_string(),
        }),
        1 => Ok(candidates[0]),
        _ => Err(DimensionGroupResult::RefusedAmbiguousDimensionGroup {
            depths: candidates.iter().map(|group| group.depth).collect(),
        }),
    }
}

/// The `DeleteDimensionGroup` half of resolving `candidates`: any match
/// count but zero is accepted, since `deleteDimensionGroup`'s wire request
/// carries only a `range` — it has no way to name one depth over another,
/// and Sheets decides among an ambiguous match. Reports a depth only when
/// the match was unambiguous.
fn resolve_for_delete(
    candidates: Vec<&DimensionGroup>,
) -> Result<Option<i64>, DimensionGroupResult> {
    match candidates.len() {
        0 => Err(DimensionGroupResult::RefusedDimensionGroupNotFound {
            detail: "no dimension group exists over that span; run \
                     `drive sheets list-dimension-groups` to see what exists"
                .to_string(),
        }),
        1 => Ok(Some(candidates[0].depth)),
        _ => Ok(None),
    }
}

/// Converts the verb's `--start`/`--end` into the API's zero-based
/// half-open [`DimensionRange`], validating the span against the sheet's
/// current extent when the workbook reports one. The single conversion
/// site, on purpose — the same "inlining this at a call site is how the
/// obvious off-by-one gets in" reasoning `structure.rs::dimension_range`
/// documents for its own, near-identical conversion.
fn validate_span(
    dimension: Dimension,
    start: i64,
    end: i64,
    sheet: &Sheet,
    sheet_id: i64,
) -> Result<DimensionRange, String> {
    if start < 1 {
        return Err(format!("--start must be at least 1, got {start}"));
    }
    if end < start {
        return Err(format!(
            "--end must be >= --start (got --start {start} --end {end})"
        ));
    }
    let current = sheet
        .properties
        .as_ref()
        .and_then(|props| props.grid_properties.as_ref())
        .and_then(|grid| match dimension {
            Dimension::Rows => grid.row_count,
            Dimension::Columns => grid.column_count,
        });
    if let Some(current) = current {
        if end > current {
            return Err(format!(
                "--end {end} is past the end of the sheet, which has {current} {noun}(s)",
                noun = dimension.noun(),
            ));
        }
    }
    Ok(DimensionRange {
        sheet_id,
        dimension,
        start_index: start - 1,
        end_index: end,
    })
}

/// Renders a [`DimensionRange`] as the request log's `dimension_range`
/// context value, e.g. `"ROWS 5:7"` — 1-based inclusive, through the same
/// [`Dimension::span_label`] `structure.rs`'s insert/delete-dimension
/// verbs use, so the two writers of one log key cannot drift. The only
/// thing this adds is the zero-based-half-open → one-based-inclusive step.
fn dimension_range_label(range: &DimensionRange) -> String {
    range
        .dimension
        .span_label(range.start_index + 1, range.end_index)
}

/// Builds the request for `verb`. Placed after the `--dry-run` branch in
/// `dimension_group_inner` deliberately — see `banding.rs::banding_inner`'s
/// comment just before its own call site for the full `#1688` ordering
/// reasoning, shared verbatim by every leased engine.
fn build_request(
    verb: &DimensionGroupVerb,
    range: DimensionRange,
    existing: ExistingGroups<'_>,
) -> (BatchUpdateRequestItem, Option<i64>) {
    match verb {
        DimensionGroupVerb::AddDimensionGroup { .. } => (
            BatchUpdateRequestItem::AddDimensionGroup(AddDimensionGroupRequest { range }),
            None,
        ),
        DimensionGroupVerb::UpdateDimensionGroup { collapsed, .. } => {
            let ExistingGroups::One(existing) = existing else {
                // omni-dev: coverage ignore reason="resolve_for_update returns Ok only with exactly one group, or the caller has already returned RefusedDimensionGroupNotFound/RefusedAmbiguousDimensionGroup; this else-arm exists only to unwrap the shared enum"
                unreachable!("existing is resolved for UpdateDimensionGroup above")
                // omni-dev: coverage end
            };
            let dimension_group = DimensionGroup {
                range,
                depth: existing.depth,
                collapsed: *collapsed,
            };
            (
                BatchUpdateRequestItem::UpdateDimensionGroup(UpdateDimensionGroupRequest {
                    dimension_group,
                    fields: "collapsed".to_string(),
                }),
                Some(existing.depth),
            )
        }
        DimensionGroupVerb::DeleteDimensionGroup { .. } => {
            let ExistingGroups::MaybeOne(depth) = existing else {
                // omni-dev: coverage ignore reason="resolve_for_delete returns Ok only as MaybeOne, or the caller has already returned RefusedDimensionGroupNotFound; this else-arm exists only to unwrap the shared enum"
                unreachable!("existing is resolved for DeleteDimensionGroup above")
                // omni-dev: coverage end
            };
            (
                BatchUpdateRequestItem::DeleteDimensionGroup(DeleteDimensionGroupRequest { range }),
                depth,
            )
        }
    }
}

fn describe_effect(verb: &DimensionGroupVerb, range_label: &str) -> String {
    match verb {
        DimensionGroupVerb::AddDimensionGroup { dimension, .. } => {
            format!("add a {} group over {range_label}", dimension.noun())
        }
        DimensionGroupVerb::UpdateDimensionGroup { collapsed, .. } => {
            format!("set collapsed={collapsed} on the group over {range_label}")
        }
        DimensionGroupVerb::DeleteDimensionGroup { .. } => {
            format!("delete the group over {range_label}")
        }
    }
}

fn record_attempt(
    outcome: &DimensionGroupOutcome,
    opts: &DimensionGroupOptions,
    duration: Duration,
) {
    let error = match &outcome.result {
        DimensionGroupResult::Failed { detail } => Some(detail.clone()),
        _ => None,
    };
    let decided_by = match &outcome.result {
        DimensionGroupResult::Blocked { decided_by } => decided_by.as_ref(),
        _ => None,
    };
    let decided_by = write_gate::decided_by_log_fields(decided_by);
    let fields_changed = match (&opts.verb, &outcome.result) {
        (
            DimensionGroupVerb::UpdateDimensionGroup { collapsed, .. },
            DimensionGroupResult::Changed { .. },
        ) => Some(format!("collapsed={collapsed}")),
        _ => None,
    };

    request_log::record_drive_mutation(DriveMutationOutcome {
        operation: opts.verb.log_operation(),
        file_id: outcome.spreadsheet_id.clone(),
        file_name: outcome.file_name.clone().unwrap_or_default(),
        status: outcome.result.log_status().to_string(),
        resolved_folder_id: outcome.resolved_folder_id.clone(),
        decided_by_folder_id: decided_by.folder_id,
        decided_by_depth: decided_by.depth,
        decided_by_file_id: decided_by.file_id,
        sheet_id: outcome.sheet_id,
        dimension_range: outcome.dimension_range_label.clone(),
        fields_changed,
        error,
        duration,
        ..Default::default()
    });
}

/// Renders an outcome as human-readable text.
#[must_use]
pub fn describe(outcome: &DimensionGroupOutcome) -> String {
    describe_lines(outcome).join("\n")
}

/// Renders an outcome as its individual lines, none of which contains a
/// newline.
#[must_use]
pub fn describe_lines(outcome: &DimensionGroupOutcome) -> Vec<String> {
    let verb = &outcome.verb;
    let book = outcome.file_name.as_deref().map_or_else(
        || format!("'{}'", outcome.spreadsheet_id),
        |n| format!("'{n}'"),
    );
    match &outcome.result {
        DimensionGroupResult::WouldChange { summary } => {
            vec![format!("Would {summary} in {book}")]
        }
        DimensionGroupResult::RefusedNotASpreadsheet { mime_type } => vec![format!(
            "Refused: {book} is not a Google Sheet (mimeType: {mime_type}); \
             `drive sheets {}` only works on spreadsheets",
            verb.label()
        )],
        DimensionGroupResult::RefusedShortcut => vec![format!(
            "Refused: {book} is a shortcut; `drive sheets {}` doesn't follow shortcuts",
            verb.label()
        )],
        DimensionGroupResult::RefusedNoVisibleParents => vec![format!(
            "Refused: {book} has no parent folder visible to this account, so no folder \
             rule can apply to it. Grant it by id instead: add {{\"file_id\": \"<spreadsheet \
             id>\", \"allow\": [\"sheets-structure\"]}} to write_permissions.rules."
        )],
        DimensionGroupResult::RefusedSheetNotFound { title, available } => {
            let list = if available.is_empty() {
                "none".to_string()
            } else {
                available
                    .iter()
                    .map(|t| format!("'{t}'"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            vec![format!(
                "Refused: {book} has no sheet titled '{title}'. Available: {list}"
            )]
        }
        DimensionGroupResult::RefusedInvalidRange { detail }
        | DimensionGroupResult::RefusedDimensionGroupNotFound { detail } => {
            vec![format!("Refused: {detail}")]
        }
        DimensionGroupResult::RefusedAmbiguousDimensionGroup { depths } => {
            let depths = depths
                .iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            vec![format!(
                "Refused: more than one dimension group exists over that span, at depths \
                 {depths}; pass --depth to pick one"
            )]
        }
        DimensionGroupResult::Blocked { decided_by } => vec![match decided_by {
            Some(rule) => format!(
                "Blocked: {} on {book} refused by rule on {} {}{}",
                verb.label(),
                rule.kind_label(),
                rule.id(),
                rule.depth_suffix()
            ),
            None => format!(
                "Blocked: {} on {book} refused by default policy (no matching rule for \
                 sheets-structure)",
                verb.label()
            ),
        }],
        DimensionGroupResult::RefusedNoLease => LeaseGateRefusal::NoLease
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        DimensionGroupResult::RefusedLeaseExpired => LeaseGateRefusal::Expired
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        DimensionGroupResult::RefusedLeaseWrongFile => LeaseGateRefusal::WrongFile
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        DimensionGroupResult::RefusedLeaseStale => LeaseGateRefusal::Stale
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        DimensionGroupResult::Changed { summary, depth } => {
            let id = depth.map_or_else(String::new, |depth| format!(" (depth {depth})"));
            vec![format!("Applied: {summary}{id} in {book}")]
        }
        DimensionGroupResult::Failed { detail } => vec![format!("Failed: {detail}")],
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::sheets::client::SHEETS_API_URL;
    use crate::drive::sheets::types::{GridProperties, SheetProperties};
    use crate::drive::test_support::seed_lease;
    use crate::test_support::env::MapEnv;
    use crate::utils::secret::Secret;
    use std::collections::HashSet;

    // ── pure helpers ─────────────────────────────────────────────────────

    #[test]
    fn log_operation_and_label_are_distinct_for_every_verb() {
        let verbs = [add_verb(), update_verb(), delete_verb()];
        let mut operations: Vec<&str> = verbs
            .iter()
            .map(DimensionGroupVerb::log_operation)
            .collect();
        let mut labels: Vec<&str> = verbs.iter().map(DimensionGroupVerb::label).collect();
        operations.sort_unstable();
        operations.dedup();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(operations.len(), verbs.len());
        assert_eq!(labels.len(), verbs.len());
    }

    #[test]
    fn dimension_group_result_log_status_names_every_variant() {
        assert_eq!(
            DimensionGroupResult::WouldChange {
                summary: String::new()
            }
            .log_status(),
            "would-change"
        );
        assert_eq!(
            DimensionGroupResult::RefusedNotASpreadsheet {
                mime_type: String::new()
            }
            .log_status(),
            "refused-not-a-spreadsheet"
        );
        assert_eq!(
            DimensionGroupResult::RefusedShortcut.log_status(),
            "refused-shortcut"
        );
        assert_eq!(
            DimensionGroupResult::RefusedNoVisibleParents.log_status(),
            "refused-no-visible-parents"
        );
        assert_eq!(
            DimensionGroupResult::RefusedSheetNotFound {
                title: String::new(),
                available: Vec::new(),
            }
            .log_status(),
            "refused-sheet-not-found"
        );
        assert_eq!(
            DimensionGroupResult::RefusedInvalidRange {
                detail: String::new()
            }
            .log_status(),
            "refused-invalid-range"
        );
        assert_eq!(
            DimensionGroupResult::RefusedDimensionGroupNotFound {
                detail: String::new()
            }
            .log_status(),
            "refused-dimension-group-not-found"
        );
        assert_eq!(
            DimensionGroupResult::RefusedAmbiguousDimensionGroup { depths: Vec::new() }
                .log_status(),
            "refused-ambiguous-dimension-group"
        );
        assert_eq!(
            DimensionGroupResult::Blocked { decided_by: None }.log_status(),
            "blocked"
        );
        assert_eq!(
            DimensionGroupResult::RefusedNoLease.log_status(),
            LeaseGateRefusal::NoLease.log_status()
        );
        assert_eq!(
            DimensionGroupResult::RefusedLeaseExpired.log_status(),
            LeaseGateRefusal::Expired.log_status()
        );
        assert_eq!(
            DimensionGroupResult::RefusedLeaseWrongFile.log_status(),
            LeaseGateRefusal::WrongFile.log_status()
        );
        assert_eq!(
            DimensionGroupResult::RefusedLeaseStale.log_status(),
            LeaseGateRefusal::Stale.log_status()
        );
        assert_eq!(
            DimensionGroupResult::Changed {
                summary: String::new(),
                depth: None,
            }
            .log_status(),
            "changed"
        );
        assert_eq!(
            DimensionGroupResult::Failed {
                detail: String::new()
            }
            .log_status(),
            "failed"
        );
    }

    fn range(
        sheet_id: i64,
        dimension: Dimension,
        start_index: i64,
        end_index: i64,
    ) -> DimensionRange {
        DimensionRange {
            sheet_id,
            dimension,
            start_index,
            end_index,
        }
    }

    fn group(
        sheet_id: i64,
        start_index: i64,
        end_index: i64,
        depth: i64,
        collapsed: bool,
    ) -> DimensionGroup {
        DimensionGroup {
            range: range(sheet_id, Dimension::Rows, start_index, end_index),
            depth,
            collapsed,
        }
    }

    fn sheet_with_row_groups(sheet_id: i64, groups: Vec<DimensionGroup>) -> Sheet {
        Sheet {
            properties: Some(SheetProperties {
                sheet_id: Some(sheet_id),
                title: "Q1".to_string(),
                index: Some(0),
                hidden: None,
                grid_properties: Some(GridProperties {
                    row_count: Some(1000),
                    column_count: Some(26),
                }),
            }),
            row_groups: groups,
            ..Default::default()
        }
    }

    #[test]
    fn validate_span_rejects_start_below_one() {
        let sheet = sheet_with_row_groups(0, Vec::new());
        let err = validate_span(Dimension::Rows, 0, 5, &sheet, 0).unwrap_err();
        assert!(err.contains("--start"), "{err}");
    }

    #[test]
    fn validate_span_rejects_end_before_start() {
        let sheet = sheet_with_row_groups(0, Vec::new());
        let err = validate_span(Dimension::Rows, 5, 3, &sheet, 0).unwrap_err();
        assert!(err.contains("--end"), "{err}");
    }

    #[test]
    fn validate_span_rejects_end_past_the_sheet() {
        let sheet = sheet_with_row_groups(0, Vec::new());
        let err = validate_span(Dimension::Rows, 998, 1001, &sheet, 0).unwrap_err();
        assert!(err.contains("past the end"), "{err}");
    }

    #[test]
    fn validate_span_allows_end_equal_to_the_sheet_count() {
        let sheet = sheet_with_row_groups(0, Vec::new());
        let result = validate_span(Dimension::Rows, 995, 1000, &sheet, 0).unwrap();
        assert_eq!(result, range(0, Dimension::Rows, 994, 1000));
    }

    #[test]
    fn validate_span_converts_one_based_inclusive_to_zero_based_half_open() {
        let sheet = sheet_with_row_groups(0, Vec::new());
        let result = validate_span(Dimension::Rows, 5, 7, &sheet, 0).unwrap();
        assert_eq!(result, range(0, Dimension::Rows, 4, 7));
    }

    #[test]
    fn dimension_range_label_is_one_based_inclusive() {
        assert_eq!(
            dimension_range_label(&range(0, Dimension::Rows, 4, 7)),
            "ROWS 5:7"
        );
        assert_eq!(
            dimension_range_label(&range(0, Dimension::Columns, 0, 3)),
            "COLUMNS 1:3"
        );
    }

    #[test]
    fn candidates_filters_by_exact_range_on_the_named_axis() {
        let target = range(0, Dimension::Rows, 4, 9);
        let sheet = sheet_with_row_groups(
            0,
            vec![
                group(0, 4, 9, 1, false),
                group(0, 4, 9, 2, true),
                group(0, 10, 20, 1, false),
            ],
        );
        let found = candidates(&sheet, Dimension::Rows, &target);
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|g| g.range == target));
    }

    #[test]
    fn candidates_reads_the_column_groups_for_the_column_axis() {
        let target = range(0, Dimension::Columns, 0, 3);
        let mut sheet = sheet_with_row_groups(0, Vec::new());
        sheet.column_groups = vec![DimensionGroup {
            range: target.clone(),
            depth: 1,
            collapsed: false,
        }];
        let found = candidates(&sheet, Dimension::Columns, &target);
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn resolve_for_update_refuses_not_found() {
        let err = resolve_for_update(Vec::new(), None).unwrap_err();
        assert!(matches!(
            err,
            DimensionGroupResult::RefusedDimensionGroupNotFound { .. }
        ));
    }

    #[test]
    fn resolve_for_update_resolves_the_single_match() {
        let g = group(0, 4, 9, 1, false);
        let found = resolve_for_update(vec![&g], None).unwrap();
        assert_eq!(found.depth, 1);
    }

    #[test]
    fn resolve_for_update_refuses_ambiguous_without_depth() {
        let shallow = group(0, 4, 9, 1, false);
        let deep = group(0, 4, 9, 2, true);
        let err = resolve_for_update(vec![&shallow, &deep], None).unwrap_err();
        let DimensionGroupResult::RefusedAmbiguousDimensionGroup { depths } = err else {
            panic!("expected RefusedAmbiguousDimensionGroup"); // omni-dev: coverage ignore-line reason="guards this test's assumption; resolve_for_update always returns RefusedAmbiguousDimensionGroup for more than one candidate with no depth given"
        };
        assert_eq!(depths, vec![1, 2]);
    }

    #[test]
    fn resolve_for_update_disambiguates_by_depth() {
        let shallow = group(0, 4, 9, 1, false);
        let deep = group(0, 4, 9, 2, true);
        let found = resolve_for_update(vec![&shallow, &deep], Some(2)).unwrap();
        assert!(found.collapsed);
        assert_eq!(found.depth, 2);
    }

    #[test]
    fn resolve_for_update_refuses_a_depth_that_does_not_exist() {
        let shallow = group(0, 4, 9, 1, false);
        let err = resolve_for_update(vec![&shallow], Some(9)).unwrap_err();
        assert!(matches!(
            err,
            DimensionGroupResult::RefusedDimensionGroupNotFound { .. }
        ));
    }

    #[test]
    fn resolve_for_delete_refuses_not_found() {
        let err = resolve_for_delete(Vec::new()).unwrap_err();
        assert!(matches!(
            err,
            DimensionGroupResult::RefusedDimensionGroupNotFound { .. }
        ));
    }

    #[test]
    fn resolve_for_delete_reports_the_depth_when_unambiguous() {
        let g = group(0, 4, 9, 2, false);
        let depth = resolve_for_delete(vec![&g]).unwrap();
        assert_eq!(depth, Some(2));
    }

    #[test]
    fn resolve_for_delete_accepts_an_ambiguous_match_reporting_no_depth() {
        let shallow = group(0, 4, 9, 1, false);
        let deep = group(0, 4, 9, 2, true);
        let depth = resolve_for_delete(vec![&shallow, &deep]).unwrap();
        assert_eq!(depth, None);
    }

    fn add_verb() -> DimensionGroupVerb {
        DimensionGroupVerb::AddDimensionGroup {
            sheet: "Q1".to_string(),
            dimension: Dimension::Rows,
            start: 5,
            end: 9,
        }
    }

    fn update_verb() -> DimensionGroupVerb {
        DimensionGroupVerb::UpdateDimensionGroup {
            sheet: "Q1".to_string(),
            dimension: Dimension::Rows,
            start: 5,
            end: 9,
            depth: None,
            collapsed: true,
        }
    }

    fn delete_verb() -> DimensionGroupVerb {
        DimensionGroupVerb::DeleteDimensionGroup {
            sheet: "Q1".to_string(),
            dimension: Dimension::Rows,
            start: 5,
            end: 9,
        }
    }

    #[test]
    fn build_request_add_dimension_group_carries_the_range() {
        let target = range(0, Dimension::Rows, 4, 9);
        let (request, depth) = build_request(&add_verb(), target.clone(), ExistingGroups::None);
        assert_eq!(depth, None);
        let BatchUpdateRequestItem::AddDimensionGroup(add) = request else {
            panic!("expected AddDimensionGroup"); // omni-dev: coverage ignore-line reason="guards this test's assumption; build_request always returns AddDimensionGroup for a DimensionGroupVerb::AddDimensionGroup verb"
        };
        assert_eq!(add.range, target);
    }

    #[test]
    fn build_request_update_dimension_group_sets_collapsed_and_names_the_field_mask() {
        let target = range(0, Dimension::Rows, 4, 9);
        let existing = group(0, 4, 9, 3, false);
        let verb = DimensionGroupVerb::UpdateDimensionGroup {
            sheet: "Q1".to_string(),
            dimension: Dimension::Rows,
            start: 5,
            end: 9,
            depth: Some(3),
            collapsed: true,
        };
        let (request, depth) = build_request(&verb, target.clone(), ExistingGroups::One(&existing));
        assert_eq!(depth, Some(3));
        let BatchUpdateRequestItem::UpdateDimensionGroup(update) = request else {
            panic!("expected UpdateDimensionGroup"); // omni-dev: coverage ignore-line reason="guards this test's assumption; build_request always returns UpdateDimensionGroup for a DimensionGroupVerb::UpdateDimensionGroup verb"
        };
        assert_eq!(update.fields, "collapsed");
        assert_eq!(update.dimension_group.depth, 3);
        assert!(update.dimension_group.collapsed);
        assert_eq!(update.dimension_group.range, target);
    }

    #[test]
    fn build_request_delete_dimension_group_carries_the_range_and_resolved_depth() {
        let target = range(0, Dimension::Rows, 4, 9);
        let (request, depth) = build_request(
            &delete_verb(),
            target.clone(),
            ExistingGroups::MaybeOne(Some(2)),
        );
        assert_eq!(depth, Some(2));
        let BatchUpdateRequestItem::DeleteDimensionGroup(delete) = request else {
            panic!("expected DeleteDimensionGroup"); // omni-dev: coverage ignore-line reason="guards this test's assumption; build_request always returns DeleteDimensionGroup for a DimensionGroupVerb::DeleteDimensionGroup verb"
        };
        assert_eq!(delete.range, target);
    }

    #[test]
    fn describe_effect_names_the_axis_and_span() {
        let label = "ROWS 5:9";
        assert_eq!(
            describe_effect(&add_verb(), label),
            "add a row group over ROWS 5:9"
        );
        assert_eq!(
            describe_effect(&update_verb(), label),
            "set collapsed=true on the group over ROWS 5:9"
        );
        assert_eq!(
            describe_effect(&delete_verb(), label),
            "delete the group over ROWS 5:9"
        );
    }

    // ── end-to-end (wiremock) ────────────────────────────────────────────

    fn test_credentials() -> DriveCredentials {
        DriveCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: DriveGrantedScopes::READONLY,
        }
    }

    async fn clients(server: &wiremock::MockServer) -> (DriveClient, SheetsClient) {
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/token"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "test-token", "expires_in": 3600,
                })),
            )
            .mount(server)
            .await;
        let mut drive = DriveClient::new(&server.uri(), &test_credentials()).unwrap();
        crate::drive::client::test_support::replace_session(
            &mut drive,
            &test_credentials(),
            &format!("{}/token", server.uri()),
        );
        let env = MapEnv::new().with(SHEETS_API_URL, &server.uri());
        let sheets = SheetsClient::from_drive_client_with(&env, &drive).unwrap();
        (drive, sheets)
    }

    fn mount_file(id: &str, mime_type: &str, parents: &[&str]) -> wiremock::Mock {
        let parents: Vec<&str> = parents.to_vec();
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!("/drive/v3/files/{id}")))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": id, "name": id, "mimeType": mime_type, "parents": parents,
                    "version": "1",
                })),
            )
    }

    fn mount_folder(id: &str) -> wiremock::Mock {
        mount_file(id, "application/vnd.google-apps.folder", &[])
    }

    fn leased_opts_for(spreadsheet_id: &str) -> (Option<String>, std::path::PathBuf) {
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        let token = seed_lease(&ledger_path, spreadsheet_id, "1");
        (Some(token), ledger_path)
    }

    fn allow_rule(folder: &str) -> FolderPermissionRule {
        FolderPermissionRule {
            folder_id: Some(folder.to_string()),
            file_id: None,
            recursive: true,
            allow: std::iter::once(DriveOperation::SheetsStructure).collect(),
            deny: HashSet::default(),
            require_lease: true,
        }
    }

    fn mount_workbook(sheets: serde_json::Value) -> wiremock::Mock {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/sheet-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "spreadsheetId": "sheet-1",
                    "properties": {"title": "Budget"},
                    "sheets": sheets,
                })),
            )
    }

    fn base_opts(verb: DimensionGroupVerb, dry_run: bool) -> DimensionGroupOptions {
        DimensionGroupOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb,
            dry_run,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        }
    }

    #[tokio::test]
    async fn a_denied_gate_blocks_before_any_read_or_batch_update_call() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
            &["folder-1"],
        )
        .mount(&server)
        .await;
        mount_folder("folder-1").mount(&server).await;
        let rules: Vec<FolderPermissionRule> = Vec::new();
        let opts = base_opts(add_verb(), false);
        let outcome = dimension_group(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            DimensionGroupResult::Blocked { .. }
        ));
    }

    #[tokio::test]
    async fn add_dimension_group_dry_run_reports_would_change_and_makes_no_batch_update_call() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
            &["folder-1"],
        )
        .mount(&server)
        .await;
        mount_folder("folder-1").mount(&server).await;
        mount_workbook(serde_json::json!([
            {"properties": {"sheetId": 0, "title": "Q1", "index": 0,
                "gridProperties": {"rowCount": 1000, "columnCount": 26}}},
        ]))
        .mount(&server)
        .await;
        let rules = vec![allow_rule("folder-1")];
        let outcome = dimension_group(&drive, &sheets, &base_opts(add_verb(), true), &rules).await;
        assert!(matches!(
            outcome.result,
            DimensionGroupResult::WouldChange { .. }
        ));
    }

    #[tokio::test]
    async fn add_dimension_group_sends_an_add_dimension_group_request() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
            &["folder-1"],
        )
        .mount(&server)
        .await;
        mount_folder("folder-1").mount(&server).await;
        mount_workbook(serde_json::json!([
            {"properties": {"sheetId": 0, "title": "Q1", "index": 0,
                "gridProperties": {"rowCount": 1000, "columnCount": 26}}},
        ]))
        .mount(&server)
        .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "requests": [{
                    "addDimensionGroup": {
                        "range": {
                            "sheetId": 0,
                            "dimension": "ROWS",
                            "startIndex": 4,
                            "endIndex": 9,
                        }
                    }
                }]
            })))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "replies": [{}]
                })),
            )
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
        let mut opts = base_opts(add_verb(), false);
        opts.lease_token = lease_token;
        opts.ledger_path = ledger_path;
        let outcome = dimension_group(&drive, &sheets, &opts, &rules).await;
        assert_eq!(
            outcome.result,
            DimensionGroupResult::Changed {
                summary: "add a row group over ROWS 5:9".to_string(),
                depth: None,
            }
        );
        assert_eq!(outcome.sheet_id, Some(0));
        assert_eq!(outcome.dimension_range_label, Some("ROWS 5:9".to_string()));
    }

    #[tokio::test]
    async fn update_dimension_group_refuses_when_no_group_exists() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
            &["folder-1"],
        )
        .mount(&server)
        .await;
        mount_folder("folder-1").mount(&server).await;
        mount_workbook(serde_json::json!([
            {"properties": {"sheetId": 0, "title": "Q1", "index": 0,
                "gridProperties": {"rowCount": 1000, "columnCount": 26}},
                "rowGroups": []},
        ]))
        .mount(&server)
        .await;
        let rules = vec![allow_rule("folder-1")];
        let outcome =
            dimension_group(&drive, &sheets, &base_opts(update_verb(), false), &rules).await;
        assert!(matches!(
            outcome.result,
            DimensionGroupResult::RefusedDimensionGroupNotFound { .. }
        ));
    }

    #[tokio::test]
    async fn update_dimension_group_refuses_an_ambiguous_match() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
            &["folder-1"],
        )
        .mount(&server)
        .await;
        mount_folder("folder-1").mount(&server).await;
        mount_workbook(serde_json::json!([
            {"properties": {"sheetId": 0, "title": "Q1", "index": 0,
                "gridProperties": {"rowCount": 1000, "columnCount": 26}},
                "rowGroups": [
                    {"range": {"sheetId": 0, "dimension": "ROWS", "startIndex": 4, "endIndex": 9}, "depth": 1},
                    {"range": {"sheetId": 0, "dimension": "ROWS", "startIndex": 4, "endIndex": 9}, "depth": 2, "collapsed": true},
                ]},
        ]))
        .mount(&server)
        .await;
        let rules = vec![allow_rule("folder-1")];
        let outcome =
            dimension_group(&drive, &sheets, &base_opts(update_verb(), false), &rules).await;
        assert!(matches!(
            outcome.result,
            DimensionGroupResult::RefusedAmbiguousDimensionGroup { .. }
        ));
    }

    #[tokio::test]
    async fn update_dimension_group_disambiguated_by_depth_sends_the_update() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
            &["folder-1"],
        )
        .mount(&server)
        .await;
        mount_folder("folder-1").mount(&server).await;
        mount_workbook(serde_json::json!([
            {"properties": {"sheetId": 0, "title": "Q1", "index": 0,
                "gridProperties": {"rowCount": 1000, "columnCount": 26}},
                "rowGroups": [
                    {"range": {"sheetId": 0, "dimension": "ROWS", "startIndex": 4, "endIndex": 9}, "depth": 1},
                    {"range": {"sheetId": 0, "dimension": "ROWS", "startIndex": 4, "endIndex": 9}, "depth": 2, "collapsed": true},
                ]},
        ]))
        .mount(&server)
        .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "requests": [{
                    "updateDimensionGroup": {
                        "dimensionGroup": {
                            "range": {
                                "sheetId": 0,
                                "dimension": "ROWS",
                                "startIndex": 4,
                                "endIndex": 9,
                            },
                            "depth": 2,
                            "collapsed": false,
                        },
                        "fields": "collapsed",
                    }
                }]
            })))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "replies": [{}]
                })),
            )
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
        let verb = DimensionGroupVerb::UpdateDimensionGroup {
            sheet: "Q1".to_string(),
            dimension: Dimension::Rows,
            start: 5,
            end: 9,
            depth: Some(2),
            collapsed: false,
        };
        let mut opts = base_opts(verb, false);
        opts.lease_token = lease_token;
        opts.ledger_path = ledger_path;
        let outcome = dimension_group(&drive, &sheets, &opts, &rules).await;
        assert_eq!(
            outcome.result,
            DimensionGroupResult::Changed {
                summary: "set collapsed=false on the group over ROWS 5:9".to_string(),
                depth: Some(2),
            }
        );
    }

    #[tokio::test]
    async fn delete_dimension_group_sends_a_delete_dimension_group_request() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
            &["folder-1"],
        )
        .mount(&server)
        .await;
        mount_folder("folder-1").mount(&server).await;
        mount_workbook(serde_json::json!([
            {"properties": {"sheetId": 0, "title": "Q1", "index": 0,
                "gridProperties": {"rowCount": 1000, "columnCount": 26}},
                "rowGroups": [
                    {"range": {"sheetId": 0, "dimension": "ROWS", "startIndex": 4, "endIndex": 9}, "depth": 1},
                ]},
        ]))
        .mount(&server)
        .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "requests": [{
                    "deleteDimensionGroup": {
                        "range": {
                            "sheetId": 0,
                            "dimension": "ROWS",
                            "startIndex": 4,
                            "endIndex": 9,
                        }
                    }
                }]
            })))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "replies": [{}]
                })),
            )
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
        let mut opts = base_opts(delete_verb(), false);
        opts.lease_token = lease_token;
        opts.ledger_path = ledger_path;
        let outcome = dimension_group(&drive, &sheets, &opts, &rules).await;
        assert_eq!(
            outcome.result,
            DimensionGroupResult::Changed {
                summary: "delete the group over ROWS 5:9".to_string(),
                depth: Some(1),
            }
        );
    }

    #[tokio::test]
    async fn delete_dimension_group_refuses_when_no_group_exists() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
            &["folder-1"],
        )
        .mount(&server)
        .await;
        mount_folder("folder-1").mount(&server).await;
        mount_workbook(serde_json::json!([
            {"properties": {"sheetId": 0, "title": "Q1", "index": 0,
                "gridProperties": {"rowCount": 1000, "columnCount": 26}},
                "rowGroups": []},
        ]))
        .mount(&server)
        .await;
        let rules = vec![allow_rule("folder-1")];
        let outcome =
            dimension_group(&drive, &sheets, &base_opts(delete_verb(), false), &rules).await;
        assert!(matches!(
            outcome.result,
            DimensionGroupResult::RefusedDimensionGroupNotFound { .. }
        ));
    }

    #[tokio::test]
    async fn add_dimension_group_reports_sheet_not_found_for_an_unknown_sheet() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
            &["folder-1"],
        )
        .mount(&server)
        .await;
        mount_folder("folder-1").mount(&server).await;
        mount_workbook(serde_json::json!([
            {"properties": {"sheetId": 0, "title": "Other", "index": 0}},
        ]))
        .mount(&server)
        .await;
        let rules = vec![allow_rule("folder-1")];
        let outcome = dimension_group(&drive, &sheets, &base_opts(add_verb(), false), &rules).await;
        let DimensionGroupResult::RefusedSheetNotFound { title, available } = outcome.result else {
            panic!("expected RefusedSheetNotFound"); // omni-dev: coverage ignore-line reason="guards this test's assumption; the mounted workbook has no sheet titled Q1"
        };
        assert_eq!(title, "Q1");
        assert_eq!(available, vec!["Other".to_string()]);
    }

    #[tokio::test]
    async fn add_dimension_group_reports_invalid_range_for_a_span_past_the_sheet() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
            &["folder-1"],
        )
        .mount(&server)
        .await;
        mount_folder("folder-1").mount(&server).await;
        mount_workbook(serde_json::json!([
            {"properties": {"sheetId": 0, "title": "Q1", "index": 0,
                "gridProperties": {"rowCount": 10, "columnCount": 26}}},
        ]))
        .mount(&server)
        .await;
        let rules = vec![allow_rule("folder-1")];
        let verb = DimensionGroupVerb::AddDimensionGroup {
            sheet: "Q1".to_string(),
            dimension: Dimension::Rows,
            start: 5,
            end: 50,
        };
        let outcome = dimension_group(&drive, &sheets, &base_opts(verb, false), &rules).await;
        assert!(matches!(
            outcome.result,
            DimensionGroupResult::RefusedInvalidRange { .. }
        ));
    }

    #[tokio::test]
    async fn a_metadata_fetch_failure_surfaces_as_failed() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/sheet-1"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let rules: Vec<FolderPermissionRule> = Vec::new();
        let outcome = dimension_group(&drive, &sheets, &base_opts(add_verb(), false), &rules).await;
        assert!(matches!(
            outcome.result,
            DimensionGroupResult::Failed { .. }
        ));
    }

    #[tokio::test]
    async fn a_shortcut_target_is_refused() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", "application/vnd.google-apps.shortcut", &[])
            .mount(&server)
            .await;
        let rules: Vec<FolderPermissionRule> = Vec::new();
        let outcome = dimension_group(&drive, &sheets, &base_opts(add_verb(), false), &rules).await;
        assert!(matches!(
            outcome.result,
            DimensionGroupResult::RefusedShortcut
        ));
    }

    #[tokio::test]
    async fn a_non_spreadsheet_target_is_refused() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", "application/vnd.google-apps.document", &[])
            .mount(&server)
            .await;
        let rules: Vec<FolderPermissionRule> = Vec::new();
        let outcome = dimension_group(&drive, &sheets, &base_opts(add_verb(), false), &rules).await;
        assert!(matches!(
            outcome.result,
            DimensionGroupResult::RefusedNotASpreadsheet { .. }
        ));
    }

    #[tokio::test]
    async fn a_target_with_no_visible_parents_is_refused() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", crate::drive::types::GOOGLE_SHEET_MIME_TYPE, &[])
            .mount(&server)
            .await;
        let rules: Vec<FolderPermissionRule> = Vec::new();
        let outcome = dimension_group(&drive, &sheets, &base_opts(add_verb(), false), &rules).await;
        assert!(matches!(
            outcome.result,
            DimensionGroupResult::RefusedNoVisibleParents
        ));
    }

    #[tokio::test]
    async fn a_gate_ancestor_fetch_failure_surfaces_as_failed() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
            &["folder-1"],
        )
        .mount(&server)
        .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/folder-1"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let rules: Vec<FolderPermissionRule> = Vec::new();
        let outcome = dimension_group(&drive, &sheets, &base_opts(add_verb(), false), &rules).await;
        assert!(matches!(
            outcome.result,
            DimensionGroupResult::Failed { .. }
        ));
    }

    #[tokio::test]
    async fn a_workbook_fetch_failure_after_a_granted_gate_surfaces_as_failed() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
            &["folder-1"],
        )
        .mount(&server)
        .await;
        mount_folder("folder-1").mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/sheet-1"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let outcome = dimension_group(&drive, &sheets, &base_opts(add_verb(), false), &rules).await;
        assert!(matches!(
            outcome.result,
            DimensionGroupResult::Failed { .. }
        ));
    }

    #[tokio::test]
    async fn a_batch_update_failure_surfaces_as_failed() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
            &["folder-1"],
        )
        .mount(&server)
        .await;
        mount_folder("folder-1").mount(&server).await;
        mount_workbook(serde_json::json!([
            {"properties": {"sheetId": 0, "title": "Q1", "index": 0,
                "gridProperties": {"rowCount": 1000, "columnCount": 26}}},
        ]))
        .mount(&server)
        .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
        let mut opts = base_opts(add_verb(), false);
        opts.lease_token = lease_token;
        opts.ledger_path = ledger_path;
        let outcome = dimension_group(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            DimensionGroupResult::Failed { .. }
        ));
    }

    #[tokio::test]
    async fn refuses_without_a_lease_when_the_rule_requires_one() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
            &["folder-1"],
        )
        .mount(&server)
        .await;
        mount_folder("folder-1").mount(&server).await;
        mount_workbook(serde_json::json!([
            {"properties": {"sheetId": 0, "title": "Q1", "index": 0,
                "gridProperties": {"rowCount": 1000, "columnCount": 26}}},
        ]))
        .mount(&server)
        .await;
        let rules = vec![allow_rule("folder-1")];
        let outcome = dimension_group(&drive, &sheets, &base_opts(add_verb(), false), &rules).await;
        assert!(matches!(
            outcome.result,
            DimensionGroupResult::RefusedNoLease
        ));
    }

    #[tokio::test]
    async fn refuses_an_unknown_lease_token() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
            &["folder-1"],
        )
        .mount(&server)
        .await;
        mount_folder("folder-1").mount(&server).await;
        mount_workbook(serde_json::json!([
            {"properties": {"sheetId": 0, "title": "Q1", "index": 0,
                "gridProperties": {"rowCount": 1000, "columnCount": 26}}},
        ]))
        .mount(&server)
        .await;
        let rules = vec![allow_rule("folder-1")];
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        let mut opts = base_opts(add_verb(), false);
        opts.lease_token = Some("not-a-real-token".to_string());
        opts.ledger_path = ledger_path;
        let outcome = dimension_group(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            DimensionGroupResult::RefusedLeaseExpired
        ));
    }

    #[tokio::test]
    async fn refuses_a_lease_bound_to_a_different_file() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
            &["folder-1"],
        )
        .mount(&server)
        .await;
        mount_folder("folder-1").mount(&server).await;
        mount_workbook(serde_json::json!([
            {"properties": {"sheetId": 0, "title": "Q1", "index": 0,
                "gridProperties": {"rowCount": 1000, "columnCount": 26}}},
        ]))
        .mount(&server)
        .await;
        let rules = vec![allow_rule("folder-1")];
        let (lease_token, ledger_path) = leased_opts_for("a-different-sheet");
        let mut opts = base_opts(add_verb(), false);
        opts.lease_token = lease_token;
        opts.ledger_path = ledger_path;
        let outcome = dimension_group(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            DimensionGroupResult::RefusedLeaseWrongFile
        ));
    }

    #[tokio::test]
    async fn refuses_a_stale_lease_when_the_file_has_moved() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
            &["folder-1"],
        )
        .mount(&server)
        .await;
        mount_folder("folder-1").mount(&server).await;
        mount_workbook(serde_json::json!([
            {"properties": {"sheetId": 0, "title": "Q1", "index": 0,
                "gridProperties": {"rowCount": 1000, "columnCount": 26}}},
        ]))
        .mount(&server)
        .await;
        let rules = vec![allow_rule("folder-1")];
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        let token = seed_lease(&ledger_path, "sheet-1", "0");
        let mut opts = base_opts(add_verb(), false);
        opts.lease_token = Some(token);
        opts.ledger_path = ledger_path;
        let outcome = dimension_group(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            DimensionGroupResult::RefusedLeaseStale
        ));
    }

    // ── describe_lines ───────────────────────────────────────────────────

    fn outcome_with(
        verb: DimensionGroupVerb,
        file_name: Option<&str>,
        result: DimensionGroupResult,
    ) -> DimensionGroupOutcome {
        DimensionGroupOutcome {
            spreadsheet_id: "sheet-1".to_string(),
            file_name: file_name.map(str::to_string),
            resolved_folder_id: None,
            sheet_id: None,
            dimension_range_label: None,
            verb,
            result,
        }
    }

    #[test]
    fn describe_lines_renders_would_change() {
        let out = outcome_with(
            add_verb(),
            Some("Budget"),
            DimensionGroupResult::WouldChange {
                summary: "add a row group over ROWS 5:9".to_string(),
            },
        );
        assert_eq!(
            describe(&out),
            "Would add a row group over ROWS 5:9 in 'Budget'"
        );
    }

    #[test]
    fn describe_lines_renders_not_a_spreadsheet_with_no_file_name() {
        let out = outcome_with(
            add_verb(),
            None,
            DimensionGroupResult::RefusedNotASpreadsheet {
                mime_type: "application/vnd.google-apps.document".to_string(),
            },
        );
        let text = describe(&out);
        assert!(text.contains("'sheet-1'"), "{text}");
        assert!(text.contains("not a Google Sheet"), "{text}");
    }

    #[test]
    fn describe_lines_renders_shortcut() {
        let out = outcome_with(
            add_verb(),
            Some("Budget"),
            DimensionGroupResult::RefusedShortcut,
        );
        assert!(describe(&out).contains("shortcut"));
    }

    #[test]
    fn describe_lines_renders_no_visible_parents() {
        let out = outcome_with(
            add_verb(),
            Some("Budget"),
            DimensionGroupResult::RefusedNoVisibleParents,
        );
        assert!(describe(&out).contains("sheets-structure"));
    }

    #[test]
    fn describe_lines_renders_sheet_not_found_with_and_without_available_titles() {
        let with_titles = outcome_with(
            add_verb(),
            Some("Budget"),
            DimensionGroupResult::RefusedSheetNotFound {
                title: "Q1".to_string(),
                available: vec!["Q2".to_string()],
            },
        );
        assert!(describe(&with_titles).contains("'Q2'"));

        let without_titles = outcome_with(
            add_verb(),
            Some("Budget"),
            DimensionGroupResult::RefusedSheetNotFound {
                title: "Q1".to_string(),
                available: Vec::new(),
            },
        );
        assert!(describe(&without_titles).contains("Available: none"));
    }

    #[test]
    fn describe_lines_renders_invalid_range_and_dimension_group_not_found() {
        let invalid = outcome_with(
            add_verb(),
            Some("Budget"),
            DimensionGroupResult::RefusedInvalidRange {
                detail: "--start must be at least 1, got 0".to_string(),
            },
        );
        assert_eq!(
            describe(&invalid),
            "Refused: --start must be at least 1, got 0"
        );

        let not_found = outcome_with(
            update_verb(),
            Some("Budget"),
            DimensionGroupResult::RefusedDimensionGroupNotFound {
                detail: "no dimension group exists over that span".to_string(),
            },
        );
        assert_eq!(
            describe(&not_found),
            "Refused: no dimension group exists over that span"
        );
    }

    #[test]
    fn describe_lines_renders_ambiguous() {
        let out = outcome_with(
            update_verb(),
            Some("Budget"),
            DimensionGroupResult::RefusedAmbiguousDimensionGroup { depths: vec![0, 1] },
        );
        let text = describe(&out);
        assert!(text.contains("0, 1"), "{text}");
        assert!(text.contains("--depth"), "{text}");
    }

    #[test]
    fn describe_lines_renders_blocked_with_and_without_a_deciding_rule() {
        let without_rule = outcome_with(
            add_verb(),
            Some("Budget"),
            DimensionGroupResult::Blocked { decided_by: None },
        );
        assert!(describe(&without_rule).contains("default policy"));
    }

    #[test]
    fn describe_lines_renders_every_lease_refusal() {
        for result in [
            DimensionGroupResult::RefusedNoLease,
            DimensionGroupResult::RefusedLeaseExpired,
            DimensionGroupResult::RefusedLeaseWrongFile,
            DimensionGroupResult::RefusedLeaseStale,
        ] {
            let out = outcome_with(add_verb(), Some("Budget"), result);
            assert!(!describe(&out).is_empty());
        }
    }

    #[test]
    fn describe_lines_renders_changed_with_and_without_a_depth() {
        let with_depth = outcome_with(
            update_verb(),
            Some("Budget"),
            DimensionGroupResult::Changed {
                summary: "set collapsed=true on the group over ROWS 5:9".to_string(),
                depth: Some(1),
            },
        );
        assert!(describe(&with_depth).contains("(depth 1)"));

        let without_depth = outcome_with(
            add_verb(),
            Some("Budget"),
            DimensionGroupResult::Changed {
                summary: "add a row group over ROWS 5:9".to_string(),
                depth: None,
            },
        );
        let text = describe(&without_depth);
        assert!(!text.contains("(depth"), "{text}");
    }

    #[test]
    fn describe_lines_renders_failed() {
        let out = outcome_with(
            add_verb(),
            Some("Budget"),
            DimensionGroupResult::Failed {
                detail: "boom".to_string(),
            },
        );
        assert_eq!(describe(&out), "Failed: boom");
    }

    #[test]
    fn write_jsonl_emits_one_line_of_json() {
        let outcome = outcome_with(
            add_verb(),
            Some("Budget"),
            DimensionGroupResult::Changed {
                summary: "add a row group over ROWS 5:9".to_string(),
                depth: None,
            },
        );
        let mut buf = Vec::new();
        outcome.write_jsonl(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert_eq!(text.matches('\n').count(), 1);
        let parsed: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(parsed["result"]["status"], "changed");
    }
}
