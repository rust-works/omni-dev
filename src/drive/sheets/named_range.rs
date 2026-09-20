//! Named ranges via `spreadsheets.batchUpdate` (issue #1796,
//! [ADR-0081](../../../docs/adrs/adr-0081.md) §2).
//!
//! Gated by `DriveOperation::SheetsStructure`, like every other additive
//! Sheets verb — a named range is a label over a region, and adding,
//! renaming/re-pointing, or removing one never touches a cell's stored
//! value or formula text. `delete-named-range` is the one verb with a
//! visible consequence: every formula referencing the removed name starts
//! evaluating to `#NAME?`. ADR-0081 §2 mitigates this the way ADR-0078 §8
//! mitigates `merge-cells`'s data loss (`format.rs`'s `discarded_cells`):
//! before deleting, in both `--dry-run` and the real run, this module scans
//! the workbook's formulas for the name and reports the count and A1
//! locations of every reference — **never** the formula text or a cell's
//! value.
//!
//! Three mutating verbs (`add-named-range`/`update-named-range`/
//! `delete-named-range`) plus one read (`list-named-ranges`, ungated like
//! `list-protections`). Shape mirrors `protection.rs`: compose a target,
//! resolve it against a freshly-fetched workbook, gate, dry-run, mutate,
//! log.
//!
//! **`update-named-range`/`delete-named-range` resolve their target by
//! exact name match** against the workbook's current named ranges — the
//! name, not the server-assigned id, is the one stable handle a CLI user
//! would actually type (the id is discoverable only via
//! `list-named-ranges`). Sheets enforces unique names workbook-wide, so
//! unlike `protection.rs`'s range-based lookup this can never be
//! ambiguous — only found or not found.

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
use crate::drive::sheets::api::{SheetsApi, ValueRenderOption, MAX_RANGES_PER_BATCH};
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::grid_range;
use crate::drive::sheets::target_gate;
use crate::drive::sheets::types::{
    AddNamedRangeRequest, BatchUpdateRequestItem, BatchUpdateResponse, DeleteNamedRangeRequest,
    GridRange, NamedRange, Spreadsheet, UpdateNamedRangeRequest,
};
use crate::drive::types::SheetTargetRefusal;
use crate::drive::write_gate::{self, DecidingRule, DriveOperation, FolderPermissionRule};
use crate::request_log::{self, DriveMutationOutcome};

/// Which mutation to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamedRangeVerb {
    /// Add a new named range (or, with `whole_sheet`, one covering an
    /// entire sheet).
    AddNamedRange {
        /// The name to create. Must be unique workbook-wide — a duplicate
        /// is left to the API's own error rather than pre-checked here.
        name: String,
        /// A sheet title, supplying a prefix for a bare `range`. Also the
        /// target sheet directly when `whole_sheet` is set.
        sheet: Option<String>,
        /// An explicit A1 range, which may carry its own `Sheet!` prefix.
        /// Mutually exclusive with `whole_sheet`.
        range: Option<String>,
        /// Cover the entire sheet named by `sheet`, rather than a range
        /// within it.
        whole_sheet: bool,
    },
    /// Change an existing named range's name and/or the range it covers.
    UpdateNamedRange {
        /// The existing name to change, by exact match.
        name: String,
        /// The new name, when renaming.
        new_name: Option<String>,
        /// A sheet title for the new range, supplying a prefix for a bare
        /// `range`. Also the target sheet directly when `whole_sheet` is
        /// set. Omitting `sheet`, `range` and `whole_sheet` entirely keeps
        /// the existing range unchanged — this verb may be a rename only.
        sheet: Option<String>,
        /// An explicit new A1 range, which may carry its own `Sheet!`
        /// prefix. Mutually exclusive with `whole_sheet`.
        range: Option<String>,
        /// Re-point at the entire sheet named by `sheet`, rather than a
        /// range within it.
        whole_sheet: bool,
    },
    /// Remove a named range.
    DeleteNamedRange {
        /// The existing name to remove, by exact match.
        name: String,
    },
}

impl NamedRangeVerb {
    const fn log_operation(&self) -> &'static str {
        match self {
            Self::AddNamedRange { .. } => "sheets-add-named-range",
            Self::UpdateNamedRange { .. } => "sheets-update-named-range",
            Self::DeleteNamedRange { .. } => "sheets-delete-named-range",
        }
    }

    const fn label(&self) -> &'static str {
        match self {
            Self::AddNamedRange { .. } => "add-named-range",
            Self::UpdateNamedRange { .. } => "update-named-range",
            Self::DeleteNamedRange { .. } => "delete-named-range",
        }
    }
}

