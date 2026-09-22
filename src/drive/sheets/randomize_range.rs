//! `randomize-range` — shuffles the row order within a bounded range into
//! an unpredictable, server-chosen order (issue #1845).
//!
//! Split out of #1830, unblocked by #1831's [ADR-0083](../../../docs/adrs/adr-0083.md).
//! `sort_range.rs`'s near-exact template, minus the sort-key machinery.
//!
//! ADR-0083 §3 initially placed `randomizeRange` under `SheetsWrite` alone,
//! on a values-only reading — a permutation of the range's own cells is a
//! strict special case of what a `sheets-write` grant already permits via
//! `clear`+`write`. §5 made that provisional on live verification: whether
//! the server carries a row's formatting, notes and data-validation rules
//! along with it when reordering, and what happens to an in-range formula.
//!
//! Live-verified 2026-09-22 against a probe sheet in `omni-dev-test`
//! (issue #1845's plan comments): **formatting, notes and data-validation
//! rules move with the row**, and **an in-range formula moves with its row
//! with its relative references rewritten** to keep pointing at its own
//! row. Formatting is `SheetsStructure`'s own subject matter, so §5's
//! fixed consequence applies and the gate is the union of `SheetsWrite`
//! and `SheetsStructure` — [`target_gate::resolve_all`], the same shape
//! `text_to_columns.rs` took. The same live run also confirmed §3's
//! record-decoupling caveat: cells in the same rows but outside the
//! selected columns do not move, so a range narrower than its rows can
//! silently detach a record's other columns.
//!
//! **The resulting order can never be previewed, and this crate never sees
//! it even after a real run.** `randomizeRange` carries no response
//! object, and which order the server settles on is entirely its own
//! choice — `--dry-run` (and the real run) instead report the range being
//! reordered, leading with the range-width-vs-sheet-width record-integrity
//! caveat before the smaller "references outside the range may see a
//! different row's value" caveat, and stating plainly that the resulting
//! order is not knowable — never a cell's contents (ADR-0083 §6).

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
use crate::drive::sheets::grid_range::width_warning;
use crate::drive::sheets::types::{BatchUpdateRequestItem, RandomizeRangeRequest};
use crate::drive::sheets::{a1, grid_range, target_gate};
use crate::drive::types::SheetTargetRefusal;
use crate::drive::write_gate::{self, DecidingRule, DriveOperation, FolderPermissionRule};
use crate::request_log::{self, DriveMutationOutcome};

const LOG_OPERATION: &str = "sheets-randomize-range";

