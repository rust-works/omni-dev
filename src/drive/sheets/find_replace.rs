//! Find and replace cell text through `spreadsheets.batchUpdate` (issue #1841).
//!
//! ADR-0083 §§1, 6 and 7 place every scope under `SheetsWrite`, require the
//! ordinary optional lease flow, and prohibit a local match-count preview:
//! Sheets owns the matching semantics, including Java regular expressions.

#![allow(missing_docs)] // The CLI-facing outcome models are self-describing in JSON.

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
use crate::drive::sheets::types::{
    BatchUpdateRequestItem, FindReplaceRequest, FindReplaceResponse, FindReplaceScope, GridRange,
    Spreadsheet,
};
use crate::drive::sheets::{a1, grid_range, target_gate};
use crate::drive::types::SheetTargetRefusal;
use crate::drive::write_gate::{self, DecidingRule, DriveOperation, FolderPermissionRule};
use crate::request_log::{self, DriveMutationOutcome};

/// A find/replace request as received from the CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindReplaceOptions {
    pub spreadsheet_id: String,
    pub find: String,
    pub replacement: String,
    pub sheet: Option<String>,
    pub range: Option<String>,
    pub whole_sheet: bool,
    pub all_sheets: bool,
    pub match_case: bool,
    pub match_entire_cell: bool,
    pub search_by_regex: bool,
    pub include_formulas: bool,
    pub dry_run: bool,
    pub lease_token: Option<String>,
    pub ledger_path: PathBuf,
}

/// The resolved scope recorded and rendered without recording cell contents.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum FindReplaceTarget {
    Range { range: String, sheet_id: i64 },
    Sheet { sheet: String, sheet_id: i64 },
    AllSheets,
}

impl FindReplaceTarget {
    fn request_scope(&self, grid: Option<GridRange>) -> Option<FindReplaceScope> {
        match self {
            Self::Range { .. } => grid.map(|range| FindReplaceScope::Range { range }),
            Self::Sheet { sheet_id, .. } => Some(FindReplaceScope::Sheet {
                sheet_id: *sheet_id,
            }),
            Self::AllSheets => Some(FindReplaceScope::AllSheets { all_sheets: true }),
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::Range { range, .. } => format!("range {range}"),
            Self::Sheet { sheet, .. } => format!("sheet '{sheet}'"),
            Self::AllSheets => "all sheets".to_string(),
        }
    }
}

/// Counts supplied by a successful Sheets reply.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FindReplaceCounts {
    pub values_changed: i64,
    pub formulas_changed: i64,
    pub rows_changed: i64,
    pub sheets_changed: i64,
    pub occurrences_changed: i64,
}

impl From<FindReplaceResponse> for FindReplaceCounts {
    fn from(value: FindReplaceResponse) -> Self {
        Self {
            values_changed: value.values_changed,
            formulas_changed: value.formulas_changed,
            rows_changed: value.rows_changed,
            sheets_changed: value.sheets_changed,
            occurrences_changed: value.occurrences_changed,
        }
    }
}

/// Result of a find/replace attempt.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum FindReplaceResult {
    WouldChange {
        target: FindReplaceTarget,
        summary: String,
    },
    Changed {
        target: FindReplaceTarget,
        counts: Option<FindReplaceCounts>,
    },
    RefusedInvalidRequest {
        detail: String,
    },
    RefusedNotASpreadsheet {
        mime_type: String,
    },
    RefusedShortcut,
    RefusedNoVisibleParents,
    RefusedSheetNotFound {
        title: String,
        available: Vec<String>,
    },
    Blocked {
        decided_by: Option<DecidingRule>,
    },
    RefusedNoLease,
    RefusedLeaseExpired,
    RefusedLeaseWrongFile,
    RefusedLeaseStale,
    Failed {
        detail: String,
    },
}

impl FromLeaseRefusal for FindReplaceResult {
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

impl FindReplaceResult {
    fn log_status(&self) -> &'static str {
        match self {
            Self::WouldChange { .. } => "would-change",
            Self::Changed { .. } => "changed",
            Self::RefusedInvalidRequest { .. } => "refused-invalid-request",
            Self::RefusedNotASpreadsheet { .. } => "refused-not-a-spreadsheet",
            Self::RefusedShortcut => "refused-shortcut",
            Self::RefusedNoVisibleParents => "refused-no-visible-parents",
            Self::RefusedSheetNotFound { .. } => "refused-sheet-not-found",
            Self::Blocked { .. } => "blocked",
            Self::RefusedNoLease => "refused-no-lease",
            Self::RefusedLeaseExpired => "refused-lease-expired",
            Self::RefusedLeaseWrongFile => "refused-lease-wrong-file",
            Self::RefusedLeaseStale => "refused-lease-stale",
            Self::Failed { .. } => "failed",
        }
    }
}

