//! `delete-duplicates` — remove rows within a range that duplicate an
//! earlier row (issue #1844).
//!
//! [ADR-0083](../../../docs/adrs/adr-0083.md) §2 places this under
//! `DriveOperation::SheetsDelete`, alone in this tranche: cells within the
//! selected range are removed and the survivors shift up, which is
//! `delete-range`'s shape, not `clear`'s. Content outside the range
//! stays in place, so a narrow selection can misalign records. It is also
//! the **first `sheets-delete` verb outside `structure.rs`**, which is
//! why `recovery_note` was lifted to `lease::check` rather than copied here.
//!
//! Two things make it unlike every other deletion this crate performs:
//!
//! - **The server chooses the rows.** Every other `sheets-delete` verb
//!   removes cells the caller named by address. Here the API applies its
//!   own equality rule — case-, formatting- and formula-insensitive, first
//!   instance kept, duplicates need not be adjacent — and removes whatever
//!   it finds, filter-hidden rows included. §2 accepts that as within what
//!   a `sheets-delete` grant consents to, and requires the preview and the
//!   real run to state the rule rather than leave it implicit.
//! - **`--dry-run` lists no rows at all.** §6 puts this verb in the
//!   server-decided tier: reproducing the equality rule locally would mean
//!   guessing which rendering of each value the server compares, and a
//!   wrong guess here costs a *row*, not a match. So the preview names the
//!   range and the compared columns and states the rule, "rather than a
//!   row list it cannot vouch for".
//!
//! There is deliberately no `--whole-sheet` scope, unlike
//! `trim_whitespace.rs`: every fully blank row in a range duplicates every
//! other, so a whole-sheet dedupe would delete the tab's entire trailing
//! empty region. The same hazard applies to a bounded range that runs past
//! the data, which is why it is called out in the preview.

#![allow(missing_docs)] // The CLI-facing outcome models are self-describing in JSON.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::cli::drive::format::{write_scalar_jsonl, JsonlSerialize};
use crate::drive::client::DriveClient;
use crate::drive::files_api::FilesApi;
use crate::drive::lease::check::{
    conclude_native_leased_write, gate_optional_leased_write, recovery_note, FromLeaseRefusal,
    LeaseGateRefusal, LeasedWrite,
};
use crate::drive::lease::ledger::LeaseBackup;
use crate::drive::sheets::api::SheetsApi;
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::types::{
    BatchUpdateRequestItem, DeleteDuplicatesRequest, Dimension, DimensionRange, GridRange,
};
use crate::drive::sheets::{a1, grid_range, target_gate};
use crate::drive::types::SheetTargetRefusal;
use crate::drive::write_gate::{self, DecidingRule, DriveOperation, FolderPermissionRule};
use crate::request_log::{self, DriveMutationOutcome};

const LOG_OPERATION: &str = "sheets-delete-duplicates";

/// The caveat both the preview and the real run carry, stating the rule
/// the *server* applies — §2's requirement, since the caller never names
/// the rows this verb removes.
const EQUALITY_RULE: &str = "the API keeps the first instance of each duplicate and removes the \
                             rest; duplicates need not be adjacent, rows differing only in letter \
                             case, formatting or formulas still count as duplicates, and rows \
                             hidden by a filter are removed along with visible ones";

/// The hazard a range running past the data creates. Blank rows duplicate
/// one another, so every blank row after the first inside the range is a
/// duplicate of it.
const BLANK_ROW_CAVEAT: &str = "blank rows inside the range duplicate one another, so a range \
                                extending past the data can remove every blank row but the first";