/// Per-call options.
#[derive(Debug, Clone)]
pub struct NamedRangeOptions {
    /// Spreadsheet id.
    pub spreadsheet_id: String,
    /// Which mutation to perform.
    pub verb: NamedRangeVerb,
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
pub enum NamedRangeResult {
    /// `--dry-run`, and the gate would allow it.
    WouldChange {
        /// A human-readable summary of the effect.
        summary: String,
        /// `delete-named-range` only: the A1 locations of every formula
        /// referencing the name being removed. Empty for every other verb.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        referencing_formulas: Vec<String>,
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
    /// The `--sheet`/`--range`/`--whole-sheet` combination was invalid, or
    /// `update-named-range` named nothing to change.
    RefusedInvalidRange {
        /// What was wrong and why.
        detail: String,
    },
    /// `update-named-range`/`delete-named-range` found no named range with
    /// this exact name.
    RefusedNotFound {
        /// The name that was searched for.
        name: String,
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
        /// The named range's stable id — server-assigned for
        /// `add-named-range`, otherwise the one resolved against.
        named_range_id: Option<String>,
        /// Same preview as [`Self::WouldChange`], read before the delete
        /// took effect.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        referencing_formulas: Vec<String>,
    },
    /// An API or validation error.
    Failed {
        /// A human-readable summary of what failed.
        detail: String,
    },
}

impl FromLeaseRefusal for NamedRangeResult {
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

impl NamedRangeResult {
    fn log_status(&self) -> &'static str {
        match self {
            Self::WouldChange { .. } => "would-change",
            Self::RefusedNotASpreadsheet { .. } => "refused-not-a-spreadsheet",
            Self::RefusedShortcut => "refused-shortcut",
            Self::RefusedNoVisibleParents => "refused-no-visible-parents",
            Self::RefusedSheetNotFound { .. } => "refused-sheet-not-found",
            Self::RefusedInvalidRange { .. } => "refused-invalid-range",
            Self::RefusedNotFound { .. } => "refused-not-found",
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
pub struct NamedRangeOutcome {
    /// The spreadsheet acted on.
    pub spreadsheet_id: String,
    /// Its Drive file name, when the metadata fetch got that far.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    /// The folder the gate evaluated against.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_folder_id: Option<String>,
    /// Which mutation was attempted. Not serialised.
    #[serde(skip)]
    pub verb: NamedRangeVerb,
    /// What happened.
    pub result: NamedRangeResult,
}

impl JsonlSerialize for NamedRangeOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> anyhow::Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Runs one named-range mutation, logging every attempt that isn't a dry
/// run.
pub async fn named_range(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &NamedRangeOptions,
    rules: &[FolderPermissionRule],
) -> NamedRangeOutcome {
    let started = Instant::now();
    let outcome = named_range_inner(drive, sheets, opts, rules).await;
    if !opts.dry_run {
        record_attempt(&outcome, opts, started.elapsed());
    }
    outcome
}

async fn named_range_inner(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &NamedRangeOptions,
    rules: &[FolderPermissionRule],
) -> NamedRangeOutcome {
    let bare = |result| NamedRangeOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: None,
        resolved_folder_id: None,
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
            return bare(NamedRangeResult::Failed { detail })
        }
        target_gate::TargetGateOutcome::Refused { target, refusal } => {
            let result = match refusal {
                SheetTargetRefusal::Shortcut => NamedRangeResult::RefusedShortcut,
                SheetTargetRefusal::NotASpreadsheet { mime_type } => {
                    NamedRangeResult::RefusedNotASpreadsheet { mime_type }
                }
                SheetTargetRefusal::NoVisibleParents => NamedRangeResult::RefusedNoVisibleParents,
            };
            return NamedRangeOutcome {
                spreadsheet_id: opts.spreadsheet_id.clone(),
                file_name: Some(target.name),
                resolved_folder_id: None,
                verb: opts.verb.clone(),
                result,
            };
        }
        target_gate::TargetGateOutcome::GateFetchFailed { target, detail } => {
            return NamedRangeOutcome {
                spreadsheet_id: opts.spreadsheet_id.clone(),
                file_name: Some(target.name),
                resolved_folder_id: None,
                verb: opts.verb.clone(),
                result: NamedRangeResult::Failed { detail },
            };
        }
        target_gate::TargetGateOutcome::Gated {
            target,
            decision,
            resolved_folder_id,
            requires_lease,
        } => (target, decision, resolved_folder_id, requires_lease),
    };

