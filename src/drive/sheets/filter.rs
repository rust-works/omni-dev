//! The basic filter and filter views via `spreadsheets.batchUpdate` (issue
//! #1794, [ADR-0081](../../../docs/adrs/adr-0081.md)).
//!
//! Gated by [`DriveOperation::SheetsStructure`] — every mutating verb here
//! reaches the same operation, unlike `structure.rs`'s split between
//! `SheetsStructure` and `SheetsDelete`. ADR-0081 §1 settles this: a filter
//! hides rows, which is view state, not data.
//!
//! Two shapes, each with its own upsert/CRUD story:
//!
//! - **The basic filter** has at most one per sheet, so `set-basic-filter`
//!   is a plain upsert — no existing state to merge with — and
//!   `clear-basic-filter` needs no identifier beyond the sheet
//!   (`ClearBasicFilterRequest` carries only a `sheetId`).
//! - **Filter views** are many, named and directly id-addressed. Unlike
//!   `protection.rs`'s protected ranges (which have no user-facing handle
//!   and so are resolved by exact range match), a filter view's
//!   `filterViewId` is the stable handle `list-filter-views` discovers, so
//!   `update-filter-view`/`delete-filter-view` take it directly via
//!   `--filter-view-id` — there is no ambiguous-match case to handle.
//!
//! `list-filter-views` is read-only and ungated, like `list-protections`
//! (ADR-0081).
//!
//! **Two documented cuts, not silent gaps** — matching `validation.rs`'s own
//! framing of its curated condition surface:
//!
//! - **No `duplicate-filter-view`.** The issue's own background section
//!   names `duplicateFilterView` as part of the API surface, but its
//!   "Proposed verbs" list omits it; this module ships exactly that list.
//! - **`FilterCriteria` supports only `hiddenValues`** — the literal
//!   "uncheck a value in the dropdown" filter, the most common real use.
//!   Condition-based filter criteria (the same `BooleanCondition`
//!   vocabulary `validation.rs` curates for data validation) is not
//!   exposed.
//!
//! Shape mirrors `protection.rs`: compose a target, resolve it against a
//! freshly-fetched workbook, gate, dry-run, mutate, log.

use std::collections::BTreeMap;
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
use crate::drive::sheets::a1;
use crate::drive::sheets::api::SheetsApi;
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::grid_range;
use crate::drive::sheets::target_gate;
use crate::drive::sheets::types::{
    AddFilterViewRequest, BasicFilter, BatchUpdateRequestItem, ClearBasicFilterRequest,
    DeleteFilterViewRequest, FilterCriteria, FilterView, GridRange, SetBasicFilterRequest,
    SortOrder, SortSpec, Spreadsheet, UpdateFilterViewRequest,
};
use crate::drive::types::SheetTargetRefusal;
use crate::drive::write_gate::{self, DecidingRule, DriveOperation, FolderPermissionRule};
use crate::request_log::{self, DriveMutationOutcome};

/// Which mutation to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterVerb {
    /// Upsert the basic filter on a sheet.
    SetBasicFilter {
        /// The sheet to filter.
        sheet: String,
        /// The filtered A1 range, optionally carrying its own `Sheet!`
        /// prefix.
        range: String,
        /// `<COL>:<asc|desc>` sort specs, in priority order.
        sort_by: Vec<String>,
        /// `<COL>:<v1,v2,...>` hidden-value criteria, one per column.
        hide_values: Vec<String>,
    },
    /// Remove a sheet's basic filter.
    ClearBasicFilter {
        /// The sheet to clear the basic filter from.
        sheet: String,
    },
    /// Add a named filter view.
    AddFilterView {
        /// The sheet to filter.
        sheet: String,
        /// The filtered A1 range, optionally carrying its own `Sheet!`
        /// prefix.
        range: String,
        /// A human-readable name for the view.
        title: Option<String>,
        /// `<COL>:<asc|desc>` sort specs, in priority order.
        sort_by: Vec<String>,
        /// `<COL>:<v1,v2,...>` hidden-value criteria, one per column.
        hide_values: Vec<String>,
    },
    /// Change an existing filter view's title, range, sort order, or hidden
    /// values.
    UpdateFilterView {
        /// Which filter view to change, discovered via `list-filter-views`.
        filter_view_id: i64,
        /// A sheet title, supplying a prefix for a bare `range`, when
        /// changing the filtered range.
        sheet: Option<String>,
        /// The new filtered A1 range, when changing it.
        range: Option<String>,
        /// The new title, when changing it.
        title: Option<String>,
        /// Sort specs to merge in, replacing any existing entry sharing a
        /// column, else appending.
        sort_by: Vec<String>,
        /// Hidden-value criteria to merge in, replacing any existing entry
        /// for the named column.
        hide_values: Vec<String>,
        /// Reset the sort order to empty before applying `sort_by`.
        clear_sort: bool,
        /// Reset the criteria to empty before applying `hide_values`.
        clear_criteria: bool,
    },
    /// Remove a filter view.
    DeleteFilterView {
        /// Which filter view to remove, discovered via `list-filter-views`.
        filter_view_id: i64,
    },
}

impl FilterVerb {
    const fn log_operation(&self) -> &'static str {
        match self {
            Self::SetBasicFilter { .. } => "sheets-set-basic-filter",
            Self::ClearBasicFilter { .. } => "sheets-clear-basic-filter",
            Self::AddFilterView { .. } => "sheets-add-filter-view",
            Self::UpdateFilterView { .. } => "sheets-update-filter-view",
            Self::DeleteFilterView { .. } => "sheets-delete-filter-view",
        }
    }

    const fn label(&self) -> &'static str {
        match self {
            Self::SetBasicFilter { .. } => "set-basic-filter",
            Self::ClearBasicFilter { .. } => "clear-basic-filter",
            Self::AddFilterView { .. } => "add-filter-view",
            Self::UpdateFilterView { .. } => "update-filter-view",
            Self::DeleteFilterView { .. } => "delete-filter-view",
        }
    }
}

/// Per-call options.
#[derive(Debug, Clone)]
pub struct FilterOptions {
    /// Spreadsheet id.
    pub spreadsheet_id: String,
    /// Which mutation to perform.
    pub verb: FilterVerb,
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
pub enum FilterResult {
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
    /// The verb's own arguments were invalid — a bad `--sort-by`/
    /// `--hide-values` format, a malformed `--sheet`/`--range` pair, or
    /// (for `update-filter-view`) nothing to change.
    RefusedInvalidRange {
        /// What was wrong and why.
        detail: String,
    },
    /// `update-filter-view`/`delete-filter-view` named a `--filter-view-id`
    /// that does not exist in this workbook.
    RefusedFilterViewNotFound {
        /// The id that was not found.
        filter_view_id: i64,
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
        /// The filter view's stable id — server-assigned for
        /// `add-filter-view`, otherwise the one resolved against. `None`
        /// for `set-basic-filter`/`clear-basic-filter`.
        filter_view_id: Option<i64>,
    },
    /// An API or validation error.
    Failed {
        /// A human-readable summary of what failed.
        detail: String,
    },
}

impl FromLeaseRefusal for FilterResult {
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

impl FilterResult {
    fn log_status(&self) -> &'static str {
        match self {
            Self::WouldChange { .. } => "would-change",
            Self::RefusedNotASpreadsheet { .. } => "refused-not-a-spreadsheet",
            Self::RefusedShortcut => "refused-shortcut",
            Self::RefusedNoVisibleParents => "refused-no-visible-parents",
            Self::RefusedSheetNotFound { .. } => "refused-sheet-not-found",
            Self::RefusedInvalidRange { .. } => "refused-invalid-range",
            Self::RefusedFilterViewNotFound { .. } => "refused-filter-view-not-found",
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
pub struct FilterOutcome {
    /// The spreadsheet acted on.
    pub spreadsheet_id: String,
    /// Its Drive file name, when the metadata fetch got that far.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    /// The folder the gate evaluated against.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_folder_id: Option<String>,
    /// The sheet a `set-basic-filter`/`clear-basic-filter` resolved
    /// against, once known — the only identifier that can distinguish
    /// which sheet in a multi-sheet workbook was affected, since a basic
    /// filter (unlike a filter view) is scoped to a sheet rather than
    /// id-addressed (issue #1794, `docs/log.md`). `None` before the
    /// workbook resolves it, and for every other verb.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sheet_id: Option<i64>,
    /// Which mutation was attempted. Not serialised.
    #[serde(skip)]
    pub verb: FilterVerb,
    /// What happened.
    pub result: FilterResult,
}

impl JsonlSerialize for FilterOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> anyhow::Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Runs one filter mutation, logging every attempt that isn't a dry run.
pub async fn filter(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &FilterOptions,
    rules: &[FolderPermissionRule],
) -> FilterOutcome {
    let started = Instant::now();
    let outcome = filter_inner(drive, sheets, opts, rules).await;
    if !opts.dry_run {
        record_attempt(&outcome, opts, started.elapsed());
    }
    outcome
}

async fn filter_inner(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &FilterOptions,
    rules: &[FolderPermissionRule],
) -> FilterOutcome {
    let bare = |result| FilterOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: None,
        resolved_folder_id: None,
        sheet_id: None,
        verb: opts.verb.clone(),
        result,
    };

    let sort_specs = match parse_sort_specs(sort_by_flags(&opts.verb)) {
        Ok(specs) => specs,
        Err(detail) => return bare(FilterResult::RefusedInvalidRange { detail }),
    };
    let criteria = match parse_hidden_values(hide_values_flags(&opts.verb)) {
        Ok(criteria) => criteria,
        Err(detail) => return bare(FilterResult::RefusedInvalidRange { detail }),
    };
    let composed_range = match compose_target(&opts.verb) {
        Ok(composed) => composed,
        Err(detail) => return bare(FilterResult::RefusedInvalidRange { detail }),
    };
    if let Err(detail) = validate_verb(&opts.verb) {
        return bare(FilterResult::RefusedInvalidRange { detail });
    }

    let (target, decision, resolved_folder_id, requires_lease) = match target_gate::resolve(
        drive,
        &opts.spreadsheet_id,
        DriveOperation::SheetsStructure,
        rules,
    )
    .await
    {
        target_gate::TargetGateOutcome::MetadataFetchFailed { detail } => {
            return bare(FilterResult::Failed { detail })
        }
        target_gate::TargetGateOutcome::Refused { target, refusal } => {
            let result = match refusal {
                SheetTargetRefusal::Shortcut => FilterResult::RefusedShortcut,
                SheetTargetRefusal::NotASpreadsheet { mime_type } => {
                    FilterResult::RefusedNotASpreadsheet { mime_type }
                }
                SheetTargetRefusal::NoVisibleParents => FilterResult::RefusedNoVisibleParents,
            };
            return FilterOutcome {
                spreadsheet_id: opts.spreadsheet_id.clone(),
                file_name: Some(target.name),
                resolved_folder_id: None,
                sheet_id: None,
                verb: opts.verb.clone(),
                result,
            };
        }
        target_gate::TargetGateOutcome::GateFetchFailed { target, detail } => {
            return FilterOutcome {
                spreadsheet_id: opts.spreadsheet_id.clone(),
                file_name: Some(target.name),
                resolved_folder_id: None,
                sheet_id: None,
                verb: opts.verb.clone(),
                result: FilterResult::Failed { detail },
            };
        }
        target_gate::TargetGateOutcome::Gated {
            target,
            decision,
            resolved_folder_id,
            requires_lease,
        } => (target, decision, resolved_folder_id, requires_lease),
    };

    // Returns before the sheet/range resolution just below always have
    // `sheet_id: None` — there is nothing to resolve it against yet.
    // Distinct from `gated` below (never a shadow of it) so a future early
    // return accidentally added between the two is a compile error
    // ("cannot find value `gated`"), not a silent bind to the wrong
    // closure.
    let pre_gated = |result| FilterOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: Some(target.name.clone()),
        resolved_folder_id: resolved_folder_id.clone(),
        sheet_id: None,
        verb: opts.verb.clone(),
        result,
    };

