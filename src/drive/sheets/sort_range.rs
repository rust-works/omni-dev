//! `sort-range` — reorder rows in a caller-named range (issue #1842).
//!
//! ADR-0083 §§3 and 6 place this under `SheetsWrite`: it permutes cells in
//! the named range and discards none. Preview intentionally does not read
//! values or attempt to reproduce Sheets' comparison semantics.

#![allow(missing_docs)]

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
use crate::drive::sheets::types::{BatchUpdateRequestItem, SortOrder, SortRangeRequest, SortSpec};
use crate::drive::sheets::{a1, grid_range, target_gate};
use crate::drive::types::SheetTargetRefusal;
use crate::drive::write_gate::{self, DecidingRule, DriveOperation, FolderPermissionRule};
use crate::request_log::{self, DriveMutationOutcome};

const LOG_OPERATION: &str = "sheets-sort-range";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortRangeOptions {
    pub spreadsheet_id: String,
    pub sheet: Option<String>,
    pub range: Option<String>,
    /// Ordered `COLUMN:asc|desc` flags.
    pub sort_by: Vec<String>,
    pub dry_run: bool,
    pub lease_token: Option<String>,
    pub ledger_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum SortRangeResult {
    WouldChange {
        range: String,
        sort_specs: Vec<SortSpec>,
        width_warning: Option<String>,
    },
    Changed {
        range: String,
        sort_specs: Vec<SortSpec>,
        width_warning: Option<String>,
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

impl FromLeaseRefusal for SortRangeResult {
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

impl SortRangeResult {
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

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SortRangeOutcome {
    pub spreadsheet_id: String,
    pub file_name: Option<String>,
    pub resolved_folder_id: Option<String>,
    pub result: SortRangeResult,
}

impl JsonlSerialize for SortRangeOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> anyhow::Result<()> {
        write_scalar_jsonl(self, out)
    }
}

pub async fn sort_range(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &SortRangeOptions,
    rules: &[FolderPermissionRule],
) -> SortRangeOutcome {
    let started = Instant::now();
    let outcome = sort_range_inner(drive, sheets, opts, rules).await;
    if !opts.dry_run {
        record_attempt(&outcome, started.elapsed());
    }
    outcome
}

async fn sort_range_inner(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &SortRangeOptions,
    rules: &[FolderPermissionRule],
) -> SortRangeOutcome {
    let bare = |result| SortRangeOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: None,
        resolved_folder_id: None,
        result,
    };
    let composed = match a1::compose(opts.sheet.as_deref(), opts.range.as_deref()) {
        Ok(range) => range,
        Err(err) => {
            return bare(SortRangeResult::RefusedInvalidRequest {
                detail: err.to_string(),
            })
        }
    };
    let sort_specs = match parse_sort_specs(&opts.sort_by) {
        Ok(specs) if !specs.is_empty() => specs,
        Ok(_) => {
            return bare(SortRangeResult::RefusedInvalidRequest {
                detail: "pass at least one --sort-by COLUMN:asc|desc".to_string(),
            })
        }
        Err(detail) => return bare(SortRangeResult::RefusedInvalidRequest { detail }),
    };
    let (target, decision, resolved_folder_id, requires_lease) = match target_gate::resolve(
        drive,
        &opts.spreadsheet_id,
        DriveOperation::SheetsWrite,
        rules,
    )
    .await
    {
        target_gate::TargetGateOutcome::MetadataFetchFailed { detail } => {
            return bare(SortRangeResult::Failed { detail })
        }
        target_gate::TargetGateOutcome::Refused { target, refusal } => {
            let result = match refusal {
                SheetTargetRefusal::Shortcut => SortRangeResult::RefusedShortcut,
                SheetTargetRefusal::NotASpreadsheet { mime_type } => {
                    SortRangeResult::RefusedNotASpreadsheet { mime_type }
                }
                SheetTargetRefusal::NoVisibleParents => SortRangeResult::RefusedNoVisibleParents,
            };
            return SortRangeOutcome {
                spreadsheet_id: opts.spreadsheet_id.clone(),
                file_name: Some(target.name),
                resolved_folder_id: None,
                result,
            };
        }
        target_gate::TargetGateOutcome::GateFetchFailed { target, detail } => {
            return SortRangeOutcome {
                spreadsheet_id: opts.spreadsheet_id.clone(),
                file_name: Some(target.name),
                resolved_folder_id: None,
                result: SortRangeResult::Failed { detail },
            }
        }
        target_gate::TargetGateOutcome::Gated {
            target,
            decision,
            resolved_folder_id,
            requires_lease,
        } => (target, decision, resolved_folder_id, requires_lease),
    };
    let gated = |result| SortRangeOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: Some(target.name.clone()),
        resolved_folder_id: resolved_folder_id.clone(),
        result,
    };
    if decision.verdict == write_gate::Verdict::Deny {
        return gated(SortRangeResult::Blocked {
            decided_by: decision.decided_by,
        });
    }
    let api = SheetsApi::new(sheets);
    let workbook = match api.get_spreadsheet(&opts.spreadsheet_id).await {
        Ok(book) => book,
        Err(err) => {
            return gated(SortRangeResult::Failed {
                detail: format!("{err:#}"),
            })
        }
    };
    let (_, grid) = match grid_range::resolve_grid_range(
        &workbook,
        &composed,
        |detail| SortRangeResult::RefusedInvalidRequest { detail },
        |title, available| SortRangeResult::RefusedSheetNotFound { title, available },
    ) {
        Ok(value) => value,
        Err(result) => return gated(result),
    };
    if !grid_range::is_bounded(&grid) {
        return gated(SortRangeResult::RefusedInvalidRequest {
            detail: format!(
                "'{composed}' is open-ended; sort-range needs a fully bounded range (e.g. A1:D10)"
            ),
        });
    }
    let start_column = grid.start_column_index.unwrap_or(0);
    let end_column = grid.end_column_index.unwrap_or(0);
    if let Some(spec) = sort_specs
        .iter()
        .find(|spec| spec.dimension_index < start_column || spec.dimension_index >= end_column)
    {
        return gated(SortRangeResult::RefusedInvalidRequest {
            detail: format!(
                "sort column {} is outside the selected range's columns {}..{} (zero-based, end exclusive)",
                spec.dimension_index, start_column, end_column
            ),
        });
    }
    let allocated_columns = grid_range::find_sheet_by_id(&workbook, grid.sheet_id)
        .and_then(|sheet| sheet.properties.as_ref())
        .and_then(|props| props.grid_properties.as_ref())
        .and_then(|props| props.column_count);
    let width_warning = width_warning(start_column, end_column, allocated_columns);
    let request = BatchUpdateRequestItem::SortRange(SortRangeRequest {
        range: grid,
        sort_specs: sort_specs.clone(),
    });
    if opts.dry_run {
        return gated(SortRangeResult::WouldChange {
            range: composed,
            sort_specs,
            width_warning,
        });
    }
    let leased = LeasedWrite {
        log_prefix: "drive sheets sort-range",
        operation: LOG_OPERATION,
        ledger_path: &opts.ledger_path,
        file_id: &opts.spreadsheet_id,
    };
    let files_api = FilesApi::new(drive);
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
        Ok(_) => SortRangeResult::Changed {
            range: composed,
            sort_specs,
            width_warning,
        },
        Err(err) => SortRangeResult::Failed {
            detail: format!("{err:#}"),
        },
    };
    gated(result)
}

fn width_warning(start: i64, end: i64, allocated: Option<i64>) -> Option<String> {
    match allocated {
        Some(columns) if start == 0 && end >= columns => None,
        Some(columns) => Some(format!(
            "selected columns {start}..{end} of the sheet's {columns} allocated columns; if data exists outside the selection, sorting can separate records from their other columns"
        )),
        None => Some(
            "the selected range may be narrower than its rows; sorting can separate records from columns outside the selection".to_string(),
        ),
    }
}

fn parse_sort_specs(flags: &[String]) -> Result<Vec<SortSpec>, String> {
    flags
        .iter()
        .map(|flag| {
            let (column, order) = flag
                .split_once(':')
                .ok_or_else(|| format!("'{flag}' is not COLUMN:asc|desc"))?;
            let dimension_index = column
                .trim()
                .parse()
                .map_err(|_| format!("'{column}' in '{flag}' is not a column index"))?;
            if dimension_index < 0 {
                return Err(format!(
                    "'{column}' in '{flag}' must be a non-negative column index"
                ));
            }
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

fn record_attempt(outcome: &SortRangeOutcome, duration: Duration) {
    let decided_by = match &outcome.result {
        SortRangeResult::Blocked { decided_by } => decided_by.as_ref(),
        _ => None,
    };
    let decided_by = write_gate::decided_by_log_fields(decided_by);
    let fields_changed = match &outcome.result {
        SortRangeResult::Changed {
            range, sort_specs, ..
        } => Some(format!("sorted {range} by {}", render_specs(sort_specs))),
        _ => None,
    };
    let error = match &outcome.result {
        SortRangeResult::RefusedInvalidRequest { detail } | SortRangeResult::Failed { detail } => {
            Some(detail.clone())
        }
        _ => None,
    };
    request_log::record_drive_mutation(DriveMutationOutcome {
        operation: LOG_OPERATION,
        file_id: outcome.spreadsheet_id.clone(),
        file_name: outcome.file_name.clone().unwrap_or_default(),
        status: outcome.result.log_status().to_string(),
        resolved_folder_id: outcome.resolved_folder_id.clone(),
        decided_by_folder_id: decided_by.folder_id,
        decided_by_depth: decided_by.depth,
        decided_by_file_id: decided_by.file_id,
        fields_changed,
        error,
        duration,
        ..Default::default()
    });
}

pub fn describe_lines(outcome: &SortRangeOutcome) -> Vec<String> {
    let book = outcome.file_name.as_deref().map_or_else(
        || format!("'{}'", outcome.spreadsheet_id),
        |name| format!("'{name}'"),
    );
    match &outcome.result {
        SortRangeResult::WouldChange {
            range,
            sort_specs,
            width_warning,
        } => change_lines(
            format!(
                "Would sort {range} in {book} by {}",
                render_specs(sort_specs)
            ),
            width_warning.as_deref(),
            false,
        ),
        SortRangeResult::Changed {
            range,
            sort_specs,
            width_warning,
        } => change_lines(
            format!(
                "Applied sort of {range} in {book} by {}",
                render_specs(sort_specs)
            ),
            width_warning.as_deref(),
            true,
        ),
        SortRangeResult::RefusedInvalidRequest { detail } => vec![format!("Refused: {detail}")],
        SortRangeResult::RefusedNotASpreadsheet { mime_type } => vec![format!(
            "Refused: {book} is not a Google Sheet (mimeType: {mime_type})"
        )],
        SortRangeResult::RefusedShortcut => vec![format!(
            "Refused: {book} is a shortcut; sort-range doesn't follow shortcuts"
        )],
        SortRangeResult::RefusedNoVisibleParents => {
            vec![format!("Refused: {book} has no visible parent folder")]
        }
        SortRangeResult::RefusedSheetNotFound { title, available } => vec![format!(
            "Refused: {book} has no sheet titled '{title}'. Available: {}",
            available.join(", ")
        )],
        SortRangeResult::Blocked { .. } => vec![format!(
            "Blocked: sort-range on {book} requires an allowing sheets-write rule"
        )],
        SortRangeResult::RefusedNoLease => LeaseGateRefusal::NoLease
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        SortRangeResult::RefusedLeaseExpired => LeaseGateRefusal::Expired
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        SortRangeResult::RefusedLeaseWrongFile => LeaseGateRefusal::WrongFile
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        SortRangeResult::RefusedLeaseStale => LeaseGateRefusal::Stale
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        SortRangeResult::Failed { detail } => vec![format!("Failed: {detail}")],
    }
}

fn change_lines(summary: String, width_warning: Option<&str>, changed: bool) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(warning) = width_warning {
        lines.push(format!("Warning: {warning}"));
    }
    lines.push(summary);
    lines.push(if changed {
        "  references outside the range may now observe values from a different row".to_string()
    } else {
        "  references outside the range may observe values from a different row after sorting"
            .to_string()
    });
    lines
}

fn render_specs(specs: &[SortSpec]) -> String {
    specs
        .iter()
        .map(|spec| {
            format!(
                "{} {}",
                spec.dimension_index,
                match spec.sort_order {
                    SortOrder::Ascending => "asc",
                    SortOrder::Descending => "desc",
                }
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::sheets::client::SHEETS_API_URL;
    use crate::drive::test_support::seed_lease;
    use crate::drive::types::GOOGLE_SHEET_MIME_TYPE;
    use crate::test_support::env::MapEnv;
    use crate::utils::secret::Secret;
    use std::collections::HashSet;

    async fn clients(server: &wiremock::MockServer) -> (DriveClient, SheetsClient) {
        let credentials = DriveCredentials {
            client_id: "client-1".into(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: DriveGrantedScopes::READONLY,
        };
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/token"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"access_token": "test-token", "expires_in": 3600}),
            ))
            .mount(server)
            .await;
        let mut drive = DriveClient::new(&server.uri(), &credentials).unwrap();
        crate::drive::client::test_support::replace_session(
            &mut drive,
            &credentials,
            &format!("{}/token", server.uri()),
        );
        let env = MapEnv::new().with(SHEETS_API_URL, &server.uri());
        let sheets = SheetsClient::from_drive_client_with(&env, &drive).unwrap();
        (drive, sheets)
    }

    async fn mount_metadata(server: &wiremock::MockServer) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/sheet-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "sheet-1", "name": "Budget", "mimeType": GOOGLE_SHEET_MIME_TYPE,
                    "parents": ["parent-1"], "version": "1"
                })),
            )
            .mount(server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/parent-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "parent-1", "name": "parent-1",
                    "mimeType": "application/vnd.google-apps.folder", "parents": []
                })),
            )
            .mount(server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/sheet-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "spreadsheetId": "sheet-1",
                    "sheets": [{"properties": {
                        "sheetId": 0, "title": "Q1",
                        "gridProperties": {"rowCount": 100, "columnCount": 6}
                    }}]
                })),
            )
            .mount(server)
            .await;
    }

    /// `version: "1"` throughout — matches [`options`]'s default (no seeded
    /// lease).
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

    /// A `spreadsheets.get` reply for `sheet-1` with a single `Q1` sheet
    /// (sheetId 0) whose `gridProperties` omits `columnCount` — so
    /// [`width_warning`] can never learn the sheet's allocated width.
    fn mount_workbook_no_column_count() -> wiremock::Mock {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/sheet-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "spreadsheetId": "sheet-1",
                    "sheets": [{"properties": {
                        "sheetId": 0, "title": "Q1", "gridProperties": {"rowCount": 100}
                    }}]
                })),
            )
    }

    fn options(dry_run: bool) -> SortRangeOptions {
        SortRangeOptions {
            spreadsheet_id: "sheet-1".into(),
            sheet: Some("Q1".into()),
            range: Some("A2:C10".into()),
            sort_by: vec!["2:desc".into(), "0:asc".into()],
            dry_run,
            lease_token: None,
            ledger_path: PathBuf::from("/tmp/unused-sort-range-ledger"),
        }
    }

    fn rule() -> FolderPermissionRule {
        FolderPermissionRule {
            folder_id: Some("parent-1".into()),
            file_id: None,
            recursive: true,
            allow: std::iter::once(DriveOperation::SheetsWrite).collect(),
            deny: HashSet::default(),
            require_lease: false,
        }
    }

    fn rule_requiring_lease() -> FolderPermissionRule {
        FolderPermissionRule {
            require_lease: true,
            ..rule()
        }
    }

    #[test]
    fn parses_ordered_sort_specs() {
        let specs = parse_sort_specs(&["2:desc".into(), "0:ascending".into()]).unwrap();
        assert_eq!(specs[0].dimension_index, 2);
        assert_eq!(specs[0].sort_order, SortOrder::Descending);
        assert_eq!(specs[1].dimension_index, 0);
    }

    #[test]
    fn preview_never_claims_to_predict_sort_order() {
        let outcome = SortRangeOutcome {
            spreadsheet_id: "id".into(),
            file_name: Some("Book".into()),
            resolved_folder_id: None,
            result: SortRangeResult::WouldChange {
                range: "Q1!A1:B3".into(),
                sort_specs: vec![SortSpec {
                    dimension_index: 0,
                    sort_order: SortOrder::Ascending,
                }],
                width_warning: width_warning(0, 2, Some(5)),
            },
        };
        let lines = describe_lines(&outcome).join("\n");
        assert!(lines.contains("separate records"));
        assert!(lines.contains("references outside"));
    }

    #[tokio::test]
    async fn dry_run_uses_metadata_without_reading_values_or_mutating() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        let outcome = sort_range(&drive, &sheets, &options(true), &[rule()]).await;
        let SortRangeResult::WouldChange { width_warning, .. } = outcome.result else {
            panic!("expected would-change"); // omni-dev: coverage ignore-line reason="this let-else panic only runs if the match failed to bind the expected variant; this test always constructs that exact variant, so the branch never executes"
        };
        assert!(width_warning
            .unwrap()
            .contains("3 of the sheet's 6 allocated columns"));
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 4); // OAuth refresh plus three metadata reads.
        assert!(requests
            .iter()
            .all(|request| { request.method.as_str() == "GET" || request.url.path() == "/token" }));
    }

    #[tokio::test]
    async fn real_run_sends_one_ordered_sort_range_request() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"replies": [{}]})),
            )
            .mount(&server)
            .await;
        let outcome = sort_range(&drive, &sheets, &options(false), &[rule()]).await;
        assert!(
            matches!(outcome.result, SortRangeResult::Changed { .. }),
            "{outcome:?}"
        );
        let requests = server.received_requests().await.unwrap();
        let batch = requests
            .iter()
            .find(|request| request.url.path().ends_with(":batchUpdate"))
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&batch.body).unwrap();
        assert_eq!(
            body["requests"][0]["sortRange"]["range"],
            serde_json::json!({
                "sheetId": 0, "startRowIndex": 1, "endRowIndex": 10,
                "startColumnIndex": 0, "endColumnIndex": 3
            })
        );
        assert_eq!(
            body["requests"][0]["sortRange"]["sortSpecs"],
            serde_json::json!([
                {"dimensionIndex": 2, "sortOrder": "DESCENDING"},
                {"dimensionIndex": 0, "sortOrder": "ASCENDING"}
            ])
        );
    }

    #[tokio::test]
    async fn rejects_sort_column_outside_the_range_before_mutating() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        let mut opts = options(false);
        opts.sort_by = vec!["3:asc".into()];
        let outcome = sort_range(&drive, &sheets, &opts, &[rule()]).await;
        assert!(matches!(
            outcome.result,
            SortRangeResult::RefusedInvalidRequest { .. }
        ));
        assert!(server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| { request.method.as_str() == "GET" || request.url.path() == "/token" }));
    }

    // ── refusals before any network call ────────────────────────────────

    #[tokio::test]
    async fn rejects_a_malformed_sort_by_flag_before_any_network_call() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        // No metadata mocks: a malformed flag must short-circuit first.
        let mut opts = options(false);
        opts.sort_by = vec!["no-colon-here".into()];
        let outcome = sort_range(&drive, &sheets, &opts, &[rule()]).await;
        let SortRangeResult::RefusedInvalidRequest { detail } = &outcome.result else {
            panic!("expected RefusedInvalidRequest, got {:?}", outcome.result); // omni-dev: coverage ignore-line reason="this let-else panic only runs if the match failed to bind the expected variant; this test always constructs that exact variant, so the branch never executes"
        };
        assert!(detail.contains("COLUMN:asc|desc"), "{detail}");
        assert!(outcome.file_name.is_none());
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn rejects_an_empty_sort_by_list_before_any_network_call() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        let mut opts = options(false);
        opts.sort_by = vec![];
        let outcome = sort_range(&drive, &sheets, &opts, &[rule()]).await;
        let SortRangeResult::RefusedInvalidRequest { detail } = &outcome.result else {
            panic!("expected RefusedInvalidRequest, got {:?}", outcome.result); // omni-dev: coverage ignore-line reason="this let-else panic only runs if the match failed to bind the expected variant; this test always constructs that exact variant, so the branch never executes"
        };
        assert!(detail.contains("pass at least one --sort-by"), "{detail}");
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn rejects_a_negative_sort_column_index_before_any_network_call() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        let mut opts = options(false);
        opts.sort_by = vec!["-1:asc".into()];
        let outcome = sort_range(&drive, &sheets, &opts, &[rule()]).await;
        let SortRangeResult::RefusedInvalidRequest { detail } = &outcome.result else {
            panic!("expected RefusedInvalidRequest, got {:?}", outcome.result); // omni-dev: coverage ignore-line reason="this let-else panic only runs if the match failed to bind the expected variant; this test always constructs that exact variant, so the branch never executes"
        };
        assert!(detail.contains("non-negative"), "{detail}");
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn rejects_an_invalid_sort_order_word_before_any_network_call() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        let mut opts = options(false);
        opts.sort_by = vec!["0:sideways".into()];
        let outcome = sort_range(&drive, &sheets, &opts, &[rule()]).await;
        let SortRangeResult::RefusedInvalidRequest { detail } = &outcome.result else {
            panic!("expected RefusedInvalidRequest, got {:?}", outcome.result); // omni-dev: coverage ignore-line reason="this let-else panic only runs if the match failed to bind the expected variant; this test always constructs that exact variant, so the branch never executes"
        };
        assert!(detail.contains("'asc' or 'desc'"), "{detail}");
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn rejects_a_range_with_conflicting_sheet_prefix_before_any_network_call() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        let mut opts = options(false);
        opts.sheet = Some("Other".into());
        opts.range = Some("Sheet1!A1:B2".into());
        let outcome = sort_range(&drive, &sheets, &opts, &[rule()]).await;
        let SortRangeResult::RefusedInvalidRequest { detail } = &outcome.result else {
            panic!("expected RefusedInvalidRequest, got {:?}", outcome.result); // omni-dev: coverage ignore-line reason="this let-else panic only runs if the match failed to bind the expected variant; this test always constructs that exact variant, so the branch never executes"
        };
        assert!(detail.contains("already names a sheet"), "{detail}");
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    // ── refusals surfaced by the target gate ────────────────────────────

    #[tokio::test]
    async fn a_metadata_fetch_failure_surfaces_as_failed() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/sheet-1"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;
        let outcome = sort_range(&drive, &sheets, &options(false), &[rule()]).await;
        assert!(matches!(outcome.result, SortRangeResult::Failed { .. }));
    }

    #[tokio::test]
    async fn non_spreadsheet_is_refused_before_any_gate_or_sheets_call() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", "application/pdf", &["parent-1"])
            .mount(&server)
            .await;
        // Deliberately no mock for parent-1 (the gate never runs) and none
        // for any Sheets endpoint.
        let outcome = sort_range(&drive, &sheets, &options(false), &[rule()]).await;
        assert!(matches!(
            outcome.result,
            SortRangeResult::RefusedNotASpreadsheet { .. }
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
        let outcome = sort_range(&drive, &sheets, &options(false), &[rule()]).await;
        assert!(matches!(outcome.result, SortRangeResult::RefusedShortcut));
        let text = describe_lines(&outcome).join("\n");
        assert!(text.contains("is a shortcut"), "{text}");
    }

    #[tokio::test]
    async fn a_sheet_with_no_visible_parents_is_refused_distinctly_from_a_blocked_one() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &[])
            .mount(&server)
            .await;
        let outcome = sort_range(&drive, &sheets, &options(false), &[]).await;
        assert!(matches!(
            outcome.result,
            SortRangeResult::RefusedNoVisibleParents
        ));
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
        let outcome = sort_range(&drive, &sheets, &options(false), &[rule()]).await;
        assert!(matches!(outcome.result, SortRangeResult::Failed { .. }));
    }

    // ── the gate itself ───────────────────────────────────────────────────

    #[tokio::test]
    async fn denied_target_is_blocked_and_records_the_attempt() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        // No workbook or batchUpdate mock: either call would 404.
        let outcome = sort_range(&drive, &sheets, &options(false), &[]).await;
        assert!(
            matches!(
                outcome.result,
                SortRangeResult::Blocked { decided_by: None }
            ),
            "{:?}",
            outcome.result
        );
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
        let outcome = sort_range(&drive, &sheets, &options(false), &[rule()]).await;
        assert!(matches!(outcome.result, SortRangeResult::Failed { .. }));
    }

    // ── the range itself, once the workbook is in hand ───────────────────

    #[tokio::test]
    async fn a_range_without_a_sheet_prefix_is_refused_as_invalid() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        let mut opts = options(false);
        opts.sheet = None;
        opts.range = Some("A2:C10".into()); // no `Sheet!` prefix, and no `--sheet` either.
        let outcome = sort_range(&drive, &sheets, &opts, &[rule()]).await;
        let SortRangeResult::RefusedInvalidRequest { detail } = &outcome.result else {
            panic!("expected RefusedInvalidRequest, got {:?}", outcome.result); // omni-dev: coverage ignore-line reason="this let-else panic only runs if the match failed to bind the expected variant; this test always constructs that exact variant, so the branch never executes"
        };
        assert!(detail.contains("does not name a sheet"), "{detail}");
        assert!(server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| { request.method.as_str() == "GET" || request.url.path() == "/token" }));
    }

    #[tokio::test]
    async fn a_range_naming_an_unknown_sheet_is_refused_with_available_titles() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        let mut opts = options(false);
        opts.sheet = None;
        opts.range = Some("Nope!A2:C10".into());
        let outcome = sort_range(&drive, &sheets, &opts, &[rule()]).await;
        let SortRangeResult::RefusedSheetNotFound { title, available } = &outcome.result else {
            panic!("expected RefusedSheetNotFound, got {:?}", outcome.result); // omni-dev: coverage ignore-line reason="this let-else panic only runs if the match failed to bind the expected variant; this test always constructs that exact variant, so the branch never executes"
        };
        assert_eq!(title, "Nope");
        assert_eq!(available, &["Q1".to_string()]);
    }

    #[tokio::test]
    async fn an_open_ended_range_is_refused_as_invalid() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        let mut opts = options(false);
        opts.sheet = None;
        opts.range = Some("Q1!A:A".into());
        let outcome = sort_range(&drive, &sheets, &opts, &[rule()]).await;
        let SortRangeResult::RefusedInvalidRequest { detail } = &outcome.result else {
            panic!("expected RefusedInvalidRequest, got {:?}", outcome.result); // omni-dev: coverage ignore-line reason="this let-else panic only runs if the match failed to bind the expected variant; this test always constructs that exact variant, so the branch never executes"
        };
        assert!(detail.contains("open-ended"), "{detail}");
        assert!(server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| { request.method.as_str() == "GET" || request.url.path() == "/token" }));
    }

    #[tokio::test]
    async fn a_sheet_with_no_recorded_column_count_gets_the_generic_width_warning() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook_no_column_count().mount(&server).await;
        let outcome = sort_range(&drive, &sheets, &options(true), &[rule()]).await;
        let SortRangeResult::WouldChange { width_warning, .. } = outcome.result else {
            panic!("expected would-change"); // omni-dev: coverage ignore-line reason="this let-else panic only runs if the match failed to bind the expected variant; this test always constructs that exact variant, so the branch never executes"
        };
        assert!(width_warning.unwrap().contains("may be narrower"));
    }

    #[tokio::test]
    async fn a_batch_update_error_is_reported_as_failed() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let outcome = sort_range(&drive, &sheets, &options(false), &[rule()]).await;
        assert!(matches!(outcome.result, SortRangeResult::Failed { .. }));
    }

    #[tokio::test]
    async fn a_batch_update_error_under_an_active_lease_records_the_failure() {
        // Unlike `a_batch_update_error_is_reported_as_failed` (no lease held
        // at all), this holds a real lease grant so the failure path also
        // exercises `conclude_native_leased_write`'s own error recording.
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let ledger_path = tempfile::tempdir().unwrap().keep().join("ledger.jsonl");
        let token = seed_lease(&ledger_path, "sheet-1", "1");
        let mut opts = options(false);
        opts.lease_token = Some(token);
        opts.ledger_path = ledger_path;
        let outcome = sort_range(&drive, &sheets, &opts, &[rule_requiring_lease()]).await;
        assert!(matches!(outcome.result, SortRangeResult::Failed { .. }));
    }

    // ── the Drive write lease (ADR-0080 §9) ─────────────────────────────

    #[tokio::test]
    async fn refuses_without_a_lease_when_the_rule_requires_one() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        // No batchUpdate mock: a refusal must make zero mutating calls.
        let outcome = sort_range(&drive, &sheets, &options(false), &[rule_requiring_lease()]).await;
        assert_eq!(outcome.result, SortRangeResult::RefusedNoLease);
    }

    #[tokio::test]
    async fn refuses_an_unknown_lease_token() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        let mut opts = options(false);
        opts.lease_token = Some("bogus-token".into());
        // Never seeded — no ledger exists at this fresh path.
        opts.ledger_path = tempfile::tempdir().unwrap().keep().join("ledger.jsonl");
        let outcome = sort_range(&drive, &sheets, &opts, &[rule_requiring_lease()]).await;
        assert_eq!(outcome.result, SortRangeResult::RefusedLeaseExpired);
    }

    #[tokio::test]
    async fn refuses_a_lease_bound_to_a_different_file() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        let ledger_path = tempfile::tempdir().unwrap().keep().join("ledger.jsonl");
        let token = seed_lease(&ledger_path, "other-sheet", "1");
        let mut opts = options(false);
        opts.lease_token = Some(token);
        opts.ledger_path = ledger_path;
        let outcome = sort_range(&drive, &sheets, &opts, &[rule_requiring_lease()]).await;
        assert_eq!(outcome.result, SortRangeResult::RefusedLeaseWrongFile);
    }

    #[tokio::test]
    async fn refuses_a_stale_lease() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        // Live version "1" (`mount_metadata`'s default) but the lease was
        // acquired at "0" — the file has moved since.
        mount_metadata(&server).await;
        let ledger_path = tempfile::tempdir().unwrap().keep().join("ledger.jsonl");
        let token = seed_lease(&ledger_path, "sheet-1", "0");
        let mut opts = options(false);
        opts.lease_token = Some(token);
        opts.ledger_path = ledger_path;
        let outcome = sort_range(&drive, &sheets, &opts, &[rule_requiring_lease()]).await;
        assert_eq!(outcome.result, SortRangeResult::RefusedLeaseStale);
    }

    // ── pure unit coverage: lease-refusal mapping, JSONL, describe/log ──

    #[test]
    fn lease_refusals_map_to_their_sort_range_results() {
        assert_eq!(
            SortRangeResult::from_no_lease(),
            SortRangeResult::RefusedNoLease
        );
        assert_eq!(
            SortRangeResult::from_lease_expired(),
            SortRangeResult::RefusedLeaseExpired
        );
        assert_eq!(
            SortRangeResult::from_lease_wrong_file(),
            SortRangeResult::RefusedLeaseWrongFile
        );
        assert_eq!(
            SortRangeResult::from_lease_stale(),
            SortRangeResult::RefusedLeaseStale
        );
        assert_eq!(
            SortRangeResult::from_lease_failed("ledger unavailable".into()),
            SortRangeResult::Failed {
                detail: "ledger unavailable".into()
            }
        );
    }

    #[test]
    fn write_jsonl_emits_one_line_of_json() {
        let outcome = SortRangeOutcome {
            spreadsheet_id: "sheet-1".into(),
            file_name: Some("Budget".into()),
            resolved_folder_id: Some("parent-1".into()),
            result: SortRangeResult::Changed {
                range: "Q1!A2:C10".into(),
                sort_specs: vec![SortSpec {
                    dimension_index: 0,
                    sort_order: SortOrder::Ascending,
                }],
                width_warning: None,
            },
        };
        let mut buf = Vec::new();
        outcome.write_jsonl(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert_eq!(text.matches('\n').count(), 1);
        let parsed: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(parsed["result"]["status"], "changed");
    }

    /// One of every [`SortRangeResult`] variant.
    ///
    /// The `match` below is exhaustive and wildcard-free on purpose: adding
    /// a variant breaks this build, which is what forces the new arm
    /// through the tests below.
    fn every_sort_range_result() -> Vec<SortRangeResult> {
        let all = vec![
            SortRangeResult::WouldChange {
                range: "Q1!A1:B3".into(),
                sort_specs: vec![SortSpec {
                    dimension_index: 0,
                    sort_order: SortOrder::Ascending,
                }],
                width_warning: None,
            },
            SortRangeResult::Changed {
                range: "Q1!A1:B3".into(),
                sort_specs: vec![SortSpec {
                    dimension_index: 0,
                    sort_order: SortOrder::Descending,
                }],
                width_warning: Some("narrow".to_string()),
            },
            SortRangeResult::RefusedInvalidRequest {
                detail: "bad".into(),
            },
            SortRangeResult::RefusedNotASpreadsheet {
                mime_type: "application/pdf".into(),
            },
            SortRangeResult::RefusedShortcut,
            SortRangeResult::RefusedNoVisibleParents,
            SortRangeResult::RefusedSheetNotFound {
                title: "Nope".into(),
                available: vec!["Q1".into()],
            },
            SortRangeResult::Blocked { decided_by: None },
            SortRangeResult::Blocked {
                decided_by: Some(DecidingRule::Folder {
                    folder_id: "folder-1".into(),
                    depth: 2,
                }),
            },
            SortRangeResult::Blocked {
                decided_by: Some(DecidingRule::File {
                    file_id: "sheet-1".into(),
                }),
            },
            SortRangeResult::RefusedNoLease,
            SortRangeResult::RefusedLeaseExpired,
            SortRangeResult::RefusedLeaseWrongFile,
            SortRangeResult::RefusedLeaseStale,
            SortRangeResult::Failed {
                detail: "boom".into(),
            },
        ];
        for result in &all {
            match result {
                SortRangeResult::WouldChange { .. }
                | SortRangeResult::Changed { .. }
                | SortRangeResult::RefusedInvalidRequest { .. }
                | SortRangeResult::RefusedNotASpreadsheet { .. }
                | SortRangeResult::RefusedShortcut
                | SortRangeResult::RefusedNoVisibleParents
                | SortRangeResult::RefusedSheetNotFound { .. }
                | SortRangeResult::Blocked { .. }
                | SortRangeResult::RefusedNoLease
                | SortRangeResult::RefusedLeaseExpired
                | SortRangeResult::RefusedLeaseWrongFile
                | SortRangeResult::RefusedLeaseStale
                | SortRangeResult::Failed { .. } => (),
            }
        }
        all
    }

    #[test]
    fn every_describe_arm_renders_at_least_one_line_with_no_control_characters() {
        for result in every_sort_range_result() {
            let outcome = SortRangeOutcome {
                spreadsheet_id: "sheet-1".into(),
                file_name: Some("Quarterly Plan".into()),
                resolved_folder_id: None,
                result,
            };
            let lines = describe_lines(&outcome);
            assert!(
                !lines.is_empty(),
                "describe_lines emitted no lines for {:?}",
                outcome.result
            );
            for line in &lines {
                assert!(
                    !line.chars().any(char::is_control),
                    "describe_lines emitted a control character for {:?}: {lines:?}",
                    outcome.result
                );
            }
        }
    }

    #[test]
    fn describe_lines_falls_back_to_the_spreadsheet_id_with_no_file_name() {
        let outcome = SortRangeOutcome {
            spreadsheet_id: "sheet-1".into(),
            file_name: None,
            resolved_folder_id: None,
            result: SortRangeResult::RefusedShortcut,
        };
        let lines = describe_lines(&outcome);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("'sheet-1'"), "{lines:?}");
    }

    #[test]
    fn log_status_covers_every_variant() {
        for result in every_sort_range_result() {
            assert!(!result.log_status().is_empty());
        }
        assert_eq!(
            SortRangeResult::Blocked { decided_by: None }.log_status(),
            "blocked"
        );
        assert_eq!(
            SortRangeResult::Changed {
                range: "Q1!A1:B2".into(),
                sort_specs: vec![],
                width_warning: None
            }
            .log_status(),
            "changed"
        );
        assert_eq!(
            SortRangeResult::RefusedLeaseStale.log_status(),
            "refused-lease-stale"
        );
    }
}