    let gated = |result| NamedRangeOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: Some(target.name.clone()),
        resolved_folder_id: resolved_folder_id.clone(),
        verb: opts.verb.clone(),
        result,
    };

    if decision.verdict == write_gate::Verdict::Deny {
        return gated(NamedRangeResult::Blocked {
            decided_by: decision.decided_by,
        });
    }

    let api = SheetsApi::new(sheets);
    let workbook = match api
        .get_spreadsheet_with_named_ranges(&opts.spreadsheet_id)
        .await
    {
        Ok(workbook) => workbook,
        Err(err) => {
            return gated(NamedRangeResult::Failed {
                detail: format!("{err:#}"),
            })
        }
    };

    // `update-named-range`/`delete-named-range` resolve their target by
    // exact name match against the workbook's current named ranges — done
    // once, here, before the dry-run check, so a preview refuses a
    // nonexistent target exactly like a real attempt would.
    let existing = match &opts.verb {
        NamedRangeVerb::AddNamedRange { .. } => None,
        NamedRangeVerb::UpdateNamedRange { name, .. }
        | NamedRangeVerb::DeleteNamedRange { name } => {
            match find_existing_named_range(&workbook, name) {
                Ok(existing) => Some(existing),
                Err(result) => return gated(result),
            }
        }
    };

    let new_grid = match &opts.verb {
        NamedRangeVerb::AddNamedRange {
            sheet,
            range,
            whole_sheet,
            ..
        } => match resolve_grid(&workbook, sheet.as_deref(), range.as_deref(), *whole_sheet) {
            Ok(grid) => Some(grid),
            Err(result) => return gated(result),
        },
        NamedRangeVerb::UpdateNamedRange {
            sheet,
            range,
            whole_sheet,
            ..
        } => {
            match resolve_optional_grid(&workbook, sheet.as_deref(), range.as_deref(), *whole_sheet)
            {
                Ok(grid) => grid,
                Err(result) => return gated(result),
            }
        }
        NamedRangeVerb::DeleteNamedRange { .. } => None,
    };

    if let NamedRangeVerb::UpdateNamedRange { new_name, .. } = &opts.verb {
        if new_name.is_none() && new_grid.is_none() {
            return gated(NamedRangeResult::RefusedInvalidRange {
                detail: "update-named-range needs --new-name and/or a new range \
                         (--range/--sheet/--whole-sheet); nothing to change"
                    .to_string(),
            });
        }
    }

    // `delete-named-range` only (ADR-0081 §2): scan the workbook's formulas
    // for the name being removed — unconditionally, before the dry-run
    // check, so `--dry-run` and the real run report identically.
    let referencing_formulas = if let NamedRangeVerb::DeleteNamedRange { name } = &opts.verb {
        match scan_referencing_formulas(&api, &opts.spreadsheet_id, &workbook, name).await {
            Ok(locations) => locations,
            Err(detail) => return gated(NamedRangeResult::Failed { detail }),
        }
    } else {
        Vec::new()
    };

    let summary = describe_effect(&opts.verb);

    if opts.dry_run {
        return gated(NamedRangeResult::WouldChange {
            summary,
            referencing_formulas,
        });
    }

    let (request, existing_id) = match &opts.verb {
        NamedRangeVerb::AddNamedRange { name, .. } => {
            let Some(grid) = new_grid else {
                unreachable!("new_grid is resolved for AddNamedRange above")
            };
            (
                BatchUpdateRequestItem::AddNamedRange(AddNamedRangeRequest {
                    named_range: NamedRange {
                        named_range_id: None,
                        name: name.clone(),
                        range: grid,
                    },
                }),
                None,
            )
        }
        NamedRangeVerb::UpdateNamedRange { new_name, .. } => {
            let Some(existing) = existing else {
                unreachable!("existing is resolved for UpdateNamedRange above")
            };
            (
                BatchUpdateRequestItem::UpdateNamedRange(build_update(
                    existing, new_name, new_grid,
                )),
                existing.named_range_id.clone(),
            )
        }
        NamedRangeVerb::DeleteNamedRange { .. } => {
            let Some(existing) = existing else {
                unreachable!("existing is resolved for DeleteNamedRange above")
            };
            let id = existing.named_range_id.clone();
            (
                BatchUpdateRequestItem::DeleteNamedRange(DeleteNamedRangeRequest {
                    named_range_id: id.clone().unwrap_or_default(),
                }),
                id,
            )
        }
    };