    if decision.verdict == write_gate::Verdict::Deny {
        return pre_gated(FilterResult::Blocked {
            decided_by: decision.decided_by,
        });
    }

    let api = SheetsApi::new(sheets);
    let workbook = match api
        .get_spreadsheet_with_filter_views(&opts.spreadsheet_id)
        .await
    {
        Ok(workbook) => workbook,
        Err(err) => {
            return pre_gated(FilterResult::Failed {
                detail: format!("{err:#}"),
            })
        }
    };

    let resolved_target =
        match resolve_sheet_target(&workbook, &opts.verb, composed_range.as_deref()) {
            Ok(resolved) => resolved,
            Err(result) => return pre_gated(result),
        };
    let sheet_id = resolved_target.map(|grid| grid.sheet_id);

    // `set-basic-filter`/`clear-basic-filter` have no id-addressed handle
    // the way a filter view does, so the sheet id resolved above is the
    // only thing that can tell an audit record which sheet in a
    // multi-sheet workbook was affected (docs/log.md).
    let gated = |result| FilterOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: Some(target.name.clone()),
        resolved_folder_id: resolved_folder_id.clone(),
        sheet_id: match &opts.verb {
            FilterVerb::SetBasicFilter { .. } | FilterVerb::ClearBasicFilter { .. } => sheet_id,
            _ => None,
        },
        verb: opts.verb.clone(),
        result,
    };

    let existing = match &opts.verb {
        FilterVerb::UpdateFilterView { filter_view_id, .. }
        | FilterVerb::DeleteFilterView { filter_view_id } => {
            match find_existing_filter_view(&workbook, *filter_view_id) {
                Ok(existing) => Some(existing),
                Err(result) => return gated(result),
            }
        }
        FilterVerb::SetBasicFilter { .. }
        | FilterVerb::ClearBasicFilter { .. }
        | FilterVerb::AddFilterView { .. } => None,
    };

    let summary = describe_effect(&opts.verb);

    if opts.dry_run {
        return gated(FilterResult::WouldChange { summary });
    }

    // Only past the dry-run return does building the actual request do any
    // work — in particular `update-filter-view`'s merge onto the existing
    // view's `sort_specs`/`criteria`. Every branch here reuses
    // `resolved_target` rather than re-parsing `composed_range` a second
    // time; `resolve_sheet_target` already did that work once, above.
    let (request, existing_id) = match &opts.verb {
        FilterVerb::SetBasicFilter { .. } => {
            let Some(grid) = resolved_target else {
                // omni-dev: coverage ignore-line reason="resolve_sheet_target returns Some for SetBasicFilter or has already returned its refusal; this else-arm exists only to unwrap the shared Option"
                unreachable!("resolved_target is resolved for SetBasicFilter above")
            };
            (
                BatchUpdateRequestItem::SetBasicFilter(SetBasicFilterRequest {
                    filter: BasicFilter {
                        range: Some(grid),
                        sort_specs,
                        criteria,
                    },
                }),
                None,
            )
        }
        FilterVerb::ClearBasicFilter { .. } => {
            let Some(sheet_id) = sheet_id else {
                // omni-dev: coverage ignore-line reason="resolve_sheet_target returns Some for ClearBasicFilter or has already returned its refusal; this else-arm exists only to unwrap the shared Option"
                unreachable!("sheet_id is resolved for ClearBasicFilter above")
            };
            (
                BatchUpdateRequestItem::ClearBasicFilter(ClearBasicFilterRequest { sheet_id }),
                None,
            )
        }
        FilterVerb::AddFilterView { title, .. } => {
            let Some(grid) = resolved_target else {
                // omni-dev: coverage ignore-line reason="resolve_sheet_target returns Some for AddFilterView or has already returned its refusal; this else-arm exists only to unwrap the shared Option"
                unreachable!("resolved_target is resolved for AddFilterView above")
            };
            (
                BatchUpdateRequestItem::AddFilterView(AddFilterViewRequest {
                    filter: FilterView {
                        filter_view_id: None,
                        title: title.clone(),
                        range: Some(grid),
                        sort_specs,
                        criteria,
                    },
                }),
                None,
            )
        }
        FilterVerb::UpdateFilterView {
            filter_view_id,
            title,
            clear_sort,
            clear_criteria,
            ..
        } => {
            let Some(existing) = existing else {
                // omni-dev: coverage ignore-line reason="find_existing_filter_view returns Some for UpdateFilterView or has already returned RefusedFilterViewNotFound; this else-arm exists only to unwrap the shared Option"
                unreachable!("existing is resolved for UpdateFilterView above")
            };
            let update = build_update(
                existing,
                *filter_view_id,
                title,
                resolved_target,
                sort_specs,
                criteria,
                *clear_sort,
                *clear_criteria,
            );
            (
                BatchUpdateRequestItem::UpdateFilterView(update),
                Some(*filter_view_id),
            )
        }
        FilterVerb::DeleteFilterView { filter_view_id } => (
            BatchUpdateRequestItem::DeleteFilterView(DeleteFilterViewRequest {
                filter_id: *filter_view_id,
            }),
            Some(*filter_view_id),
        ),
    };