const RANGE_ONLY_CAVEAT: &str = "only cells inside the selected range are removed and shifted up; \
                                 columns outside it stay in place, so a range narrower than the \
                                 sheet can misalign records";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteDuplicatesOptions {
    pub spreadsheet_id: String,
    pub sheet: Option<String>,
    pub range: Option<String>,
    /// Absolute, zero-based sheet column indexes to compare, in the order
    /// given. Empty means the API compares every column in the range.
    pub comparison_columns: Vec<i64>,
    pub dry_run: bool,
    pub lease_token: Option<String>,
    pub ledger_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum DeleteDuplicatesResult {
    WouldRemove {
        range: String,
        /// The compared columns, or empty for "every column in the range".
        comparison_columns: Vec<i64>,
        /// The API leaves content outside the selected rectangle in place.
        content_outside_range_untouched: bool,
    },
    Removed {
        range: String,
        comparison_columns: Vec<i64>,
        /// The API leaves content outside the selected rectangle in place.
        content_outside_range_untouched: bool,
        /// The server's own count. `None` when the reply carried no
        /// `deleteDuplicates` object — `find-replace`'s precedent for a
        /// response that succeeded but reported nothing.
        duplicates_removed_count: Option<i64>,
        /// Where the lease that authorised this write backed the file up,
        /// so the recovery message names a real copy rather than assuming
        /// one exists. `None` under a `require_lease: false` rule.
        #[serde(skip_serializing_if = "Option::is_none")]
        backup: Option<Box<LeaseBackup>>,
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

impl FromLeaseRefusal for DeleteDuplicatesResult {
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

impl DeleteDuplicatesResult {
    fn log_status(&self) -> &'static str {
        match self {
            Self::WouldRemove { .. } => "would-remove",
            Self::Removed { .. } => "removed",
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
pub struct DeleteDuplicatesOutcome {
    pub spreadsheet_id: String,
    pub file_name: Option<String>,
    pub resolved_folder_id: Option<String>,
    pub result: DeleteDuplicatesResult,
}

impl JsonlSerialize for DeleteDuplicatesOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> anyhow::Result<()> {
        write_scalar_jsonl(self, out)
    }
}

pub async fn delete_duplicates(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &DeleteDuplicatesOptions,
    rules: &[FolderPermissionRule],
) -> DeleteDuplicatesOutcome {
    let started = Instant::now();
    let outcome = delete_duplicates_inner(drive, sheets, opts, rules).await;
    if !opts.dry_run {
        record_attempt(&outcome, started.elapsed());
    }
    outcome
}

async fn delete_duplicates_inner(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &DeleteDuplicatesOptions,
    rules: &[FolderPermissionRule],
) -> DeleteDuplicatesOutcome {
    let bare = |result| DeleteDuplicatesOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: None,
        resolved_folder_id: None,
        result,
    };
    let composed = match a1::compose(opts.sheet.as_deref(), opts.range.as_deref()) {
        Ok(range) => range,
        Err(err) => {
            return bare(DeleteDuplicatesResult::RefusedInvalidRequest {
                detail: err.to_string(),
            })
        }
    };
    if let Err(detail) = validate_comparison_columns(&opts.comparison_columns) {
        return bare(DeleteDuplicatesResult::RefusedInvalidRequest { detail });
    }
    let (target, decision, resolved_folder_id, requires_lease) = match target_gate::resolve(
        drive,
        &opts.spreadsheet_id,
        DriveOperation::SheetsDelete,
        rules,
    )
    .await
    {
        target_gate::TargetGateOutcome::MetadataFetchFailed { detail } => {
            return bare(DeleteDuplicatesResult::Failed { detail })
        }
        target_gate::TargetGateOutcome::Refused { target, refusal } => {
            let result = match refusal {
                SheetTargetRefusal::Shortcut => DeleteDuplicatesResult::RefusedShortcut,
                SheetTargetRefusal::NotASpreadsheet { mime_type } => {
                    DeleteDuplicatesResult::RefusedNotASpreadsheet { mime_type }
                }
                SheetTargetRefusal::NoVisibleParents => {
                    DeleteDuplicatesResult::RefusedNoVisibleParents
                }
            };
            return DeleteDuplicatesOutcome {
                spreadsheet_id: opts.spreadsheet_id.clone(),
                file_name: Some(target.name),
                resolved_folder_id: None,
                result,
            };
        }
        target_gate::TargetGateOutcome::GateFetchFailed { target, detail } => {
            return DeleteDuplicatesOutcome {
                spreadsheet_id: opts.spreadsheet_id.clone(),
                file_name: Some(target.name),
                resolved_folder_id: None,
                result: DeleteDuplicatesResult::Failed { detail },
            }
        }
        target_gate::TargetGateOutcome::Gated {
            target,
            decision,
            resolved_folder_id,
            requires_lease,
        } => (target, decision, resolved_folder_id, requires_lease),
    };
    let gated = |result| DeleteDuplicatesOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: Some(target.name.clone()),
        resolved_folder_id: resolved_folder_id.clone(),
        result,
    };
    if decision.verdict == write_gate::Verdict::Deny {
        return gated(DeleteDuplicatesResult::Blocked {
            decided_by: decision.decided_by,
        });
    }
    let api = SheetsApi::new(sheets);
    let workbook = match api.get_spreadsheet(&opts.spreadsheet_id).await {
        Ok(book) => book,
        Err(err) => {
            return gated(DeleteDuplicatesResult::Failed {
                detail: format!("{err:#}"),
            })
        }
    };
    let (_, grid) = match grid_range::resolve_grid_range(
        &workbook,
        &composed,
        |detail| DeleteDuplicatesResult::RefusedInvalidRequest { detail },
        |title, available| DeleteDuplicatesResult::RefusedSheetNotFound { title, available },
    ) {
        Ok(value) => value,
        Err(result) => return gated(result),
    };
    // Bounded-only, `sort-range`'s rule. A dedupe over an open-ended range
    // is exactly the blank-row hazard in the module docs, made total: it
    // would reach every allocated row of the sheet.
    if !grid_range::is_bounded(&grid) {
        return gated(DeleteDuplicatesResult::RefusedInvalidRequest {
            detail: format!(
                "'{composed}' is open-ended; delete-duplicates needs a fully bounded range (e.g. \
                 A1:D100) so it cannot reach past the data"
            ),
        });
    }
    if let Err(detail) = columns_within_range(&opts.comparison_columns, &grid) {
        return gated(DeleteDuplicatesResult::RefusedInvalidRequest { detail });
    }

    // Built before the gate, not after — #1688/#1742's ordering invariant,
    // shared verbatim by every leased engine.
    let request = BatchUpdateRequestItem::DeleteDuplicates(DeleteDuplicatesRequest {
        range: grid,
        comparison_columns: opts
            .comparison_columns
            .iter()
            .map(|&index| DimensionRange {
                sheet_id: grid.sheet_id,
                dimension: Dimension::Columns,
                start_index: index,
                end_index: index + 1,
            })
            .collect(),
    });

    if opts.dry_run {
        return gated(DeleteDuplicatesResult::WouldRemove {
            range: composed,
            comparison_columns: opts.comparison_columns.clone(),
            content_outside_range_untouched: true,
        });
    }

    let leased = LeasedWrite {
        log_prefix: "drive sheets delete-duplicates",
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
        Ok(response) => DeleteDuplicatesResult::Removed {
            range: composed,
            comparison_columns: opts.comparison_columns.clone(),
            content_outside_range_untouched: true,
            duplicates_removed_count: response
                .replies
                .into_iter()
                .next()
                .and_then(|reply| reply.delete_duplicates)
                .map(|reply| reply.duplicates_removed_count),
            backup: lease_grant
                .as_ref()
                .map(|grant| Box::new(grant.backup.clone())),
        },
        Err(err) => DeleteDuplicatesResult::Failed {
            detail: format!("{err:#}"),
        },
    };
    drop(lease_grant);
    gated(result)
}

/// Rejects the column indexes that are wrong on their own terms, before
/// any network call. Bounds against the *range* need the resolved grid, so
/// they are checked separately by [`columns_within_range`].
fn validate_comparison_columns(columns: &[i64]) -> Result<(), String> {
    if let Some(negative) = columns.iter().find(|&&index| index < 0) {
        return Err(format!(
            "--comparison-column {negative} must be a non-negative, zero-based column index"
        ));
    }
    let mut seen = Vec::with_capacity(columns.len());
    for &index in columns {
        if seen.contains(&index) {
            return Err(format!(
                "--comparison-column {index} is given more than once"
            ));
        }
        seen.push(index);
    }
    Ok(())
}

/// Every compared column must lie inside the selected range — `sort-range`'s
/// rule for `--sort-by`, and the same absolute, zero-based convention.
fn columns_within_range(columns: &[i64], grid: &GridRange) -> Result<(), String> {
    let start = grid.start_column_index.unwrap_or(0);
    let end = grid.end_column_index.unwrap_or(0);
    match columns.iter().find(|&&index| index < start || index >= end) {
        Some(index) => Err(format!(
            "comparison column {index} is outside the selected range's columns {start}..{end} \
             (zero-based, end exclusive)"
        )),
        None => Ok(()),
    }
}

fn record_attempt(outcome: &DeleteDuplicatesOutcome, duration: Duration) {
    let decided_by = match &outcome.result {
        DeleteDuplicatesResult::Blocked { decided_by } => decided_by.as_ref(),
        _ => None,
    };
    let decided_by = write_gate::decided_by_log_fields(decided_by);
    let fields_changed = match &outcome.result {
        DeleteDuplicatesResult::Removed {
            range,
            comparison_columns,
            duplicates_removed_count,
            ..
        } => Some(match duplicates_removed_count {
            Some(count) => format!(
                "removed {count} duplicate row(s) from {range} comparing {}",
                render_columns(comparison_columns)
            ),
            None => format!(
                "removed duplicate rows from {range} comparing {}; the API reported no count",
                render_columns(comparison_columns)
            ),
        }),
        _ => None,
    };
    let error = match &outcome.result {
        DeleteDuplicatesResult::RefusedInvalidRequest { detail }
        | DeleteDuplicatesResult::Failed { detail } => Some(detail.clone()),
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

pub fn describe_lines(outcome: &DeleteDuplicatesOutcome) -> Vec<String> {
    let book = outcome.file_name.as_deref().map_or_else(
        || format!("'{}'", outcome.spreadsheet_id),
        |name| format!("'{name}'"),
    );
    match &outcome.result {
        DeleteDuplicatesResult::WouldRemove {
            range,
            comparison_columns,
            ..
        } => vec![
            format!("Warning: {BLANK_ROW_CAVEAT}"),
            format!("Warning: {RANGE_ONLY_CAVEAT}"),
            format!(
                "Would remove duplicate rows from {range} in {book}, comparing {}",
                render_columns(comparison_columns)
            ),
            format!("  {EQUALITY_RULE}"),
            "  which rows would be removed is decided by the API and cannot be previewed"
                .to_string(),
        ],
        DeleteDuplicatesResult::Removed {
            range,
            comparison_columns,
            duplicates_removed_count,
            backup,
            ..
        } => vec![
            format!("Warning: {RANGE_ONLY_CAVEAT}"),
            match duplicates_removed_count {
                Some(count) => format!(
                    "Removed {count} duplicate row(s) from {range} in {book}, comparing {}",
                    render_columns(comparison_columns)
                ),
                None => format!(
                    "Removed duplicate rows from {range} in {book}, comparing {}; the API \
                     reported no count",
                    render_columns(comparison_columns)
                ),
            },
            format!("  {EQUALITY_RULE}"),
            format!("  {}", recovery_note(backup.as_deref(), false)),
        ],
        DeleteDuplicatesResult::RefusedInvalidRequest { detail } => {
            vec![format!("Refused: {detail}")]
        }
        DeleteDuplicatesResult::RefusedNotASpreadsheet { mime_type } => vec![format!(
            "Refused: {book} is not a Google Sheet (mimeType: {mime_type})"
        )],
        DeleteDuplicatesResult::RefusedShortcut => vec![format!(
            "Refused: {book} is a shortcut; delete-duplicates doesn't follow shortcuts"
        )],
        DeleteDuplicatesResult::RefusedNoVisibleParents => {
            vec![format!("Refused: {book} has no visible parent folder")]
        }
        DeleteDuplicatesResult::RefusedSheetNotFound { title, available } => vec![format!(
            "Refused: {book} has no sheet titled '{title}'. Available: {}",
            available.join(", ")
        )],
        DeleteDuplicatesResult::Blocked { .. } => vec![format!(
            "Blocked: delete-duplicates on {book} requires an allowing sheets-delete rule"
        )],
        DeleteDuplicatesResult::RefusedNoLease => LeaseGateRefusal::NoLease
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        DeleteDuplicatesResult::RefusedLeaseExpired => LeaseGateRefusal::Expired
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        DeleteDuplicatesResult::RefusedLeaseWrongFile => LeaseGateRefusal::WrongFile
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        DeleteDuplicatesResult::RefusedLeaseStale => LeaseGateRefusal::Stale
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        DeleteDuplicatesResult::Failed { detail } => vec![format!("Failed: {detail}")],
    }
}

/// Renders the compared columns for a message, naming the API's default
/// explicitly rather than printing an empty list.
fn render_columns(columns: &[i64]) -> String {
    if columns.is_empty() {
        return "every column in the range".to_string();
    }
    format!(
        "column(s) {}",
        columns
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::lease::ledger::LeaseBackup;
    use crate::drive::sheets::client::SHEETS_API_URL;
    use crate::drive::test_support::{seed_lease, seed_lease_with_backup};
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

    fn mount_batch_update(body: serde_json::Value) -> wiremock::Mock {
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
    }

    fn options(dry_run: bool) -> DeleteDuplicatesOptions {
        DeleteDuplicatesOptions {
            spreadsheet_id: "sheet-1".into(),
            sheet: Some("Q1".into()),
            range: Some("A2:C10".into()),
            comparison_columns: Vec::new(),
            dry_run,
            lease_token: None,
            ledger_path: PathBuf::from("/tmp/unused-delete-duplicates-ledger"),
        }
    }

    fn rule() -> FolderPermissionRule {
        FolderPermissionRule {
            folder_id: Some("parent-1".into()),
            file_id: None,
            recursive: true,
            allow: std::iter::once(DriveOperation::SheetsDelete).collect(),
            deny: HashSet::default(),
            require_lease: false,
        }
    }

    fn rule_allowing(operation: DriveOperation) -> FolderPermissionRule {
        FolderPermissionRule {
            allow: std::iter::once(operation).collect(),
            ..rule()
        }
    }

    fn rule_requiring_lease() -> FolderPermissionRule {
        FolderPermissionRule {
            require_lease: true,
            ..rule()
        }
    }

    // ── comparison-column validation (pure) ─────────────────────────────

    #[test]
    fn comparison_columns_refuse_negatives_and_repeats() {
        assert!(validate_comparison_columns(&[0, 2]).is_ok());
        assert!(validate_comparison_columns(&[]).is_ok());
        assert!(validate_comparison_columns(&[-1])
            .unwrap_err()
            .contains("non-negative"));
        assert!(validate_comparison_columns(&[1, 1])
            .unwrap_err()
            .contains("more than once"));
    }

    #[test]
    fn comparison_columns_must_lie_inside_the_range() {
        let grid = GridRange {
            sheet_id: 0,
            start_row_index: Some(1),
            end_row_index: Some(10),
            start_column_index: Some(2),
            end_column_index: Some(5),
        };
        assert!(columns_within_range(&[2, 4], &grid).is_ok());
        // Below the start and at the exclusive end are both outside.
        assert!(columns_within_range(&[1], &grid)
            .unwrap_err()
            .contains("columns 2..5"));
        assert!(columns_within_range(&[5], &grid).is_err());
    }

    // ── rendering (pure) ────────────────────────────────────────────────

    #[test]
    fn render_columns_names_the_api_default_rather_than_an_empty_list() {
        assert_eq!(render_columns(&[]), "every column in the range");
        assert_eq!(render_columns(&[0, 2]), "column(s) 0, 2");
    }

    #[test]
    fn the_preview_states_the_servers_rule_and_never_claims_a_row_list() {
        let outcome = DeleteDuplicatesOutcome {
            spreadsheet_id: "sheet-1".into(),
            file_name: Some("Budget".into()),
            resolved_folder_id: None,
            result: DeleteDuplicatesResult::WouldRemove {
                range: "'Q1'!A2:C10".into(),
                comparison_columns: vec![0],
                content_outside_range_untouched: true,
            },
        };
        let lines = describe_lines(&outcome).join("\n");
        assert!(lines.contains("keeps the first instance"), "{lines}");
        assert!(lines.contains("hidden by a filter"), "{lines}");
        assert!(lines.contains("cannot be previewed"), "{lines}");
        assert!(lines.contains("blank rows"), "{lines}");
        assert!(lines.contains("can misalign records"), "{lines}");
    }

    /// ADR-0077 §5's recovery tail, from the lifted shared `recovery_note`
    /// — both arms, since the `None` one must never claim a backup exists.
    #[test]
    fn the_real_run_names_the_lease_backup_or_says_there_was_none() {
        let with_backup = DeleteDuplicatesOutcome {
            spreadsheet_id: "sheet-1".into(),
            file_name: Some("Budget".into()),
            resolved_folder_id: None,
            result: DeleteDuplicatesResult::Removed {
                range: "'Q1'!A2:C10".into(),
                comparison_columns: Vec::new(),
                content_outside_range_untouched: true,
                duplicates_removed_count: Some(2),
                backup: Some(Box::new(LeaseBackup::DriveCopy {
                    file_id: "copy-9".into(),
                })),
            },
        };
        let lines = describe_lines(&with_backup).join("\n");
        assert!(lines.contains("Drive copy copy-9"), "{lines}");
        assert!(lines.contains("drive lease restore"), "{lines}");
        assert!(lines.contains("can misalign records"), "{lines}");

        let without = DeleteDuplicatesOutcome {
            result: DeleteDuplicatesResult::Removed {
                range: "'Q1'!A2:C10".into(),
                comparison_columns: Vec::new(),
                content_outside_range_untouched: true,
                duplicates_removed_count: Some(2),
                backup: None,
            },
            ..with_backup
        };
        let lines = describe_lines(&without).join("\n");
        assert!(lines.contains("no lease backup was taken"), "{lines}");
        assert!(!lines.contains("Drive copy"), "{lines}");
    }

    #[test]
    fn lease_refusals_map_to_their_delete_duplicates_results() {
        assert_eq!(
            DeleteDuplicatesResult::from_no_lease(),
            DeleteDuplicatesResult::RefusedNoLease
        );
        assert_eq!(
            DeleteDuplicatesResult::from_lease_expired(),
            DeleteDuplicatesResult::RefusedLeaseExpired
        );
        assert_eq!(
            DeleteDuplicatesResult::from_lease_wrong_file(),
            DeleteDuplicatesResult::RefusedLeaseWrongFile
        );
        assert_eq!(
            DeleteDuplicatesResult::from_lease_stale(),
            DeleteDuplicatesResult::RefusedLeaseStale
        );
        assert_eq!(
            DeleteDuplicatesResult::from_lease_failed("boom".into()),
            DeleteDuplicatesResult::Failed {
                detail: "boom".into()
            }
        );
    }

    fn every_result() -> Vec<DeleteDuplicatesResult> {
        vec![
            DeleteDuplicatesResult::WouldRemove {
                range: "'Q1'!A2:C10".into(),
                comparison_columns: vec![0],
                content_outside_range_untouched: true,
            },
            DeleteDuplicatesResult::Removed {
                range: "'Q1'!A2:C10".into(),
                comparison_columns: Vec::new(),
                content_outside_range_untouched: true,
                duplicates_removed_count: Some(2),
                backup: None,
            },
            DeleteDuplicatesResult::Removed {
                range: "'Q1'!A2:C10".into(),
                comparison_columns: vec![1],
                content_outside_range_untouched: true,
                duplicates_removed_count: None,
                backup: None,
            },
            DeleteDuplicatesResult::RefusedInvalidRequest {
                detail: "bad".into(),
            },
            DeleteDuplicatesResult::RefusedNotASpreadsheet {
                mime_type: "application/pdf".into(),
            },
            DeleteDuplicatesResult::RefusedShortcut,
            DeleteDuplicatesResult::RefusedNoVisibleParents,
            DeleteDuplicatesResult::RefusedSheetNotFound {
                title: "Nope".into(),
                available: vec!["Q1".into()],
            },
            DeleteDuplicatesResult::Blocked { decided_by: None },
            DeleteDuplicatesResult::RefusedNoLease,
            DeleteDuplicatesResult::RefusedLeaseExpired,
            DeleteDuplicatesResult::RefusedLeaseWrongFile,
            DeleteDuplicatesResult::RefusedLeaseStale,
            DeleteDuplicatesResult::Failed {
                detail: "boom".into(),
            },
        ]
    }

    #[test]
    fn every_result_renders_and_carries_a_distinct_log_status() {
        let mut statuses = HashSet::new();
        for result in every_result() {
            let outcome = DeleteDuplicatesOutcome {
                spreadsheet_id: "sheet-1".into(),
                file_name: Some("Budget".into()),
                resolved_folder_id: None,
                result,
            };
            assert!(
                !describe_lines(&outcome).is_empty(),
                "{:?} rendered nothing",
                outcome.result
            );
            statuses.insert(outcome.result.log_status());
        }
        // 14 results, 13 statuses: the two `Removed`s share one.
        assert_eq!(statuses.len(), 13);
    }

    #[test]
    fn describe_lines_falls_back_to_the_spreadsheet_id_with_no_file_name() {
        let outcome = DeleteDuplicatesOutcome {
            spreadsheet_id: "sheet-1".into(),
            file_name: None,
            resolved_folder_id: None,
            result: DeleteDuplicatesResult::RefusedShortcut,
        };
        assert!(describe_lines(&outcome).join("\n").contains("'sheet-1'"));
    }

    #[test]
    fn write_jsonl_emits_one_line_of_json() {
        let outcome = DeleteDuplicatesOutcome {
            spreadsheet_id: "sheet-1".into(),
            file_name: Some("Budget".into()),
            resolved_folder_id: None,
            result: DeleteDuplicatesResult::Removed {
                range: "'Q1'!A2:C10".into(),
                comparison_columns: vec![0],
                content_outside_range_untouched: true,
                duplicates_removed_count: Some(2),
                backup: None,
            },
        };
        let mut buf = Vec::new();
        outcome.write_jsonl(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains("\"duplicates_removed_count\":2"), "{text}");
        // The absent backup is omitted, not rendered as null.
        assert!(!text.contains("backup"), "{text}");
    }

    // ── the gate ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn gate_denies_sheets_delete_by_default() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        let outcome = delete_duplicates(&drive, &sheets, &options(true), &[]).await;
        assert!(
            matches!(outcome.result, DeleteDuplicatesResult::Blocked { .. }),
            "{outcome:?}"
        );
    }

    /// ADR-0083 §2's whole point, as an executable claim: this verb needs
    /// the stronger grant, so `sheets-write` — which every other verb in
    /// the tranche runs on — must not open it.
    #[tokio::test]
    async fn a_sheets_write_grant_alone_does_not_permit_delete_duplicates() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        let outcome = delete_duplicates(
            &drive,
            &sheets,
            &options(true),
            &[rule_allowing(DriveOperation::SheetsWrite)],
        )
        .await;
        assert!(
            matches!(outcome.result, DeleteDuplicatesResult::Blocked { .. }),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_sheets_structure_grant_alone_does_not_permit_delete_duplicates() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        let outcome = delete_duplicates(
            &drive,
            &sheets,
            &options(true),
            &[rule_allowing(DriveOperation::SheetsStructure)],
        )
        .await;
        assert!(
            matches!(outcome.result, DeleteDuplicatesResult::Blocked { .. }),
            "{outcome:?}"
        );
    }

    // ── the two paths ───────────────────────────────────────────────────

    #[tokio::test]
    async fn dry_run_reads_no_values_and_never_mutates() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        let mut opts = options(true);
        opts.comparison_columns = vec![0, 2];
        let outcome = delete_duplicates(&drive, &sheets, &opts, &[rule()]).await;
        let json = serde_json::to_value(&outcome).unwrap();
        assert_eq!(json["result"]["content_outside_range_untouched"], true);
        assert_eq!(
            outcome.result,
            DeleteDuplicatesResult::WouldRemove {
                range: "'Q1'!A2:C10".into(),
                comparison_columns: vec![0, 2],
                content_outside_range_untouched: true,
            }
        );
        let requests = server.received_requests().await.unwrap();
        assert!(
            !requests.iter().any(|r| r.url.path().contains("/values/")),
            "the server-decided preview read values"
        );
        assert!(!requests
            .iter()
            .any(|r| r.url.path().ends_with(":batchUpdate")));
    }

    #[tokio::test]
    async fn real_run_sends_one_delete_duplicates_request_with_column_ranges() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        mount_batch_update(serde_json::json!({
            "replies": [{"deleteDuplicates": {"duplicatesRemovedCount": 4}}]
        }))
        .mount(&server)
        .await;
        let mut opts = options(false);
        opts.comparison_columns = vec![0, 2];
        let outcome = delete_duplicates(&drive, &sheets, &opts, &[rule()]).await;
        assert!(
            matches!(
                outcome.result,
                DeleteDuplicatesResult::Removed {
                    duplicates_removed_count: Some(4),
                    ..
                }
            ),
            "{outcome:?}"
        );

        let requests = server.received_requests().await.unwrap();
        let batch = requests
            .iter()
            .find(|r| r.url.path().ends_with(":batchUpdate"))
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&batch.body).unwrap();
        let request = &body["requests"][0]["deleteDuplicates"];
        assert_eq!(
            request["range"],
            serde_json::json!({
                "sheetId": 0, "startRowIndex": 1, "endRowIndex": 10,
                "startColumnIndex": 0, "endColumnIndex": 3
            })
        );
        assert_eq!(
            request["comparisonColumns"],
            serde_json::json!([
                {"sheetId": 0, "dimension": "COLUMNS", "startIndex": 0, "endIndex": 1},
                {"sheetId": 0, "dimension": "COLUMNS", "startIndex": 2, "endIndex": 3}
            ])
        );
    }

    /// The API's "analyze all columns" default is an *absent* field, not
    /// an empty array.
    #[tokio::test]
    async fn no_comparison_columns_omits_the_field_entirely() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        mount_batch_update(serde_json::json!({"replies": [{}]}))
            .mount(&server)
            .await;
        let _ = delete_duplicates(&drive, &sheets, &options(false), &[rule()]).await;
        let requests = server.received_requests().await.unwrap();
        let batch = requests
            .iter()
            .find(|r| r.url.path().ends_with(":batchUpdate"))
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&batch.body).unwrap();
        assert!(body["requests"][0]["deleteDuplicates"]
            .get("comparisonColumns")
            .is_none());
    }

    #[tokio::test]
    async fn a_reply_without_a_delete_duplicates_object_reports_removed_with_no_count() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        mount_batch_update(serde_json::json!({"replies": [{}]}))
            .mount(&server)
            .await;
        let outcome = delete_duplicates(&drive, &sheets, &options(false), &[rule()]).await;
        assert!(
            matches!(
                outcome.result,
                DeleteDuplicatesResult::Removed {
                    duplicates_removed_count: None,
                    ..
                }
            ),
            "{outcome:?}"
        );
        assert!(describe_lines(&outcome)
            .join("\n")
            .contains("reported no count"));
    }

    // ── refusals ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn an_open_ended_range_is_refused_so_it_cannot_reach_past_the_data() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        let mut opts = options(true);
        opts.range = Some("A:C".into());
        let outcome = delete_duplicates(&drive, &sheets, &opts, &[rule()]).await;
        let DeleteDuplicatesResult::RefusedInvalidRequest { detail } = &outcome.result else {
            panic!("expected a refusal"); // omni-dev: coverage ignore-line reason="this let-else panic only runs if the match failed to bind the expected variant; an open-ended range always refuses here"
        };
        assert!(detail.contains("open-ended"), "{detail}");
        assert!(detail.contains("past the data"), "{detail}");
    }

    #[tokio::test]
    async fn a_comparison_column_outside_the_range_is_refused_before_mutating() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        let mut opts = options(false);
        opts.comparison_columns = vec![4];
        let outcome = delete_duplicates(&drive, &sheets, &opts, &[rule()]).await;
        assert!(
            matches!(
                outcome.result,
                DeleteDuplicatesResult::RefusedInvalidRequest { .. }
            ),
            "{outcome:?}"
        );
        assert!(!server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|r| r.url.path().ends_with(":batchUpdate")));
    }

    #[tokio::test]
    async fn a_repeated_comparison_column_is_refused_before_any_network_call() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        let mut opts = options(true);
        opts.comparison_columns = vec![1, 1];
        let outcome = delete_duplicates(&drive, &sheets, &opts, &[rule()]).await;
        assert!(matches!(
            outcome.result,
            DeleteDuplicatesResult::RefusedInvalidRequest { .. }
        ));
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_unknown_sheet_is_refused_with_the_available_titles() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        let mut opts = options(true);
        opts.sheet = Some("Nope".into());
        let outcome = delete_duplicates(&drive, &sheets, &opts, &[rule()]).await;
        assert_eq!(
            outcome.result,
            DeleteDuplicatesResult::RefusedSheetNotFound {
                title: "Nope".into(),
                available: vec!["Q1".into()],
            }
        );
    }

    #[tokio::test]
    async fn a_batch_update_failure_surfaces_as_failed() {
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
        let outcome = delete_duplicates(&drive, &sheets, &options(false), &[rule()]).await;
        assert!(
            matches!(outcome.result, DeleteDuplicatesResult::Failed { .. }),
            "{outcome:?}"
        );
    }

    // ── the Drive write lease (ADR-0080 §9) ─────────────────────────────

    #[tokio::test]
    async fn refuses_without_a_lease_when_the_rule_requires_one() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        // No batchUpdate mock: a refusal must make zero mutating calls.
        let outcome =
            delete_duplicates(&drive, &sheets, &options(false), &[rule_requiring_lease()]).await;
        assert_eq!(outcome.result, DeleteDuplicatesResult::RefusedNoLease);
    }

    #[tokio::test]
    async fn refuses_a_lease_bound_to_a_different_file() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("leases.json");
        let token = seed_lease(&ledger_path, "other-file", "1");
        let mut opts = options(false);
        opts.lease_token = Some(token);
        opts.ledger_path = ledger_path;
        let outcome = delete_duplicates(&drive, &sheets, &opts, &[rule_requiring_lease()]).await;
        assert_eq!(
            outcome.result,
            DeleteDuplicatesResult::RefusedLeaseWrongFile
        );
    }

    /// The end-to-end shape of the recovery message: the backup the engine
    /// reports must be the one the ledger actually recorded.
    #[tokio::test]
    async fn a_leased_run_reports_the_ledgers_own_backup() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        mount_batch_update(serde_json::json!({
            "replies": [{"deleteDuplicates": {"duplicatesRemovedCount": 1}}]
        }))
        .mount(&server)
        .await;
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("leases.json");
        let token = seed_lease_with_backup(
            &ledger_path,
            "sheet-1",
            "1",
            LeaseBackup::DriveCopy {
                file_id: "copy-42".into(),
            },
        );
        let mut opts = options(false);
        opts.lease_token = Some(token);
        opts.ledger_path = ledger_path;
        let outcome = delete_duplicates(&drive, &sheets, &opts, &[rule_requiring_lease()]).await;
        assert!(
            matches!(
                &outcome.result,
                DeleteDuplicatesResult::Removed { backup: Some(backup), .. }
                    if **backup == LeaseBackup::DriveCopy { file_id: "copy-42".into() }
            ),
            "{outcome:?}"
        );
        assert!(describe_lines(&outcome).join("\n").contains("copy-42"));
    }

    /// A `require_lease: false` rule reaches the write with no lease and
    /// therefore no backup, so the message must fall back to "version
    /// history is the only recovery path" rather than invent a copy.
    #[tokio::test]
    async fn an_unleased_run_never_claims_a_backup_exists() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        mount_batch_update(serde_json::json!({"replies": [{}]}))
            .mount(&server)
            .await;
        let outcome = delete_duplicates(&drive, &sheets, &options(false), &[rule()]).await;
        let lines = describe_lines(&outcome).join("\n");
        assert!(lines.contains("no lease backup was taken"), "{lines}");
        assert!(
            lines.contains("version history is the only recovery path"),
            "{lines}"
        );
    }
}