/// The operations this verb's gate is the union of, in the order a
/// refusal reports them. `SheetsWrite` for the cell values a reorder
/// permutes, `SheetsStructure` for the formatting, notes and
/// data-validation rules live verification found travelling with each
/// row — see the module docs.
const GATE_OPERATIONS: &[DriveOperation] =
    &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RandomizeRangeOptions {
    pub spreadsheet_id: String,
    pub sheet: Option<String>,
    pub range: Option<String>,
    pub dry_run: bool,
    pub lease_token: Option<String>,
    pub ledger_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum RandomizeRangeResult {
    WouldChange {
        range: String,
        width_warning: Option<String>,
    },
    Changed {
        range: String,
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
        /// Which of [`GATE_OPERATIONS`] denied first — the union gate
        /// refuses as soon as one of the two does, and which one it was
        /// is the only actionable part of the message.
        operation: DriveOperation,
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

impl FromLeaseRefusal for RandomizeRangeResult {
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

impl RandomizeRangeResult {
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
pub struct RandomizeRangeOutcome {
    pub spreadsheet_id: String,
    pub file_name: Option<String>,
    pub resolved_folder_id: Option<String>,
    pub result: RandomizeRangeResult,
}

impl JsonlSerialize for RandomizeRangeOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> anyhow::Result<()> {
        write_scalar_jsonl(self, out)
    }
}

pub async fn randomize_range(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &RandomizeRangeOptions,
    rules: &[FolderPermissionRule],
) -> RandomizeRangeOutcome {
    let started = Instant::now();
    let outcome = randomize_range_inner(drive, sheets, opts, rules).await;
    if !opts.dry_run {
        record_attempt(&outcome, started.elapsed());
    }
    outcome
}

async fn randomize_range_inner(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &RandomizeRangeOptions,
    rules: &[FolderPermissionRule],
) -> RandomizeRangeOutcome {
    let bare = |result| RandomizeRangeOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: None,
        resolved_folder_id: None,
        result,
    };
    let composed = match a1::compose(opts.sheet.as_deref(), opts.range.as_deref()) {
        Ok(range) => range,
        Err(err) => {
            return bare(RandomizeRangeResult::RefusedInvalidRequest {
                detail: err.to_string(),
            })
        }
    };

    let (target, verdict, denied, resolved_folder_id, requires_lease) =
        match target_gate::resolve_all(drive, &opts.spreadsheet_id, GATE_OPERATIONS, rules).await {
            target_gate::TargetGateUnionOutcome::MetadataFetchFailed { detail } => {
                return bare(RandomizeRangeResult::Failed { detail })
            }
            target_gate::TargetGateUnionOutcome::Refused { target, refusal } => {
                let result = match refusal {
                    SheetTargetRefusal::Shortcut => RandomizeRangeResult::RefusedShortcut,
                    SheetTargetRefusal::NotASpreadsheet { mime_type } => {
                        RandomizeRangeResult::RefusedNotASpreadsheet { mime_type }
                    }
                    SheetTargetRefusal::NoVisibleParents => {
                        RandomizeRangeResult::RefusedNoVisibleParents
                    }
                };
                return RandomizeRangeOutcome {
                    spreadsheet_id: opts.spreadsheet_id.clone(),
                    file_name: Some(target.name),
                    resolved_folder_id: None,
                    result,
                };
            }
            target_gate::TargetGateUnionOutcome::GateFetchFailed { target, detail } => {
                return RandomizeRangeOutcome {
                    spreadsheet_id: opts.spreadsheet_id.clone(),
                    file_name: Some(target.name),
                    resolved_folder_id: None,
                    result: RandomizeRangeResult::Failed { detail },
                }
            }
            target_gate::TargetGateUnionOutcome::Gated {
                target,
                verdict,
                denied,
                resolved_folder_id,
                requires_lease,
            } => (target, verdict, denied, resolved_folder_id, requires_lease),
        };
    let gated = |result| RandomizeRangeOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: Some(target.name.clone()),
        resolved_folder_id: resolved_folder_id.clone(),
        result,
    };
    if verdict == write_gate::Verdict::Deny {
        // `denied` is `Some` whenever the verdict is `Deny`; the fallback
        // names the first operation rather than inventing one, matching
        // `text_to_columns.rs`'s own arm.
        let (operation, decided_by) = denied.unwrap_or((GATE_OPERATIONS[0], None));
        return gated(RandomizeRangeResult::Blocked {
            operation,
            decided_by,
        });
    }
    let api = SheetsApi::new(sheets);
    let workbook = match api.get_spreadsheet(&opts.spreadsheet_id).await {
        Ok(book) => book,
        Err(err) => {
            return gated(RandomizeRangeResult::Failed {
                detail: format!("{err:#}"),
            })
        }
    };
    let (_, grid) = match grid_range::resolve_grid_range(
        &workbook,
        &composed,
        |detail| RandomizeRangeResult::RefusedInvalidRequest { detail },
        |title, available| RandomizeRangeResult::RefusedSheetNotFound { title, available },
    ) {
        Ok(value) => value,
        Err(result) => return gated(result),
    };
    if !grid_range::is_bounded(&grid) {
        return gated(RandomizeRangeResult::RefusedInvalidRequest {
            detail: format!(
                "'{composed}' is open-ended; randomize-range needs a fully bounded range (e.g. A1:D10)"
            ),
        });
    }
    let start_column = grid.start_column_index.unwrap_or(0);
    let end_column = grid.end_column_index.unwrap_or(0);
    let allocated_columns = grid_range::find_sheet_by_id(&workbook, grid.sheet_id)
        .and_then(|sheet| sheet.properties.as_ref())
        .and_then(|props| props.grid_properties.as_ref())
        .and_then(|props| props.column_count);
    let width_warning = width_warning("randomizing", start_column, end_column, allocated_columns);
    let request = BatchUpdateRequestItem::RandomizeRange(RandomizeRangeRequest { range: grid });
    if opts.dry_run {
        return gated(RandomizeRangeResult::WouldChange {
            range: composed,
            width_warning,
        });
    }
    let leased = LeasedWrite {
        log_prefix: "drive sheets randomize-range",
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
        Ok(_) => RandomizeRangeResult::Changed {
            range: composed,
            width_warning,
        },
        Err(err) => RandomizeRangeResult::Failed {
            detail: format!("{err:#}"),
        },
    };
    gated(result)
}

fn record_attempt(outcome: &RandomizeRangeOutcome, duration: Duration) {
    let decided_by = match &outcome.result {
        RandomizeRangeResult::Blocked { decided_by, .. } => decided_by.as_ref(),
        _ => None,
    };
    let decided_by = write_gate::decided_by_log_fields(decided_by);
    let fields_changed = match &outcome.result {
        RandomizeRangeResult::Changed { range, .. } => Some(format!("randomized {range}")),
        _ => None,
    };
    let error = match &outcome.result {
        RandomizeRangeResult::RefusedInvalidRequest { detail }
        | RandomizeRangeResult::Failed { detail } => Some(detail.clone()),
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

/// The line stating that the resulting row order can never be known ahead
/// of, or reported after, the request — server-side randomness, the same
/// class of limitation `auto-fill` has for a different reason.
const ORDER_CAVEAT: &str =
    "the resulting order is chosen by the server and cannot be previewed or reported; the previous row order is not preserved";

pub fn describe_lines(outcome: &RandomizeRangeOutcome) -> Vec<String> {
    let book = outcome.file_name.as_deref().map_or_else(
        || format!("'{}'", outcome.spreadsheet_id),
        |name| format!("'{name}'"),
    );
    match &outcome.result {
        RandomizeRangeResult::WouldChange {
            range,
            width_warning,
        } => change_lines(
            format!("Would randomize the row order of {range} in {book}"),
            width_warning.as_deref(),
            false,
        ),
        RandomizeRangeResult::Changed {
            range,
            width_warning,
        } => change_lines(
            format!("Randomized the row order of {range} in {book}"),
            width_warning.as_deref(),
            true,
        ),
        RandomizeRangeResult::RefusedInvalidRequest { detail } => {
            vec![format!("Refused: {detail}")]
        }
        RandomizeRangeResult::RefusedNotASpreadsheet { mime_type } => vec![format!(
            "Refused: {book} is not a Google Sheet (mimeType: {mime_type})"
        )],
        RandomizeRangeResult::RefusedShortcut => vec![format!(
            "Refused: {book} is a shortcut; randomize-range doesn't follow shortcuts"
        )],
        RandomizeRangeResult::RefusedNoVisibleParents => {
            vec![format!("Refused: {book} has no visible parent folder")]
        }
        RandomizeRangeResult::RefusedSheetNotFound { title, available } => vec![format!(
            "Refused: {book} has no sheet titled '{title}'. Available: {}",
            available.join(", ")
        )],
        RandomizeRangeResult::Blocked {
            operation,
            decided_by,
        } => vec![match decided_by {
            Some(rule) => format!(
                "Blocked: randomize-range on {book} refused by rule on {} {}{}",
                rule.kind_label(),
                rule.id(),
                rule.depth_suffix()
            ),
            None => format!(
                "Blocked: randomize-range on {book} refused by default policy (no matching rule \
                 for {operation})"
            ),
        }],
        RandomizeRangeResult::RefusedNoLease => LeaseGateRefusal::NoLease
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        RandomizeRangeResult::RefusedLeaseExpired => LeaseGateRefusal::Expired
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        RandomizeRangeResult::RefusedLeaseWrongFile => LeaseGateRefusal::WrongFile
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        RandomizeRangeResult::RefusedLeaseStale => LeaseGateRefusal::Stale
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        RandomizeRangeResult::Failed { detail } => vec![format!("Failed: {detail}")],
    }
}

fn change_lines(summary: String, width_warning: Option<&str>, changed: bool) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(warning) = width_warning {
        lines.push(format!("Warning: {warning}"));
    }
    lines.push(summary);
    lines.push(format!("  {ORDER_CAVEAT}"));
    lines.push(if changed {
        "  references outside the range may now observe values from a different row".to_string()
    } else {
        "  references outside the range may observe values from a different row after randomizing"
            .to_string()
    });
    lines
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
    /// (sheetId 0) whose `gridProperties` omits `columnCount` — so the
    /// shared `width_warning` helper can never learn the sheet's
    /// allocated width.
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

    fn options(dry_run: bool) -> RandomizeRangeOptions {
        RandomizeRangeOptions {
            spreadsheet_id: "sheet-1".into(),
            sheet: Some("Q1".into()),
            range: Some("A2:C10".into()),
            dry_run,
            lease_token: None,
            ledger_path: PathBuf::from("/tmp/unused-randomize-range-ledger"),
        }
    }

    fn rule() -> FolderPermissionRule {
        FolderPermissionRule {
            folder_id: Some("parent-1".into()),
            file_id: None,
            recursive: true,
            allow: GATE_OPERATIONS.iter().copied().collect(),
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

    /// Allows `sheets-write` but explicitly denies `sheets-structure` — the
    /// union gate must refuse even though one of its two operations would
    /// have allowed it.
    fn rule_missing_sheets_structure() -> FolderPermissionRule {
        FolderPermissionRule {
            folder_id: Some("parent-1".into()),
            file_id: None,
            recursive: true,
            allow: std::iter::once(DriveOperation::SheetsWrite).collect(),
            deny: std::iter::once(DriveOperation::SheetsStructure).collect(),
            require_lease: false,
        }
    }

    #[test]
    fn preview_never_claims_to_predict_the_resulting_order() {
        let outcome = RandomizeRangeOutcome {
            spreadsheet_id: "id".into(),
            file_name: Some("Book".into()),
            resolved_folder_id: None,
            result: RandomizeRangeResult::WouldChange {
                range: "Q1!A1:B3".into(),
                width_warning: width_warning("randomizing", 0, 2, Some(5)),
            },
        };
        let lines = describe_lines(&outcome).join("\n");
        assert!(lines.contains("separate records"));
        assert!(lines.contains("cannot be previewed"));
        assert!(lines.contains("references outside"));
    }

    #[tokio::test]
    async fn dry_run_uses_metadata_without_reading_values_or_mutating() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        let outcome = randomize_range(&drive, &sheets, &options(true), &[rule()]).await;
        let RandomizeRangeResult::WouldChange { width_warning, .. } = outcome.result else {
            panic!("expected would-change"); // omni-dev: coverage ignore-line reason="this let-else panic only runs if the match failed to bind the expected variant; this test always constructs that exact variant, so the branch never executes"
        };
        assert!(width_warning
            .unwrap()
            .contains("3 of the sheet's 6 allocated columns"));
        let requests = server.received_requests().await.unwrap();
        // OAuth refresh, the target and workbook reads, plus the parent
        // folder read once per gated operation: `resolve_all` walks the
        // ancestor chain independently for each of `GATE_OPERATIONS`'s two
        // operations, so `parent-1` is fetched twice.
        assert_eq!(requests.len(), 5);
        assert!(requests
            .iter()
            .all(|request| { request.method.as_str() == "GET" || request.url.path() == "/token" }));
    }

    #[tokio::test]
    async fn real_run_sends_one_randomize_range_request() {
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
        let outcome = randomize_range(&drive, &sheets, &options(false), &[rule()]).await;
        assert!(
            matches!(outcome.result, RandomizeRangeResult::Changed { .. }),
            "{outcome:?}"
        );
        let requests = server.received_requests().await.unwrap();
        let batch = requests
            .iter()
            .find(|request| request.url.path().ends_with(":batchUpdate"))
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&batch.body).unwrap();
        assert_eq!(
            body["requests"][0]["randomizeRange"]["range"],
            serde_json::json!({
                "sheetId": 0, "startRowIndex": 1, "endRowIndex": 10,
                "startColumnIndex": 0, "endColumnIndex": 3
            })
        );
    }

    // ── refusals before any network call ────────────────────────────────

    #[tokio::test]
    async fn rejects_a_range_with_conflicting_sheet_prefix_before_any_network_call() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        let mut opts = options(false);
        opts.sheet = Some("Other".into());
        opts.range = Some("Sheet1!A1:B2".into());
        let outcome = randomize_range(&drive, &sheets, &opts, &[rule()]).await;
        let RandomizeRangeResult::RefusedInvalidRequest { detail } = &outcome.result else {
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
        let outcome = randomize_range(&drive, &sheets, &options(false), &[rule()]).await;
        assert!(matches!(
            outcome.result,
            RandomizeRangeResult::Failed { .. }
        ));
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
        let outcome = randomize_range(&drive, &sheets, &options(false), &[rule()]).await;
        assert!(matches!(
            outcome.result,
            RandomizeRangeResult::RefusedNotASpreadsheet { .. }
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
        let outcome = randomize_range(&drive, &sheets, &options(false), &[rule()]).await;
        assert!(matches!(
            outcome.result,
            RandomizeRangeResult::RefusedShortcut
        ));
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
        let outcome = randomize_range(&drive, &sheets, &options(false), &[]).await;
        assert!(matches!(
            outcome.result,
            RandomizeRangeResult::RefusedNoVisibleParents
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
        let outcome = randomize_range(&drive, &sheets, &options(false), &[rule()]).await;
        assert!(matches!(
            outcome.result,
            RandomizeRangeResult::Failed { .. }
        ));
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
        let outcome = randomize_range(&drive, &sheets, &options(false), &[]).await;
        assert!(
            matches!(
                outcome.result,
                RandomizeRangeResult::Blocked {
                    decided_by: None,
                    ..
                }
            ),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn a_rule_allowing_sheets_write_but_denying_sheets_structure_is_still_blocked() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        let outcome = randomize_range(
            &drive,
            &sheets,
            &options(false),
            &[rule_missing_sheets_structure()],
        )
        .await;
        let RandomizeRangeResult::Blocked { operation, .. } = outcome.result else {
            panic!("expected Blocked, got {:?}", outcome.result); // omni-dev: coverage ignore-line reason="this let-else panic only runs if the match failed to bind the expected variant; this test always constructs that exact variant, so the branch never executes"
        };
        assert_eq!(operation, DriveOperation::SheetsStructure);
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
        let outcome = randomize_range(&drive, &sheets, &options(false), &[rule()]).await;
        assert!(matches!(
            outcome.result,
            RandomizeRangeResult::Failed { .. }
        ));
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
        let outcome = randomize_range(&drive, &sheets, &opts, &[rule()]).await;
        let RandomizeRangeResult::RefusedInvalidRequest { detail } = &outcome.result else {
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
        let outcome = randomize_range(&drive, &sheets, &opts, &[rule()]).await;
        let RandomizeRangeResult::RefusedSheetNotFound { title, available } = &outcome.result
        else {
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
        let outcome = randomize_range(&drive, &sheets, &opts, &[rule()]).await;
        let RandomizeRangeResult::RefusedInvalidRequest { detail } = &outcome.result else {
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
        let outcome = randomize_range(&drive, &sheets, &options(true), &[rule()]).await;
        let RandomizeRangeResult::WouldChange { width_warning, .. } = outcome.result else {
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
        let outcome = randomize_range(&drive, &sheets, &options(false), &[rule()]).await;
        assert!(matches!(
            outcome.result,
            RandomizeRangeResult::Failed { .. }
        ));
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
        let outcome = randomize_range(&drive, &sheets, &opts, &[rule_requiring_lease()]).await;
        assert!(matches!(
            outcome.result,
            RandomizeRangeResult::Failed { .. }
        ));
    }

    // ── the Drive write lease (ADR-0080 §9) ─────────────────────────────

    #[tokio::test]
    async fn refuses_without_a_lease_when_the_rule_requires_one() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        // No batchUpdate mock: a refusal must make zero mutating calls.
        let outcome =
            randomize_range(&drive, &sheets, &options(false), &[rule_requiring_lease()]).await;
        assert_eq!(outcome.result, RandomizeRangeResult::RefusedNoLease);
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
        let outcome = randomize_range(&drive, &sheets, &opts, &[rule_requiring_lease()]).await;
        assert_eq!(outcome.result, RandomizeRangeResult::RefusedLeaseExpired);
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
        let outcome = randomize_range(&drive, &sheets, &opts, &[rule_requiring_lease()]).await;
        assert_eq!(outcome.result, RandomizeRangeResult::RefusedLeaseWrongFile);
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
        let outcome = randomize_range(&drive, &sheets, &opts, &[rule_requiring_lease()]).await;
        assert_eq!(outcome.result, RandomizeRangeResult::RefusedLeaseStale);
    }

    // ── pure unit coverage: lease-refusal mapping, JSONL, describe/log ──

    #[test]
    fn lease_refusals_map_to_their_randomize_range_results() {
        assert_eq!(
            RandomizeRangeResult::from_no_lease(),
            RandomizeRangeResult::RefusedNoLease
        );
        assert_eq!(
            RandomizeRangeResult::from_lease_expired(),
            RandomizeRangeResult::RefusedLeaseExpired
        );
        assert_eq!(
            RandomizeRangeResult::from_lease_wrong_file(),
            RandomizeRangeResult::RefusedLeaseWrongFile
        );
        assert_eq!(
            RandomizeRangeResult::from_lease_stale(),
            RandomizeRangeResult::RefusedLeaseStale
        );
        assert_eq!(
            RandomizeRangeResult::from_lease_failed("ledger unavailable".into()),
            RandomizeRangeResult::Failed {
                detail: "ledger unavailable".into()
            }
        );
    }

    #[test]
    fn write_jsonl_emits_one_line_of_json() {
        let outcome = RandomizeRangeOutcome {
            spreadsheet_id: "sheet-1".into(),
            file_name: Some("Budget".into()),
            resolved_folder_id: Some("parent-1".into()),
            result: RandomizeRangeResult::Changed {
                range: "Q1!A2:C10".into(),
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

    /// One of every [`RandomizeRangeResult`] variant.
    ///
    /// The `match` below is exhaustive and wildcard-free on purpose: adding
    /// a variant breaks this build, which is what forces the new arm
    /// through the tests below.
    fn every_randomize_range_result() -> Vec<RandomizeRangeResult> {
        let all = vec![
            RandomizeRangeResult::WouldChange {
                range: "Q1!A1:B3".into(),
                width_warning: None,
            },
            RandomizeRangeResult::Changed {
                range: "Q1!A1:B3".into(),
                width_warning: Some("narrow".to_string()),
            },
            RandomizeRangeResult::RefusedInvalidRequest {
                detail: "bad".into(),
            },
            RandomizeRangeResult::RefusedNotASpreadsheet {
                mime_type: "application/pdf".into(),
            },
            RandomizeRangeResult::RefusedShortcut,
            RandomizeRangeResult::RefusedNoVisibleParents,
            RandomizeRangeResult::RefusedSheetNotFound {
                title: "Nope".into(),
                available: vec!["Q1".into()],
            },
            RandomizeRangeResult::Blocked {
                operation: DriveOperation::SheetsWrite,
                decided_by: None,
            },
            RandomizeRangeResult::Blocked {
                operation: DriveOperation::SheetsStructure,
                decided_by: Some(DecidingRule::Folder {
                    folder_id: "folder-1".into(),
                    depth: 2,
                }),
            },
            RandomizeRangeResult::Blocked {
                operation: DriveOperation::SheetsWrite,
                decided_by: Some(DecidingRule::File {
                    file_id: "sheet-1".into(),
                }),
            },
            RandomizeRangeResult::RefusedNoLease,
            RandomizeRangeResult::RefusedLeaseExpired,
            RandomizeRangeResult::RefusedLeaseWrongFile,
            RandomizeRangeResult::RefusedLeaseStale,
            RandomizeRangeResult::Failed {
                detail: "boom".into(),
            },
        ];
        for result in &all {
            match result {
                RandomizeRangeResult::WouldChange { .. }
                | RandomizeRangeResult::Changed { .. }
                | RandomizeRangeResult::RefusedInvalidRequest { .. }
                | RandomizeRangeResult::RefusedNotASpreadsheet { .. }
                | RandomizeRangeResult::RefusedShortcut
                | RandomizeRangeResult::RefusedNoVisibleParents
                | RandomizeRangeResult::RefusedSheetNotFound { .. }
                | RandomizeRangeResult::Blocked { .. }
                | RandomizeRangeResult::RefusedNoLease
                | RandomizeRangeResult::RefusedLeaseExpired
                | RandomizeRangeResult::RefusedLeaseWrongFile
                | RandomizeRangeResult::RefusedLeaseStale
                | RandomizeRangeResult::Failed { .. } => (),
            }
        }
        all
    }

    #[test]
    fn every_describe_arm_renders_at_least_one_line_with_no_control_characters() {
        for result in every_randomize_range_result() {
            let outcome = RandomizeRangeOutcome {
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
        let outcome = RandomizeRangeOutcome {
            spreadsheet_id: "sheet-1".into(),
            file_name: None,
            resolved_folder_id: None,
            result: RandomizeRangeResult::RefusedShortcut,
        };
        let lines = describe_lines(&outcome);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("'sheet-1'"), "{lines:?}");
    }

    #[test]
    fn blocked_with_no_deciding_rule_names_the_denied_operation() {
        let outcome = RandomizeRangeOutcome {
            spreadsheet_id: "sheet-1".into(),
            file_name: Some("Budget".into()),
            resolved_folder_id: None,
            result: RandomizeRangeResult::Blocked {
                operation: DriveOperation::SheetsStructure,
                decided_by: None,
            },
        };
        let lines = describe_lines(&outcome).join("\n");
        assert!(lines.contains("sheets-structure"), "{lines}");
    }

    #[test]
    fn log_status_covers_every_variant() {
        for result in every_randomize_range_result() {
            assert!(!result.log_status().is_empty());
        }
        assert_eq!(
            RandomizeRangeResult::Blocked {
                operation: DriveOperation::SheetsWrite,
                decided_by: None
            }
            .log_status(),
            "blocked"
        );
        assert_eq!(
            RandomizeRangeResult::Changed {
                range: "Q1!A1:B2".into(),
                width_warning: None
            }
            .log_status(),
            "changed"
        );
        assert_eq!(
            RandomizeRangeResult::RefusedLeaseStale.log_status(),
            "refused-lease-stale"
        );
    }
}