    // The lease check (ADR-0080 §9) sits here: after the permission gate
    // and the `--dry-run` branch, before the mutating call — see
    // `protection.rs::protection_inner`'s doc comment for the full
    // reasoning, shared verbatim by every leased engine.
    let files_api = FilesApi::new(drive);
    let leased = LeasedWrite {
        log_prefix: "drive sheets filter",
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
        Err(err) => return gated(err.into_result()),
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
        Ok(response) => {
            let filter_view_id = added_filter_view_id(&response).or(existing_id);
            FilterResult::Changed {
                summary,
                filter_view_id,
            }
        }
        Err(err) => FilterResult::Failed {
            detail: format!("{err:#}"),
        },
    };
    drop(lease_grant);
    gated(result)
}

/// `--sort-by` flags for whichever verb carries them.
fn sort_by_flags(verb: &FilterVerb) -> &[String] {
    match verb {
        FilterVerb::SetBasicFilter { sort_by, .. }
        | FilterVerb::AddFilterView { sort_by, .. }
        | FilterVerb::UpdateFilterView { sort_by, .. } => sort_by,
        FilterVerb::ClearBasicFilter { .. } | FilterVerb::DeleteFilterView { .. } => &[],
    }
}

/// `--hide-values` flags for whichever verb carries them.
fn hide_values_flags(verb: &FilterVerb) -> &[String] {
    match verb {
        FilterVerb::SetBasicFilter { hide_values, .. }
        | FilterVerb::AddFilterView { hide_values, .. }
        | FilterVerb::UpdateFilterView { hide_values, .. } => hide_values,
        FilterVerb::ClearBasicFilter { .. } | FilterVerb::DeleteFilterView { .. } => &[],
    }
}

/// Parses `<COL>:<asc|desc>` flags into [`SortSpec`]s, in the order given.
fn parse_sort_specs(flags: &[String]) -> Result<Vec<SortSpec>, String> {
    flags
        .iter()
        .map(|flag| {
            let (col, order) = flag.split_once(':').ok_or_else(|| {
                format!("'{flag}' is not COLUMN:asc|desc — expected e.g. '0:asc'")
            })?;
            let dimension_index: i64 = col
                .trim()
                .parse()
                .map_err(|_| format!("'{col}' in '{flag}' is not a column index"))?;
            let sort_order = match order.trim().to_ascii_lowercase().as_str() {
                "asc" | "ascending" => SortOrder::Ascending,
                "desc" | "descending" => SortOrder::Descending,
                other => return Err(format!("'{other}' in '{flag}' is not 'asc' or 'desc'")),
            };
            Ok(SortSpec {
                dimension_index,
                sort_order,
            })
        })
        .collect()
}

/// Parses `<COL>:<v1,v2,...>` flags into a `criteria` map.
fn parse_hidden_values(flags: &[String]) -> Result<BTreeMap<String, FilterCriteria>, String> {
    let mut criteria = BTreeMap::new();
    for flag in flags {
        let (col, values) = flag.split_once(':').ok_or_else(|| {
            format!("'{flag}' is not COLUMN:VALUE[,VALUE...] — expected e.g. '1:Foo,Bar'")
        })?;
        let dimension_index: i64 = col
            .trim()
            .parse()
            .map_err(|_| format!("'{col}' in '{flag}' is not a column index"))?;
        if values.is_empty() {
            return Err(format!("'{flag}' names no values to hide"));
        }
        let hidden_values: Vec<String> = values.split(',').map(str::to_string).collect();
        criteria.insert(
            dimension_index.to_string(),
            FilterCriteria { hidden_values },
        );
    }
    Ok(criteria)
}

/// Composes the `--sheet`/`--range` pair a verb carries into one string,
/// exactly like `protection.rs`/`format.rs`. Verbs with no range flags at
/// all (`ClearBasicFilter`, `DeleteFilterView`) never call this.
fn compose_target(verb: &FilterVerb) -> Result<Option<String>, String> {
    match verb {
        FilterVerb::SetBasicFilter { sheet, range, .. }
        | FilterVerb::AddFilterView { sheet, range, .. } => a1::compose(Some(sheet), Some(range))
            .map(Some)
            .map_err(|err| err.to_string()),
        FilterVerb::UpdateFilterView { sheet, range, .. } => {
            match (sheet.as_deref(), range.as_deref()) {
                (None, None) => Ok(None),
                (sheet, range) => a1::compose(sheet, range)
                    .map(Some)
                    .map_err(|err| err.to_string()),
            }
        }
        FilterVerb::ClearBasicFilter { .. } | FilterVerb::DeleteFilterView { .. } => Ok(None),
    }
}

/// Rejects a verb whose own arguments are internally inconsistent, cheaply,
/// before ever fetching the workbook.
fn validate_verb(verb: &FilterVerb) -> Result<(), String> {
    if let FilterVerb::UpdateFilterView {
        sheet,
        range,
        title,
        sort_by,
        hide_values,
        clear_sort,
        clear_criteria,
        ..
    } = verb
    {
        let nothing_to_change = sheet.is_none()
            && range.is_none()
            && title.is_none()
            && sort_by.is_empty()
            && hide_values.is_empty()
            && !clear_sort
            && !clear_criteria;
        if nothing_to_change {
            return Err(
                "nothing to change: pass --title, --sheet/--range, --sort-by, --hide-values, \
                 --clear-sort, or --clear-criteria"
                    .to_string(),
            );
        }
    }
    Ok(())
}

/// Resolves the sheet (and, for every verb but `ClearBasicFilter`, the full
/// [`GridRange`]) a verb targets — once, so callers never need to re-parse
/// the same `--sheet`/`--range` composition a second time to get the range
/// they already resolved a sheet id from. `ClearBasicFilter`'s grid carries
/// only `sheet_id`, with every other bound `Default`, since it names a
/// sheet directly rather than a range within it — the same "whole sheet"
/// shape `protection.rs::resolve_grid`'s `--whole-sheet` case uses.
/// `UpdateFilterView` may target no sheet at all (leaving the existing
/// view's range untouched), hence the `Option`; `DeleteFilterView` never
/// does.
fn resolve_sheet_target(
    workbook: &Spreadsheet,
    verb: &FilterVerb,
    composed: Option<&str>,
) -> Result<Option<GridRange>, FilterResult> {
    match verb {
        FilterVerb::SetBasicFilter { .. } | FilterVerb::AddFilterView { .. } => {
            let composed = composed.unwrap_or_default();
            let (_, grid) = grid_range::resolve_grid_range(
                workbook,
                composed,
                |detail| FilterResult::RefusedInvalidRange { detail },
                |title, available| FilterResult::RefusedSheetNotFound { title, available },
            )?;
            Ok(Some(grid))
        }
        FilterVerb::ClearBasicFilter { sheet } => {
            let sheet_id = find_sheet_id(workbook, sheet)?;
            Ok(Some(GridRange {
                sheet_id,
                ..Default::default()
            }))
        }
        FilterVerb::UpdateFilterView { .. } => match composed {
            Some(composed) => {
                let (_, grid) = grid_range::resolve_grid_range(
                    workbook,
                    composed,
                    |detail| FilterResult::RefusedInvalidRange { detail },
                    |title, available| FilterResult::RefusedSheetNotFound { title, available },
                )?;
                Ok(Some(grid))
            }
            None => Ok(None),
        },
        FilterVerb::DeleteFilterView { .. } => Ok(None),
    }
}

fn find_sheet_id(workbook: &Spreadsheet, title: &str) -> Result<i64, FilterResult> {
    grid_range::find_sheet_id(workbook, title, |title, available| {
        FilterResult::RefusedSheetNotFound { title, available }
    })
}

/// Finds the one filter view whose id exactly matches `filter_view_id`.
/// Filter views are directly id-addressed (the id comes from
/// `list-filter-views`), so unlike `protection.rs::find_existing_protection`
/// there is no ambiguous-match case — ids are unique by construction.
fn find_existing_filter_view(
    workbook: &Spreadsheet,
    filter_view_id: i64,
) -> Result<&FilterView, FilterResult> {
    workbook
        .sheets
        .iter()
        .flat_map(|sheet| sheet.filter_views.iter())
        .find(|view| view.filter_view_id == Some(filter_view_id))
        .ok_or(FilterResult::RefusedFilterViewNotFound { filter_view_id })
}

/// Builds the `updateFilterView` request, merging onto the existing view's
/// current `sort_specs`/`criteria` — the full resulting state, since
/// Sheets' `fields` mask replaces each named field wholesale, never merging
/// per-entry (the same reasoning as `protection.rs::build_update`'s editor
/// list). `--clear-sort`/`--clear-criteria` reset to empty first; each
/// parsed `SortSpec` then replaces any existing entry sharing its
/// `dimension_index` (else appends), and each parsed criteria column
/// overwrites that key (else the existing entry survives untouched).
#[allow(clippy::too_many_arguments)]
fn build_update(
    existing: &FilterView,
    filter_view_id: i64,
    title: &Option<String>,
    range: Option<GridRange>,
    sort_by: Vec<SortSpec>,
    hide_values: BTreeMap<String, FilterCriteria>,
    clear_sort: bool,
    clear_criteria: bool,
) -> UpdateFilterViewRequest {
    let mut fields = Vec::new();
    let mut update = FilterView {
        filter_view_id: Some(filter_view_id),
        ..Default::default()
    };
    if let Some(title) = title {
        update.title = Some(title.clone());
        fields.push("title");
    }
    if let Some(range) = range {
        update.range = Some(range);
        fields.push("range");
    }
    if clear_sort || !sort_by.is_empty() {
        let mut resulting = if clear_sort {
            Vec::new()
        } else {
            existing.sort_specs.clone()
        };
        for spec in sort_by {
            if let Some(slot) = resulting
                .iter_mut()
                .find(|s| s.dimension_index == spec.dimension_index)
            {
                *slot = spec;
            } else {
                resulting.push(spec);
            }
        }
        update.sort_specs = resulting;
        fields.push("sortSpecs");
    }
    if clear_criteria || !hide_values.is_empty() {
        let mut resulting = if clear_criteria {
            BTreeMap::new()
        } else {
            existing.criteria.clone()
        };
        for (col, criteria) in hide_values {
            resulting.insert(col, criteria);
        }
        update.criteria = resulting;
        fields.push("criteria");
    }
    UpdateFilterViewRequest {
        filter: update,
        fields: fields.join(","),
    }
}

fn added_filter_view_id(
    response: &crate::drive::sheets::types::BatchUpdateResponse,
) -> Option<i64> {
    response
        .replies
        .iter()
        .find_map(|reply| reply.add_filter_view.as_ref())
        .and_then(|added| added.filter.as_ref())
        .and_then(|filter| filter.filter_view_id)
}

fn describe_effect(verb: &FilterVerb) -> String {
    match verb {
        FilterVerb::SetBasicFilter {
            sort_by,
            hide_values,
            ..
        } => describe_filter_effect("set basic filter", sort_by, hide_values),
        FilterVerb::ClearBasicFilter { .. } => "clear basic filter".to_string(),
        FilterVerb::AddFilterView {
            title,
            sort_by,
            hide_values,
            ..
        } => {
            let named = title
                .as_deref()
                .map_or_else(String::new, |t| format!(" '{t}'"));
            describe_filter_effect(&format!("add filter view{named}"), sort_by, hide_values)
        }
        FilterVerb::UpdateFilterView {
            title,
            sort_by,
            hide_values,
            clear_sort,
            clear_criteria,
            ..
        } => {
            let mut parts = Vec::new();
            if let Some(title) = title {
                parts.push(format!("title='{title}'"));
            }
            if *clear_sort {
                parts.push("clear sort".to_string());
            }
            if !sort_by.is_empty() {
                parts.push(format!("sort {}", sort_by.join(",")));
            }
            if *clear_criteria {
                parts.push("clear criteria".to_string());
            }
            if !hide_values.is_empty() {
                parts.push(format!("hide {}", hide_values.join(",")));
            }
            if parts.is_empty() {
                "update filter view".to_string()
            } else {
                format!("update filter view ({})", parts.join(" "))
            }
        }
        FilterVerb::DeleteFilterView { .. } => "delete filter view".to_string(),
    }
}

fn describe_filter_effect(prefix: &str, sort_by: &[String], hide_values: &[String]) -> String {
    let mut parts = Vec::new();
    if !sort_by.is_empty() {
        parts.push(format!("sort {}", sort_by.join(",")));
    }
    if !hide_values.is_empty() {
        parts.push(format!("hide {}", hide_values.join(",")));
    }
    if parts.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix} ({})", parts.join(" "))
    }
}

