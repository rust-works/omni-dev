//! Developer metadata (issue #1795,
//! [ADR-0081](../../../docs/adrs/adr-0081.md) §4).
//!
//! `set-developer-metadata`/`delete-developer-metadata` (via
//! `spreadsheets.batchUpdate`) and `search-developer-metadata` (via the
//! distinct `developerMetadata:search` endpoint).
//!
//! `set-developer-metadata`/`delete-developer-metadata` are gated by
//! [`DriveOperation::SheetsStructure`], shaped like `format.rs`:
//! `target_gate::resolve` for the target and gate, then a
//! `spreadsheets.get` (needed to resolve a `--sheet` title to its numeric
//! id, and to render a location back to a sheet *title* for display), then
//! a `developerMetadata:search` call — this module's `read_discarded_cells`
//! equivalent — that both decides what to do (create vs. update; what a
//! delete would remove) and backs the mandatory preview from the very same
//! read, then a `--dry-run` early return, then the mutation, then one
//! `drivemutation` record.
//!
//! **`search-developer-metadata` is read-only and consults no gate** —
//! [`search`] is a thin wrapper with no `target_gate::resolve` call, no
//! lease, matching `read.rs`'s own ungated shape.
//!
//! **Restricted to `DOCUMENT` visibility everywhere, in two layers.** Every
//! filter this module builds includes `visibility: DOCUMENT_VISIBILITY`, so
//! the server itself never returns or is asked to touch a `PROJECT`
//! entry — the primary mitigation. [`assert_document_visibility`] is the
//! defense-in-depth second layer, applied to every entry this module ever
//! reads, on every path (`Set`, `Delete`, `search`). There is no
//! `--visibility` flag on any of the three CLI commands, so no user input
//! can reach anything but [`DOCUMENT_VISIBILITY`].
//!
//! **`delete-developer-metadata` never assumes exactly one match.**
//! `DeleteDeveloperMetadataRequest.data_filter` is singular but can match
//! (and delete) several entries in one call — Sheets bulk-operates by
//! filter, not by id — so the preview lists every entry a real run would
//! remove, and a real run removes all of them in the one request.

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
    BatchUpdateRequestItem, CreateDeveloperMetadataRequest, DataFilter,
    DeleteDeveloperMetadataRequest, DeveloperMetadata, DeveloperMetadataLocation,
    DeveloperMetadataLookupFilter, DeveloperMetadataValueUpdate, Dimension, DimensionRange,
    NewDeveloperMetadata, Spreadsheet, UpdateDeveloperMetadataRequest, DOCUMENT_VISIBILITY,
};
use crate::drive::types::SheetTargetRefusal;
use crate::drive::write_gate::{self, DecidingRule, DriveOperation, FolderPermissionRule};
use crate::request_log::{self, DriveMutationOutcome};

/// Which mutation to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeveloperMetadataVerb {
    /// Create a new entry, or update every existing entry matching `key`
    /// and location.
    Set {
        /// The key this entry is looked up by.
        key: String,
        /// The value to write.
        value: String,
        /// Sheet title, when the location is sheet- or dimension-scoped.
        /// `None` (together with `dimension`/`start`/`end`) means
        /// spreadsheet-scoped.
        sheet: Option<String>,
        /// Rows or columns, when the location is a dimension span.
        dimension: Option<Dimension>,
        /// 1-based first row/column, inclusive, when scoped to a dimension.
        start: Option<i64>,
        /// 1-based last row/column, inclusive, when scoped to a dimension.
        end: Option<i64>,
    },
    /// Remove every entry matching `key` and location.
    Delete {
        /// The key to remove.
        key: String,
        /// See [`Self::Set::sheet`].
        sheet: Option<String>,
        /// See [`Self::Set::dimension`].
        dimension: Option<Dimension>,
        /// See [`Self::Set::start`].
        start: Option<i64>,
        /// See [`Self::Set::end`].
        end: Option<i64>,
    },
}

impl DeveloperMetadataVerb {
    /// The `operation` this verb records in the request log.
    const fn log_operation(&self) -> &'static str {
        match self {
            Self::Set { .. } => "sheets-set-developer-metadata",
            Self::Delete { .. } => "sheets-delete-developer-metadata",
        }
    }

    /// The CLI subcommand that spells this verb, for error messages.
    const fn label(&self) -> &'static str {
        match self {
            Self::Set { .. } => "set-developer-metadata",
            Self::Delete { .. } => "delete-developer-metadata",
        }
    }
}

/// Per-call options.
#[derive(Debug, Clone)]
pub struct DeveloperMetadataOptions {
    /// Spreadsheet id.
    pub spreadsheet_id: String,
    /// Which mutation to perform.
    pub verb: DeveloperMetadataVerb,
    /// Classify and describe only; never call `batchUpdate`.
    pub dry_run: bool,
    /// The lease token presented via `--lease`. Checked only when the
    /// deciding rule requires one (ADR-0080 §1/§9); `None` is only ever
    /// valid when it does not.
    pub lease_token: Option<String>,
    /// Path to the lease ledger the token is checked against.
    pub ledger_path: PathBuf,
}

/// One developer-metadata entry as reported in a preview or a completed
/// result — the key, value and location the ADR-0081 §4 preview names.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DeveloperMetadataEntry {
    /// The server-assigned id.
    pub metadata_id: i64,
    /// The key this entry is looked up by.
    pub key: String,
    /// The value stored under `key`.
    pub value: String,
    /// A human-readable rendering of where this entry is attached, e.g.
    /// `"sheet 'Q1'"` or `"row(s) 2-5 of sheet 'Q1'"`.
    pub location: String,
}