    // The lease check (ADR-0080 §9) sits here: after the permission gate
    // and the `--dry-run` branch, before the mutating call — see
    // `protection.rs::protection_inner`'s doc comment for the full
    // reasoning, shared verbatim by every leased engine.
    let files_api = FilesApi::new(drive);
    let leased = LeasedWrite {
        log_prefix: "drive sheets named-range",
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
            let named_range_id = added_named_range_id(&response).or(existing_id);
            NamedRangeResult::Changed {
                summary,
                named_range_id,
                referencing_formulas,
            }
        }
        Err(err) => NamedRangeResult::Failed {
            detail: format!("{err:#}"),
        },
    };
    drop(lease_grant);
    gated(result)
}

/// Composes and resolves `--sheet`/`--range`/`--whole-sheet` into a
/// concrete [`GridRange`] — mandatory, exactly like `protection.rs`'s
/// `compose_target`/`resolve_grid` pair for `protect-range`. Used by
/// `add-named-range`, where a target is always required.
fn resolve_grid(
    workbook: &Spreadsheet,
    sheet: Option<&str>,
    range: Option<&str>,
    whole_sheet: bool,
) -> Result<GridRange, NamedRangeResult> {
    if whole_sheet {
        if range.is_some() {
            return Err(NamedRangeResult::RefusedInvalidRange {
                detail: "--whole-sheet and --range are mutually exclusive".to_string(),
            });
        }
        let Some(sheet) = sheet else {
            return Err(NamedRangeResult::RefusedInvalidRange {
                detail: "--whole-sheet needs --sheet to name the sheet".to_string(),
            });
        };
        let sheet_id = find_sheet_id(workbook, sheet)?;
        return Ok(GridRange {
            sheet_id,
            ..Default::default()
        });
    }
    let composed =
        a1::compose(sheet, range).map_err(|err| NamedRangeResult::RefusedInvalidRange {
            detail: err.to_string(),
        })?;
    let (_, grid) = grid_range::resolve_grid_range(
        workbook,
        &composed,
        |detail| NamedRangeResult::RefusedInvalidRange { detail },
        |title, available| NamedRangeResult::RefusedSheetNotFound { title, available },
    )?;
    Ok(grid)
}

/// `update-named-range`'s range resolution: like [`resolve_grid`], except
/// omitting `--sheet`, `--range` and `--whole-sheet` entirely means "keep
/// the existing range" (`Ok(None)`) rather than an error, since the verb
/// may be a rename only.
fn resolve_optional_grid(
    workbook: &Spreadsheet,
    sheet: Option<&str>,
    range: Option<&str>,
    whole_sheet: bool,
) -> Result<Option<GridRange>, NamedRangeResult> {
    if !whole_sheet && sheet.is_none() && range.is_none() {
        return Ok(None);
    }
    resolve_grid(workbook, sheet, range, whole_sheet).map(Some)
}

fn find_sheet_id(workbook: &Spreadsheet, title: &str) -> Result<i64, NamedRangeResult> {
    grid_range::find_sheet_id(workbook, title, |title, available| {
        NamedRangeResult::RefusedSheetNotFound { title, available }
    })
}

/// Finds the one existing named range with an exact `name` match —
/// `update-named-range`/`delete-named-range`'s only stable handle, since a
/// named range's server-assigned id is discoverable only via
/// `list-named-ranges`. Sheets enforces unique names workbook-wide, so
/// unlike `protection.rs::find_existing_protection` this never needs an
/// "ambiguous" branch — only found or not found.
fn find_existing_named_range<'a>(
    workbook: &'a Spreadsheet,
    name: &str,
) -> Result<&'a NamedRange, NamedRangeResult> {
    workbook
        .named_ranges
        .iter()
        .find(|nr| nr.name == name)
        .ok_or_else(|| NamedRangeResult::RefusedNotFound {
            name: name.to_string(),
        })
}

/// Builds the `updateNamedRange` request and its `fields` mask from what
/// actually changed, mirroring `protection.rs::build_update`'s incremental-
/// mask pattern. `named_range`/`range` are always sent in full (unlike
/// `ProtectedRangeUpdate`'s `Option` fields) since [`NamedRange`] has no
/// field this request must exclude — the `fields` mask alone controls what
/// the server applies, so re-sending an unchanged value is harmless.
fn build_update(
    existing: &NamedRange,
    new_name: &Option<String>,
    new_grid: Option<GridRange>,
) -> UpdateNamedRangeRequest {
    let mut fields = Vec::new();
    let mut update = existing.clone();
    if let Some(new_name) = new_name {
        update.name.clone_from(new_name);
        fields.push("name");
    }
    if let Some(grid) = new_grid {
        update.range = grid;
        fields.push("range");
    }
    UpdateNamedRangeRequest {
        named_range: update,
        fields: fields.join(","),
    }
}