/// Full outcome, including metadata useful for structured output and logs.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FindReplaceOutcome {
    pub spreadsheet_id: String,
    pub file_name: Option<String>,
    pub resolved_folder_id: Option<String>,
    pub result: FindReplaceResult,
}

impl JsonlSerialize for FindReplaceOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> anyhow::Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Runs one attempt, logging every real attempt.
pub async fn find_replace(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &FindReplaceOptions,
    rules: &[FolderPermissionRule],
) -> FindReplaceOutcome {
    let started = Instant::now();
    let outcome = find_replace_inner(drive, sheets, opts, rules).await;
    if !opts.dry_run {
        record_attempt(&outcome, opts, started.elapsed());
    }
    outcome
}

async fn find_replace_inner(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &FindReplaceOptions,
    rules: &[FolderPermissionRule],
) -> FindReplaceOutcome {
    let bare = |result| FindReplaceOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: None,
        resolved_folder_id: None,
        result,
    };
    if opts.find.is_empty() {
        return bare(FindReplaceResult::RefusedInvalidRequest {
            detail: "--find must not be empty".to_string(),
        });
    }
    if let Err(detail) = validate_scope_syntax(opts) {
        return bare(FindReplaceResult::RefusedInvalidRequest { detail });
    }
    let (target_meta, decision, resolved_folder_id, requires_lease) = match target_gate::resolve(
        drive,
        &opts.spreadsheet_id,
        DriveOperation::SheetsWrite,
        rules,
    )
    .await
    {
        target_gate::TargetGateOutcome::MetadataFetchFailed { detail } => {
            return bare(FindReplaceResult::Failed { detail })
        }
        target_gate::TargetGateOutcome::Refused { target, refusal } => {
            let result = match refusal {
                SheetTargetRefusal::Shortcut => FindReplaceResult::RefusedShortcut,
                SheetTargetRefusal::NotASpreadsheet { mime_type } => {
                    FindReplaceResult::RefusedNotASpreadsheet { mime_type }
                }
                SheetTargetRefusal::NoVisibleParents => FindReplaceResult::RefusedNoVisibleParents,
            };
            return FindReplaceOutcome {
                spreadsheet_id: opts.spreadsheet_id.clone(),
                file_name: Some(target.name),
                resolved_folder_id: None,
                result,
            };
        }
        target_gate::TargetGateOutcome::GateFetchFailed { target, detail } => {
            return FindReplaceOutcome {
                spreadsheet_id: opts.spreadsheet_id.clone(),
                file_name: Some(target.name),
                resolved_folder_id: None,
                result: FindReplaceResult::Failed { detail },
            }
        }
        target_gate::TargetGateOutcome::Gated {
            target,
            decision,
            resolved_folder_id,
            requires_lease,
        } => (target, decision, resolved_folder_id, requires_lease),
    };
    let gated = |result| FindReplaceOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: Some(target_meta.name.clone()),
        resolved_folder_id: resolved_folder_id.clone(),
        result,
    };
    if decision.verdict == write_gate::Verdict::Deny {
        return gated(FindReplaceResult::Blocked {
            decided_by: decision.decided_by,
        });
    }
    let api = SheetsApi::new(sheets);
    let workbook = match api.get_spreadsheet(&opts.spreadsheet_id).await {
        Ok(workbook) => workbook,
        Err(err) => {
            return gated(FindReplaceResult::Failed {
                detail: format!("{err:#}"),
            })
        }
    };
    let (target, grid) = match resolve_target(opts, &workbook) {
        Ok(value) => value,
        Err(result) => return gated(result),
    };
    let Some(scope) = target.request_scope(grid) else {
        return gated(FindReplaceResult::Failed {
            detail: "internal error: a range scope did not resolve a GridRange".to_string(),
        });
    };
    let request = FindReplaceRequest {
        find: opts.find.clone(),
        replacement: opts.replacement.clone(),
        match_case: opts.match_case,
        match_entire_cell: opts.match_entire_cell,
        search_by_regex: opts.search_by_regex,
        include_formulas: opts.include_formulas,
        scope,
    };
    if opts.dry_run {
        return gated(FindReplaceResult::WouldChange {
            target,
            summary: preview_summary(opts),
        });
    }
    let files_api = FilesApi::new(drive);
    let leased = LeasedWrite {
        log_prefix: "drive sheets find-replace",
        operation: "sheets-find-replace",
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
    let response = match conclude_native_leased_write(
        leased,
        &lease_grant,
        &files_api,
        api.batch_update(
            &opts.spreadsheet_id,
            vec![BatchUpdateRequestItem::FindReplace(request)],
        )
        .await,
        |err| format!("{err:#}"),
    )
    .await
    {
        Ok(response) => response,
        Err(err) => {
            return gated(FindReplaceResult::Failed {
                detail: format!("{err:#}"),
            })
        }
    };
    let counts = response
        .replies
        .into_iter()
        .next()
        .and_then(|reply| reply.find_replace)
        .map(Into::into);
    gated(FindReplaceResult::Changed { target, counts })
}