impl DeveloperMetadataEntry {
    fn from_entry(workbook: &Spreadsheet, entry: &DeveloperMetadata) -> Self {
        Self {
            metadata_id: entry.metadata_id,
            key: entry.metadata_key.clone(),
            value: entry.metadata_value.clone(),
            location: describe_location(workbook, &entry.location),
        }
    }
}

/// What happened (or, under `--dry-run`, would happen).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum DeveloperMetadataResult {
    /// `--dry-run`: no existing entry matched; a new one would be created.
    WouldCreate {
        /// The key that would be created.
        key: String,
        /// The value it would carry.
        value: String,
        /// Where it would be attached.
        location: String,
    },
    /// `--dry-run`: one or more existing entries matched; their value
    /// would change. `previous` is every matched entry as it stands now.
    WouldUpdate {
        /// The value every matched entry would be changed to.
        new_value: String,
        /// Every entry that matched, before the change.
        previous: Vec<DeveloperMetadataEntry>,
    },
    /// `--dry-run` for `delete-developer-metadata`: every entry that would
    /// be removed.
    WouldDelete {
        /// The entries that would be removed.
        entries: Vec<DeveloperMetadataEntry>,
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
    /// The `--sheet`/`--dimension`/`--start`/`--end` combination was
    /// invalid — a partial combination, or an out-of-order span.
    RefusedInvalidLocation {
        /// What was wrong and why.
        detail: String,
    },
    /// `delete-developer-metadata` found no entry matching `key` and the
    /// given location — nothing to preview or remove.
    RefusedNotFound {
        /// The key that was searched for.
        key: String,
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
    /// The file has moved since the lease's recorded `version`.
    RefusedLeaseStale,
    /// A new entry was created.
    Created {
        /// The key that was created.
        key: String,
        /// The value it carries.
        value: String,
        /// Where it is attached.
        location: String,
    },
    /// Every matched entry's value was changed.
    Updated {
        /// The value every matched entry was changed to.
        new_value: String,
        /// Every entry that matched, before the change.
        previous: Vec<DeveloperMetadataEntry>,
    },
    /// Every matched entry was removed.
    Deleted {
        /// The entries that were removed.
        entries: Vec<DeveloperMetadataEntry>,
    },
    /// An API or validation error.
    Failed {
        /// A human-readable summary of what failed.
        detail: String,
    },
}

impl FromLeaseRefusal for DeveloperMetadataResult {
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

impl DeveloperMetadataResult {
    fn log_status(&self) -> &'static str {
        match self {
            Self::WouldCreate { .. } | Self::WouldUpdate { .. } | Self::WouldDelete { .. } => {
                "would-change"
            }
            Self::RefusedNotASpreadsheet { .. } => "refused-not-a-spreadsheet",
            Self::RefusedShortcut => "refused-shortcut",
            Self::RefusedNoVisibleParents => "refused-no-visible-parents",
            Self::RefusedSheetNotFound { .. } => "refused-sheet-not-found",
            Self::RefusedInvalidLocation { .. } => "refused-invalid-range",
            Self::RefusedNotFound { .. } => "refused-not-found",
            Self::Blocked { .. } => "blocked",
            Self::RefusedNoLease => LeaseGateRefusal::NoLease.log_status(),
            Self::RefusedLeaseExpired => LeaseGateRefusal::Expired.log_status(),
            Self::RefusedLeaseWrongFile => LeaseGateRefusal::WrongFile.log_status(),
            Self::RefusedLeaseStale => LeaseGateRefusal::Stale.log_status(),
            Self::Created { .. } | Self::Updated { .. } | Self::Deleted { .. } => "changed",
            Self::Failed { .. } => "failed",
        }
    }
}

/// The full outcome of one attempt.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DeveloperMetadataOutcome {
    /// The spreadsheet acted on.
    pub spreadsheet_id: String,
    /// Its Drive file name, when the metadata fetch got that far.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    /// The folder the gate evaluated against.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_folder_id: Option<String>,
    /// Which mutation was attempted. Not serialised — carried so `describe`
    /// can render from the outcome alone.
    #[serde(skip)]
    pub verb: DeveloperMetadataVerb,
    /// What happened.
    pub result: DeveloperMetadataResult,
}

impl JsonlSerialize for DeveloperMetadataOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> anyhow::Result<()> {
        write_scalar_jsonl(self, out)
    }
}

fn find_sheet_id(workbook: &Spreadsheet, title: &str) -> Result<i64, DeveloperMetadataResult> {
    grid_range::find_sheet_id(workbook, title, |title, available| {
        DeveloperMetadataResult::RefusedSheetNotFound { title, available }
    })
}