fn record_attempt(outcome: &FilterOutcome, opts: &FilterOptions, duration: Duration) {
    let error = match &outcome.result {
        FilterResult::Failed { detail } => Some(detail.clone()),
        _ => None,
    };
    let decided_by = match &outcome.result {
        FilterResult::Blocked { decided_by } => decided_by.as_ref(),
        _ => None,
    };
    let decided_by = write_gate::decided_by_log_fields(decided_by);
    let filter_view_id = match &outcome.result {
        FilterResult::Changed { filter_view_id, .. } => *filter_view_id,
        _ => None,
    };
    let fields_changed = match &opts.verb {
        FilterVerb::UpdateFilterView { .. } => match &outcome.result {
            FilterResult::Changed { summary, .. } => Some(summary.clone()),
            _ => None,
        },
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
        filter_view_id,
        fields_changed,
        error,
        duration,
        ..Default::default()
    });
}

/// Renders an outcome as human-readable text.
#[must_use]
pub fn describe(outcome: &FilterOutcome) -> String {
    describe_lines(outcome).join("\n")
}

/// Renders an outcome as its individual lines, none of which contains a
/// newline.
#[must_use]
pub fn describe_lines(outcome: &FilterOutcome) -> Vec<String> {
    let verb = &outcome.verb;
    let book = outcome.file_name.as_deref().map_or_else(
        || format!("'{}'", outcome.spreadsheet_id),
        |n| format!("'{n}'"),
    );
    match &outcome.result {
        FilterResult::WouldChange { summary } => vec![format!("Would {summary} in {book}")],
        FilterResult::RefusedNotASpreadsheet { mime_type } => vec![format!(
            "Refused: {book} is not a Google Sheet (mimeType: {mime_type}); \
             `drive sheets {}` only works on spreadsheets",
            verb.label()
        )],
        FilterResult::RefusedShortcut => vec![format!(
            "Refused: {book} is a shortcut; `drive sheets {}` doesn't follow shortcuts",
            verb.label()
        )],
        FilterResult::RefusedNoVisibleParents => vec![format!(
            "Refused: {book} has no parent folder visible to this account, so no folder \
             rule can apply to it. Grant it by id instead: add {{\"file_id\": \"<spreadsheet \
             id>\", \"allow\": [\"sheets-structure\"]}} to write_permissions.rules."
        )],
        FilterResult::RefusedSheetNotFound { title, available } => {
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
        FilterResult::RefusedInvalidRange { detail } => vec![format!("Refused: {detail}")],
        FilterResult::RefusedFilterViewNotFound { filter_view_id } => vec![format!(
            "Refused: {book} has no filter view with id {filter_view_id}; run \
             `drive sheets list-filter-views` to see what exists"
        )],
        FilterResult::Blocked { decided_by } => vec![match decided_by {
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
        FilterResult::RefusedNoLease => LeaseGateRefusal::NoLease
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        FilterResult::RefusedLeaseExpired => LeaseGateRefusal::Expired
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        FilterResult::RefusedLeaseWrongFile => LeaseGateRefusal::WrongFile
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        FilterResult::RefusedLeaseStale => LeaseGateRefusal::Stale
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        FilterResult::Changed {
            summary,
            filter_view_id,
        } => {
            let id = filter_view_id.map_or_else(String::new, |id| format!(" (id {id})"));
            vec![format!("Applied: {summary}{id} in {book}")]
        }
        FilterResult::Failed { detail } => vec![format!("Failed: {detail}")],
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::sheets::client::SHEETS_API_URL;
    use crate::drive::sheets::types::Sheet;
    use crate::drive::test_support::seed_lease;
    use crate::test_support::env::MapEnv;
    use crate::utils::secret::Secret;
    use std::collections::HashSet;

    // ── parse_sort_specs / parse_hidden_values ──────────────────────────

    #[test]
    fn parse_sort_specs_accepts_asc_and_desc() {
        let specs = parse_sort_specs(&["0:asc".to_string(), "2:desc".to_string()]).unwrap();
        assert_eq!(
            specs,
            vec![
                SortSpec {
                    dimension_index: 0,
                    sort_order: SortOrder::Ascending
                },
                SortSpec {
                    dimension_index: 2,
                    sort_order: SortOrder::Descending
                },
            ]
        );
    }

    #[test]
    fn parse_sort_specs_rejects_missing_colon() {
        let err = parse_sort_specs(&["0asc".to_string()]).unwrap_err();
        assert!(err.contains("COLUMN:asc|desc"), "{err}");
    }

    #[test]
    fn parse_sort_specs_rejects_non_numeric_column() {
        let err = parse_sort_specs(&["x:asc".to_string()]).unwrap_err();
        assert!(err.contains("not a column index"), "{err}");
    }

    #[test]
    fn parse_sort_specs_rejects_unknown_direction() {
        let err = parse_sort_specs(&["0:sideways".to_string()]).unwrap_err();
        assert!(err.contains("not 'asc' or 'desc'"), "{err}");
    }

    #[test]
    fn parse_hidden_values_builds_a_criteria_map() {
        let criteria = parse_hidden_values(&["1:Foo,Bar".to_string()]).unwrap();
        assert_eq!(
            criteria.get("1").unwrap().hidden_values,
            vec!["Foo".to_string(), "Bar".to_string()]
        );
    }

    #[test]
    fn parse_hidden_values_rejects_missing_colon() {
        let err = parse_hidden_values(&["1Foo".to_string()]).unwrap_err();
        assert!(err.contains("COLUMN:VALUE"), "{err}");
    }

    #[test]
    fn parse_hidden_values_rejects_empty_value_list() {
        let err = parse_hidden_values(&["1:".to_string()]).unwrap_err();
        assert!(err.contains("names no values"), "{err}");
    }

    // ── log_operation / label ────────────────────────────────────────────

    #[test]
    fn every_verb_has_a_distinct_log_operation_and_label() {
        let verbs = [
            FilterVerb::SetBasicFilter {
                sheet: String::new(),
                range: String::new(),
                sort_by: Vec::new(),
                hide_values: Vec::new(),
            },
            FilterVerb::ClearBasicFilter {
                sheet: String::new(),
            },
            FilterVerb::AddFilterView {
                sheet: String::new(),
                range: String::new(),
                title: None,
                sort_by: Vec::new(),
                hide_values: Vec::new(),
            },
            FilterVerb::UpdateFilterView {
                filter_view_id: 1,
                sheet: None,
                range: None,
                title: None,
                sort_by: Vec::new(),
                hide_values: Vec::new(),
                clear_sort: false,
                clear_criteria: false,
            },
            FilterVerb::DeleteFilterView { filter_view_id: 1 },
        ];
        let ops: HashSet<&str> = verbs.iter().map(FilterVerb::log_operation).collect();
        let labels: HashSet<&str> = verbs.iter().map(FilterVerb::label).collect();
        assert_eq!(ops.len(), verbs.len());
        assert_eq!(labels.len(), verbs.len());
    }

    // ── validate_verb ────────────────────────────────────────────────────

    #[test]
    fn validate_verb_rejects_update_filter_view_with_nothing_to_change() {
        let verb = FilterVerb::UpdateFilterView {
            filter_view_id: 1,
            sheet: None,
            range: None,
            title: None,
            sort_by: Vec::new(),
            hide_values: Vec::new(),
            clear_sort: false,
            clear_criteria: false,
        };
        let err = validate_verb(&verb).unwrap_err();
        assert!(err.contains("nothing to change"), "{err}");
    }

    #[test]
    fn validate_verb_accepts_update_filter_view_with_only_clear_sort() {
        let verb = FilterVerb::UpdateFilterView {
            filter_view_id: 1,
            sheet: None,
            range: None,
            title: None,
            sort_by: Vec::new(),
            hide_values: Vec::new(),
            clear_sort: true,
            clear_criteria: false,
        };
        assert!(validate_verb(&verb).is_ok());
    }

    // ── build_update ─────────────────────────────────────────────────────

    fn filter_view(id: i64) -> FilterView {
        FilterView {
            filter_view_id: Some(id),
            ..Default::default()
        }
    }

    #[test]
    fn build_update_preserves_untouched_criteria_columns() {
        let mut existing = filter_view(3);
        existing.criteria.insert(
            "0".to_string(),
            FilterCriteria {
                hidden_values: vec!["Keep".to_string()],
            },
        );
        let mut hide_values = BTreeMap::new();
        hide_values.insert(
            "1".to_string(),
            FilterCriteria {
                hidden_values: vec!["New".to_string()],
            },
        );
        let request = build_update(
            &existing,
            3,
            &None,
            None,
            Vec::new(),
            hide_values,
            false,
            false,
        );
        assert_eq!(
            request.filter.criteria.get("0").unwrap().hidden_values,
            vec!["Keep".to_string()]
        );
        assert_eq!(
            request.filter.criteria.get("1").unwrap().hidden_values,
            vec!["New".to_string()]
        );
        assert_eq!(request.fields, "criteria");
    }

    #[test]
    fn build_update_clear_criteria_drops_untouched_columns() {
        let mut existing = filter_view(3);
        existing.criteria.insert(
            "0".to_string(),
            FilterCriteria {
                hidden_values: vec!["Keep".to_string()],
            },
        );
        let request = build_update(
            &existing,
            3,
            &None,
            None,
            Vec::new(),
            BTreeMap::new(),
            false,
            true,
        );
        assert!(request.filter.criteria.is_empty());
        assert_eq!(request.fields, "criteria");
    }

    #[test]
    fn build_update_sort_spec_replaces_matching_column_and_appends_others() {
        let mut existing = filter_view(3);
        existing.sort_specs = vec![
            SortSpec {
                dimension_index: 0,
                sort_order: SortOrder::Ascending,
            },
            SortSpec {
                dimension_index: 1,
                sort_order: SortOrder::Ascending,
            },
        ];
        let request = build_update(
            &existing,
            3,
            &None,
            None,
            vec![
                SortSpec {
                    dimension_index: 0,
                    sort_order: SortOrder::Descending,
                },
                SortSpec {
                    dimension_index: 2,
                    sort_order: SortOrder::Ascending,
                },
            ],
            BTreeMap::new(),
            false,
            false,
        );
        assert_eq!(
            request.filter.sort_specs,
            vec![
                SortSpec {
                    dimension_index: 0,
                    sort_order: SortOrder::Descending
                },
                SortSpec {
                    dimension_index: 1,
                    sort_order: SortOrder::Ascending
                },
                SortSpec {
                    dimension_index: 2,
                    sort_order: SortOrder::Ascending
                },
            ]
        );
    }

    #[test]
    fn build_update_sets_title_and_field_mask() {
        let existing = filter_view(3);
        let request = build_update(
            &existing,
            3,
            &Some("New title".to_string()),
            None,
            Vec::new(),
            BTreeMap::new(),
            false,
            false,
        );
        assert_eq!(request.filter.title, Some("New title".to_string()));
        assert_eq!(request.fields, "title");
    }

    // ── find_existing_filter_view ───────────────────────────────────────

    fn workbook_with_filter_views(views: Vec<FilterView>) -> Spreadsheet {
        Spreadsheet {
            sheets: vec![Sheet {
                properties: None,
                filter_views: views,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn find_existing_filter_view_matches_by_id() {
        let workbook = workbook_with_filter_views(vec![filter_view(3), filter_view(5)]);
        let found = find_existing_filter_view(&workbook, 5).unwrap();
        assert_eq!(found.filter_view_id, Some(5));
    }

    #[test]
    fn find_existing_filter_view_refuses_when_absent() {
        let workbook = workbook_with_filter_views(vec![filter_view(3)]);
        let err = find_existing_filter_view(&workbook, 99).unwrap_err();
        assert!(matches!(
            err,
            FilterResult::RefusedFilterViewNotFound { filter_view_id: 99 }
        ));
    }

    // ── write_jsonl / log_status ─────────────────────────────────────────

    #[test]
    fn write_jsonl_emits_one_line_of_json() {
        let outcome = FilterOutcome {
            spreadsheet_id: "sheet-1".to_string(),
            file_name: Some("Budget".to_string()),
            resolved_folder_id: None,
            sheet_id: None,
            verb: FilterVerb::DeleteFilterView { filter_view_id: 3 },
            result: FilterResult::Changed {
                summary: "delete filter view".to_string(),
                filter_view_id: Some(3),
            },
        };
        let mut buf = Vec::new();
        outcome.write_jsonl(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert_eq!(text.matches('\n').count(), 1);
        let parsed: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(parsed["result"]["status"], "changed");
    }

    #[test]
    fn filter_result_log_status_names_every_variant() {
        assert_eq!(
            FilterResult::WouldChange {
                summary: String::new()
            }
            .log_status(),
            "would-change"
        );
        assert_eq!(
            FilterResult::RefusedFilterViewNotFound { filter_view_id: 1 }.log_status(),
            "refused-filter-view-not-found"
        );
        assert_eq!(
            FilterResult::Failed {
                detail: String::new()
            }
            .log_status(),
            "failed"
        );
    }

    // ── end-to-end via wiremock ──────────────────────────────────────────

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
        let opts = FilterOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: FilterVerb::SetBasicFilter {
                sheet: "Q1".to_string(),
                range: "A1:D10".to_string(),
                sort_by: Vec::new(),
                hide_values: Vec::new(),
            },
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = filter(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, FilterResult::Blocked { .. }));
    }

    #[tokio::test]
    async fn set_basic_filter_sends_a_set_basic_filter_request() {
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
            {"properties": {"sheetId": 0, "title": "Q1", "index": 0}},
        ]))
        .mount(&server)
        .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "replies": [{}]
                })),
            )
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
        let opts = FilterOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: FilterVerb::SetBasicFilter {
                sheet: "Q1".to_string(),
                range: "A1:D10".to_string(),
                sort_by: vec!["0:asc".to_string()],
                hide_values: vec!["1:Foo,Bar".to_string()],
            },
            dry_run: false,
            lease_token,
            ledger_path,
        };
        let outcome = filter(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, FilterResult::Changed { .. }));
        // A basic filter has no id of its own to log (unlike a filter
        // view's `filter_view_id`), so the resolved sheet id is the only
        // thing that can tell an audit record which sheet was affected —
        // see `record_attempt`/`docs/log.md`.
        assert_eq!(outcome.sheet_id, Some(0));
    }

    #[tokio::test]
    async fn clear_basic_filter_reports_the_resolved_sheet_id() {
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
            {"properties": {"sheetId": 7, "title": "Q1", "index": 0}},
        ]))
        .mount(&server)
        .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "replies": [{}]
                })),
            )
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
        let opts = FilterOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: FilterVerb::ClearBasicFilter {
                sheet: "Q1".to_string(),
            },
            dry_run: false,
            lease_token,
            ledger_path,
        };
        let outcome = filter(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, FilterResult::Changed { .. }));
        assert_eq!(outcome.sheet_id, Some(7));
    }

    #[tokio::test]
    async fn add_filter_view_reports_the_server_assigned_id() {
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
            {"properties": {"sheetId": 0, "title": "Q1", "index": 0}},
        ]))
        .mount(&server)
        .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "replies": [{"addFilterView": {"filter": {"filterViewId": 42}}}]
                })),
            )
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
        let opts = FilterOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: FilterVerb::AddFilterView {
                sheet: "Q1".to_string(),
                range: "A1:D10".to_string(),
                title: Some("Open only".to_string()),
                sort_by: Vec::new(),
                hide_values: Vec::new(),
            },
            dry_run: false,
            lease_token,
            ledger_path,
        };
        let outcome = filter(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            FilterResult::Changed { filter_view_id, .. } => {
                assert_eq!(filter_view_id, Some(42));
            }
            other => panic!("expected Changed, got {other:?}"),
        }
        // Unlike the basic filter, a filter view is already identified by
        // its own `filter_view_id`, so `sheet_id` stays unset here — see
        // `clear_basic_filter_reports_the_resolved_sheet_id` for the case
        // that needs it.
        assert_eq!(outcome.sheet_id, None);
    }

    #[tokio::test]
    async fn update_filter_view_refuses_an_unknown_id() {
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
            {"properties": {"sheetId": 0, "title": "Q1", "index": 0}, "filterViews": []},
        ]))
        .mount(&server)
        .await;
        let rules = vec![allow_rule("folder-1")];
        let opts = FilterOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: FilterVerb::UpdateFilterView {
                filter_view_id: 99,
                sheet: None,
                range: None,
                title: Some("New".to_string()),
                sort_by: Vec::new(),
                hide_values: Vec::new(),
                clear_sort: false,
                clear_criteria: false,
            },
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = filter(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            FilterResult::RefusedFilterViewNotFound { filter_view_id: 99 }
        ));
    }

    #[tokio::test]
    async fn delete_filter_view_sends_a_delete_filter_view_request() {
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
            {"properties": {"sheetId": 0, "title": "Q1", "index": 0},
             "filterViews": [{"filterViewId": 7}]},
        ]))
        .mount(&server)
        .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "replies": [{}]
                })),
            )
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
        let opts = FilterOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: FilterVerb::DeleteFilterView { filter_view_id: 7 },
            dry_run: false,
            lease_token,
            ledger_path,
        };
        let outcome = filter(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            FilterResult::Changed { filter_view_id, .. } => {
                assert_eq!(filter_view_id, Some(7));
            }
            other => panic!("expected Changed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dry_run_makes_no_batch_update_call() {
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
            {"properties": {"sheetId": 0, "title": "Q1", "index": 0}},
        ]))
        .mount(&server)
        .await;
        // Deliberately no mock for `POST .../batchUpdate`: a call there
        // would panic the mock server on an unexpected request, proving
        // dry-run never issues one.
        let rules = vec![allow_rule("folder-1")];
        let opts = FilterOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: FilterVerb::ClearBasicFilter {
                sheet: "Q1".to_string(),
            },
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = filter(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, FilterResult::WouldChange { .. }));
    }

    // ── refusals before any call ─────────────────────────────────────────

    fn set_verb(sort_by: Vec<String>, hide_values: Vec<String>) -> FilterVerb {
        FilterVerb::SetBasicFilter {
            sheet: "Q1".to_string(),
            range: "A1:D10".to_string(),
            sort_by,
            hide_values,
        }
    }

    fn update_verb() -> FilterVerb {
        FilterVerb::UpdateFilterView {
            filter_view_id: 7,
            sheet: None,
            range: None,
            title: Some("Renamed".to_string()),
            sort_by: Vec::new(),
            hide_values: Vec::new(),
            clear_sort: false,
            clear_criteria: false,
        }
    }

    fn unleased_opts(verb: FilterVerb) -> FilterOptions {
        FilterOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb,
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        }
    }

    /// A refusal decided from the flags alone must make no HTTP call at
    /// all: the mock server has nothing mounted, so any request would fail
    /// the outcome as `Failed` rather than the refusal asserted here.
    async fn refused_from_flags_alone(verb: FilterVerb) -> FilterOutcome {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        let rules = vec![allow_rule("folder-1")];
        let outcome = filter(&drive, &sheets, &unleased_opts(verb), &rules).await;
        assert_eq!(outcome.file_name, None);
        outcome
    }

    #[tokio::test]
    async fn an_invalid_sort_spec_is_refused_before_any_call() {
        let outcome =
            refused_from_flags_alone(set_verb(vec!["0:sideways".to_string()], Vec::new())).await;
        match outcome.result {
            FilterResult::RefusedInvalidRange { detail } => {
                assert!(detail.contains("not 'asc' or 'desc'"), "{detail}");
            }
            other => panic!("expected RefusedInvalidRange, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_invalid_hidden_values_flag_is_refused_before_any_call() {
        let outcome = refused_from_flags_alone(set_verb(Vec::new(), vec!["1:".to_string()])).await;
        match outcome.result {
            FilterResult::RefusedInvalidRange { detail } => {
                assert!(detail.contains("names no values"), "{detail}");
            }
            other => panic!("expected RefusedInvalidRange, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_sheet_beside_a_prefixed_range_is_refused_before_any_call() {
        let verb = FilterVerb::AddFilterView {
            sheet: "Q1".to_string(),
            range: "Q2!A1:D10".to_string(),
            title: None,
            sort_by: Vec::new(),
            hide_values: Vec::new(),
        };
        let outcome = refused_from_flags_alone(verb).await;
        match outcome.result {
            FilterResult::RefusedInvalidRange { detail } => {
                assert!(detail.contains("already names a sheet"), "{detail}");
            }
            other => panic!("expected RefusedInvalidRange, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn update_filter_view_with_nothing_to_change_is_refused_before_any_call() {
        let verb = FilterVerb::UpdateFilterView {
            filter_view_id: 7,
            sheet: None,
            range: None,
            title: None,
            sort_by: Vec::new(),
            hide_values: Vec::new(),
            clear_sort: false,
            clear_criteria: false,
        };
        let outcome = refused_from_flags_alone(verb).await;
        match outcome.result {
            FilterResult::RefusedInvalidRange { detail } => {
                assert!(detail.contains("nothing to change"), "{detail}");
            }
            other => panic!("expected RefusedInvalidRange, got {other:?}"),
        }
    }

    // ── the target gate ──────────────────────────────────────────────────

    #[tokio::test]
    async fn a_metadata_fetch_failure_surfaces_as_failed() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/sheet-1"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;
        let rules: Vec<FolderPermissionRule> = Vec::new();
        let opts = unleased_opts(set_verb(Vec::new(), Vec::new()));
        let outcome = filter(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, FilterResult::Failed { .. }));
        assert_eq!(outcome.file_name, None);
    }

    #[tokio::test]
    async fn a_shortcut_target_is_refused() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            "application/vnd.google-apps.shortcut",
            &["folder-1"],
        )
        .mount(&server)
        .await;
        let rules = vec![allow_rule("folder-1")];
        let opts = unleased_opts(set_verb(Vec::new(), Vec::new()));
        let outcome = filter(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, FilterResult::RefusedShortcut));
        assert_eq!(outcome.file_name.as_deref(), Some("sheet-1"));
    }

    #[tokio::test]
    async fn a_non_spreadsheet_target_is_refused() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            "application/vnd.google-apps.document",
            &["folder-1"],
        )
        .mount(&server)
        .await;
        let rules = vec![allow_rule("folder-1")];
        let opts = unleased_opts(FilterVerb::DeleteFilterView { filter_view_id: 7 });
        let outcome = filter(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            FilterResult::RefusedNotASpreadsheet { mime_type } => {
                assert_eq!(mime_type, "application/vnd.google-apps.document");
            }
            other => panic!("expected RefusedNotASpreadsheet, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_target_with_no_visible_parents_is_refused() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", crate::drive::types::GOOGLE_SHEET_MIME_TYPE, &[])
            .mount(&server)
            .await;
        let rules: Vec<FolderPermissionRule> = Vec::new();
        let opts = unleased_opts(set_verb(Vec::new(), Vec::new()));
        let outcome = filter(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            FilterResult::RefusedNoVisibleParents
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
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let opts = unleased_opts(set_verb(Vec::new(), Vec::new()));
        let outcome = filter(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, FilterResult::Failed { .. }));
        assert_eq!(outcome.file_name.as_deref(), Some("sheet-1"));
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
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let opts = unleased_opts(set_verb(Vec::new(), Vec::new()));
        let outcome = filter(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, FilterResult::Failed { .. }));
        assert_eq!(outcome.sheet_id, None);
    }

    #[tokio::test]
    async fn set_basic_filter_refuses_an_unknown_sheet() {
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
            {"properties": {"sheetId": 0, "title": "Q2", "index": 0}},
        ]))
        .mount(&server)
        .await;
        let rules = vec![allow_rule("folder-1")];
        let opts = unleased_opts(set_verb(Vec::new(), Vec::new()));
        let outcome = filter(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            FilterResult::RefusedSheetNotFound { title, available } => {
                assert_eq!(title, "Q1");
                assert_eq!(available, vec!["Q2".to_string()]);
            }
            other => panic!("expected RefusedSheetNotFound, got {other:?}"),
        }
    }

    // ── update-filter-view end to end ────────────────────────────────────

    fn mount_workbook_with_view_seven() -> wiremock::Mock {
        mount_workbook(serde_json::json!([
            {"properties": {"sheetId": 0, "title": "Q1", "index": 0},
             "filterViews": [{
                 "filterViewId": 7,
                 "title": "Old",
                 "range": {"sheetId": 0, "startRowIndex": 0, "endRowIndex": 10,
                           "startColumnIndex": 0, "endColumnIndex": 4},
                 "sortSpecs": [{"dimensionIndex": 0, "sortOrder": "ASCENDING"}],
                 "criteria": {"0": {"hiddenValues": ["Old"]}},
             }]},
        ]))
    }

    #[tokio::test]
    async fn update_filter_view_merges_onto_the_existing_view_and_reports_its_id() {
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
        mount_workbook_with_view_seven().mount(&server).await;
        // The `fields` mask names every changed field, and the merged
        // `sortSpecs`/`criteria` carry the existing entries alongside the
        // new ones — a partial body that would 404 (and so fail the
        // outcome) if the merge dropped either.
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "requests": [{"updateFilterView": {
                    "fields": "title,range,sortSpecs,criteria",
                    "filter": {
                        "filterViewId": 7,
                        "title": "Renamed",
                        "range": {"sheetId": 0, "startRowIndex": 0, "endRowIndex": 20,
                                  "startColumnIndex": 0, "endColumnIndex": 4},
                        "sortSpecs": [
                            {"dimensionIndex": 0, "sortOrder": "ASCENDING"},
                            {"dimensionIndex": 2, "sortOrder": "DESCENDING"},
                        ],
                        "criteria": {
                            "0": {"hiddenValues": ["Old"]},
                            "1": {"hiddenValues": ["Closed"]},
                        },
                    },
                }}]
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
        let opts = FilterOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: FilterVerb::UpdateFilterView {
                filter_view_id: 7,
                sheet: Some("Q1".to_string()),
                range: Some("A1:D20".to_string()),
                title: Some("Renamed".to_string()),
                sort_by: vec!["2:desc".to_string()],
                hide_values: vec!["1:Closed".to_string()],
                clear_sort: false,
                clear_criteria: false,
            },
            dry_run: false,
            lease_token,
            ledger_path,
        };
        let outcome = filter(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            FilterResult::Changed {
                filter_view_id,
                summary,
            } => {
                assert_eq!(filter_view_id, Some(7));
                assert_eq!(
                    summary,
                    "update filter view (title='Renamed' sort 2:desc hide 1:Closed)"
                );
            }
            other => panic!("expected Changed, got {other:?}"),
        }
        // A filter view is identified by its own id, so `sheet_id` stays
        // unset even though a range was resolved to build the request.
        assert_eq!(outcome.sheet_id, None);
    }

    #[tokio::test]
    async fn update_filter_view_with_only_a_range_change_is_described_bare() {
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
        mount_workbook_with_view_seven().mount(&server).await;
        let rules = vec![allow_rule("folder-1")];
        let opts = FilterOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: FilterVerb::UpdateFilterView {
                filter_view_id: 7,
                sheet: None,
                range: Some("Q1!A1:D20".to_string()),
                title: None,
                sort_by: Vec::new(),
                hide_values: Vec::new(),
                clear_sort: false,
                clear_criteria: false,
            },
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = filter(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            FilterResult::WouldChange { summary } => {
                assert_eq!(summary, "update filter view");
            }
            other => panic!("expected WouldChange, got {other:?}"),
        }
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
        mount_workbook_with_view_seven().mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
        let opts = FilterOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: update_verb(),
            dry_run: false,
            lease_token,
            ledger_path,
        };
        let outcome = filter(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            FilterResult::Failed { detail } => assert!(detail.contains("500"), "{detail}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    // ── the Drive write lease (ADR-0080 §9) ──────────────────────────────

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
        mount_workbook_with_view_seven().mount(&server).await;
        // No batchUpdate mock mounted — a refusal must make zero mutating
        // calls.
        let rules = vec![allow_rule("folder-1")];
        let opts = unleased_opts(FilterVerb::DeleteFilterView { filter_view_id: 7 });
        let outcome = filter(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, FilterResult::RefusedNoLease));
        assert_eq!(outcome.result.log_status(), "refused-no-lease");
    }

    // ── describe_effect ──────────────────────────────────────────────────

    #[test]
    fn describe_effect_names_the_view_and_its_sort_and_hidden_values() {
        let verb = FilterVerb::AddFilterView {
            sheet: "Q1".to_string(),
            range: "A1:D10".to_string(),
            title: Some("Open only".to_string()),
            sort_by: vec!["0:asc".to_string()],
            hide_values: vec!["1:Closed".to_string()],
        };
        assert_eq!(
            describe_effect(&verb),
            "add filter view 'Open only' (sort 0:asc hide 1:Closed)"
        );
        assert_eq!(
            describe_effect(&set_verb(Vec::new(), Vec::new())),
            "set basic filter"
        );
    }

    #[test]
    fn describe_effect_lists_every_update_filter_view_part_in_order() {
        let verb = FilterVerb::UpdateFilterView {
            filter_view_id: 7,
            sheet: None,
            range: None,
            title: Some("Renamed".to_string()),
            sort_by: vec!["0:asc".to_string()],
            hide_values: vec!["1:Closed".to_string()],
            clear_sort: true,
            clear_criteria: true,
        };
        assert_eq!(
            describe_effect(&verb),
            "update filter view (title='Renamed' clear sort sort 0:asc clear criteria hide 1:Closed)"
        );
    }

    // ── describe / describe_lines ────────────────────────────────────────

    fn outcome_with(
        verb: FilterVerb,
        file_name: Option<&str>,
        result: FilterResult,
    ) -> FilterOutcome {
        FilterOutcome {
            spreadsheet_id: "sheet-1".to_string(),
            file_name: file_name.map(str::to_string),
            resolved_folder_id: None,
            sheet_id: None,
            verb,
            result,
        }
    }

    #[test]
    fn describe_lines_renders_would_change() {
        let out = outcome_with(
            set_verb(Vec::new(), Vec::new()),
            Some("Budget"),
            FilterResult::WouldChange {
                summary: "set basic filter".to_string(),
            },
        );
        assert_eq!(describe(&out), "Would set basic filter in 'Budget'");
    }

    #[test]
    fn describe_lines_renders_not_a_spreadsheet_with_no_file_name() {
        let out = outcome_with(
            set_verb(Vec::new(), Vec::new()),
            None,
            FilterResult::RefusedNotASpreadsheet {
                mime_type: "text/plain".to_string(),
            },
        );
        let text = describe(&out);
        assert!(text.contains("'sheet-1'"), "{text}");
        assert!(text.contains("set-basic-filter"), "{text}");
        assert!(text.contains("text/plain"), "{text}");
    }

    #[test]
    fn describe_lines_renders_shortcut_and_no_visible_parents() {
        let shortcut = outcome_with(
            FilterVerb::DeleteFilterView { filter_view_id: 7 },
            Some("Budget"),
            FilterResult::RefusedShortcut,
        );
        let text = describe(&shortcut);
        assert!(text.contains("shortcut"), "{text}");
        assert!(text.contains("delete-filter-view"), "{text}");

        let orphan = outcome_with(
            set_verb(Vec::new(), Vec::new()),
            Some("Budget"),
            FilterResult::RefusedNoVisibleParents,
        );
        assert!(describe(&orphan).contains("sheets-structure"));
    }

    #[test]
    fn describe_lines_renders_sheet_not_found_with_and_without_available_titles() {
        let none = outcome_with(
            set_verb(Vec::new(), Vec::new()),
            Some("Budget"),
            FilterResult::RefusedSheetNotFound {
                title: "Q1".to_string(),
                available: Vec::new(),
            },
        );
        assert!(describe(&none).contains("Available: none"));

        let some = outcome_with(
            set_verb(Vec::new(), Vec::new()),
            Some("Budget"),
            FilterResult::RefusedSheetNotFound {
                title: "Q1".to_string(),
                available: vec!["Q2".to_string(), "Q3".to_string()],
            },
        );
        assert!(describe(&some).contains("Available: 'Q2', 'Q3'"));
    }

    #[test]
    fn describe_lines_renders_invalid_range_and_filter_view_not_found() {
        let invalid = outcome_with(
            set_verb(Vec::new(), Vec::new()),
            Some("Budget"),
            FilterResult::RefusedInvalidRange {
                detail: "bad range".to_string(),
            },
        );
        assert_eq!(describe(&invalid), "Refused: bad range");

        let missing = outcome_with(
            update_verb(),
            Some("Budget"),
            FilterResult::RefusedFilterViewNotFound { filter_view_id: 7 },
        );
        let text = describe(&missing);
        assert!(text.contains("no filter view with id 7"), "{text}");
        assert!(text.contains("list-filter-views"), "{text}");
    }

    #[test]
    fn describe_lines_renders_blocked_with_and_without_a_deciding_rule() {
        let folder_rule = outcome_with(
            update_verb(),
            Some("Budget"),
            FilterResult::Blocked {
                decided_by: Some(DecidingRule::Folder {
                    folder_id: "folder-1".to_string(),
                    depth: 2,
                }),
            },
        );
        let text = describe(&folder_rule);
        assert!(text.contains("update-filter-view"), "{text}");
        assert!(text.contains("folder folder-1 (depth 2)"), "{text}");

        let default_policy = outcome_with(
            FilterVerb::ClearBasicFilter {
                sheet: "Q1".to_string(),
            },
            Some("Budget"),
            FilterResult::Blocked { decided_by: None },
        );
        let text = describe(&default_policy);
        assert!(text.contains("default policy"), "{text}");
        assert!(text.contains("sheets-structure"), "{text}");
    }

    #[test]
    fn describe_lines_renders_every_lease_refusal_with_the_lease_acquire_hint() {
        for (result, phrase) in [
            (FilterResult::RefusedNoLease, "requires a Drive write lease"),
            (
                FilterResult::RefusedLeaseExpired,
                "expired, released, or unknown",
            ),
            (
                FilterResult::RefusedLeaseWrongFile,
                "acquired for a different file",
            ),
            (
                FilterResult::RefusedLeaseStale,
                "changed since the lease was acquired",
            ),
        ] {
            let out = outcome_with(update_verb(), Some("Budget"), result);
            let text = describe(&out);
            assert!(text.contains(phrase), "{text}");
            assert!(text.contains("drive lease acquire sheet-1"), "{text}");
        }
    }

    #[test]
    fn describe_lines_renders_changed_with_and_without_an_id() {
        let with_id = outcome_with(
            FilterVerb::DeleteFilterView { filter_view_id: 7 },
            Some("Budget"),
            FilterResult::Changed {
                summary: "delete filter view".to_string(),
                filter_view_id: Some(7),
            },
        );
        assert_eq!(
            describe(&with_id),
            "Applied: delete filter view (id 7) in 'Budget'"
        );

        let without_id = outcome_with(
            set_verb(Vec::new(), Vec::new()),
            Some("Budget"),
            FilterResult::Changed {
                summary: "set basic filter".to_string(),
                filter_view_id: None,
            },
        );
        assert_eq!(
            describe(&without_id),
            "Applied: set basic filter in 'Budget'"
        );
    }

    #[test]
    fn describe_lines_renders_failed() {
        let out = outcome_with(
            set_verb(Vec::new(), Vec::new()),
            Some("Budget"),
            FilterResult::Failed {
                detail: "boom".to_string(),
            },
        );
        assert_eq!(describe(&out), "Failed: boom");
    }

    // ── the other lease refusals ─────────────────────────────────────────

    /// Mounts a granted gate and a workbook holding filter view 7, so the
    /// only thing left to decide the outcome is the lease.
    async fn mount_granted_gate_with_view_seven(server: &wiremock::MockServer) {
        mount_file(
            "sheet-1",
            crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
            &["folder-1"],
        )
        .mount(server)
        .await;
        mount_folder("folder-1").mount(server).await;
        mount_workbook_with_view_seven().mount(server).await;
    }

    fn fresh_ledger_path() -> std::path::PathBuf {
        tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl")
    }

    #[tokio::test]
    async fn refuses_an_unknown_lease_token() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_granted_gate_with_view_seven(&server).await;
        let rules = vec![allow_rule("folder-1")];
        // Never seeded — the ledger knows nothing of this token.
        let opts = FilterOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: update_verb(),
            dry_run: false,
            lease_token: Some("bogus-token".to_string()),
            ledger_path: fresh_ledger_path(),
        };
        let outcome = filter(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, FilterResult::RefusedLeaseExpired));
        assert_eq!(outcome.result.log_status(), "refused-lease-expired");
    }

    #[tokio::test]
    async fn refuses_a_lease_bound_to_a_different_file() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_granted_gate_with_view_seven(&server).await;
        let rules = vec![allow_rule("folder-1")];
        let ledger_path = fresh_ledger_path();
        // Seeded for a *different* spreadsheet id.
        let token = seed_lease(&ledger_path, "some-other-sheet", "1");
        let opts = FilterOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: update_verb(),
            dry_run: false,
            lease_token: Some(token),
            ledger_path,
        };
        let outcome = filter(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            FilterResult::RefusedLeaseWrongFile
        ));
        assert_eq!(outcome.result.log_status(), "refused-lease-wrong-file");
    }

    #[tokio::test]
    async fn refuses_a_stale_lease_when_the_file_has_moved() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        // `mount_file` always returns version "1"; the lease below was
        // acquired against version "0" — a foreign edit landed since.
        mount_granted_gate_with_view_seven(&server).await;
        let rules = vec![allow_rule("folder-1")];
        let ledger_path = fresh_ledger_path();
        let token = seed_lease(&ledger_path, "sheet-1", "0");
        let opts = FilterOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: update_verb(),
            dry_run: false,
            lease_token: Some(token),
            ledger_path,
        };
        let outcome = filter(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, FilterResult::RefusedLeaseStale));
        assert_eq!(outcome.result.log_status(), "refused-lease-stale");
    }

    // ── sheet resolution per verb ────────────────────────────────────────

    #[tokio::test]
    async fn clear_basic_filter_refuses_an_unknown_sheet() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_granted_gate_with_view_seven(&server).await;
        let rules = vec![allow_rule("folder-1")];
        let opts = unleased_opts(FilterVerb::ClearBasicFilter {
            sheet: "Nope".to_string(),
        });
        let outcome = filter(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            FilterResult::RefusedSheetNotFound { title, available } => {
                assert_eq!(title, "Nope");
                assert_eq!(available, vec!["Q1".to_string()]);
            }
            other => panic!("expected RefusedSheetNotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn update_filter_view_refuses_a_range_on_an_unknown_sheet() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_granted_gate_with_view_seven(&server).await;
        let rules = vec![allow_rule("folder-1")];
        let opts = unleased_opts(FilterVerb::UpdateFilterView {
            filter_view_id: 7,
            sheet: Some("Nope".to_string()),
            range: Some("A1:D20".to_string()),
            title: None,
            sort_by: Vec::new(),
            hide_values: Vec::new(),
            clear_sort: false,
            clear_criteria: false,
        });
        let outcome = filter(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            FilterResult::RefusedSheetNotFound { .. }
        ));
    }

    #[test]
    fn build_update_clear_sort_discards_the_existing_order_before_merging() {
        let mut existing = filter_view(3);
        existing.sort_specs = vec![SortSpec {
            dimension_index: 0,
            sort_order: SortOrder::Ascending,
        }];
        let request = build_update(
            &existing,
            3,
            &None,
            None,
            vec![SortSpec {
                dimension_index: 2,
                sort_order: SortOrder::Descending,
            }],
            BTreeMap::new(),
            true,
            false,
        );
        assert_eq!(
            request.filter.sort_specs,
            vec![SortSpec {
                dimension_index: 2,
                sort_order: SortOrder::Descending,
            }]
        );
        assert_eq!(request.fields, "sortSpecs");
    }

    #[tokio::test]
    async fn set_basic_filter_refuses_an_unparseable_range() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_granted_gate_with_view_seven(&server).await;
        let rules = vec![allow_rule("folder-1")];
        // `A1:B` mixes a cell with a whole column on a *different* column,
        // which no supported A1 form spells — so it passes the flag-level
        // `a1::compose` check and only fails once the workbook's sheet is
        // known and the grid is parsed against it.
        let opts = unleased_opts(FilterVerb::SetBasicFilter {
            sheet: "Q1".to_string(),
            range: "A1:B".to_string(),
            sort_by: Vec::new(),
            hide_values: Vec::new(),
        });
        let outcome = filter(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            FilterResult::RefusedInvalidRange { detail } => {
                assert!(detail.contains("not a recognised A1 range"), "{detail}");
            }
            other => panic!("expected RefusedInvalidRange, got {other:?}"),
        }
        assert_eq!(outcome.file_name.as_deref(), Some("sheet-1"));
    }

    #[tokio::test]
    async fn update_filter_view_refuses_an_unparseable_range() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_granted_gate_with_view_seven(&server).await;
        let rules = vec![allow_rule("folder-1")];
        let opts = unleased_opts(FilterVerb::UpdateFilterView {
            filter_view_id: 7,
            sheet: Some("Q1".to_string()),
            range: Some("A1:B".to_string()),
            title: None,
            sort_by: Vec::new(),
            hide_values: Vec::new(),
            clear_sort: false,
            clear_criteria: false,
        });
        let outcome = filter(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            FilterResult::RefusedInvalidRange { .. }
        ));
    }

    #[tokio::test]
    async fn a_failed_pre_lease_refetch_is_reported_as_failed_with_no_batch_update_call() {
        // The gate's own resolve step succeeds off the first `files.get`,
        // but the fresh re-fetch feeding the staleness check (ADR-0080 §6)
        // fails — the change must report `Failed` and never reach
        // `batchUpdate`.
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
            &["folder-1"],
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/sheet-1"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .with_priority(2)
            .mount(&server)
            .await;
        mount_folder("folder-1").mount(&server).await;
        mount_workbook_with_view_seven().mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
        let opts = FilterOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: FilterVerb::DeleteFilterView { filter_view_id: 7 },
            dry_run: false,
            lease_token,
            ledger_path,
        };
        let outcome = filter(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, FilterResult::Failed { .. }));
    }
}
