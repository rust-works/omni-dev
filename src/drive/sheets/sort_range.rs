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
            panic!("expected would-change");
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
}