fn added_named_range_id(response: &BatchUpdateResponse) -> Option<String> {
    response
        .replies
        .iter()
        .find_map(|reply| reply.add_named_range.as_ref())
        .and_then(|added| added.named_range.as_ref())
        .and_then(|nr| nr.named_range_id.clone())
}

/// Scans every sheet's formulas for a bare reference to `name`, returning
/// the A1 location of each match — **never** the formula text or a cell's
/// value (ADR-0081 §2's data-exposure line). Word-boundary matched and
/// case-insensitive, so a name like `Foo` doesn't match inside `Foobar`,
/// and a formula spelling it `FOO` is still caught — matching Sheets' own
/// case-insensitive resolution of a named-range reference, and erring
/// toward over- rather than under-reporting for a safety preview.
async fn scan_referencing_formulas(
    api: &SheetsApi<'_>,
    spreadsheet_id: &str,
    workbook: &Spreadsheet,
    name: &str,
) -> Result<Vec<String>, String> {
    let titles = workbook.sheet_titles();
    if titles.is_empty() {
        return Ok(Vec::new());
    }
    let pattern = format!(r"(?i)\b{}\b", regex::escape(name));
    let re = regex::Regex::new(&pattern).map_err(|err| format!("{err:#}"))?;

    let mut locations = Vec::new();
    for chunk in titles.chunks(MAX_RANGES_PER_BATCH) {
        let ranges: Vec<String> = chunk.iter().map(|t| a1::quote_sheet_title(t)).collect();
        let response = api
            .values_batch_get(spreadsheet_id, &ranges, ValueRenderOption::Formula)
            .await
            .map_err(|err| format!("{err:#}"))?;
        for value_range in response.value_ranges {
            // Match on the range the server echoes, never on request order
            // — see `ValueRange::range`.
            let Some(title) = value_range.range.as_deref().and_then(a1::sheet_title_of) else {
                continue;
            };
            for (row_idx, row) in value_range.values.iter().enumerate() {
                for (col_idx, cell) in row.iter().enumerate() {
                    let Some(formula) = cell.as_str() else {
                        continue;
                    };
                    // Only a formula (leading `=`) can reference a named
                    // range — a plain text cell whose content merely
                    // contains the name is not a reference.
                    if !formula.starts_with('=') || !re.is_match(formula) {
                        continue;
                    }
                    let address = format!(
                        "{}{}",
                        grid_range::column_index_to_letters(col_idx as i64),
                        row_idx + 1
                    );
                    let location = a1::compose(Some(&title), Some(&address))
                        .unwrap_or_else(|_| format!("{title}!{address}"));
                    locations.push(location);
                }
            }
        }
    }
    Ok(locations)
}

fn describe_effect(verb: &NamedRangeVerb) -> String {
    match verb {
        NamedRangeVerb::AddNamedRange { name, .. } => format!("add named range '{name}'"),
        NamedRangeVerb::UpdateNamedRange { name, new_name, .. } => match new_name {
            Some(new_name) => format!("rename named range '{name}' to '{new_name}'"),
            None => format!("update named range '{name}'"),
        },
        NamedRangeVerb::DeleteNamedRange { name } => format!("delete named range '{name}'"),
    }
}

fn record_attempt(outcome: &NamedRangeOutcome, opts: &NamedRangeOptions, duration: Duration) {
    let error = match &outcome.result {
        NamedRangeResult::Failed { detail } => Some(detail.clone()),
        _ => None,
    };
    let decided_by = match &outcome.result {
        NamedRangeResult::Blocked { decided_by } => decided_by.as_ref(),
        _ => None,
    };
    let decided_by = write_gate::decided_by_log_fields(decided_by);
    let (named_range_id, referencing_formula_locations) = match &outcome.result {
        NamedRangeResult::Changed {
            named_range_id,
            referencing_formulas,
            ..
        } => (named_range_id.clone(), referencing_formulas.clone()),
        _ => (None, Vec::new()),
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
        named_range_id,
        referencing_formula_locations,
        error,
        duration,
        ..Default::default()
    });
}