/// Resolves `--sheet`/`--dimension`/`--start`/`--end` into exactly one of
/// the three location shapes Sheets models — inferred from which flags
/// were given, never from a separate `--scope` flag: none given means
/// spreadsheet-scoped, `--sheet` alone means sheet-scoped, and all four
/// together mean dimension-scoped (applying the same 1-based-inclusive to
/// 0-based-half-open conversion, and the same bounds check, as
/// `format.rs::resolve_target` does for `update-dimension-properties`).
/// Any other combination — a partial one — is refused rather than silently
/// falling back to a different scope than the caller likely intended.
fn resolve_location(
    workbook: &Spreadsheet,
    sheet: Option<&str>,
    dimension: Option<Dimension>,
    start: Option<i64>,
    end: Option<i64>,
) -> Result<DeveloperMetadataLocation, DeveloperMetadataResult> {
    match (sheet, dimension, start, end) {
        (None, None, None, None) => Ok(DeveloperMetadataLocation::spreadsheet()),
        (Some(sheet), None, None, None) => {
            let sheet_id = find_sheet_id(workbook, sheet)?;
            Ok(DeveloperMetadataLocation::sheet(sheet_id))
        }
        (Some(sheet), Some(dimension), Some(start), Some(end)) => {
            let sheet_id = find_sheet_id(workbook, sheet)?;
            if start < 1 || end < start {
                return Err(DeveloperMetadataResult::RefusedInvalidLocation {
                    detail: format!(
                        "--start must be at least 1 and --end must be >= --start (got \
                         --start {start} --end {end})"
                    ),
                });
            }
            Ok(DeveloperMetadataLocation::dimension(DimensionRange {
                sheet_id,
                dimension,
                start_index: start - 1,
                end_index: end,
            }))
        }
        (None, Some(_) | None, ..) if dimension.is_some() || start.is_some() || end.is_some() => {
            Err(DeveloperMetadataResult::RefusedInvalidLocation {
                detail: "--dimension/--start/--end need --sheet too".to_string(),
            })
        }
        _ => Err(DeveloperMetadataResult::RefusedInvalidLocation {
            detail: "--dimension, --start and --end must be given together (a row/column \
                     location), or none of the three alongside --sheet (a whole-sheet \
                     location), or none of --sheet/--dimension/--start/--end at all (a \
                     whole-spreadsheet location)"
                .to_string(),
        }),
    }
}