fn resolve_target(
    opts: &FindReplaceOptions,
    workbook: &Spreadsheet,
) -> Result<(FindReplaceTarget, Option<GridRange>), FindReplaceResult> {
    debug_assert!(validate_scope_syntax(opts).is_ok());
    if opts.all_sheets {
        if opts.sheet.is_some() {
            return Err(FindReplaceResult::RefusedInvalidRequest {
                detail: "--all-sheets cannot be combined with --sheet".to_string(),
            });
        }
        return Ok((FindReplaceTarget::AllSheets, None));
    }
    if opts.whole_sheet {
        if opts.range.is_some() {
            return Err(FindReplaceResult::RefusedInvalidRequest {
                detail: "--whole-sheet cannot be combined with --range".to_string(),
            });
        }
        let Some(sheet) = opts.sheet.as_deref() else {
            return Err(FindReplaceResult::RefusedInvalidRequest {
                detail: "--whole-sheet requires --sheet".to_string(),
            });
        };
        let sheet_id = grid_range::find_sheet_id(workbook, sheet, |title, available| {
            FindReplaceResult::RefusedSheetNotFound { title, available }
        })?;
        return Ok((
            FindReplaceTarget::Sheet {
                sheet: sheet.to_string(),
                sheet_id,
            },
            None,
        ));
    }
    let composed = a1::compose(opts.sheet.as_deref(), opts.range.as_deref()).map_err(|err| {
        FindReplaceResult::RefusedInvalidRequest {
            detail: err.to_string(),
        }
    })?;
    let (_, grid) = grid_range::resolve_grid_range(
        workbook,
        &composed,
        |detail| FindReplaceResult::RefusedInvalidRequest { detail },
        |title, available| FindReplaceResult::RefusedSheetNotFound { title, available },
    )?;
    Ok((
        FindReplaceTarget::Range {
            range: composed,
            sheet_id: grid.sheet_id,
        },
        Some(grid),
    ))
}

fn validate_scope_syntax(opts: &FindReplaceOptions) -> Result<(), String> {
    let scope_count = usize::from(opts.range.is_some())
        + usize::from(opts.whole_sheet)
        + usize::from(opts.all_sheets);
    if scope_count != 1 {
        return Err("pass exactly one scope: --range, --whole-sheet, or --all-sheets".to_string());
    }
    if opts.all_sheets && opts.sheet.is_some() {
        return Err("--all-sheets cannot be combined with --sheet".to_string());
    }
    if opts.whole_sheet && opts.sheet.is_none() {
        return Err("--whole-sheet requires --sheet".to_string());
    }
    Ok(())
}

fn preview_summary(opts: &FindReplaceOptions) -> String {
    format!("find/replace with match-case={}, match-entire-cell={}, search-by-regex={}, include-formulas={}; Sheets computes match counts when executed", opts.match_case, opts.match_entire_cell, opts.search_by_regex, opts.include_formulas)
}

fn record_attempt(outcome: &FindReplaceOutcome, _opts: &FindReplaceOptions, duration: Duration) {
    let decided_by = match &outcome.result {
        FindReplaceResult::Blocked { decided_by } => decided_by.as_ref(),
        _ => None,
    };
    let decided_by = write_gate::decided_by_log_fields(decided_by);
    let (occurrences_changed, updated_cells, fields_changed) = match &outcome.result {
        FindReplaceResult::Changed {
            target,
            counts: Some(counts),
        } => (
            Some(counts.occurrences_changed),
            Some(counts.values_changed + counts.formulas_changed),
            Some(format!(
                "{}; rows_changed={}, sheets_changed={}",
                target.describe(),
                counts.rows_changed,
                counts.sheets_changed
            )),
        ),
        FindReplaceResult::Changed {
            target,
            counts: None,
        } => (
            None,
            None,
            Some(format!(
                "{}; response counts unavailable",
                target.describe()
            )),
        ),
        FindReplaceResult::WouldChange { target, .. } => (None, None, Some(target.describe())),
        _ => (None, None, None),
    };
    let error = match &outcome.result {
        FindReplaceResult::Failed { detail }
        | FindReplaceResult::RefusedInvalidRequest { detail } => Some(detail.clone()),
        _ => None,
    };
    request_log::record_drive_mutation(DriveMutationOutcome {
        operation: "sheets-find-replace",
        file_id: outcome.spreadsheet_id.clone(),
        file_name: outcome.file_name.clone().unwrap_or_default(),
        status: outcome.result.log_status().to_string(),
        resolved_folder_id: outcome.resolved_folder_id.clone(),
        decided_by_folder_id: decided_by.folder_id,
        decided_by_depth: decided_by.depth,
        decided_by_file_id: decided_by.file_id,
        occurrences_changed,
        updated_cells,
        fields_changed,
        error,
        duration,
        ..Default::default()
    });
}

