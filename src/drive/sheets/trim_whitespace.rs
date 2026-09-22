//! `trim-whitespace` — trim whitespace in every cell of a range (issue
//! #1844).
//!
//! [ADR-0083](../../../docs/adrs/adr-0083.md) §1 places this under
//! `DriveOperation::SheetsWrite`: it rewrites ordinary cell content in
//! place, which a `sheets clear` followed by a `sheets write` of the same
//! range could already do under that grant.
//!
//! The trim rule, measured live against the API (issue #1844): leading
//! and trailing whitespace is stripped **and each internal run collapses
//! to a single space** (`a   b` becomes `a b`), a cell of nothing but
//! whitespace becomes blank, a formula's text is untouched, and text that
//! trims to something starting `=` or `+` stays a string rather than
//! becoming a formula.
//!
//! **The preview never predicts which cells change.** §6 puts this verb in
//! the first `--dry-run` tier — one bounded read, reporting the *count and
//! A1 locations* of the range's non-blank cells and never their values —
//! but stops short of naming the cells that *would* change, because
//! deciding that means reproducing the server's trim rule locally, the
//! same "preview that lies" risk §6 declined for `findReplace`. So every
//! non-blank cell is a candidate and the wording is always "may be
//! trimmed". The gap is real, not theoretical: a live run over a 13-cell
//! range previewed all 13 as candidates and the server changed 5.
//!
//! **The read happens on `--dry-run` only**, unlike `auto_fill.rs` and
//! `paste.rs`, which read on both paths so their real runs can report the
//! same cells. This verb's real run has something strictly better: the
//! server's own `cellsChangedCount`, an exact count of what it actually
//! changed. Reading the range again would spend a call to print a worse
//! number beside the good one.

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
use crate::drive::sheets::api::{SheetsApi, ValueRenderOption};
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::types::{
    BatchUpdateRequestItem, GridRange, Sheet, Spreadsheet, TrimWhitespaceRequest,
};
use crate::drive::sheets::{a1, grid_range, target_gate};
use crate::drive::types::SheetTargetRefusal;
use crate::drive::write_gate::{self, DecidingRule, DriveOperation, FolderPermissionRule};
use crate::request_log::{self, DriveMutationOutcome};

const LOG_OPERATION: &str = "sheets-trim-whitespace";