/// Converts a [`resolve_location`] error into a plain message, for
/// [`search`], which has no `DeveloperMetadataOutcome`/verb to attach a
/// structured result to.
fn location_error_to_string(result: DeveloperMetadataResult) -> String {
    match result {
        DeveloperMetadataResult::RefusedSheetNotFound { title, available } => {
            let list = if available.is_empty() {
                "none".to_string()
            } else {
                available
                    .iter()
                    .map(|t| format!("'{t}'"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            format!("no sheet titled '{title}'. Available: {list}")
        }
        DeveloperMetadataResult::RefusedInvalidLocation { detail } => detail,
        other => unreachable!(
            "resolve_location only ever returns RefusedSheetNotFound or \
             RefusedInvalidLocation, got {other:?}"
        ),
    }
}

/// Renders a location back to a human-readable string, for previews and
/// [`DeveloperMetadataEntry::location`].
fn describe_location(workbook: &Spreadsheet, location: &DeveloperMetadataLocation) -> String {
    if location.spreadsheet == Some(true) {
        return "the whole spreadsheet".to_string();
    }
    if let Some(sheet_id) = location.sheet_id {
        return format!("sheet {}", sheet_title_or_id(workbook, sheet_id));
    }
    if let Some(range) = &location.dimension_range {
        return format!(
            "{}(s) {}-{} of sheet {}",
            range.dimension.noun(),
            range.start_index + 1,
            range.end_index,
            sheet_title_or_id(workbook, range.sheet_id)
        );
    }
    "an unknown location".to_string()
}

fn sheet_title_or_id(workbook: &Spreadsheet, sheet_id: i64) -> String {
    workbook
        .sheets
        .iter()
        .filter_map(|sheet| sheet.properties.as_ref())
        .find(|props| props.sheet_id == Some(sheet_id))
        .map_or_else(
            || format!("id {sheet_id}"),
            |props| format!("'{}'", props.title),
        )
}

/// The defense-in-depth half of the DOCUMENT-only guarantee described in
/// the module doc comment: every entry this module ever reads must have
/// `visibility == DOCUMENT_VISIBILITY`. Every filter already restricts the
/// *search* to `DOCUMENT`, so this should never actually fire — a failure
/// here means the server returned something the filter should have
/// excluded, treated as a hard error worth surfacing loudly, never a
/// silent drop (issue #1795, ADR-0081 §4).
fn assert_document_visibility(entries: &[DeveloperMetadata]) -> Result<(), String> {
    for entry in entries {
        if entry.visibility != DOCUMENT_VISIBILITY {
            return Err(format!(
                "developerMetadata.search returned an entry (id {}) with visibility \
                 '{}', not '{DOCUMENT_VISIBILITY}' — refusing to act on or display it \
                 (issue #1795, ADR-0081 §4)",
                entry.metadata_id, entry.visibility
            ));
        }
    }
    Ok(())
}

/// Searches for entries matching `key` and/or `location`, asserting the
/// DOCUMENT-only guarantee on the result. Shared by [`developer_metadata`]
/// (via `Set`/`Delete`'s preview-and-decide read) and [`search`].
async fn find_document_metadata(
    api: &SheetsApi<'_>,
    spreadsheet_id: &str,
    key: Option<&str>,
    location: Option<&DeveloperMetadataLocation>,
) -> Result<Vec<DeveloperMetadata>, String> {
    let filter = build_filter(key, location);
    let response = api
        .search_developer_metadata(spreadsheet_id, vec![filter])
        .await
        .map_err(|err| format!("{err:#}"))?;
    let entries: Vec<DeveloperMetadata> = response
        .matched_developer_metadata
        .into_iter()
        .map(|matched| matched.developer_metadata)
        .collect();
    assert_document_visibility(&entries)?;
    Ok(entries)
}

/// Builds the one `DataFilter` shape every caller in this module sends —
/// always restricted to [`DOCUMENT_VISIBILITY`], `EXACT_LOCATION` matching
/// whenever a location is given (never `INTERSECTING_LOCATION`, which
/// would also match a dimension range merely straddling the requested
/// one).
fn build_filter(key: Option<&str>, location: Option<&DeveloperMetadataLocation>) -> DataFilter {
    DataFilter {
        developer_metadata_lookup: DeveloperMetadataLookupFilter {
            metadata_location: location.cloned(),
            metadata_key: key.map(str::to_string),
            visibility: Some(DOCUMENT_VISIBILITY.to_string()),
            location_matching_strategy: location.map(|_| "EXACT_LOCATION".to_string()),
        },
    }
}

/// Runs one `set-developer-metadata`/`delete-developer-metadata` mutation,
/// logging every attempt that isn't a dry run. Never returns `Err`,
/// matching `format.rs::format`.
pub async fn developer_metadata(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &DeveloperMetadataOptions,
    rules: &[FolderPermissionRule],
) -> DeveloperMetadataOutcome {
    let started = Instant::now();
    let outcome = developer_metadata_inner(drive, sheets, opts, rules).await;
    if !opts.dry_run {
        record_attempt(&outcome, opts, started.elapsed());
    }
    outcome
}

async fn developer_metadata_inner(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &DeveloperMetadataOptions,
    rules: &[FolderPermissionRule],
) -> DeveloperMetadataOutcome {
    let bare = |result| DeveloperMetadataOutcome {
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
            return bare(DeveloperMetadataResult::Failed { detail })
        }
        target_gate::TargetGateOutcome::Refused { target, refusal } => {
            let result = match refusal {
                SheetTargetRefusal::Shortcut => DeveloperMetadataResult::RefusedShortcut,
                SheetTargetRefusal::NotASpreadsheet { mime_type } => {
                    DeveloperMetadataResult::RefusedNotASpreadsheet { mime_type }
                }
                SheetTargetRefusal::NoVisibleParents => {
                    DeveloperMetadataResult::RefusedNoVisibleParents
                }
            };
            return DeveloperMetadataOutcome {
                spreadsheet_id: opts.spreadsheet_id.clone(),
                file_name: Some(target.name),
                resolved_folder_id: None,
                verb: opts.verb.clone(),
                result,
            };
        }
        target_gate::TargetGateOutcome::GateFetchFailed { target, detail } => {
            return DeveloperMetadataOutcome {
                spreadsheet_id: opts.spreadsheet_id.clone(),
                file_name: Some(target.name),
                resolved_folder_id: None,
                verb: opts.verb.clone(),
                result: DeveloperMetadataResult::Failed { detail },
            };
        }
        target_gate::TargetGateOutcome::Gated {
            target,
            decision,
            resolved_folder_id,
            requires_lease,
        } => (target, decision, resolved_folder_id, requires_lease),
    };

    let gated = |result| DeveloperMetadataOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: Some(target.name.clone()),
        resolved_folder_id: resolved_folder_id.clone(),
        verb: opts.verb.clone(),
        result,
    };

    if decision.verdict == write_gate::Verdict::Deny {
        return gated(DeveloperMetadataResult::Blocked {
            decided_by: decision.decided_by,
        });
    }

    let api = SheetsApi::new(sheets);
    let workbook = match api.get_spreadsheet(&opts.spreadsheet_id).await {
        Ok(workbook) => workbook,
        Err(err) => {
            return gated(DeveloperMetadataResult::Failed {
                detail: format!("{err:#}"),
            })
        }
    };

    let (key, sheet, dimension, start, end): (
        &str,
        Option<&str>,
        Option<Dimension>,
        Option<i64>,
        Option<i64>,
    ) = match &opts.verb {
        DeveloperMetadataVerb::Set {
            key,
            sheet,
            dimension,
            start,
            end,
            ..
        }
        | DeveloperMetadataVerb::Delete {
            key,
            sheet,
            dimension,
            start,
            end,
        } => (key.as_str(), sheet.as_deref(), *dimension, *start, *end),
    };

    let location = match resolve_location(&workbook, sheet, dimension, start, end) {
        Ok(location) => location,
        Err(result) => return gated(result),
    };

    // The read that both decides what to do (create vs. update; what a
    // delete would remove) and backs the mandatory preview — same "one
    // read serves both the dry-run and the real path" reasoning as
    // `format.rs::read_discarded_cells` for `merge-cells`.
    let matches = match find_document_metadata(
        &api,
        &opts.spreadsheet_id,
        Some(key),
        Some(&location),
    )
    .await
    {
        Ok(matches) => matches,
        Err(detail) => return gated(DeveloperMetadataResult::Failed { detail }),
    };
    let entries: Vec<DeveloperMetadataEntry> = matches
        .iter()
        .map(|entry| DeveloperMetadataEntry::from_entry(&workbook, entry))
        .collect();
    let filter = build_filter(Some(key), Some(&location));

    // No fallible step follows building `request` — unlike `format.rs`'s
    // `build_request` (whose `parse_hex_color` can fail), everything
    // needed here is already known, so there is no "build the request
    // last" ordering concern (#1688) to observe.
    let (preview_result, changed_result, request) = match &opts.verb {
        DeveloperMetadataVerb::Set { value, .. } => {
            if entries.is_empty() {
                let location_desc = describe_location(&workbook, &location);
                (
                    DeveloperMetadataResult::WouldCreate {
                        key: key.to_string(),
                        value: value.clone(),
                        location: location_desc.clone(),
                    },
                    DeveloperMetadataResult::Created {
                        key: key.to_string(),
                        value: value.clone(),
                        location: location_desc,
                    },
                    BatchUpdateRequestItem::CreateDeveloperMetadata(
                        CreateDeveloperMetadataRequest {
                            developer_metadata: NewDeveloperMetadata {
                                metadata_key: key.to_string(),
                                metadata_value: value.clone(),
                                location,
                                visibility: DOCUMENT_VISIBILITY.to_string(),
                            },
                        },
                    ),
                )
            } else {
                (
                    DeveloperMetadataResult::WouldUpdate {
                        new_value: value.clone(),
                        previous: entries.clone(),
                    },
                    DeveloperMetadataResult::Updated {
                        new_value: value.clone(),
                        previous: entries.clone(),
                    },
                    BatchUpdateRequestItem::UpdateDeveloperMetadata(
                        UpdateDeveloperMetadataRequest {
                            data_filters: vec![filter],
                            developer_metadata: DeveloperMetadataValueUpdate {
                                metadata_value: value.clone(),
                            },
                            fields: "metadataValue".to_string(),
                        },
                    ),
                )
            }
        }
        DeveloperMetadataVerb::Delete { .. } => {
            if entries.is_empty() {
                return gated(DeveloperMetadataResult::RefusedNotFound {
                    key: key.to_string(),
                });
            }
            (
                DeveloperMetadataResult::WouldDelete {
                    entries: entries.clone(),
                },
                DeveloperMetadataResult::Deleted {
                    entries: entries.clone(),
                },
                BatchUpdateRequestItem::DeleteDeveloperMetadata(DeleteDeveloperMetadataRequest {
                    data_filter: filter,
                }),
            )
        }
    };

    if opts.dry_run {
        return gated(preview_result);
    }

    let files_api = FilesApi::new(drive);
    let leased = LeasedWrite {
        log_prefix: "drive sheets developer-metadata",
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
        Ok(_response) => changed_result,
        Err(err) => DeveloperMetadataResult::Failed {
            detail: format!("{err:#}"),
        },
    };
    drop(lease_grant);
    gated(result)
}

/// The raw `--sheet`/`--dimension`/`--start`/`--end` flags [`search`]
/// resolves into an optional location filter.
///
/// Bundled into one struct so `search` stays under clippy's
/// argument-count limit, and named exactly like
/// [`DeveloperMetadataVerb::Set`]'s own location fields.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SearchLocationFilter<'a> {
    /// See [`DeveloperMetadataVerb::Set::sheet`].
    pub sheet: Option<&'a str>,
    /// See [`DeveloperMetadataVerb::Set::dimension`].
    pub dimension: Option<Dimension>,
    /// See [`DeveloperMetadataVerb::Set::start`].
    pub start: Option<i64>,
    /// See [`DeveloperMetadataVerb::Set::end`].
    pub end: Option<i64>,
}