/// Human-readable lines, free of request content except the safe scope.
#[must_use]
pub fn describe_lines(outcome: &FindReplaceOutcome) -> Vec<String> {
    let book = outcome.file_name.as_deref().map_or_else(
        || format!("'{}'", outcome.spreadsheet_id),
        |name| format!("'{name}'"),
    );
    match &outcome.result {
        FindReplaceResult::WouldChange { target, summary } => vec![format!("Would run find/replace in {} on {book}. {summary}", target.describe())],
        FindReplaceResult::Changed { target, counts: Some(c) } => vec![format!("Applied find/replace in {} on {book}: values={}, formulas={}, rows={}, sheets={}, occurrences={}", target.describe(), c.values_changed, c.formulas_changed, c.rows_changed, c.sheets_changed, c.occurrences_changed)],
        FindReplaceResult::Changed { target, counts: None } => vec![format!("Applied find/replace in {} on {book}; Sheets returned no change counts", target.describe())],
        FindReplaceResult::RefusedInvalidRequest { detail } => vec![format!("Refused: {detail}")],
        FindReplaceResult::RefusedNotASpreadsheet { mime_type } => vec![format!("Refused: {book} is not a Google Sheet (mimeType: {mime_type})")],
        FindReplaceResult::RefusedShortcut => vec![format!("Refused: {book} is a shortcut; find-replace doesn't follow shortcuts")],
        FindReplaceResult::RefusedNoVisibleParents => vec![format!("Refused: {book} has no visible parent folder")],
        FindReplaceResult::RefusedSheetNotFound { title, available } => vec![format!("Refused: {book} has no sheet titled '{title}'. Available: {}", available.join(", "))],
        FindReplaceResult::Blocked { decided_by } => vec![match decided_by { Some(rule) => format!("Blocked: find-replace on {book} refused by rule on {} {}{}", rule.kind_label(), rule.id(), rule.depth_suffix()), None => format!("Blocked: find-replace on {book} refused by default policy (no matching rule for sheets-write)") }],
        FindReplaceResult::RefusedNoLease => LeaseGateRefusal::NoLease.describe_line(&outcome.spreadsheet_id, &book).into_iter().collect(),
        FindReplaceResult::RefusedLeaseExpired => LeaseGateRefusal::Expired.describe_line(&outcome.spreadsheet_id, &book).into_iter().collect(),
        FindReplaceResult::RefusedLeaseWrongFile => LeaseGateRefusal::WrongFile.describe_line(&outcome.spreadsheet_id, &book).into_iter().collect(),
        FindReplaceResult::RefusedLeaseStale => LeaseGateRefusal::Stale.describe_line(&outcome.spreadsheet_id, &book).into_iter().collect(),
        FindReplaceResult::Failed { detail } => vec![format!("Failed: {detail}")],
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn options() -> FindReplaceOptions {
        FindReplaceOptions {
            spreadsheet_id: "sheet-1".to_string(),
            find: "draft".to_string(),
            replacement: "final".to_string(),
            sheet: None,
            range: Some("Q1!A1".to_string()),
            whole_sheet: false,
            all_sheets: false,
            match_case: false,
            match_entire_cell: false,
            search_by_regex: false,
            include_formulas: false,
            dry_run: true,
            lease_token: None,
            ledger_path: PathBuf::from("/tmp/unused-ledger"),
        }
    }

    #[test]
    fn validate_scope_syntax_requires_one_unambiguous_scope() {
        let opts = options();
        assert!(validate_scope_syntax(&opts).is_ok());

        let mut ambiguous = opts.clone();
        ambiguous.all_sheets = true;
        assert!(validate_scope_syntax(&ambiguous)
            .unwrap_err()
            .contains("exactly one scope"));

        let mut whole_without_sheet = opts;
        whole_without_sheet.range = None;
        whole_without_sheet.whole_sheet = true;
        assert!(validate_scope_syntax(&whole_without_sheet)
            .unwrap_err()
            .contains("requires --sheet"));
    }
}