/// Renders an outcome as human-readable text.
#[must_use]
pub fn describe(outcome: &NamedRangeOutcome) -> String {
    describe_lines(outcome).join("\n")
}

/// Renders an outcome as its individual lines, none of which contains a
/// newline.
#[must_use]
pub fn describe_lines(outcome: &NamedRangeOutcome) -> Vec<String> {
    let verb = &outcome.verb;
    let book = outcome.file_name.as_deref().map_or_else(
        || format!("'{}'", outcome.spreadsheet_id),
        |n| format!("'{n}'"),
    );
    match &outcome.result {
        NamedRangeResult::WouldChange {
            summary,
            referencing_formulas,
        } => {
            let mut lines = vec![format!("Would {summary} in {book}")];
            lines.extend(referencing_formula_lines(referencing_formulas));
            lines
        }
        NamedRangeResult::RefusedNotASpreadsheet { mime_type } => vec![format!(
            "Refused: {book} is not a Google Sheet (mimeType: {mime_type}); \
             `drive sheets {}` only works on spreadsheets",
            verb.label()
        )],
        NamedRangeResult::RefusedShortcut => vec![format!(
            "Refused: {book} is a shortcut; `drive sheets {}` doesn't follow shortcuts",
            verb.label()
        )],
        NamedRangeResult::RefusedNoVisibleParents => vec![format!(
            "Refused: {book} has no parent folder visible to this account, so no folder \
             rule can apply to it. Grant it by id instead: add {{\"file_id\": \"<spreadsheet \
             id>\", \"allow\": [\"sheets-structure\"]}} to write_permissions.rules."
        )],
        NamedRangeResult::RefusedSheetNotFound { title, available } => {
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
        NamedRangeResult::RefusedInvalidRange { detail } => vec![format!("Refused: {detail}")],
        NamedRangeResult::RefusedNotFound { name } => vec![format!(
            "Refused: {book} has no named range '{name}'; run \
             `drive sheets list-named-ranges` to see what exists"
        )],
        NamedRangeResult::Blocked { decided_by } => vec![match decided_by {
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
        NamedRangeResult::RefusedNoLease => LeaseGateRefusal::NoLease
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        NamedRangeResult::RefusedLeaseExpired => LeaseGateRefusal::Expired
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        NamedRangeResult::RefusedLeaseWrongFile => LeaseGateRefusal::WrongFile
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        NamedRangeResult::RefusedLeaseStale => LeaseGateRefusal::Stale
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        NamedRangeResult::Changed {
            summary,
            named_range_id,
            referencing_formulas,
        } => {
            let id = named_range_id
                .as_deref()
                .map_or_else(String::new, |id| format!(" (id {id})"));
            let mut lines = vec![format!("Applied: {summary}{id} in {book}")];
            lines.extend(referencing_formula_lines(referencing_formulas));
            lines
        }
        NamedRangeResult::Failed { detail } => vec![format!("Failed: {detail}")],
    }
}

/// Renders the referencing-formula preview (ADR-0081 §2) as human-readable
/// lines: a count line, then each A1 location on its own line. Empty when
/// nothing references the name, and for every verb but `delete-named-range`.
fn referencing_formula_lines(referencing_formulas: &[String]) -> Vec<String> {
    if referencing_formulas.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![format!(
        "{} formula(s) reference this name and will start evaluating to #NAME? once it's \
         removed:",
        referencing_formulas.len()
    )];
    lines.extend(referencing_formulas.iter().map(|loc| format!("  {loc}")));
    lines
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

    fn grid(sheet_id: i64) -> GridRange {
        GridRange {
            sheet_id,
            start_row_index: Some(0),
            end_row_index: Some(5),
            start_column_index: Some(0),
            end_column_index: Some(1),
        }
    }

    fn named(id: &str, name: &str, range: GridRange) -> NamedRange {
        NamedRange {
            named_range_id: Some(id.to_string()),
            name: name.to_string(),
            range,
        }
    }

    fn workbook_with(named_ranges: Vec<NamedRange>) -> Spreadsheet {
        Spreadsheet {
            sheets: vec![Sheet {
                properties: None,
                protected_ranges: Vec::new(),
            }],
            named_ranges,
            ..Default::default()
        }
    }

    #[test]
    fn find_existing_named_range_matches_by_exact_name() {
        let workbook = workbook_with(vec![named("id-1", "Foo", grid(0))]);
        let found = find_existing_named_range(&workbook, "Foo").unwrap();
        assert_eq!(found.named_range_id.as_deref(), Some("id-1"));
    }

    #[test]
    fn find_existing_named_range_refuses_when_none_matches() {
        let workbook = workbook_with(vec![named("id-1", "Foo", grid(0))]);
        let err = find_existing_named_range(&workbook, "Bar").unwrap_err();
        assert!(matches!(err, NamedRangeResult::RefusedNotFound { name } if name == "Bar"));
    }

    #[test]
    fn resolve_grid_rejects_whole_sheet_with_range() {
        let workbook = workbook_with(Vec::new());
        let err = resolve_grid(&workbook, Some("Q1"), Some("A1"), true).unwrap_err();
        assert!(matches!(err, NamedRangeResult::RefusedInvalidRange { .. }));
    }

    #[test]
    fn resolve_grid_rejects_whole_sheet_without_a_sheet() {
        let workbook = workbook_with(Vec::new());
        let err = resolve_grid(&workbook, None, None, true).unwrap_err();
        assert!(matches!(err, NamedRangeResult::RefusedInvalidRange { .. }));
    }

    #[test]
    fn resolve_optional_grid_is_none_when_nothing_is_given() {
        let workbook = workbook_with(Vec::new());
        assert_eq!(
            resolve_optional_grid(&workbook, None, None, false).unwrap(),
            None
        );
    }

    #[test]
    fn build_update_only_sets_fields_that_changed() {
        let existing = named("id-1", "Foo", grid(0));
        let request = build_update(&existing, &None, None);
        assert_eq!(request.fields, "");
        assert_eq!(request.named_range.name, "Foo");

        let request = build_update(&existing, &Some("Bar".to_string()), None);
        assert_eq!(request.fields, "name");
        assert_eq!(request.named_range.name, "Bar");

        let new_grid = grid(1);
        let request = build_update(&existing, &None, Some(new_grid));
        assert_eq!(request.fields, "range");
        assert_eq!(request.named_range.range, new_grid);
    }

    // ── the word-boundary/case-insensitivity scan ──────────────────────

    #[test]
    fn regex_matches_the_name_but_not_a_longer_identifier_containing_it() {
        let re = regex::Regex::new(&format!(r"(?i)\b{}\b", regex::escape("Foo"))).unwrap();
        assert!(re.is_match("=SUM(Foo)"));
        assert!(re.is_match("=SUM(foo)"));
        assert!(!re.is_match("=SUM(Foobar)"));
    }

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

    /// `version: "1"` throughout — matches [`leased_opts_for`]'s default
    /// seeded lease.
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

    fn leased_opts_for(spreadsheet_id: &str) -> (Option<String>, std::path::PathBuf) {
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        let token = seed_lease(&ledger_path, spreadsheet_id, "1");
        (Some(token), ledger_path)
    }

    fn mount_folder(id: &str) -> wiremock::Mock {
        mount_file(id, "application/vnd.google-apps.folder", &[])
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

    fn mount_workbook(named_ranges: serde_json::Value) -> wiremock::Mock {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/sheet-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "spreadsheetId": "sheet-1",
                    "properties": {"title": "Budget"},
                    "sheets": [
                        {"properties": {"sheetId": 0, "title": "Q1", "index": 0}},
                        {"properties": {"sheetId": 1, "title": "Q2", "index": 1}},
                    ],
                    "namedRanges": named_ranges,
                })),
            )
    }

    fn mount_batch_get(sheet: &str, values: serde_json::Value) -> wiremock::Mock {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values:batchGet",
            ))
            .and(wiremock::matchers::query_param(
                "ranges",
                format!("'{sheet}'"),
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "valueRanges": [{"range": format!("{sheet}!A1:Z1000"), "values": values}]
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
        let opts = NamedRangeOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: NamedRangeVerb::AddNamedRange {
                name: "Foo".to_string(),
                sheet: Some("Q1".to_string()),
                range: Some("A1:A5".to_string()),
                whole_sheet: false,
            },
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = named_range(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, NamedRangeResult::Blocked { .. }));
    }

    #[tokio::test]
    async fn add_named_range_sends_an_add_named_range_request() {
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
        mount_workbook(serde_json::json!([])).mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "replies": [{"addNamedRange": {"namedRange": {"namedRangeId": "id-99"}}}]
                })),
            )
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
        let opts = NamedRangeOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: NamedRangeVerb::AddNamedRange {
                name: "Foo".to_string(),
                sheet: Some("Q1".to_string()),
                range: Some("A1:A5".to_string()),
                whole_sheet: false,
            },
            dry_run: false,
            lease_token,
            ledger_path,
        };
        let outcome = named_range(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            NamedRangeResult::Changed { named_range_id, .. } => {
                assert_eq!(named_range_id.as_deref(), Some("id-99"));
            }
            other => panic!("expected Changed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn update_named_range_renames_and_re_ranges() {
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
        mount_workbook(serde_json::json!([{
            "namedRangeId": "id-1",
            "name": "Foo",
            "range": {"sheetId": 0, "startRowIndex": 0, "endRowIndex": 5,
                      "startColumnIndex": 0, "endColumnIndex": 1},
        }]))
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
        let opts = NamedRangeOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: NamedRangeVerb::UpdateNamedRange {
                name: "Foo".to_string(),
                new_name: Some("Bar".to_string()),
                sheet: Some("Q2".to_string()),
                range: None,
                whole_sheet: true,
            },
            dry_run: false,
            lease_token,
            ledger_path,
        };
        let outcome = named_range(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            NamedRangeResult::Changed { named_range_id, .. } => {
                assert_eq!(named_range_id.as_deref(), Some("id-1"));
            }
            other => panic!("expected Changed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn update_named_range_refuses_when_no_name_matches() {
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
        mount_workbook(serde_json::json!([])).mount(&server).await;
        let rules = vec![allow_rule("folder-1")];
        let opts = NamedRangeOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: NamedRangeVerb::UpdateNamedRange {
                name: "Foo".to_string(),
                new_name: Some("Bar".to_string()),
                sheet: None,
                range: None,
                whole_sheet: false,
            },
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = named_range(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            NamedRangeResult::RefusedNotFound { .. }
        ));
    }

    #[tokio::test]
    async fn delete_named_range_dry_run_reports_referencing_formulas_without_a_decoy_match() {
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
        mount_workbook(serde_json::json!([{
            "namedRangeId": "id-1",
            "name": "Foo",
            "range": {"sheetId": 0},
        }]))
        .mount(&server)
        .await;
        // A true reference on Q1, and a decoy on Q2 that must NOT match:
        // "Foobar" contains "Foo" but is a distinct identifier.
        mount_batch_get(
            "Q1",
            serde_json::json!([["=SUM(Foo)", "plain text with Foo in it"]]),
        )
        .mount(&server)
        .await;
        mount_batch_get("Q2", serde_json::json!([["=Foobar+1"]]))
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let opts = NamedRangeOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: NamedRangeVerb::DeleteNamedRange {
                name: "Foo".to_string(),
            },
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = named_range(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            NamedRangeResult::WouldChange {
                referencing_formulas,
                ..
            } => {
                assert_eq!(referencing_formulas, vec!["'Q1'!A1".to_string()]);
            }
            other => panic!("expected WouldChange, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn delete_named_range_with_zero_references_reports_an_empty_preview() {
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
        mount_workbook(serde_json::json!([{
            "namedRangeId": "id-1",
            "name": "Foo",
            "range": {"sheetId": 0},
        }]))
        .mount(&server)
        .await;
        mount_batch_get("Q1", serde_json::json!([]))
            .mount(&server)
            .await;
        mount_batch_get("Q2", serde_json::json!([]))
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let opts = NamedRangeOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: NamedRangeVerb::DeleteNamedRange {
                name: "Foo".to_string(),
            },
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = named_range(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            NamedRangeResult::WouldChange {
                referencing_formulas,
                ..
            } => assert!(referencing_formulas.is_empty()),
            other => panic!("expected WouldChange, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn delete_named_range_real_run_sends_the_same_preview_as_dry_run() {
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
        mount_workbook(serde_json::json!([{
            "namedRangeId": "id-1",
            "name": "Foo",
            "range": {"sheetId": 0},
        }]))
        .mount(&server)
        .await;
        mount_batch_get("Q1", serde_json::json!([["=SUM(Foo)"]]))
            .mount(&server)
            .await;
        mount_batch_get("Q2", serde_json::json!([]))
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
        let opts = NamedRangeOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: NamedRangeVerb::DeleteNamedRange {
                name: "Foo".to_string(),
            },
            dry_run: false,
            lease_token,
            ledger_path,
        };
        let outcome = named_range(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            NamedRangeResult::Changed {
                named_range_id,
                referencing_formulas,
                ..
            } => {
                assert_eq!(named_range_id.as_deref(), Some("id-1"));
                assert_eq!(referencing_formulas, vec!["'Q1'!A1".to_string()]);
            }
            other => panic!("expected Changed, got {other:?}"),
        }
    }
}