impl SearchLocationFilter<'_> {
    fn is_empty(self) -> bool {
        self.sheet.is_none()
            && self.dimension.is_none()
            && self.start.is_none()
            && self.end.is_none()
    }
}

/// Searches for developer-metadata entries matching `key` and/or `filter`.
///
/// An empty `filter` (every field `None`) means "every `DOCUMENT`-visibility
/// entry in the workbook". Read-only, so — unlike [`developer_metadata`] —
/// this consults no write gate, matching `read.rs`'s own ungated shape
/// (issue #1795).
pub async fn search(
    api: &SheetsApi<'_>,
    spreadsheet_id: &str,
    workbook: &Spreadsheet,
    key: Option<&str>,
    filter: SearchLocationFilter<'_>,
) -> Result<Vec<DeveloperMetadataEntry>, String> {
    let location = if filter.is_empty() {
        None
    } else {
        Some(
            resolve_location(
                workbook,
                filter.sheet,
                filter.dimension,
                filter.start,
                filter.end,
            )
            .map_err(location_error_to_string)?,
        )
    };
    let matches = find_document_metadata(api, spreadsheet_id, key, location.as_ref()).await?;
    Ok(matches
        .iter()
        .map(|entry| DeveloperMetadataEntry::from_entry(workbook, entry))
        .collect())
}

fn record_attempt(
    outcome: &DeveloperMetadataOutcome,
    opts: &DeveloperMetadataOptions,
    duration: Duration,
) {
    let error = match &outcome.result {
        DeveloperMetadataResult::Failed { detail } => Some(detail.clone()),
        _ => None,
    };
    let decided_by = match &outcome.result {
        DeveloperMetadataResult::Blocked { decided_by } => decided_by.as_ref(),
        _ => None,
    };
    let decided_by = write_gate::decided_by_log_fields(decided_by);
    let fields_changed = match &outcome.result {
        DeveloperMetadataResult::Created { key, value, .. } => {
            Some(format!("create key={key:?} value={value:?}"))
        }
        DeveloperMetadataResult::Updated {
            new_value,
            previous,
        } => Some(format!(
            "update {} entr{} to value={new_value:?}",
            previous.len(),
            plural(previous.len())
        )),
        DeveloperMetadataResult::Deleted { entries } => Some(format!(
            "delete {} entr{}",
            entries.len(),
            plural(entries.len())
        )),
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
        fields_changed,
        error,
        duration,
        ..Default::default()
    });
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        "y"
    } else {
        "ies"
    }
}