/// Which cells the caller named.
///
/// `--whole-sheet` is offered here but deliberately *not* on
/// `delete-duplicates` — see that module's docs for why a whole-sheet
/// dedupe is a different proposition from a whole-sheet trim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrimScope {
    /// A `--range`, already carrying its own `Sheet!` prefix or supplied
    /// one from `--sheet`. May be open-ended (`A:A`); open bounds are
    /// materialised from the sheet's grid extent.
    Range,
    /// `--whole-sheet`: every cell of the sheet named by `--sheet`.
    WholeSheet,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrimWhitespaceOptions {
    pub spreadsheet_id: String,
    pub sheet: Option<String>,
    pub range: Option<String>,
    pub scope: TrimScope,
    pub dry_run: bool,
    pub lease_token: Option<String>,
    pub ledger_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum TrimWhitespaceResult {
    WouldChange {
        /// The range as sent, with any open bound materialised.
        range: String,
        /// Every non-blank cell in the range, as bare A1 addresses —
        /// **never their values**. An upper bound on what the server will
        /// actually trim, never a prediction: see the module docs.
        candidate_cells: Vec<String>,
        /// The candidate list is always an upper bound, including when it
        /// is empty. The server alone decides which cells actually change.
        candidate_cells_upper_bound: bool,
        /// The values read covered only the part of the requested range
        /// inside the sheet's currently allocated grid.
        read_clamped_to_sheet: bool,
    },
    Changed {
        range: String,
        /// The server's own count. `None` when the reply carried no
        /// `trimWhitespace` object — `find-replace`'s precedent for a
        /// response that succeeded but reported nothing.
        cells_changed_count: Option<i64>,
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

impl FromLeaseRefusal for TrimWhitespaceResult {
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

impl TrimWhitespaceResult {
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
pub struct TrimWhitespaceOutcome {
    pub spreadsheet_id: String,
    pub file_name: Option<String>,
    pub resolved_folder_id: Option<String>,
    pub result: TrimWhitespaceResult,
}

impl JsonlSerialize for TrimWhitespaceOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> anyhow::Result<()> {
        write_scalar_jsonl(self, out)
    }
}

pub async fn trim_whitespace(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &TrimWhitespaceOptions,
    rules: &[FolderPermissionRule],
) -> TrimWhitespaceOutcome {
    let started = Instant::now();
    let outcome = trim_whitespace_inner(drive, sheets, opts, rules).await;
    if !opts.dry_run {
        record_attempt(&outcome, started.elapsed());
    }
    outcome
}

async fn trim_whitespace_inner(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &TrimWhitespaceOptions,
    rules: &[FolderPermissionRule],
) -> TrimWhitespaceOutcome {
    let bare = |result| TrimWhitespaceOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: None,
        resolved_folder_id: None,
        result,
    };
    if let Err(detail) = validate_scope_syntax(opts) {
        return bare(TrimWhitespaceResult::RefusedInvalidRequest { detail });
    }
    // Composed only for the `Range` scope: `--whole-sheet` resolves its
    // sheet id by title instead, so there is no range string to compose.
    let composed = match opts.scope {
        TrimScope::WholeSheet => None,
        TrimScope::Range => match a1::compose(opts.sheet.as_deref(), opts.range.as_deref()) {
            Ok(range) => Some(range),
            Err(err) => {
                return bare(TrimWhitespaceResult::RefusedInvalidRequest {
                    detail: err.to_string(),
                })
            }
        },
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
            return bare(TrimWhitespaceResult::Failed { detail })
        }
        target_gate::TargetGateOutcome::Refused { target, refusal } => {
            let result = match refusal {
                SheetTargetRefusal::Shortcut => TrimWhitespaceResult::RefusedShortcut,
                SheetTargetRefusal::NotASpreadsheet { mime_type } => {
                    TrimWhitespaceResult::RefusedNotASpreadsheet { mime_type }
                }
                SheetTargetRefusal::NoVisibleParents => {
                    TrimWhitespaceResult::RefusedNoVisibleParents
                }
            };
            return TrimWhitespaceOutcome {
                spreadsheet_id: opts.spreadsheet_id.clone(),
                file_name: Some(target.name),
                resolved_folder_id: None,
                result,
            };
        }
        target_gate::TargetGateOutcome::GateFetchFailed { target, detail } => {
            return TrimWhitespaceOutcome {
                spreadsheet_id: opts.spreadsheet_id.clone(),
                file_name: Some(target.name),
                resolved_folder_id: None,
                result: TrimWhitespaceResult::Failed { detail },
            }
        }
        target_gate::TargetGateOutcome::Gated {
            target,
            decision,
            resolved_folder_id,
            requires_lease,
        } => (target, decision, resolved_folder_id, requires_lease),
    };
    let gated = |result| TrimWhitespaceOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: Some(target.name.clone()),
        resolved_folder_id: resolved_folder_id.clone(),
        result,
    };
    if decision.verdict == write_gate::Verdict::Deny {
        return gated(TrimWhitespaceResult::Blocked {
            decided_by: decision.decided_by,
        });
    }
    let api = SheetsApi::new(sheets);
    let workbook = match api.get_spreadsheet(&opts.spreadsheet_id).await {
        Ok(book) => book,
        Err(err) => {
            return gated(TrimWhitespaceResult::Failed {
                detail: format!("{err:#}"),
            })
        }
    };
    let (sheet_title, grid) = match resolve_scope(opts, composed.as_deref(), &workbook) {
        Ok(value) => value,
        Err(result) => return gated(result),
    };
    // Open bounds are materialised from the sheet's own extent so the
    // request, the reported range and the preview read all describe the
    // same rectangle. A `--whole-sheet` request could equally be sent with
    // every bound absent, but then the outcome could not name what it
    // acted on.
    let grid = materialise_bounds(
        &grid,
        grid_range::find_sheet_by_id(&workbook, grid.sheet_id),
    );
    let Some(range_a1) = grid_range::bounded_range_to_a1(&sheet_title, &grid) else {
        return gated(TrimWhitespaceResult::RefusedInvalidRequest {
            detail: format!(
                "'{}' resolves to no cells in the sheet's current grid",
                composed.as_deref().unwrap_or(&sheet_title)
            ),
        });
    };

    // Built before the gate, not after — #1688/#1742's ordering invariant,
    // shared verbatim by every leased engine.
    let request = BatchUpdateRequestItem::TrimWhitespace(TrimWhitespaceRequest { range: grid });

    if opts.dry_run {
        // A bounded request can still extend past the allocated grid. The
        // mutation keeps the caller's range, but values.get must read only
        // its intersection with the sheet's current cells.
        let Some(read_grid) = grid_range::clamp_to_sheet(&workbook, &grid) else {
            return gated(TrimWhitespaceResult::RefusedInvalidRequest {
                detail: format!("{range_a1} has no cells inside the sheet's current grid"),
            });
        };
        let Some(read_a1) = grid_range::bounded_range_to_a1(&sheet_title, &read_grid) else {
            return gated(TrimWhitespaceResult::RefusedInvalidRequest {
                detail: format!("{range_a1} has no cells inside the sheet's current grid"),
            });
        };
        let candidate_cells =
            match read_non_blank(&api, &opts.spreadsheet_id, &read_a1, &read_grid).await {
                Ok(cells) => cells,
                Err(detail) => return gated(TrimWhitespaceResult::Failed { detail }),
            };
        return gated(TrimWhitespaceResult::WouldChange {
            range: range_a1,
            candidate_cells,
            candidate_cells_upper_bound: true,
            read_clamped_to_sheet: read_grid != grid,
        });
    }

    let leased = LeasedWrite {
        log_prefix: "drive sheets trim-whitespace",
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
        Ok(response) => TrimWhitespaceResult::Changed {
            range: range_a1,
            cells_changed_count: response
                .replies
                .into_iter()
                .next()
                .and_then(|reply| reply.trim_whitespace)
                .map(|trim| trim.cells_changed_count),
        },
        Err(err) => TrimWhitespaceResult::Failed {
            detail: format!("{err:#}"),
        },
    };
    drop(lease_grant);
    gated(result)
}

/// Refuses the scope combinations clap cannot express on its own, so the
/// engine is safe for a non-clap caller too — `find_replace.rs`'s pattern.
fn validate_scope_syntax(opts: &TrimWhitespaceOptions) -> Result<(), String> {
    match opts.scope {
        TrimScope::WholeSheet => {
            if opts.range.is_some() {
                return Err("--whole-sheet cannot be combined with --range".to_string());
            }
            if opts.sheet.as_ref().is_none_or(|s| s.trim().is_empty()) {
                return Err("--whole-sheet needs --sheet to name the sheet to trim".to_string());
            }
            Ok(())
        }
        TrimScope::Range => {
            if opts.range.as_ref().is_none_or(|r| r.trim().is_empty()) {
                return Err("pass --range, or --whole-sheet to trim an entire sheet".to_string());
            }
            Ok(())
        }
    }
}

/// The sheet title and grid range the caller's scope names.
fn resolve_scope(
    opts: &TrimWhitespaceOptions,
    composed: Option<&str>,
    workbook: &Spreadsheet,
) -> Result<(String, GridRange), TrimWhitespaceResult> {
    match opts.scope {
        TrimScope::WholeSheet => {
            let title = opts.sheet.clone().unwrap_or_default();
            let sheet_id = grid_range::find_sheet_id(workbook, &title, |title, available| {
                TrimWhitespaceResult::RefusedSheetNotFound { title, available }
            })?;
            Ok((
                title,
                GridRange {
                    sheet_id,
                    start_row_index: None,
                    end_row_index: None,
                    start_column_index: None,
                    end_column_index: None,
                },
            ))
        }
        TrimScope::Range => {
            let composed = composed.unwrap_or_default();
            grid_range::resolve_grid_range(
                workbook,
                composed,
                |detail| TrimWhitespaceResult::RefusedInvalidRequest { detail },
                |title, available| TrimWhitespaceResult::RefusedSheetNotFound { title, available },
            )
        }
    }
}

/// Fills any absent bound of `grid` from `sheet`'s current grid extent.
///
/// A bound the sheet reports no count for is left absent, as is every
/// bound when `sheet` is `None`. Either way the caller treats a range that
/// still isn't fully bounded as naming no cells, since it cannot be read
/// or rendered — which is also why a missing sheet needs no branch of its
/// own here: it cannot happen (the id came from this same workbook a
/// moment earlier) and would refuse correctly if it did.
fn materialise_bounds(grid: &GridRange, sheet: Option<&Sheet>) -> GridRange {
    let props = sheet
        .and_then(|sheet| sheet.properties.as_ref())
        .and_then(|p| p.grid_properties.as_ref());
    GridRange {
        sheet_id: grid.sheet_id,
        start_row_index: grid.start_row_index.or(Some(0)),
        end_row_index: grid
            .end_row_index
            .or_else(|| props.and_then(|g| g.row_count)),
        start_column_index: grid.start_column_index.or(Some(0)),
        end_column_index: grid
            .end_column_index
            .or_else(|| props.and_then(|g| g.column_count)),
    }
}

/// The range's non-blank cells as A1 addresses, from one `values.get`.
///
/// `grid` has already been clamped to the sheet's current extent by the
/// caller; the batch request itself retains the caller's original range.
async fn read_non_blank(
    api: &SheetsApi<'_>,
    spreadsheet_id: &str,
    range_a1: &str,
    grid: &GridRange,
) -> Result<Vec<String>, String> {
    let values = api
        .values_get(spreadsheet_id, range_a1, ValueRenderOption::Formatted)
        .await
        .map_err(|err| format!("{err:#}"))?;
    Ok(grid_range::non_blank_locations(
        &values,
        grid.start_row_index.unwrap_or(0),
        grid.start_column_index.unwrap_or(0),
    ))
}

fn record_attempt(outcome: &TrimWhitespaceOutcome, duration: Duration) {
    let decided_by = match &outcome.result {
        TrimWhitespaceResult::Blocked { decided_by } => decided_by.as_ref(),
        _ => None,
    };
    let decided_by = write_gate::decided_by_log_fields(decided_by);
    let fields_changed = match &outcome.result {
        TrimWhitespaceResult::Changed {
            range,
            cells_changed_count,
        } => Some(match cells_changed_count {
            Some(count) => format!("trimmed {count} cell(s) in {range}"),
            None => format!("trimmed {range}; the API reported no count"),
        }),
        _ => None,
    };
    let error = match &outcome.result {
        TrimWhitespaceResult::RefusedInvalidRequest { detail }
        | TrimWhitespaceResult::Failed { detail } => Some(detail.clone()),
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

pub fn describe_lines(outcome: &TrimWhitespaceOutcome) -> Vec<String> {
    let book = outcome.file_name.as_deref().map_or_else(
        || format!("'{}'", outcome.spreadsheet_id),
        |name| format!("'{name}'"),
    );
    match &outcome.result {
        TrimWhitespaceResult::WouldChange {
            range,
            candidate_cells,
            read_clamped_to_sheet,
            ..
        } => {
            let mut lines = vec![
                format!("Would trim whitespace in {range} of {book}"),
                candidate_line(candidate_cells),
            ];
            if *read_clamped_to_sheet {
                lines.push(
                    "  the preview read was clipped to the sheet's current grid; the real run sends the requested range to Sheets".to_string(),
                );
            }
            lines
        }
        TrimWhitespaceResult::Changed {
            range,
            cells_changed_count,
        } => vec![match cells_changed_count {
            Some(count) => format!("Trimmed whitespace in {count} cell(s) of {range} in {book}"),
            None => {
                format!("Trimmed whitespace in {range} of {book}; the API reported no cell count")
            }
        }],
        TrimWhitespaceResult::RefusedInvalidRequest { detail } => {
            vec![format!("Refused: {detail}")]
        }
        TrimWhitespaceResult::RefusedNotASpreadsheet { mime_type } => vec![format!(
            "Refused: {book} is not a Google Sheet (mimeType: {mime_type})"
        )],
        TrimWhitespaceResult::RefusedShortcut => vec![format!(
            "Refused: {book} is a shortcut; trim-whitespace doesn't follow shortcuts"
        )],
        TrimWhitespaceResult::RefusedNoVisibleParents => {
            vec![format!("Refused: {book} has no visible parent folder")]
        }
        TrimWhitespaceResult::RefusedSheetNotFound { title, available } => vec![format!(
            "Refused: {book} has no sheet titled '{title}'. Available: {}",
            available.join(", ")
        )],
        TrimWhitespaceResult::Blocked { .. } => vec![format!(
            "Blocked: trim-whitespace on {book} requires an allowing sheets-write rule"
        )],
        TrimWhitespaceResult::RefusedNoLease => LeaseGateRefusal::NoLease
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        TrimWhitespaceResult::RefusedLeaseExpired => LeaseGateRefusal::Expired
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        TrimWhitespaceResult::RefusedLeaseWrongFile => LeaseGateRefusal::WrongFile
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        TrimWhitespaceResult::RefusedLeaseStale => LeaseGateRefusal::Stale
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        TrimWhitespaceResult::Failed { detail } => vec![format!("Failed: {detail}")],
    }
}

/// The indented candidate line. Always "may be trimmed", never "would be":
/// the server owns the trim rule, so a non-blank cell is a candidate, not
/// a prediction (module docs; ADR-0083 §6).
fn candidate_line(candidate_cells: &[String]) -> String {
    if candidate_cells.is_empty() {
        return "  no non-blank cells in the range".to_string();
    }
    format!(
        "  {} non-blank cell(s) may be trimmed: {}",
        candidate_cells.len(),
        grid_range::truncate_locations(candidate_cells, grid_range::RENDERED_LOCATION_LIMIT)
    )
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

    /// A `values.get` reply for any range, carrying `values`.
    fn mount_values(values: serde_json::Value) -> wiremock::Mock {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path_regex(
                r"^/v4/spreadsheets/sheet-1/values/.*$",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"values": values})),
            )
    }

    fn mount_batch_update(body: serde_json::Value) -> wiremock::Mock {
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
    }

    fn options(dry_run: bool) -> TrimWhitespaceOptions {
        TrimWhitespaceOptions {
            spreadsheet_id: "sheet-1".into(),
            sheet: Some("Q1".into()),
            range: Some("A2:C10".into()),
            scope: TrimScope::Range,
            dry_run,
            lease_token: None,
            ledger_path: PathBuf::from("/tmp/unused-trim-whitespace-ledger"),
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

    // ── scope validation (pure) ─────────────────────────────────────────

    #[test]
    fn whole_sheet_refuses_a_range_and_demands_a_sheet() {
        let mut opts = options(true);
        opts.scope = TrimScope::WholeSheet;
        assert!(validate_scope_syntax(&opts)
            .unwrap_err()
            .contains("cannot be combined with --range"));

        opts.range = None;
        opts.sheet = None;
        assert!(validate_scope_syntax(&opts)
            .unwrap_err()
            .contains("needs --sheet"));

        // A blank `--sheet` is as useless as an absent one.
        opts.sheet = Some("   ".into());
        assert!(validate_scope_syntax(&opts).is_err());

        opts.sheet = Some("Q1".into());
        assert!(validate_scope_syntax(&opts).is_ok());
    }

    #[test]
    fn range_scope_demands_a_non_empty_range_and_names_the_alternative() {
        let mut opts = options(true);
        opts.range = None;
        let err = validate_scope_syntax(&opts).unwrap_err();
        assert!(err.contains("--range"), "{err}");
        assert!(err.contains("--whole-sheet"), "{err}");

        opts.range = Some("  ".into());
        assert!(validate_scope_syntax(&opts).is_err());
    }

    // ── bound materialisation (pure) ────────────────────────────────────

    fn sheet_with(row_count: Option<i64>, column_count: Option<i64>) -> Sheet {
        let json = serde_json::json!({"properties": {
            "sheetId": 0, "title": "Q1",
            "gridProperties": {"rowCount": row_count, "columnCount": column_count}
        }});
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn materialise_bounds_fills_open_axes_from_the_sheet_extent() {
        let open = GridRange {
            sheet_id: 0,
            start_row_index: None,
            end_row_index: None,
            start_column_index: None,
            end_column_index: None,
        };
        let filled = materialise_bounds(&open, Some(&sheet_with(Some(100), Some(6))));
        assert_eq!(filled.start_row_index, Some(0));
        assert_eq!(filled.end_row_index, Some(100));
        assert_eq!(filled.start_column_index, Some(0));
        assert_eq!(filled.end_column_index, Some(6));
    }

    #[test]
    fn materialise_bounds_leaves_a_bound_the_sheet_reports_no_count_for() {
        let open = GridRange {
            sheet_id: 0,
            start_row_index: None,
            end_row_index: None,
            start_column_index: Some(0),
            end_column_index: Some(3),
        };
        let filled = materialise_bounds(&open, Some(&sheet_with(None, Some(6))));
        // A start always materialises to 0; an end can only come from the
        // sheet, so it stays absent and the caller refuses the range.
        assert_eq!(filled.start_row_index, Some(0));
        assert_eq!(filled.end_row_index, None);
    }

    /// With no sheet the ends cannot be filled, so the range stays
    /// unbounded and the caller refuses it as naming no cells — the
    /// behaviour that lets the engine skip a "sheet vanished" branch.
    #[test]
    fn materialise_bounds_without_a_sheet_leaves_the_ends_open() {
        let open = GridRange {
            sheet_id: 0,
            start_row_index: None,
            end_row_index: None,
            start_column_index: None,
            end_column_index: None,
        };
        let filled = materialise_bounds(&open, None);
        assert_eq!(filled.end_row_index, None);
        assert_eq!(filled.end_column_index, None);
        assert!(!grid_range::is_bounded(&filled));
    }

    #[test]
    fn materialise_bounds_never_widens_an_already_bounded_range() {
        let bounded = GridRange {
            sheet_id: 0,
            start_row_index: Some(1),
            end_row_index: Some(10),
            start_column_index: Some(0),
            end_column_index: Some(3),
        };
        assert_eq!(
            materialise_bounds(&bounded, Some(&sheet_with(Some(100), Some(6)))),
            bounded
        );
    }

    // ── rendering (pure) ────────────────────────────────────────────────

    #[test]
    fn the_candidate_line_says_may_be_trimmed_never_would_be() {
        let line = candidate_line(&["A1".to_string(), "B2".to_string()]);
        assert!(
            line.contains("2 non-blank cell(s) may be trimmed"),
            "{line}"
        );
        assert!(!line.contains("would be"), "{line}");
        assert_eq!(candidate_line(&[]), "  no non-blank cells in the range");
    }

    #[test]
    fn the_candidate_line_elides_past_the_render_limit_but_keeps_the_true_count() {
        let cells: Vec<String> = (1..=120).map(|row| format!("A{row}")).collect();
        let line = candidate_line(&cells);
        assert!(line.contains("120 non-blank cell(s)"), "{line}");
        assert!(line.contains("… and 70 more"), "{line}");
        assert!(!line.contains("A120"), "{line}");
    }

    #[test]
    fn lease_refusals_map_to_their_trim_whitespace_results() {
        assert_eq!(
            TrimWhitespaceResult::from_no_lease(),
            TrimWhitespaceResult::RefusedNoLease
        );
        assert_eq!(
            TrimWhitespaceResult::from_lease_expired(),
            TrimWhitespaceResult::RefusedLeaseExpired
        );
        assert_eq!(
            TrimWhitespaceResult::from_lease_wrong_file(),
            TrimWhitespaceResult::RefusedLeaseWrongFile
        );
        assert_eq!(
            TrimWhitespaceResult::from_lease_stale(),
            TrimWhitespaceResult::RefusedLeaseStale
        );
        assert_eq!(
            TrimWhitespaceResult::from_lease_failed("boom".into()),
            TrimWhitespaceResult::Failed {
                detail: "boom".into()
            }
        );
    }

    fn every_result() -> Vec<TrimWhitespaceResult> {
        vec![
            TrimWhitespaceResult::WouldChange {
                range: "Q1!A2:C10".into(),
                candidate_cells: vec!["A2".into()],
                candidate_cells_upper_bound: true,
                read_clamped_to_sheet: false,
            },
            TrimWhitespaceResult::WouldChange {
                range: "Q1!A2:C10".into(),
                candidate_cells: Vec::new(),
                candidate_cells_upper_bound: true,
                read_clamped_to_sheet: false,
            },
            TrimWhitespaceResult::Changed {
                range: "Q1!A2:C10".into(),
                cells_changed_count: Some(3),
            },
            TrimWhitespaceResult::Changed {
                range: "Q1!A2:C10".into(),
                cells_changed_count: None,
            },
            TrimWhitespaceResult::RefusedInvalidRequest {
                detail: "bad".into(),
            },
            TrimWhitespaceResult::RefusedNotASpreadsheet {
                mime_type: "application/pdf".into(),
            },
            TrimWhitespaceResult::RefusedShortcut,
            TrimWhitespaceResult::RefusedNoVisibleParents,
            TrimWhitespaceResult::RefusedSheetNotFound {
                title: "Nope".into(),
                available: vec!["Q1".into()],
            },
            TrimWhitespaceResult::Blocked { decided_by: None },
            TrimWhitespaceResult::RefusedNoLease,
            TrimWhitespaceResult::RefusedLeaseExpired,
            TrimWhitespaceResult::RefusedLeaseWrongFile,
            TrimWhitespaceResult::RefusedLeaseStale,
            TrimWhitespaceResult::Failed {
                detail: "boom".into(),
            },
        ]
    }

    #[test]
    fn every_result_renders_and_carries_a_distinct_log_status() {
        let mut statuses = HashSet::new();
        for result in every_result() {
            let outcome = TrimWhitespaceOutcome {
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
        // 15 results, 13 distinct statuses: the two `WouldChange`s share
        // one and the two `Changed`s share another.
        assert_eq!(statuses.len(), 13);
    }

    #[test]
    fn describe_lines_falls_back_to_the_spreadsheet_id_with_no_file_name() {
        let outcome = TrimWhitespaceOutcome {
            spreadsheet_id: "sheet-1".into(),
            file_name: None,
            resolved_folder_id: None,
            result: TrimWhitespaceResult::RefusedShortcut,
        };
        assert!(describe_lines(&outcome).join("\n").contains("'sheet-1'"));
    }

    #[test]
    fn write_jsonl_emits_one_line_of_json() {
        let outcome = TrimWhitespaceOutcome {
            spreadsheet_id: "sheet-1".into(),
            file_name: Some("Budget".into()),
            resolved_folder_id: None,
            result: TrimWhitespaceResult::Changed {
                range: "Q1!A2:C10".into(),
                cells_changed_count: Some(3),
            },
        };
        let mut buf = Vec::new();
        outcome.write_jsonl(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains("\"cells_changed_count\":3"), "{text}");
    }

    // ── the gate ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn gate_denies_sheets_write_by_default() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        let outcome = trim_whitespace(&drive, &sheets, &options(true), &[]).await;
        assert!(
            matches!(outcome.result, TrimWhitespaceResult::Blocked { .. }),
            "{outcome:?}"
        );
    }

    /// The other half of `delete-duplicates`' non-widening test: the two
    /// verbs in issue #1844 take different operations, so neither grant
    /// may stand in for the other.
    #[tokio::test]
    async fn a_sheets_delete_grant_alone_does_not_permit_trim_whitespace() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        let outcome = trim_whitespace(
            &drive,
            &sheets,
            &options(true),
            &[rule_allowing(DriveOperation::SheetsDelete)],
        )
        .await;
        assert!(
            matches!(outcome.result, TrimWhitespaceResult::Blocked { .. }),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_denied_target_makes_no_sheets_calls_at_all() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        let _ = trim_whitespace(&drive, &sheets, &options(true), &[]).await;
        let requests = server.received_requests().await.unwrap();
        assert!(
            !requests
                .iter()
                .any(|request| request.url.path().starts_with("/v4/spreadsheets")),
            "a blocked run reached the Sheets API"
        );
    }

    // ── the two read paths ──────────────────────────────────────────────

    #[tokio::test]
    async fn dry_run_reads_values_once_and_never_mutates() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        mount_values(serde_json::json!([
            ["  padded  ", "", "x"],
            [null, "y", null]
        ]))
        .mount(&server)
        .await;
        let outcome = trim_whitespace(&drive, &sheets, &options(true), &[rule()]).await;
        let json = serde_json::to_value(&outcome).unwrap();
        assert_eq!(json["result"]["candidate_cells_upper_bound"], true);
        let TrimWhitespaceResult::WouldChange {
            range,
            candidate_cells,
            candidate_cells_upper_bound,
            read_clamped_to_sheet,
        } = outcome.result
        else {
            panic!("expected would-change"); // omni-dev: coverage ignore-line reason="this let-else panic only runs if the match failed to bind the expected variant; the mocked run always produces it"
        };
        assert_eq!(range, "'Q1'!A2:C10");
        // Offsets are the range's own start (row 1, column 0 zero-based).
        assert_eq!(candidate_cells, vec!["A2", "C2", "B3"]);
        assert!(candidate_cells_upper_bound);
        assert!(!read_clamped_to_sheet);

        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.url.path().contains("/values/"))
                .count(),
            1
        );
        assert!(
            !requests
                .iter()
                .any(|r| r.url.path().ends_with(":batchUpdate")),
            "a dry run mutated"
        );
    }

    #[tokio::test]
    async fn dry_run_clamps_an_explicit_range_to_the_sheet_grid_for_its_read() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        mount_values(serde_json::json!([["  padded  "]]))
            .mount(&server)
            .await;
        let mut opts = options(true);
        opts.range = Some("E90:J120".into());

        let outcome = trim_whitespace(&drive, &sheets, &opts, &[rule()]).await;
        assert_eq!(
            outcome.result,
            TrimWhitespaceResult::WouldChange {
                range: "'Q1'!E90:J120".into(),
                candidate_cells: vec!["E90".into()],
                candidate_cells_upper_bound: true,
                read_clamped_to_sheet: true,
            }
        );
        assert_eq!(
            serde_json::to_value(&outcome).unwrap()["result"]["read_clamped_to_sheet"],
            true
        );
        assert!(describe_lines(&outcome)
            .join("\n")
            .contains("preview read was clipped"));
        let requests = server.received_requests().await.unwrap();
        let reads: Vec<_> = requests
            .iter()
            .filter(|request| request.url.path().contains("/values/"))
            .collect();
        assert_eq!(reads.len(), 1);
        let read_path = reads[0].url.path();
        assert!(
            read_path.contains("E90") && read_path.contains("F100"),
            "{read_path}"
        );
        assert!(!read_path.contains("J120"), "{read_path}");
    }

    /// The deviation from `auto_fill.rs`/`paste.rs` recorded in the module
    /// docs: the real run reports the server's exact count, so re-reading
    /// the range would buy nothing.
    #[tokio::test]
    async fn real_run_sends_one_trim_request_and_reads_no_values() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        mount_batch_update(serde_json::json!({
            "replies": [{"trimWhitespace": {"cellsChangedCount": 7}}]
        }))
        .mount(&server)
        .await;
        let outcome = trim_whitespace(&drive, &sheets, &options(false), &[rule()]).await;
        assert_eq!(
            outcome.result,
            TrimWhitespaceResult::Changed {
                range: "'Q1'!A2:C10".into(),
                cells_changed_count: Some(7),
            }
        );

        let requests = server.received_requests().await.unwrap();
        assert!(
            !requests.iter().any(|r| r.url.path().contains("/values/")),
            "the real run read values it did not need"
        );
        let batch = requests
            .iter()
            .find(|r| r.url.path().ends_with(":batchUpdate"))
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&batch.body).unwrap();
        assert_eq!(
            body["requests"][0]["trimWhitespace"]["range"],
            serde_json::json!({
                "sheetId": 0, "startRowIndex": 1, "endRowIndex": 10,
                "startColumnIndex": 0, "endColumnIndex": 3
            })
        );
        assert_eq!(body["requests"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn whole_sheet_sends_the_materialised_grid_extent() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        mount_batch_update(serde_json::json!({"replies": [{}]}))
            .mount(&server)
            .await;
        let mut opts = options(false);
        opts.scope = TrimScope::WholeSheet;
        opts.range = None;
        let outcome = trim_whitespace(&drive, &sheets, &opts, &[rule()]).await;
        let TrimWhitespaceResult::Changed { range, .. } = &outcome.result else {
            panic!("expected changed"); // omni-dev: coverage ignore-line reason="this let-else panic only runs if the match failed to bind the expected variant; the mocked run always produces it"
        };
        assert_eq!(range, "'Q1'!A1:F100");

        let requests = server.received_requests().await.unwrap();
        let batch = requests
            .iter()
            .find(|r| r.url.path().ends_with(":batchUpdate"))
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&batch.body).unwrap();
        assert_eq!(
            body["requests"][0]["trimWhitespace"]["range"]["endColumnIndex"],
            6
        );
    }

    #[tokio::test]
    async fn a_reply_without_a_trim_object_reports_changed_with_no_count() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        mount_batch_update(serde_json::json!({"replies": [{}]}))
            .mount(&server)
            .await;
        let outcome = trim_whitespace(&drive, &sheets, &options(false), &[rule()]).await;
        assert_eq!(
            outcome.result,
            TrimWhitespaceResult::Changed {
                range: "'Q1'!A2:C10".into(),
                cells_changed_count: None,
            }
        );
        assert!(describe_lines(&outcome)
            .join("\n")
            .contains("reported no cell count"));
    }

    // ── the "never a cell value" guard (ADR-0083 §6) ─────────────────────

    #[tokio::test]
    async fn no_cell_value_ever_reaches_the_outcome_or_its_rendering() {
        const SECRET: &str = "  zzsecretzz  ";
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        mount_values(serde_json::json!([[SECRET]]))
            .mount(&server)
            .await;
        let outcome = trim_whitespace(&drive, &sheets, &options(true), &[rule()]).await;

        let json = serde_json::to_string(&outcome).unwrap();
        assert!(!json.contains("zzsecretzz"), "{json}");
        assert!(!describe_lines(&outcome).join("\n").contains("zzsecretzz"));
    }

    // ── failure paths ───────────────────────────────────────────────────

    #[tokio::test]
    async fn a_values_read_failure_surfaces_as_failed() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path_regex(
                r"^/v4/spreadsheets/sheet-1/values/.*$",
            ))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let outcome = trim_whitespace(&drive, &sheets, &options(true), &[rule()]).await;
        assert!(
            matches!(outcome.result, TrimWhitespaceResult::Failed { .. }),
            "{outcome:?}"
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
        let outcome = trim_whitespace(&drive, &sheets, &options(false), &[rule()]).await;
        assert!(
            matches!(outcome.result, TrimWhitespaceResult::Failed { .. }),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn an_unknown_sheet_is_refused_with_the_available_titles() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        let mut opts = options(true);
        opts.scope = TrimScope::WholeSheet;
        opts.sheet = Some("Nope".into());
        opts.range = None;
        let outcome = trim_whitespace(&drive, &sheets, &opts, &[rule()]).await;
        assert_eq!(
            outcome.result,
            TrimWhitespaceResult::RefusedSheetNotFound {
                title: "Nope".into(),
                available: vec!["Q1".into()],
            }
        );
    }

    #[tokio::test]
    async fn a_scope_refusal_happens_before_any_network_call() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        let mut opts = options(true);
        opts.range = None;
        let outcome = trim_whitespace(&drive, &sheets, &opts, &[rule()]).await;
        assert!(matches!(
            outcome.result,
            TrimWhitespaceResult::RefusedInvalidRequest { .. }
        ));
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    // ── the Drive write lease (ADR-0080 §9) ─────────────────────────────

    #[tokio::test]
    async fn refuses_without_a_lease_when_the_rule_requires_one() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        // No batchUpdate mock: a refusal must make zero mutating calls.
        let outcome =
            trim_whitespace(&drive, &sheets, &options(false), &[rule_requiring_lease()]).await;
        assert_eq!(outcome.result, TrimWhitespaceResult::RefusedNoLease);
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
        let outcome = trim_whitespace(&drive, &sheets, &opts, &[rule_requiring_lease()]).await;
        assert_eq!(outcome.result, TrimWhitespaceResult::RefusedLeaseWrongFile);
    }

    #[tokio::test]
    async fn a_valid_lease_lets_the_write_through() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_metadata(&server).await;
        mount_batch_update(serde_json::json!({
            "replies": [{"trimWhitespace": {"cellsChangedCount": 1}}]
        }))
        .mount(&server)
        .await;
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("leases.json");
        let token = seed_lease(&ledger_path, "sheet-1", "1");
        let mut opts = options(false);
        opts.lease_token = Some(token);
        opts.ledger_path = ledger_path;
        let outcome = trim_whitespace(&drive, &sheets, &opts, &[rule_requiring_lease()]).await;
        assert!(
            matches!(
                outcome.result,
                TrimWhitespaceResult::Changed {
                    cells_changed_count: Some(1),
                    ..
                }
            ),
            "{outcome:?}"
        );
    }
}
