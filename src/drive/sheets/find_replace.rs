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
        // omni-dev: coverage ignore reason="request_scope only returns None for a Range target paired with no grid, but resolve_target's Range arm always returns Ok with Some(grid) alongside it; this arm exists only to unwrap the shared Option"
        return gated(FindReplaceResult::Failed {
            detail: "internal error: a range scope did not resolve a GridRange".to_string(),
        });
        // omni-dev: coverage end
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
            // omni-dev: coverage ignore reason="validate_scope_syntax already refuses --all-sheets combined with --sheet before resolve_target is ever reached; this arm exists only as a defensive re-check"
            return Err(FindReplaceResult::RefusedInvalidRequest {
                detail: "--all-sheets cannot be combined with --sheet".to_string(),
            });
            // omni-dev: coverage end
        }
        return Ok((FindReplaceTarget::AllSheets, None));
    }
    if opts.whole_sheet {
        if opts.range.is_some() {
            // omni-dev: coverage ignore reason="validate_scope_syntax's scope_count check already guarantees --range is unset whenever --whole-sheet is set, before resolve_target is ever reached; this arm exists only as a defensive re-check"
            return Err(FindReplaceResult::RefusedInvalidRequest {
                detail: "--whole-sheet cannot be combined with --range".to_string(),
            });
            // omni-dev: coverage end
        }
        let Some(sheet) = opts.sheet.as_deref() else {
            // omni-dev: coverage ignore reason="validate_scope_syntax already refuses --whole-sheet without --sheet before resolve_target is ever reached; this arm exists only as a defensive re-check"
            return Err(FindReplaceResult::RefusedInvalidRequest {
                detail: "--whole-sheet requires --sheet".to_string(),
            });
            // omni-dev: coverage end
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
    format!(
        "find {:?}, replace with {:?} (match-case={}, match-entire-cell={}, search-by-regex={}, include-formulas={}); Sheets computes match counts when executed",
        opts.find, opts.replacement, opts.match_case, opts.match_entire_cell, opts.search_by_regex, opts.include_formulas
    )
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
        FindReplaceResult::WouldChange { target, .. } => (None, None, Some(target.describe())), // omni-dev: coverage ignore-line reason="record_attempt is only called when !opts.dry_run, and WouldChange is only ever returned when opts.dry_run is true, so this arm can never run"
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

        let mut whole_without_sheet = opts.clone();
        whole_without_sheet.range = None;
        whole_without_sheet.whole_sheet = true;
        assert!(validate_scope_syntax(&whole_without_sheet)
            .unwrap_err()
            .contains("requires --sheet"));

        let mut all_sheets_with_sheet = opts;
        all_sheets_with_sheet.range = None;
        all_sheets_with_sheet.all_sheets = true;
        all_sheets_with_sheet.sheet = Some("Q1".to_string());
        assert!(validate_scope_syntax(&all_sheets_with_sheet)
            .unwrap_err()
            .contains("--all-sheets cannot be combined with --sheet"));
    }

    #[test]
    fn lease_refusals_map_to_their_find_replace_results() {
        assert_eq!(
            FindReplaceResult::from_no_lease(),
            FindReplaceResult::RefusedNoLease
        );
        assert_eq!(
            FindReplaceResult::from_lease_expired(),
            FindReplaceResult::RefusedLeaseExpired
        );
        assert_eq!(
            FindReplaceResult::from_lease_wrong_file(),
            FindReplaceResult::RefusedLeaseWrongFile
        );
        assert_eq!(
            FindReplaceResult::from_lease_stale(),
            FindReplaceResult::RefusedLeaseStale
        );
        assert_eq!(
            FindReplaceResult::from_lease_failed("ledger unavailable".into()),
            FindReplaceResult::Failed {
                detail: "ledger unavailable".into()
            }
        );
    }

    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::sheets::client::SHEETS_API_URL;
    use crate::drive::test_support::seed_lease;
    use crate::drive::types::GOOGLE_SHEET_MIME_TYPE;
    use crate::drive::write_gate::Verdict;
    use crate::test_support::env::MapEnv;
    use crate::utils::secret::Secret;
    use std::collections::HashSet;

    fn test_credentials() -> DriveCredentials {
        DriveCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: DriveGrantedScopes::READONLY,
        }
    }

    /// Both clients against one wiremock server, sharing an OAuth session.
    /// Mirrors `write.rs::tests::clients`.
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

    /// `version: "1"` throughout — matches [`opts`]'s default seeded lease.
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

    /// A `spreadsheets.get` reply with two sheets: `Q1` (sheetId 0) and
    /// `Q2` (sheetId 42).
    fn mount_workbook() -> wiremock::Mock {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/sheet-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "spreadsheetId": "sheet-1",
                    "properties": {"title": "Budget"},
                    "sheets": [
                        {"properties": {
                            "sheetId": 0, "title": "Q1", "index": 0,
                            "gridProperties": {"rowCount": 1000, "columnCount": 26}}},
                        {"properties": {
                            "sheetId": 42, "title": "Q2", "index": 1,
                            "gridProperties": {"rowCount": 500, "columnCount": 10}}}
                    ],
                })),
            )
    }

    fn mount_batch_update(body: serde_json::Value) -> wiremock::Mock {
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
    }

    fn allow_rule(folder: &str) -> FolderPermissionRule {
        FolderPermissionRule {
            folder_id: Some(folder.to_string()),
            file_id: None,
            recursive: true,
            allow: std::iter::once(DriveOperation::SheetsWrite).collect(),
            deny: HashSet::default(),
            require_lease: true,
        }
    }

    fn allow_rule_no_lease(folder: &str) -> FolderPermissionRule {
        FolderPermissionRule {
            require_lease: false,
            ..allow_rule(folder)
        }
    }

    /// Seeds a fresh, isolated ledger with a live lease for `"sheet-1"` at
    /// version `"1"` (matching [`mount_file`]'s default) and returns
    /// options for a `--range Q1!A1:B2` request against it. Mirrors
    /// `write.rs::tests::opts`'s leaked-tempdir rationale.
    fn opts(dry_run: bool) -> FindReplaceOptions {
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        let token = seed_lease(&ledger_path, "sheet-1", "1");
        FindReplaceOptions {
            spreadsheet_id: "sheet-1".to_string(),
            find: "draft".to_string(),
            replacement: "final".to_string(),
            sheet: None,
            range: Some("Q1!A1:B2".to_string()),
            whole_sheet: false,
            all_sheets: false,
            match_case: false,
            match_entire_cell: false,
            search_by_regex: false,
            include_formulas: false,
            dry_run,
            lease_token: Some(token),
            ledger_path,
        }
    }

    // ── refusals that must precede the gate and the network ────────────

    #[tokio::test]
    async fn non_spreadsheet_is_refused_before_any_gate_or_sheets_call() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", "application/pdf", &["parent-1"])
            .mount(&server)
            .await;
        // Deliberately no mock for parent-1 (the gate never runs) and none
        // for any Sheets endpoint.
        let outcome = find_replace(&drive, &sheets, &opts(false), &[allow_rule("parent-1")]).await;
        assert!(matches!(
            outcome.result,
            FindReplaceResult::RefusedNotASpreadsheet { .. }
        ));
        let text = describe_lines(&outcome).join("\n");
        assert!(text.contains("is not a Google Sheet"), "{text}");
    }

    #[tokio::test]
    async fn shortcut_is_refused_with_its_own_message_not_the_generic_one() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            "application/vnd.google-apps.shortcut",
            &["parent-1"],
        )
        .mount(&server)
        .await;
        let outcome = find_replace(&drive, &sheets, &opts(false), &[allow_rule("parent-1")]).await;
        assert!(matches!(outcome.result, FindReplaceResult::RefusedShortcut));
        let text = describe_lines(&outcome).join("\n");
        assert!(text.contains("is a shortcut"), "{text}");
        assert!(!text.contains("is not a Google Sheet"), "{text}");
    }

    #[tokio::test]
    async fn a_sheet_with_no_visible_parents_is_refused_distinctly_from_a_blocked_one() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &[])
            .mount(&server)
            .await;
        let outcome = find_replace(&drive, &sheets, &opts(false), &[]).await;
        assert!(matches!(
            outcome.result,
            FindReplaceResult::RefusedNoVisibleParents
        ));
    }

    // ── scope validation, before any network call ───────────────────────

    #[tokio::test]
    async fn empty_find_is_refused_before_any_network_call() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        // No mocks at all — the refusal must short-circuit before `files.get`.
        let mut o = opts(false);
        o.find = String::new();
        let outcome = find_replace(&drive, &sheets, &o, &[allow_rule("parent-1")]).await;
        let FindReplaceResult::RefusedInvalidRequest { detail } = &outcome.result else {
            panic!("expected RefusedInvalidRequest, got {:?}", outcome.result); // omni-dev: coverage ignore-line reason="this let-else panic only runs if the match failed to bind the expected variant; this test always constructs that exact variant, so the branch never executes"
        };
        assert!(detail.contains("--find"), "{detail}");
    }

    #[tokio::test]
    async fn ambiguous_scope_is_refused_before_any_network_call() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        let mut o = opts(false);
        o.all_sheets = true; // `--range` is also set by `opts`.
        let outcome = find_replace(&drive, &sheets, &o, &[allow_rule("parent-1")]).await;
        let FindReplaceResult::RefusedInvalidRequest { detail } = &outcome.result else {
            panic!("expected RefusedInvalidRequest, got {:?}", outcome.result); // omni-dev: coverage ignore-line reason="this let-else panic only runs if the match failed to bind the expected variant; this test always constructs that exact variant, so the branch never executes"
        };
        assert!(detail.contains("exactly one scope"), "{detail}");
    }

    #[tokio::test]
    async fn whole_sheet_without_sheet_is_refused_before_any_network_call() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        let mut o = opts(false);
        o.range = None;
        o.whole_sheet = true;
        let outcome = find_replace(&drive, &sheets, &o, &[allow_rule("parent-1")]).await;
        let FindReplaceResult::RefusedInvalidRequest { detail } = &outcome.result else {
            panic!("expected RefusedInvalidRequest, got {:?}", outcome.result); // omni-dev: coverage ignore-line reason="this let-else panic only runs if the match failed to bind the expected variant; this test always constructs that exact variant, so the branch never executes"
        };
        assert!(detail.contains("requires --sheet"), "{detail}");
    }

    #[tokio::test]
    async fn a_conflicting_sheet_and_range_fails_before_any_batch_update() {
        // Unlike the pure-syntax refusals above, `--range` carrying its own
        // sheet prefix plus `--sheet` is only caught once composed via
        // `a1::compose`, which runs after the gate and the workbook fetch
        // have already succeeded — so this needs both mocks, but must still
        // never reach `batchUpdate`.
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        // No batchUpdate mock: the refusal must short-circuit the mutation.
        let mut o = opts(false);
        o.sheet = Some("Q1".to_string());
        let outcome = find_replace(&drive, &sheets, &o, &[allow_rule("parent-1")]).await;
        let FindReplaceResult::RefusedInvalidRequest { detail } = &outcome.result else {
            panic!("expected RefusedInvalidRequest, got {:?}", outcome.result); // omni-dev: coverage ignore-line reason="this let-else panic only runs if the match failed to bind the expected variant; this test always constructs that exact variant, so the branch never executes"
        };
        assert!(detail.contains("already names a sheet"), "{detail}");
    }

    #[tokio::test]
    async fn sheet_not_found_is_refused_with_available_titles() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        let mut o = opts(false);
        o.range = None;
        o.sheet = Some("Nope".to_string());
        o.whole_sheet = true;
        let outcome = find_replace(&drive, &sheets, &o, &[allow_rule("parent-1")]).await;
        let FindReplaceResult::RefusedSheetNotFound { title, available } = &outcome.result else {
            panic!("expected RefusedSheetNotFound, got {:?}", outcome.result); // omni-dev: coverage ignore-line reason="this let-else panic only runs if the match failed to bind the expected variant; this test always constructs that exact variant, so the branch never executes"
        };
        assert_eq!(title, "Nope");
        assert_eq!(available, &["Q1".to_string(), "Q2".to_string()]);
    }

    #[tokio::test]
    async fn a_range_without_a_sheet_prefix_is_refused_as_invalid() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        let mut o = opts(false);
        o.range = Some("A1:B2".to_string()); // no `Sheet!` prefix, and no `--sheet` either.
        let outcome = find_replace(&drive, &sheets, &o, &[allow_rule("parent-1")]).await;
        let FindReplaceResult::RefusedInvalidRequest { detail } = &outcome.result else {
            panic!("expected RefusedInvalidRequest, got {:?}", outcome.result); // omni-dev: coverage ignore-line reason="this let-else panic only runs if the match failed to bind the expected variant; this test always constructs that exact variant, so the branch never executes"
        };
        assert!(detail.contains("does not name a sheet"), "{detail}");
    }

    #[tokio::test]
    async fn a_range_naming_an_unknown_sheet_is_refused_with_available_titles() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        let mut o = opts(false);
        o.range = Some("Nope!A1:B2".to_string());
        let outcome = find_replace(&drive, &sheets, &o, &[allow_rule("parent-1")]).await;
        let FindReplaceResult::RefusedSheetNotFound { title, available } = &outcome.result else {
            panic!("expected RefusedSheetNotFound, got {:?}", outcome.result); // omni-dev: coverage ignore-line reason="this let-else panic only runs if the match failed to bind the expected variant; this test always constructs that exact variant, so the branch never executes"
        };
        assert_eq!(title, "Nope");
        assert_eq!(available, &["Q1".to_string(), "Q2".to_string()]);
    }

    #[tokio::test]
    async fn a_metadata_fetch_failure_surfaces_as_failed() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/sheet-1"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;
        let outcome = find_replace(&drive, &sheets, &opts(false), &[allow_rule("parent-1")]).await;
        assert!(matches!(outcome.result, FindReplaceResult::Failed { .. }));
    }

    #[tokio::test]
    async fn a_gate_fetch_failure_surfaces_as_failed() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/parent-1"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let outcome = find_replace(&drive, &sheets, &opts(false), &[allow_rule("parent-1")]).await;
        assert!(matches!(outcome.result, FindReplaceResult::Failed { .. }));
    }

    #[tokio::test]
    async fn a_workbook_fetch_failure_surfaces_as_failed() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/sheet-1"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let outcome = find_replace(&drive, &sheets, &opts(false), &[allow_rule("parent-1")]).await;
        assert!(matches!(outcome.result, FindReplaceResult::Failed { .. }));
    }

    // ── the gate ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn denied_target_makes_zero_sheets_calls() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        // No workbook or batchUpdate mock: either call would 404.
        let outcome = find_replace(&drive, &sheets, &opts(false), &[]).await;
        assert!(
            matches!(
                outcome.result,
                FindReplaceResult::Blocked { decided_by: None }
            ),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn a_blocked_by_rule_names_the_deciding_folder_in_the_message() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        let deny_rule = FolderPermissionRule {
            folder_id: Some("parent-1".to_string()),
            file_id: None,
            recursive: true,
            allow: HashSet::default(),
            deny: std::iter::once(DriveOperation::SheetsWrite).collect(),
            require_lease: true,
        };
        let outcome = find_replace(&drive, &sheets, &opts(false), &[deny_rule]).await;
        let text = describe_lines(&outcome).join("\n");
        assert!(
            text.contains("refused by rule on folder parent-1"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn an_edit_rule_alone_does_not_permit_find_replace() {
        // The consequence of ADR-0083 §1, asserted end-to-end: an existing
        // `allow: ["edit"]` rule must not silently grant find/replace.
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        let edit_only = FolderPermissionRule {
            folder_id: Some("parent-1".to_string()),
            file_id: None,
            recursive: true,
            allow: std::iter::once(DriveOperation::Edit).collect(),
            deny: HashSet::default(),
            require_lease: true,
        };
        let outcome = find_replace(&drive, &sheets, &opts(false), &[edit_only]).await;
        assert!(matches!(outcome.result, FindReplaceResult::Blocked { .. }));
    }

    // ── dry run ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn dry_run_reports_scope_terms_and_modifiers_and_calls_no_batch_update() {
        // Regression coverage for the ADR-0083 §6 requirement that the
        // preview name the search and replacement terms, not only the
        // boolean modifiers.
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().expect(1).mount(&server).await;
        // No batchUpdate mock: a dry run must never send it.
        let mut o = opts(true);
        o.match_case = true;
        o.include_formulas = true;
        let outcome = find_replace(&drive, &sheets, &o, &[allow_rule("parent-1")]).await;
        let FindReplaceResult::WouldChange { target, summary } = &outcome.result else {
            panic!("expected WouldChange, got {:?}", outcome.result); // omni-dev: coverage ignore-line reason="this let-else panic only runs if the match failed to bind the expected variant; this test always constructs that exact variant, so the branch never executes"
        };
        assert!(matches!(target, FindReplaceTarget::Range { .. }));
        assert!(summary.contains("\"draft\""), "{summary}");
        assert!(summary.contains("\"final\""), "{summary}");
        assert!(summary.contains("match-case=true"), "{summary}");
        assert!(summary.contains("include-formulas=true"), "{summary}");
        assert!(
            summary.contains("computes match counts when executed"),
            "{summary}"
        );
    }

    #[tokio::test]
    async fn dry_run_surfaces_the_same_blocked_reasoning_as_a_real_denied_run() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .expect(2)
            .mount(&server)
            .await;
        mount_folder("parent-1").expect(2).mount(&server).await;

        let dry = find_replace(&drive, &sheets, &opts(true), &[]).await;
        let real = find_replace(&drive, &sheets, &opts(false), &[]).await;
        assert_eq!(dry.result, real.result);
    }

    // ── successful mutations ───────────────────────────────────────────

    #[tokio::test]
    async fn allowed_find_replace_sends_a_range_scope_request_and_reports_counts() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "requests": [{"findReplace": {
                    "find": "draft", "replacement": "final",
                    "matchCase": false, "matchEntireCell": false,
                    "searchByRegex": false, "includeFormulas": false,
                }}],
            })))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "replies": [{"findReplace": {
                        "valuesChanged": 2, "formulasChanged": 0,
                        "rowsChanged": 1, "sheetsChanged": 1, "occurrencesChanged": 3,
                    }}],
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let outcome = find_replace(&drive, &sheets, &opts(false), &[allow_rule("parent-1")]).await;
        let FindReplaceResult::Changed { target, counts } = &outcome.result else {
            panic!("expected Changed, got {:?}", outcome.result); // omni-dev: coverage ignore-line reason="this let-else panic only runs if the match failed to bind the expected variant; this test always constructs that exact variant, so the branch never executes"
        };
        assert!(matches!(target, FindReplaceTarget::Range { .. }));
        let counts = counts.as_ref().expect("counts present");
        assert_eq!(counts.occurrences_changed, 3);
        assert_eq!(counts.values_changed, 2);
        let text = describe_lines(&outcome).join("\n");
        assert!(text.contains("occurrences=3"), "{text}");
    }

    #[tokio::test]
    async fn allowed_find_replace_sends_a_sheet_scope_request_for_whole_sheet() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "requests": [{"findReplace": {"sheetId": 42}}],
            })))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(&server)
            .await;

        let mut o = opts(false);
        o.range = None;
        o.sheet = Some("Q2".to_string());
        o.whole_sheet = true;
        let outcome = find_replace(&drive, &sheets, &o, &[allow_rule("parent-1")]).await;
        assert!(matches!(outcome.result, FindReplaceResult::Changed { .. }));
    }

    #[tokio::test]
    async fn allowed_find_replace_sends_an_all_sheets_scope_request() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "requests": [{"findReplace": {"allSheets": true}}],
            })))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(&server)
            .await;

        let mut o = opts(false);
        o.range = None;
        o.all_sheets = true;
        let outcome = find_replace(&drive, &sheets, &o, &[allow_rule("parent-1")]).await;
        assert!(matches!(outcome.result, FindReplaceResult::Changed { .. }));
    }

    #[tokio::test]
    async fn a_response_omitting_the_find_replace_reply_reports_changed_with_no_counts() {
        // `BatchUpdateReply::find_replace` deliberately tolerates a missing
        // reply object — a successful `batchUpdate` must still report
        // success, just with unavailable counts, rather than `Failed`.
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        mount_batch_update(serde_json::json!({"replies": [{}]}))
            .mount(&server)
            .await;

        let outcome = find_replace(&drive, &sheets, &opts(false), &[allow_rule("parent-1")]).await;
        assert_eq!(
            outcome.result,
            FindReplaceResult::Changed {
                target: FindReplaceTarget::Range {
                    range: "Q1!A1:B2".to_string(),
                    sheet_id: 0,
                },
                counts: None,
            }
        );
        let text = describe_lines(&outcome).join("\n");
        assert!(text.contains("no change counts"), "{text}");
    }

    #[tokio::test]
    async fn a_batch_update_error_is_reported_as_failed() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let outcome = find_replace(&drive, &sheets, &opts(false), &[allow_rule("parent-1")]).await;
        assert!(matches!(outcome.result, FindReplaceResult::Failed { .. }));
    }

    // ── the Drive write lease (ADR-0080 §9) ─────────────────────────────

    #[tokio::test]
    async fn refuses_without_a_lease_when_the_rule_requires_one() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        // No batchUpdate mock: a refusal must make zero mutating calls.

        let mut o = opts(false);
        o.lease_token = None;
        let outcome = find_replace(&drive, &sheets, &o, &[allow_rule("parent-1")]).await;
        assert_eq!(outcome.result, FindReplaceResult::RefusedNoLease);
    }

    #[tokio::test]
    async fn refuses_an_unknown_lease_token() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;

        let mut o = opts(false);
        o.lease_token = Some("bogus-token".to_string());
        // Never seeded — no ledger exists at this fresh path.
        o.ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        let outcome = find_replace(&drive, &sheets, &o, &[allow_rule("parent-1")]).await;
        assert_eq!(outcome.result, FindReplaceResult::RefusedLeaseExpired);
    }

    #[tokio::test]
    async fn refuses_a_lease_bound_to_a_different_file() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;

        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        let token = seed_lease(&ledger_path, "other-sheet", "1");
        let mut o = opts(false);
        o.lease_token = Some(token);
        o.ledger_path = ledger_path;
        let outcome = find_replace(&drive, &sheets, &o, &[allow_rule("parent-1")]).await;
        assert_eq!(outcome.result, FindReplaceResult::RefusedLeaseWrongFile);
    }

    #[tokio::test]
    async fn refuses_a_stale_lease() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        // Live version "1" (`mount_file`'s default) but the lease was
        // acquired at "0" — the file has moved since.
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;

        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        let token = seed_lease(&ledger_path, "sheet-1", "0");
        let mut o = opts(false);
        o.lease_token = Some(token);
        o.ledger_path = ledger_path;
        let outcome = find_replace(&drive, &sheets, &o, &[allow_rule("parent-1")]).await;
        assert_eq!(outcome.result, FindReplaceResult::RefusedLeaseStale);
    }

    #[tokio::test]
    async fn a_rule_that_does_not_require_a_lease_skips_the_check_entirely() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        mount_batch_update(serde_json::json!({}))
            .expect(1)
            .mount(&server)
            .await;

        let mut o = opts(false);
        o.lease_token = None;
        let outcome = find_replace(&drive, &sheets, &o, &[allow_rule_no_lease("parent-1")]).await;
        assert!(matches!(outcome.result, FindReplaceResult::Changed { .. }));
    }

    #[tokio::test]
    async fn a_rule_that_does_not_require_a_lease_still_refuses_a_stale_one_if_presented() {
        // ADR-0080 §13: `require_lease: false` relaxes the *requirement*,
        // not the *meaning* — a token volunteered anyway is checked exactly
        // like a required one, including staleness.
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        // No batchUpdate mock: a refusal must make zero mutating calls.

        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        let token = seed_lease(&ledger_path, "sheet-1", "0");
        let mut o = opts(false);
        o.lease_token = Some(token);
        o.ledger_path = ledger_path;
        let outcome = find_replace(&drive, &sheets, &o, &[allow_rule_no_lease("parent-1")]).await;
        assert_eq!(outcome.result, FindReplaceResult::RefusedLeaseStale);
    }

    // ── the write's own audit trail (ADR-0080 §11) ─────────────────────

    #[tokio::test]
    async fn a_leased_find_replace_concludes_its_audit_pair_with_allowed() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        mount_batch_update(serde_json::json!({}))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let audit = crate::test_support::AuditLogGuard::redirect(dir.path());

        let outcome = find_replace(&drive, &sheets, &opts(false), &[allow_rule("parent-1")]).await;
        assert!(matches!(outcome.result, FindReplaceResult::Changed { .. }));

        let records = audit.records();
        assert_eq!(audit.verdicts(), ["pending", "allowed"], "{records:?}");
        // The verb, not the engine — the same `["drive", <log_operation>]`
        // this write's `drivemutation` record carries.
        assert_eq!(records[0].command, ["drive", "sheets-find-replace"]);
    }

    // ── describing an outcome / metadata ────────────────────────────────

    /// One of every [`FindReplaceResult`] variant.
    ///
    /// The `match` below is exhaustive and wildcard-free on purpose: adding
    /// a variant breaks this build, which is what forces the new arm
    /// through `every_describe_arm_renders_a_single_line`.
    fn every_find_replace_result() -> Vec<FindReplaceResult> {
        let all = vec![
            FindReplaceResult::WouldChange {
                target: FindReplaceTarget::AllSheets,
                summary: "find \"a\", replace with \"b\"".to_string(),
            },
            FindReplaceResult::Changed {
                target: FindReplaceTarget::Range {
                    range: "A1:B2".to_string(),
                    sheet_id: 0,
                },
                counts: Some(FindReplaceCounts {
                    values_changed: 1,
                    formulas_changed: 0,
                    rows_changed: 1,
                    sheets_changed: 1,
                    occurrences_changed: 1,
                }),
            },
            FindReplaceResult::Changed {
                target: FindReplaceTarget::Sheet {
                    sheet: "Q1".to_string(),
                    sheet_id: 0,
                },
                counts: None,
            },
            FindReplaceResult::RefusedInvalidRequest {
                detail: "bad".to_string(),
            },
            FindReplaceResult::RefusedNotASpreadsheet {
                mime_type: "application/pdf".to_string(),
            },
            FindReplaceResult::RefusedShortcut,
            FindReplaceResult::RefusedNoVisibleParents,
            FindReplaceResult::RefusedSheetNotFound {
                title: "Nope".to_string(),
                available: vec!["Q1".to_string()],
            },
            FindReplaceResult::Blocked { decided_by: None },
            FindReplaceResult::Blocked {
                decided_by: Some(DecidingRule::Folder {
                    folder_id: "folder-1".to_string(),
                    depth: 2,
                }),
            },
            FindReplaceResult::Blocked {
                decided_by: Some(DecidingRule::File {
                    file_id: "sheet-1".to_string(),
                }),
            },
            FindReplaceResult::RefusedNoLease,
            FindReplaceResult::RefusedLeaseExpired,
            FindReplaceResult::RefusedLeaseWrongFile,
            FindReplaceResult::RefusedLeaseStale,
            FindReplaceResult::Failed {
                detail: "boom".to_string(),
            },
        ];
        for result in &all {
            match result {
                FindReplaceResult::WouldChange { .. }
                | FindReplaceResult::Changed { .. }
                | FindReplaceResult::RefusedInvalidRequest { .. }
                | FindReplaceResult::RefusedNotASpreadsheet { .. }
                | FindReplaceResult::RefusedShortcut
                | FindReplaceResult::RefusedNoVisibleParents
                | FindReplaceResult::RefusedSheetNotFound { .. }
                | FindReplaceResult::Blocked { .. }
                | FindReplaceResult::RefusedNoLease
                | FindReplaceResult::RefusedLeaseExpired
                | FindReplaceResult::RefusedLeaseWrongFile
                | FindReplaceResult::RefusedLeaseStale
                | FindReplaceResult::Failed { .. } => (),
            }
        }
        all
    }

    #[test]
    fn every_describe_arm_renders_a_single_line() {
        for result in every_find_replace_result() {
            let outcome = FindReplaceOutcome {
                spreadsheet_id: "sheet-1".to_string(),
                file_name: Some("Quarterly Plan".to_string()),
                resolved_folder_id: None,
                result,
            };
            let lines = describe_lines(&outcome);
            assert_eq!(
                lines.len(),
                1,
                "describe_lines emitted {} lines for {:?}: {lines:?}",
                lines.len(), // omni-dev: coverage ignore-line reason="assert_eq!'s message args are only evaluated on failure, and this test always passes"
                outcome.result
            );
            assert!(
                !lines[0].chars().any(char::is_control),
                "describe_lines emitted a control character for {:?}: {lines:?}",
                outcome.result
            );
        }
    }

    #[test]
    fn describe_lines_falls_back_to_the_spreadsheet_id_with_no_file_name() {
        let outcome = FindReplaceOutcome {
            spreadsheet_id: "sheet-1".to_string(),
            file_name: None,
            resolved_folder_id: None,
            result: FindReplaceResult::RefusedShortcut,
        };
        let lines = describe_lines(&outcome);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("'sheet-1'"), "{lines:?}");
    }

    #[test]
    fn log_status_covers_every_variant() {
        for result in every_find_replace_result() {
            assert!(!result.log_status().is_empty());
        }
        assert_eq!(
            FindReplaceResult::Blocked { decided_by: None }.log_status(),
            "blocked"
        );
        assert_eq!(
            FindReplaceResult::Changed {
                target: FindReplaceTarget::AllSheets,
                counts: None
            }
            .log_status(),
            "changed"
        );
        assert_eq!(
            FindReplaceResult::RefusedLeaseStale.log_status(),
            "refused-lease-stale"
        );
    }

    #[test]
    fn write_jsonl_emits_one_line_of_json() {
        let outcome = FindReplaceOutcome {
            spreadsheet_id: "sheet-1".to_string(),
            file_name: Some("Budget".to_string()),
            resolved_folder_id: Some("parent-1".to_string()),
            result: FindReplaceResult::Changed {
                target: FindReplaceTarget::AllSheets,
                counts: Some(FindReplaceCounts {
                    values_changed: 1,
                    formulas_changed: 0,
                    rows_changed: 1,
                    sheets_changed: 1,
                    occurrences_changed: 1,
                }),
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
    fn gate_denies_sheets_write_by_default() {
        let decision =
            write_gate::resolve(&["folder".to_string()], DriveOperation::SheetsWrite, &[]);
        assert_eq!(decision.verdict, Verdict::Deny);
    }
}