fn describe_entry(entry: &DeveloperMetadataEntry) -> String {
    format!(
        "  id {}: {:?}={:?} at {}",
        entry.metadata_id, entry.key, entry.value, entry.location
    )
}

/// Renders an outcome as human-readable text.
#[must_use]
pub fn describe(outcome: &DeveloperMetadataOutcome) -> String {
    describe_lines(outcome).join("\n")
}

/// Renders an outcome as its individual lines, none of which contains a
/// newline — see `structure.rs::describe_lines` for why this shape exists.
#[must_use]
pub fn describe_lines(outcome: &DeveloperMetadataOutcome) -> Vec<String> {
    let verb = &outcome.verb;
    let book = outcome.file_name.as_deref().map_or_else(
        || format!("'{}'", outcome.spreadsheet_id),
        |n| format!("'{n}'"),
    );
    match &outcome.result {
        DeveloperMetadataResult::WouldCreate {
            key,
            value,
            location,
        } => vec![format!(
            "Would create developer metadata {key:?}={value:?} at {location} in {book}"
        )],
        DeveloperMetadataResult::WouldUpdate {
            new_value,
            previous,
        } => {
            let mut lines = vec![format!(
                "Would update {} existing entr{} to {new_value:?} in {book}:",
                previous.len(),
                plural(previous.len())
            )];
            lines.extend(previous.iter().map(describe_entry));
            lines
        }
        DeveloperMetadataResult::WouldDelete { entries } => {
            let mut lines = vec![format!(
                "Would delete {} entr{} in {book}:",
                entries.len(),
                plural(entries.len())
            )];
            lines.extend(entries.iter().map(describe_entry));
            lines
        }
        DeveloperMetadataResult::RefusedNotASpreadsheet { mime_type } => vec![format!(
            "Refused: {book} is not a Google Sheet (mimeType: {mime_type}); \
             `drive sheets {}` only works on spreadsheets",
            verb.label()
        )],
        DeveloperMetadataResult::RefusedShortcut => vec![format!(
            "Refused: {book} is a shortcut; `drive sheets {}` doesn't follow shortcuts",
            verb.label()
        )],
        DeveloperMetadataResult::RefusedNoVisibleParents => vec![format!(
            "Refused: {book} has no parent folder visible to this account, so no folder \
             rule can apply to it. Grant it by id instead: add {{\"file_id\": \"<spreadsheet \
             id>\", \"allow\": [\"sheets-structure\"]}} to write_permissions.rules."
        )],
        DeveloperMetadataResult::RefusedSheetNotFound { title, available } => {
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
        DeveloperMetadataResult::RefusedInvalidLocation { detail } => {
            vec![format!("Refused: {detail}")]
        }
        DeveloperMetadataResult::RefusedNotFound { key } => vec![format!(
            "Refused: no developer metadata matching key {key:?} at that location in {book}"
        )],
        DeveloperMetadataResult::Blocked { decided_by } => vec![match decided_by {
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
        DeveloperMetadataResult::RefusedNoLease => LeaseGateRefusal::NoLease
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        DeveloperMetadataResult::RefusedLeaseExpired => LeaseGateRefusal::Expired
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        DeveloperMetadataResult::RefusedLeaseWrongFile => LeaseGateRefusal::WrongFile
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        DeveloperMetadataResult::RefusedLeaseStale => LeaseGateRefusal::Stale
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        DeveloperMetadataResult::Created {
            key,
            value,
            location,
        } => vec![format!(
            "Created developer metadata {key:?}={value:?} at {location} in {book}"
        )],
        DeveloperMetadataResult::Updated {
            new_value,
            previous,
        } => {
            let mut lines = vec![format!(
                "Updated {} entr{} to {new_value:?} in {book}:",
                previous.len(),
                plural(previous.len())
            )];
            lines.extend(previous.iter().map(describe_entry));
            lines
        }
        DeveloperMetadataResult::Deleted { entries } => {
            let mut lines = vec![format!(
                "Deleted {} entr{} in {book}:",
                entries.len(),
                plural(entries.len())
            )];
            lines.extend(entries.iter().map(describe_entry));
            lines
        }
        DeveloperMetadataResult::Failed { detail } => vec![format!("Failed: {detail}")],
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::sheets::client::SHEETS_API_URL;
    use crate::drive::test_support::seed_lease;
    use crate::test_support::env::MapEnv;
    use crate::utils::secret::Secret;
    use std::collections::HashSet;

    fn test_workbook() -> Spreadsheet {
        serde_json::from_value(serde_json::json!({
            "spreadsheetId": "sheet-1",
            "properties": {"title": "Budget"},
            "sheets": [
                {"properties": {"sheetId": 0, "title": "Q1", "index": 0}},
            ],
        }))
        .unwrap()
    }

    #[test]
    fn resolve_location_with_no_flags_is_spreadsheet_scoped() {
        let workbook = test_workbook();
        let location = resolve_location(&workbook, None, None, None, None).unwrap();
        assert_eq!(location, DeveloperMetadataLocation::spreadsheet());
    }

    #[test]
    fn resolve_location_with_only_sheet_is_sheet_scoped() {
        let workbook = test_workbook();
        let location = resolve_location(&workbook, Some("Q1"), None, None, None).unwrap();
        assert_eq!(location, DeveloperMetadataLocation::sheet(0));
    }

    #[test]
    fn resolve_location_rejects_an_unknown_sheet() {
        let workbook = test_workbook();
        let result = resolve_location(&workbook, Some("Nope"), None, None, None).unwrap_err();
        assert!(matches!(
            result,
            DeveloperMetadataResult::RefusedSheetNotFound { title, .. } if title == "Nope"
        ));
    }

    #[test]
    fn resolve_location_with_full_dimension_converts_1_based_to_0_based() {
        let workbook = test_workbook();
        let location = resolve_location(
            &workbook,
            Some("Q1"),
            Some(Dimension::Rows),
            Some(2),
            Some(5),
        )
        .unwrap();
        assert_eq!(
            location,
            DeveloperMetadataLocation::dimension(DimensionRange {
                sheet_id: 0,
                dimension: Dimension::Rows,
                start_index: 1,
                end_index: 5,
            })
        );
    }

    #[test]
    fn resolve_location_rejects_end_before_start() {
        let workbook = test_workbook();
        let result = resolve_location(
            &workbook,
            Some("Q1"),
            Some(Dimension::Rows),
            Some(5),
            Some(2),
        )
        .unwrap_err();
        assert!(matches!(
            result,
            DeveloperMetadataResult::RefusedInvalidLocation { .. }
        ));
    }

    #[test]
    fn resolve_location_rejects_dimension_without_start_and_end() {
        let workbook = test_workbook();
        let result =
            resolve_location(&workbook, Some("Q1"), Some(Dimension::Rows), None, None).unwrap_err();
        assert!(matches!(
            result,
            DeveloperMetadataResult::RefusedInvalidLocation { .. }
        ));
    }

    #[test]
    fn resolve_location_rejects_start_end_without_sheet() {
        let workbook = test_workbook();
        let result =
            resolve_location(&workbook, None, Some(Dimension::Rows), Some(1), Some(2)).unwrap_err();
        assert!(matches!(
            result,
            DeveloperMetadataResult::RefusedInvalidLocation { .. }
        ));
    }

    fn document_entry(id: i64, visibility: &str) -> DeveloperMetadata {
        DeveloperMetadata {
            metadata_id: id,
            metadata_key: "owner".to_string(),
            metadata_value: "team-a".to_string(),
            location: DeveloperMetadataLocation::spreadsheet(),
            visibility: visibility.to_string(),
        }
    }

    #[test]
    fn assert_document_visibility_accepts_every_document_entry() {
        let entries = vec![document_entry(1, "DOCUMENT"), document_entry(2, "DOCUMENT")];
        assert!(assert_document_visibility(&entries).is_ok());
    }

    #[test]
    fn assert_document_visibility_rejects_a_project_entry() {
        // Should never happen in practice — every filter already restricts
        // the search to DOCUMENT — but this is the defense-in-depth layer,
        // so it must fire loudly if it ever does (issue #1795, ADR-0081
        // §4).
        let entries = vec![document_entry(1, "PROJECT")];
        let err = assert_document_visibility(&entries).unwrap_err();
        assert!(err.contains("PROJECT"), "{err}");
        assert!(err.contains('1'), "{err}");
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

    fn deny_rule() -> Vec<FolderPermissionRule> {
        Vec::new()
    }

    fn mount_workbook() -> wiremock::Mock {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/sheet-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "spreadsheetId": "sheet-1",
                    "properties": {"title": "Budget"},
                    "sheets": [
                        {"properties": {"sheetId": 0, "title": "Q1", "index": 0}},
                    ],
                })),
            )
    }

    fn mount_search(body: serde_json::Value) -> wiremock::Mock {
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/developerMetadata:search",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
    }

    fn mount_batch_update(body: serde_json::Value) -> wiremock::Mock {
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
    }

    fn set_opts(dry_run: bool) -> DeveloperMetadataOptions {
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        let token = seed_lease(&ledger_path, "sheet-1", "1");
        DeveloperMetadataOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: DeveloperMetadataVerb::Set {
                key: "owner".to_string(),
                value: "team-a".to_string(),
                sheet: None,
                dimension: None,
                start: None,
                end: None,
            },
            dry_run,
            lease_token: Some(token),
            ledger_path,
        }
    }

    fn delete_opts(dry_run: bool) -> DeveloperMetadataOptions {
        let mut opts = set_opts(dry_run);
        opts.verb = DeveloperMetadataVerb::Delete {
            key: "owner".to_string(),
            sheet: None,
            dimension: None,
            start: None,
            end: None,
        };
        opts
    }

    async fn setup(
        server: &wiremock::MockServer,
    ) -> (DriveClient, SheetsClient, Vec<FolderPermissionRule>) {
        let (drive, sheets) = clients(server).await;
        mount_file(
            "sheet-1",
            crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
            &["folder-1"],
        )
        .mount(server)
        .await;
        mount_folder("folder-1").mount(server).await;
        mount_workbook().mount(server).await;
        (drive, sheets, vec![allow_rule("folder-1")])
    }

    #[tokio::test]
    async fn dry_run_set_with_no_existing_entry_reports_would_create() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets, rules) = setup(&server).await;
        mount_search(serde_json::json!({"matchedDeveloperMetadata": []}))
            .mount(&server)
            .await;

        let outcome = developer_metadata(&drive, &sheets, &set_opts(true), &rules).await;
        match outcome.result {
            DeveloperMetadataResult::WouldCreate { key, value, .. } => {
                assert_eq!(key, "owner");
                assert_eq!(value, "team-a");
            }
            other => panic!("expected WouldCreate, got {other:?}"),
        }
        // No batchUpdate mock is mounted, so a stray call would 404 and the
        // outcome would be `Failed` instead.
    }

    #[tokio::test]
    async fn dry_run_set_with_an_existing_entry_reports_would_update_with_previous() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets, rules) = setup(&server).await;
        mount_search(serde_json::json!({"matchedDeveloperMetadata": [
            {"developerMetadata": {
                "metadataId": 42, "metadataKey": "owner", "metadataValue": "team-b",
                "location": {"spreadsheet": true}, "visibility": "DOCUMENT",
            }},
        ]}))
        .mount(&server)
        .await;

        let outcome = developer_metadata(&drive, &sheets, &set_opts(true), &rules).await;
        match outcome.result {
            DeveloperMetadataResult::WouldUpdate {
                new_value,
                previous,
            } => {
                assert_eq!(new_value, "team-a");
                assert_eq!(previous.len(), 1);
                assert_eq!(previous[0].value, "team-b");
                assert_eq!(previous[0].metadata_id, 42);
            }
            other => panic!("expected WouldUpdate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn set_creates_when_no_existing_entry_matches() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets, rules) = setup(&server).await;
        mount_search(serde_json::json!({"matchedDeveloperMetadata": []}))
            .mount(&server)
            .await;
        mount_batch_update(serde_json::json!({"replies": [{}]}))
            .mount(&server)
            .await;

        let outcome = developer_metadata(&drive, &sheets, &set_opts(false), &rules).await;
        match outcome.result {
            DeveloperMetadataResult::Created { key, value, .. } => {
                assert_eq!(key, "owner");
                assert_eq!(value, "team-a");
            }
            other => panic!("expected Created, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn delete_reports_not_found_when_nothing_matches() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets, rules) = setup(&server).await;
        mount_search(serde_json::json!({"matchedDeveloperMetadata": []}))
            .mount(&server)
            .await;

        let outcome = developer_metadata(&drive, &sheets, &delete_opts(false), &rules).await;
        match outcome.result {
            DeveloperMetadataResult::RefusedNotFound { key } => assert_eq!(key, "owner"),
            other => panic!("expected RefusedNotFound, got {other:?}"),
        }
        // No batchUpdate mock is mounted — the absence of a resulting
        // `Failed` outcome is itself the assertion that no delete was
        // attempted.
    }

    #[tokio::test]
    async fn delete_previews_every_matched_entry_before_deleting() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets, rules) = setup(&server).await;
        mount_search(serde_json::json!({"matchedDeveloperMetadata": [
            {"developerMetadata": {
                "metadataId": 1, "metadataKey": "owner", "metadataValue": "team-a",
                "location": {"spreadsheet": true}, "visibility": "DOCUMENT",
            }},
            {"developerMetadata": {
                "metadataId": 2, "metadataKey": "owner", "metadataValue": "team-b",
                "location": {"spreadsheet": true}, "visibility": "DOCUMENT",
            }},
        ]}))
        .mount(&server)
        .await;

        let outcome = developer_metadata(&drive, &sheets, &delete_opts(true), &rules).await;
        match outcome.result {
            DeveloperMetadataResult::WouldDelete { entries } => {
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[0].metadata_id, 1);
                assert_eq!(entries[1].metadata_id, 2);
            }
            other => panic!("expected WouldDelete, got {other:?}"),
        }
        // No batchUpdate mock is mounted, so a stray delete call would 404.
    }

    #[tokio::test]
    async fn a_denied_gate_blocks_before_any_search_or_batch_update_call() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets, _rules) = setup(&server).await;

        let outcome = developer_metadata(&drive, &sheets, &set_opts(false), &deny_rule()).await;
        assert!(
            matches!(outcome.result, DeveloperMetadataResult::Blocked { .. }),
            "{:?}",
            outcome.result
        );
        // No developerMetadata:search or batchUpdate mock is mounted — the
        // gate must short-circuit before either is ever reached.
    }

    #[tokio::test]
    async fn search_returns_every_document_visibility_match() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_workbook().mount(&server).await;
        mount_search(serde_json::json!({"matchedDeveloperMetadata": [
            {"developerMetadata": {
                "metadataId": 7, "metadataKey": "owner", "metadataValue": "team-a",
                "location": {"sheetId": 0}, "visibility": "DOCUMENT",
            }},
        ]}))
        .mount(&server)
        .await;

        let api = SheetsApi::new(&sheets);
        let workbook = api.get_spreadsheet("sheet-1").await.unwrap();
        let entries = search(
            &api,
            "sheet-1",
            &workbook,
            None,
            SearchLocationFilter::default(),
        )
        .await
        .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].location, "sheet 'Q1'");
        // No drive/v3/files, folder, or batchUpdate mock is mounted — a
        // search needs none of the write-gate machinery.
        let _ = drive;
    }
}
