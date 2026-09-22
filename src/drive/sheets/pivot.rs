//! Pivot tables via `spreadsheets.batchUpdate`'s `updateCells` request
//! (issue #1798, [ADR-0081](../../../docs/adrs/adr-0081.md) §5).
//!
//! **The one capability in the tranche with no dedicated `batchUpdate`
//! request.** A pivot table is created by `updateCells` carrying a
//! `pivotTable` in a single anchor cell's `CellData`, with a
//! `fields: "pivotTable"` mask; the server renders the pivot's result
//! outward from that anchor, overwriting whatever values are already
//! there, with an extent the request itself never states. Deletion is the
//! same request with an absent `pivotTable`, clearing the property.
//!
//! **`add-pivot-table` is gated by the union of `SheetsWrite` *and*
//! `SheetsStructure`** — the first capability in the crate needing more
//! than one [`DriveOperation`] to independently resolve `Allow`
//! (ADR-0081 §5). `delete-pivot-table` needs `SheetsWrite` alone: it only
//! ever clears the anchor's own value, no structural effect. Both verbs go
//! through [`target_gate::resolve_all`] uniformly (`add` with two
//! operations, `delete` with one), rather than branching between it and
//! the single-operation [`target_gate::resolve`], so this module has one
//! gate-handling code path instead of two.
//!
//! **`updateCells` stays value-incapable.** Issue #1643 decided
//! `updateCells` would stay unused because writing through it could bypass
//! `sheets-structure`'s "never a value" guarantee
//! ([`crate::drive::sheets::types::RepeatCellData`]'s doc comment). This
//! module is the first user of `updateCells`, but [`PivotCellData`] still
//! has no `userEnteredValue` field — the guarantee survives even under the
//! wider union gate, because there is nowhere on the wire type to put a
//! literal value regardless of what a caller asks for.
//!
//! **`--dry-run` cannot enumerate the overwritten region** — the server
//! computes it from the source data at creation time, the same shape of
//! problem ADR-0075 §6 solved for dimension-shift previews, one size
//! larger. The honest answer, and the one this module gives, is to name
//! the anchor cell, the source range, the pivot configuration, and the
//! anchor's own current content (which *is* exactly computable — the
//! anchor is always overwritten), while stating plainly that the rendered
//! extent beyond the anchor is unknown until the request is sent.
//!
//! **No `update-pivot-table` verb.** On the wire, replacing a pivot table
//! is byte-identical to creating one — the field mask simply replaces
//! whatever `pivotTable` the anchor already holds. Rather than add a verb
//! indistinguishable from `add` on the wire, `add-pivot-table` refuses an
//! anchor that already holds a pivot table
//! ([`PivotResult::RefusedAnchorHasPivotTable`]) — delete it first.
//!
//! **`list-pivot-tables`** (proposed alongside the issue's two verbs, not
//! named in it) exists for the same reason every other tranche member
//! pairs its `delete-*` with a `list-*`: `delete-pivot-table` addresses a
//! pivot by its anchor cell, and nothing else in the CLI can discover
//! anchors. It is a plain, ungated read — its engine-facing pieces
//! ([`describe_pivot_table`], [`Sheet::data`](crate::drive::sheets::types::Sheet::data))
//! are `pub(crate)`, and the CLI leaf in
//! `crate::cli::drive::sheets::pivot` renders them directly, mirroring
//! `list-conditional-formats`.

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
    BatchUpdateRequestItem, CellSnapshot, GridCoordinate, PivotCellData, PivotFilterCriteria,
    PivotFilterSpec, PivotGroup, PivotRowData, PivotTable, PivotValue, UpdateCellsRequest,
};
use crate::drive::types::SheetTargetRefusal;
use crate::drive::write_gate::{self, DecidingRule, DriveOperation, FolderPermissionRule};
use crate::request_log::{self, DriveMutationOutcome};

/// A `--row`/`--column` group's sort order — `ASCENDING`/`DESCENDING`.
/// Engine-layer, deliberately free of any `clap` derive — the CLI keeps
/// its own `ValueEnum` mirror where one is needed, the same split
/// [`crate::drive::sheets::api::ValueRenderOption`]'s doc comment
/// describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortOrder {
    Ascending,
    Descending,
}

impl SortOrder {
    const fn as_sheets_str(self) -> &'static str {
        match self {
            Self::Ascending => "ASCENDING",
            Self::Descending => "DESCENDING",
        }
    }

    const fn describe(self) -> &'static str {
        match self {
            Self::Ascending => "asc",
            Self::Descending => "desc",
        }
    }
}

/// A `--value COLUMN:FUNC` aggregation function — Sheets' `SUM`/`COUNTA`/
/// `COUNT`/`COUNTUNIQUE`/`AVERAGE`/`MAX`/`MIN`/`MEDIAN`/`PRODUCT`/`STDEV`/
/// `STDEVP`/`VAR`/`VARP`. `CUSTOM` (a value driven by a formula rather
/// than a source column) is a documented cut — see the module doc's
/// curated-surface stance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SummarizeFunction {
    Sum,
    Counta,
    Count,
    CountUnique,
    Average,
    Max,
    Min,
    Median,
    Product,
    Stdev,
    Stdevp,
    Var,
    Varp,
}

impl SummarizeFunction {
    const fn as_sheets_str(self) -> &'static str {
        match self {
            Self::Sum => "SUM",
            Self::Counta => "COUNTA",
            Self::Count => "COUNT",
            Self::CountUnique => "COUNTUNIQUE",
            Self::Average => "AVERAGE",
            Self::Max => "MAX",
            Self::Min => "MIN",
            Self::Median => "MEDIAN",
            Self::Product => "PRODUCT",
            Self::Stdev => "STDEV",
            Self::Stdevp => "STDEVP",
            Self::Var => "VAR",
            Self::Varp => "VARP",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "sum" => Self::Sum,
            "counta" => Self::Counta,
            "count" => Self::Count,
            "countunique" => Self::CountUnique,
            "average" | "avg" => Self::Average,
            "max" => Self::Max,
            "min" => Self::Min,
            "median" => Self::Median,
            "product" => Self::Product,
            "stdev" => Self::Stdev,
            "stdevp" => Self::Stdevp,
            "var" => Self::Var,
            "varp" => Self::Varp,
            _ => return None,
        })
    }
}

/// `--value-layout`'s value set — `HORIZONTAL` (values as columns, the
/// API's own default) or `VERTICAL` (values as rows).
///
/// Public: the CLI's `ValueLayoutArg` `clap::ValueEnum` mirror converts
/// into this, the same split `GradientMidTypeArg`/`GradientPointType`
/// use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueLayout {
    /// Values laid out as columns.
    Horizontal,
    /// Values laid out as rows.
    Vertical,
}

impl ValueLayout {
    const fn as_sheets_str(self) -> &'static str {
        match self {
            Self::Horizontal => "HORIZONTAL",
            Self::Vertical => "VERTICAL",
        }
    }
}

/// One parsed `--row`/`--column` group.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PivotGroupSpec {
    /// 0-based, relative to the source range's first column.
    column: i64,
    sort_order: Option<SortOrder>,
}

impl PivotGroupSpec {
    fn to_wire(&self, show_totals: bool) -> PivotGroup {
        PivotGroup {
            source_column_offset: self.column,
            show_totals,
            sort_order: self.sort_order.map(|o| o.as_sheets_str().to_string()),
        }
    }
}

/// One parsed `--value` aggregation.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PivotValueSpec {
    /// 0-based, relative to the source range's first column.
    column: i64,
    function: SummarizeFunction,
}

impl PivotValueSpec {
    fn to_wire(&self) -> PivotValue {
        PivotValue {
            source_column_offset: self.column,
            summarize_function: self.function.as_sheets_str().to_string(),
            name: None,
        }
    }
}

/// One parsed `--filter` value allow-list.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PivotFilterSpecArg {
    /// 0-based, relative to the source range's first column.
    column: i64,
    visible_values: Vec<String>,
}

impl PivotFilterSpecArg {
    fn to_wire(&self) -> PivotFilterSpec {
        PivotFilterSpec {
            column_offset_index: self.column,
            filter_criteria: PivotFilterCriteria {
                visible_values: self.visible_values.clone(),
            },
        }
    }
}

/// Which pivot-table mutation to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PivotVerb {
    /// Write a new pivot table at `anchor`, refused if one is already
    /// there.
    AddPivotTable {
        /// The sheet `anchor` lives on.
        sheet: String,
        /// A single-cell A1 reference (no `Sheet!` prefix — `sheet`
        /// supplies it).
        anchor: String,
        /// The pivot's source range — may carry its own `Sheet!` prefix
        /// (a pivot commonly sources from a different tab than it's
        /// anchored on); falls back to `sheet` when it doesn't.
        source: String,
        /// Raw `COLUMN[:asc|desc]` flags, parsed once the source range's
        /// width is known.
        rows: Vec<String>,
        /// Raw `COLUMN[:asc|desc]` flags.
        columns: Vec<String>,
        /// Raw `COLUMN:FUNC` flags. At least one required.
        values: Vec<String>,
        /// Raw `COLUMN:VALUE[,VALUE...]` flags.
        filters: Vec<String>,
        /// `--value-layout`, if given.
        value_layout: Option<ValueLayout>,
        /// `false` when `--no-totals` was given; applies to every row and
        /// column group alike (Sheets has no per-group flag on the CLI
        /// surface here).
        show_totals: bool,
    },
    /// Clear the pivot table at `anchor`, refused if there isn't one.
    DeletePivotTable {
        /// The sheet `anchor` lives on.
        sheet: String,
        /// A single-cell A1 reference (no `Sheet!` prefix — `sheet`
        /// supplies it).
        anchor: String,
    },
}

impl PivotVerb {
    /// The `operation` this verb records in the request log.
    const fn log_operation(&self) -> &'static str {
        match self {
            Self::AddPivotTable { .. } => "sheets-add-pivot-table",
            Self::DeletePivotTable { .. } => "sheets-delete-pivot-table",
        }
    }

    /// Human-readable verb for CLI output.
    const fn label(&self) -> &'static str {
        match self {
            Self::AddPivotTable { .. } => "add-pivot-table",
            Self::DeletePivotTable { .. } => "delete-pivot-table",
        }
    }

    fn sheet(&self) -> &str {
        match self {
            Self::AddPivotTable { sheet, .. } | Self::DeletePivotTable { sheet, .. } => sheet,
        }
    }

    fn anchor(&self) -> &str {
        match self {
            Self::AddPivotTable { anchor, .. } | Self::DeletePivotTable { anchor, .. } => anchor,
        }
    }

    /// Which operations must **all** resolve `Allow` (ADR-0081 §5). Order
    /// matters: [`target_gate::TargetGateUnionOutcome::Gated::denied`]
    /// names the first of these that denied, and `SheetsWrite` first is
    /// the grant an operator holding only `sheets-structure` is missing —
    /// the message that matters for `add-pivot-table`.
    const fn gate_operations(&self) -> &'static [DriveOperation] {
        match self {
            Self::AddPivotTable { .. } => {
                &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure]
            }
            Self::DeletePivotTable { .. } => &[DriveOperation::SheetsWrite],
        }
    }
}

/// Per-call options.
#[derive(Debug, Clone)]
pub struct PivotOptions {
    /// Spreadsheet id.
    pub spreadsheet_id: String,
    /// Which mutation to perform.
    pub verb: PivotVerb,
    /// Classify and describe only; never call `batchUpdate`.
    pub dry_run: bool,
    /// The lease token presented via `--lease`. Checked only when the
    /// deciding rule requires one.
    pub lease_token: Option<String>,
    /// Path to the lease ledger the token is checked against.
    pub ledger_path: PathBuf,
}

/// The substance of a `WouldChange`/`Changed` outcome — what would be (or
/// was) written, and what the anchor held beforehand.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PivotChange {
    /// The anchor cell, composed with its sheet.
    pub anchor: String,
    /// `add-pivot-table` only: the pivot's source range, composed with its
    /// sheet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// `add-pivot-table` only: a one-line summary of the rows/columns/
    /// values/filters/layout/totals that would be configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<String>,
    /// What the anchor cell held before this mutation — a plain value,
    /// empty, or an existing pivot table's own summary. Always known,
    /// unlike the rendered extent — see the module doc's `--dry-run`
    /// paragraph.
    pub anchor_currently: String,
}

/// What happened (or, under `--dry-run`, would happen).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum PivotResult {
    /// `--dry-run`, and the gate would allow it.
    WouldChange(PivotChange),
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
    /// `--sheet`/`--anchor`/`--source` failed to compose into a range, or
    /// named a sheet no range grammar could resolve.
    RefusedInvalidRange {
        /// What was wrong and why.
        detail: String,
    },
    /// `--anchor` resolved to more than one cell.
    RefusedInvalidAnchor {
        /// What was wrong and why.
        detail: String,
    },
    /// `--source` was unbounded, or a `--row`/`--column`/`--value`/
    /// `--filter` column offset fell outside it, or no `--value` was
    /// given.
    RefusedInvalidSource {
        /// What was wrong and why.
        detail: String,
    },
    /// `add-pivot-table`'s anchor already holds a pivot table.
    RefusedAnchorHasPivotTable {
        /// The anchor, composed with its sheet.
        anchor: String,
        /// A summary of the pivot table already there.
        existing_summary: String,
    },
    /// `delete-pivot-table`'s anchor holds no pivot table.
    RefusedNoPivotTableAtAnchor {
        /// The anchor, composed with its sheet.
        anchor: String,
    },
    /// The folder write-permission gate refused it.
    Blocked {
        /// Which of [`PivotVerb::gate_operations`] denied first.
        operation: DriveOperation,
        /// The rule that decided the refusal, if any.
        decided_by: Option<DecidingRule>,
    },
    /// No `--lease` was presented, and the deciding rule requires one.
    RefusedNoLease,
    /// The presented lease has expired, or was never a token this ledger
    /// knows about.
    RefusedLeaseExpired,
    /// The presented lease is bound to a different file id.
    RefusedLeaseWrongFile,
    /// The file has moved since the lease's recorded `version`.
    RefusedLeaseStale,
    /// The mutation succeeded.
    Changed(PivotChange),
    /// An API or validation error.
    Failed {
        /// A human-readable summary of what failed.
        detail: String,
    },
}

impl FromLeaseRefusal for PivotResult {
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

impl PivotResult {
    fn log_status(&self) -> &'static str {
        match self {
            Self::WouldChange(_) => "would-change",
            Self::RefusedNotASpreadsheet { .. } => "refused-not-a-spreadsheet",
            Self::RefusedShortcut => "refused-shortcut",
            Self::RefusedNoVisibleParents => "refused-no-visible-parents",
            Self::RefusedSheetNotFound { .. } => "refused-sheet-not-found",
            Self::RefusedInvalidRange { .. } => "refused-invalid-range",
            Self::RefusedInvalidAnchor { .. } => "refused-invalid-anchor",
            Self::RefusedInvalidSource { .. } => "refused-invalid-source",
            Self::RefusedAnchorHasPivotTable { .. } => "refused-anchor-has-pivot-table",
            Self::RefusedNoPivotTableAtAnchor { .. } => "refused-no-pivot-table-at-anchor",
            Self::Blocked { .. } => "blocked",
            Self::RefusedNoLease => LeaseGateRefusal::NoLease.log_status(),
            Self::RefusedLeaseExpired => LeaseGateRefusal::Expired.log_status(),
            Self::RefusedLeaseWrongFile => LeaseGateRefusal::WrongFile.log_status(),
            Self::RefusedLeaseStale => LeaseGateRefusal::Stale.log_status(),
            Self::Changed(_) => "changed",
            Self::Failed { .. } => "failed",
        }
    }
}

/// The full outcome of one attempt.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PivotOutcome {
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
    pub verb: PivotVerb,
    /// What happened.
    pub result: PivotResult,
}

impl JsonlSerialize for PivotOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> anyhow::Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Runs one pivot-table mutation, logging every attempt that isn't a dry
/// run.
pub async fn pivot(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &PivotOptions,
    rules: &[FolderPermissionRule],
) -> PivotOutcome {
    let started = Instant::now();
    let outcome = pivot_inner(drive, sheets, opts, rules).await;
    if !opts.dry_run {
        record_attempt(&outcome, opts, started.elapsed());
    }
    outcome
}

async fn pivot_inner(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &PivotOptions,
    rules: &[FolderPermissionRule],
) -> PivotOutcome {
    let bare = |result| PivotOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: None,
        resolved_folder_id: None,
        verb: opts.verb.clone(),
        result,
    };

    let anchor_composed = match a1::compose(Some(opts.verb.sheet()), Some(opts.verb.anchor())) {
        Ok(composed) => composed,
        Err(err) => {
            return bare(PivotResult::RefusedInvalidRange {
                detail: err.to_string(),
            })
        }
    };

    let operations = opts.verb.gate_operations();

    let (target, verdict, denied, resolved_folder_id, requires_lease) =
        match target_gate::resolve_all(drive, &opts.spreadsheet_id, operations, rules).await {
            target_gate::TargetGateUnionOutcome::MetadataFetchFailed { detail } => {
                return bare(PivotResult::Failed { detail })
            }
            target_gate::TargetGateUnionOutcome::Refused { target, refusal } => {
                let result = match refusal {
                    SheetTargetRefusal::Shortcut => PivotResult::RefusedShortcut,
                    SheetTargetRefusal::NotASpreadsheet { mime_type } => {
                        PivotResult::RefusedNotASpreadsheet { mime_type }
                    }
                    SheetTargetRefusal::NoVisibleParents => PivotResult::RefusedNoVisibleParents,
                };
                return PivotOutcome {
                    spreadsheet_id: opts.spreadsheet_id.clone(),
                    file_name: Some(target.name),
                    resolved_folder_id: None,
                    verb: opts.verb.clone(),
                    result,
                };
            }
            target_gate::TargetGateUnionOutcome::GateFetchFailed { target, detail } => {
                return PivotOutcome {
                    spreadsheet_id: opts.spreadsheet_id.clone(),
                    file_name: Some(target.name),
                    resolved_folder_id: None,
                    verb: opts.verb.clone(),
                    result: PivotResult::Failed { detail },
                };
            }
            target_gate::TargetGateUnionOutcome::Gated {
                target,
                verdict,
                denied,
                resolved_folder_id,
                requires_lease,
            } => (target, verdict, denied, resolved_folder_id, requires_lease),
        };

    let gated = |result| PivotOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: Some(target.name.clone()),
        resolved_folder_id: resolved_folder_id.clone(),
        verb: opts.verb.clone(),
        result,
    };

    if verdict == write_gate::Verdict::Deny {
        let (operation, decided_by) = denied.unwrap_or((operations[0], None));
        return gated(PivotResult::Blocked {
            operation,
            decided_by,
        });
    }

    let api = SheetsApi::new(sheets);
    let workbook = match api.get_spreadsheet(&opts.spreadsheet_id).await {
        Ok(workbook) => workbook,
        Err(err) => {
            return gated(PivotResult::Failed {
                detail: format!("{err:#}"),
            })
        }
    };

    let (_, anchor_grid) = match grid_range::resolve_grid_range(
        &workbook,
        &anchor_composed,
        |detail| PivotResult::RefusedInvalidRange { detail },
        |title, available| PivotResult::RefusedSheetNotFound { title, available },
    ) {
        Ok(resolved) => resolved,
        Err(result) => return gated(result),
    };

    if !grid_range::is_single_cell(&anchor_grid) {
        return gated(PivotResult::RefusedInvalidAnchor {
            detail: format!(
                "--anchor '{}' must name a single cell, not a range",
                opts.verb.anchor()
            ),
        });
    }
    let start = GridCoordinate {
        sheet_id: anchor_grid.sheet_id,
        row_index: anchor_grid.start_row_index.unwrap_or(0),
        column_index: anchor_grid.start_column_index.unwrap_or(0),
    };

    // Read the anchor's current content once — it backs the
    // occupied-anchor refusal, `delete`'s missing-pivot refusal, and every
    // `anchor_currently` preview below, so one fetch serves all three.
    let cell_workbook = match api
        .get_cell_pivot(&opts.spreadsheet_id, &anchor_composed)
        .await
    {
        Ok(workbook) => workbook,
        Err(err) => {
            return gated(PivotResult::Failed {
                detail: format!("{err:#}"),
            })
        }
    };
    let existing_cell = cell_snapshot_at(&cell_workbook, anchor_grid.sheet_id);
    let existing_pivot = existing_cell.and_then(|c| c.pivot_table.as_ref());

    let (config, source_a1, request) = match &opts.verb {
        PivotVerb::AddPivotTable {
            source,
            rows,
            columns,
            values,
            filters,
            value_layout,
            show_totals,
            ..
        } => {
            if let Some(existing) = existing_pivot {
                return gated(PivotResult::RefusedAnchorHasPivotTable {
                    anchor: anchor_composed.clone(),
                    existing_summary: describe_pivot_table(existing),
                });
            }

            let source_composed = match compose_source(opts.verb.sheet(), source) {
                Ok(composed) => composed,
                Err(detail) => return gated(PivotResult::RefusedInvalidRange { detail }),
            };
            let (_, source_grid) = match grid_range::resolve_grid_range(
                &workbook,
                &source_composed,
                |detail| PivotResult::RefusedInvalidRange { detail },
                |title, available| PivotResult::RefusedSheetNotFound { title, available },
            ) {
                Ok(resolved) => resolved,
                Err(result) => return gated(result),
            };
            if !grid_range::is_bounded(&source_grid) {
                return gated(PivotResult::RefusedInvalidSource {
                    detail: format!(
                        "--source '{source}' must be a bounded range, not an open-ended \
                         column/row span"
                    ),
                });
            }
            #[allow(clippy::unwrap_used)] // `is_bounded` just confirmed all four are `Some`.
            let width =
                source_grid.end_column_index.unwrap() - source_grid.start_column_index.unwrap();

            let row_specs = match parse_group_specs(rows) {
                Ok(specs) => specs,
                Err(detail) => return gated(PivotResult::RefusedInvalidSource { detail }),
            };
            let column_specs = match parse_group_specs(columns) {
                Ok(specs) => specs,
                Err(detail) => return gated(PivotResult::RefusedInvalidSource { detail }),
            };
            let value_specs = match parse_value_specs(values) {
                Ok(specs) => specs,
                Err(detail) => return gated(PivotResult::RefusedInvalidSource { detail }),
            };
            let filter_specs = match parse_filter_specs(filters) {
                Ok(specs) => specs,
                Err(detail) => return gated(PivotResult::RefusedInvalidSource { detail }),
            };

            if value_specs.is_empty() {
                return gated(PivotResult::RefusedInvalidSource {
                    detail: "add-pivot-table needs at least one --value".to_string(),
                });
            }

            let out_of_bounds = row_specs
                .iter()
                .map(|s| s.column)
                .chain(column_specs.iter().map(|s| s.column))
                .chain(value_specs.iter().map(|s| s.column))
                .chain(filter_specs.iter().map(|s| s.column))
                .find(|&col| col < 0 || col >= width);
            if let Some(col) = out_of_bounds {
                return gated(PivotResult::RefusedInvalidSource {
                    detail: format!(
                        "column offset {col} is out of bounds for source '{source_composed}' \
                         ({width} column(s), valid range 0-{})",
                        width - 1
                    ),
                });
            }

            let pivot_table = PivotTable {
                source: source_grid,
                rows: row_specs.iter().map(|s| s.to_wire(*show_totals)).collect(),
                columns: column_specs
                    .iter()
                    .map(|s| s.to_wire(*show_totals))
                    .collect(),
                values: value_specs.iter().map(PivotValueSpec::to_wire).collect(),
                filter_specs: filter_specs
                    .iter()
                    .map(PivotFilterSpecArg::to_wire)
                    .collect(),
                value_layout: value_layout.map(|l| l.as_sheets_str().to_string()),
            };

            let config = describe_pivot_config(
                &row_specs,
                &column_specs,
                &value_specs,
                &filter_specs,
                *value_layout,
                *show_totals,
            );

            let request = BatchUpdateRequestItem::UpdateCells(UpdateCellsRequest {
                start,
                rows: vec![PivotRowData {
                    values: vec![PivotCellData {
                        pivot_table: Some(pivot_table),
                    }],
                }],
                fields: "pivotTable".to_string(),
            });

            (Some(config), Some(source_composed), request)
        }
        PivotVerb::DeletePivotTable { .. } => {
            if existing_pivot.is_none() {
                return gated(PivotResult::RefusedNoPivotTableAtAnchor {
                    anchor: anchor_composed.clone(),
                });
            }
            let request = BatchUpdateRequestItem::UpdateCells(UpdateCellsRequest {
                start,
                rows: vec![PivotRowData {
                    values: vec![PivotCellData { pivot_table: None }],
                }],
                fields: "pivotTable".to_string(),
            });
            (None, None, request)
        }
    };

    let change = PivotChange {
        anchor: anchor_composed.clone(),
        source: source_a1,
        config,
        anchor_currently: describe_anchor_currently(existing_cell),
    };

    if opts.dry_run {
        return gated(PivotResult::WouldChange(change));
    }

    // The lease check sits here: after the permission gate and the
    // `--dry-run` branch, before the mutating call — see
    // `content_edit.rs::edit_inner`'s doc comment for the full reasoning,
    // shared verbatim by every leased engine.
    let files_api = FilesApi::new(drive);
    let leased = LeasedWrite {
        log_prefix: "drive sheets pivot",
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
        Ok(_response) => PivotResult::Changed(change),
        Err(err) => PivotResult::Failed {
            detail: format!("{err:#}"),
        },
    };
    drop(lease_grant);
    gated(result)
}

/// Composes `--source` with `--sheet`: `source` carrying its own `Sheet!`
/// prefix is used as-is (a pivot commonly sources from a different tab
/// than it's anchored on), otherwise `sheet` supplies the prefix.
fn compose_source(sheet: &str, source: &str) -> Result<String, String> {
    if a1::split_sheet_prefix(source).is_some() {
        a1::validate_range(source)
            .map(|()| source.to_string())
            .map_err(|err| err.to_string())
    } else {
        a1::compose(Some(sheet), Some(source)).map_err(|err| err.to_string())
    }
}

/// Finds the anchor's own [`CellSnapshot`] in a `get_cell_pivot` response —
/// the response's `sheets.data` is scoped by the `ranges` query parameter
/// to just the anchor, so the first sheet whose id matches, first chunk,
/// first row, first cell, is always the anchor itself when present at all.
fn cell_snapshot_at(
    workbook: &crate::drive::sheets::types::Spreadsheet,
    sheet_id: i64,
) -> Option<&CellSnapshot> {
    workbook
        .sheets
        .iter()
        .find(|s| s.sheet_id() == Some(sheet_id))
        .and_then(|s| s.data.first())
        .and_then(|gd| gd.row_data.first())
        .and_then(|rd| rd.values.first())
}

/// Parses `COLUMN[:asc|desc]` flags into [`PivotGroupSpec`]s, in the order
/// given.
fn parse_group_specs(flags: &[String]) -> Result<Vec<PivotGroupSpec>, String> {
    flags
        .iter()
        .map(|flag| {
            let (col_str, order_str) = match flag.split_once(':') {
                Some((col, order)) => (col, Some(order)),
                None => (flag.as_str(), None),
            };
            let column: i64 = col_str
                .trim()
                .parse()
                .map_err(|_| format!("'{col_str}' in '{flag}' is not a column offset"))?;
            let sort_order = match order_str {
                None => None,
                Some(order) => Some(match order.trim().to_ascii_lowercase().as_str() {
                    "asc" | "ascending" => SortOrder::Ascending,
                    "desc" | "descending" => SortOrder::Descending,
                    other => return Err(format!("'{other}' in '{flag}' is not 'asc' or 'desc'")),
                }),
            };
            Ok(PivotGroupSpec { column, sort_order })
        })
        .collect()
}

/// Parses `COLUMN:FUNC` flags into [`PivotValueSpec`]s, in the order
/// given.
fn parse_value_specs(flags: &[String]) -> Result<Vec<PivotValueSpec>, String> {
    flags
        .iter()
        .map(|flag| {
            let (col, func) = flag
                .split_once(':')
                .ok_or_else(|| format!("'{flag}' is not COLUMN:FUNC — expected e.g. '3:sum'"))?;
            let column: i64 = col
                .trim()
                .parse()
                .map_err(|_| format!("'{col}' in '{flag}' is not a column offset"))?;
            let function = SummarizeFunction::parse(func).ok_or_else(|| {
                format!(
                    "'{func}' in '{flag}' is not a recognized summarize function (sum, counta, \
                     count, countunique, average, max, min, median, product, stdev, stdevp, \
                     var, varp)"
                )
            })?;
            Ok(PivotValueSpec { column, function })
        })
        .collect()
}

/// Parses `COLUMN:VALUE[,VALUE...]` flags into [`PivotFilterSpecArg`]s, in
/// the order given.
fn parse_filter_specs(flags: &[String]) -> Result<Vec<PivotFilterSpecArg>, String> {
    flags
        .iter()
        .map(|flag| {
            let (col, values) = flag.split_once(':').ok_or_else(|| {
                format!("'{flag}' is not COLUMN:VALUE[,VALUE...] — expected e.g. '1:Foo,Bar'")
            })?;
            let column: i64 = col
                .trim()
                .parse()
                .map_err(|_| format!("'{col}' in '{flag}' is not a column offset"))?;
            if values.is_empty() {
                return Err(format!("'{flag}' names no values to filter"));
            }
            let visible_values = values.split(',').map(str::to_string).collect();
            Ok(PivotFilterSpecArg {
                column,
                visible_values,
            })
        })
        .collect()
}

/// A one-line summary of a pivot configuration, for a dry-run/changed
/// preview.
fn describe_pivot_config(
    rows: &[PivotGroupSpec],
    columns: &[PivotGroupSpec],
    values: &[PivotValueSpec],
    filters: &[PivotFilterSpecArg],
    value_layout: Option<ValueLayout>,
    show_totals: bool,
) -> String {
    let describe_group = |g: &PivotGroupSpec| {
        g.sort_order.map_or_else(
            || format!("col {}", g.column),
            |order| format!("col {} ({})", g.column, order.describe()),
        )
    };
    let rows_desc = if rows.is_empty() {
        "none".to_string()
    } else {
        rows.iter()
            .map(describe_group)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let columns_desc = if columns.is_empty() {
        "none".to_string()
    } else {
        columns
            .iter()
            .map(describe_group)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let values_desc = values
        .iter()
        .map(|v| format!("{} of col {}", v.function.as_sheets_str(), v.column))
        .collect::<Vec<_>>()
        .join(", ");
    let layout_desc = value_layout.map_or("HORIZONTAL (default)", ValueLayout::as_sheets_str);
    let totals_desc = if show_totals { "on" } else { "off" };
    let mut desc = format!(
        "rows: {rows_desc}; columns: {columns_desc}; values: {values_desc}; \
         layout: {layout_desc}; totals: {totals_desc}"
    );
    if !filters.is_empty() {
        let filters_desc = filters
            .iter()
            .map(|f| format!("col {} in [{}]", f.column, f.visible_values.join(", ")))
            .collect::<Vec<_>>()
            .join(", ");
        desc.push_str(&format!("; filters: {filters_desc}"));
    }
    desc
}

/// A short summary of an existing pivot table — used in refusal and
/// preview messages, and by [`list_entries`].
#[must_use]
pub(crate) fn describe_pivot_table(pt: &PivotTable) -> String {
    format!(
        "{} row group(s), {} column group(s), {} value(s)",
        pt.rows.len(),
        pt.columns.len(),
        pt.values.len()
    )
}

/// One pivot table found by `list-pivot-tables`.
pub(crate) struct PivotListEntry {
    /// The sheet the pivot table is anchored on.
    pub sheet: String,
    /// The anchor cell, in bare A1 form (no `Sheet!` prefix — `sheet`
    /// carries that separately).
    pub anchor: String,
    /// [`describe_pivot_table`]'s summary.
    pub summary: String,
}

/// Scans every sheet's grid data (populated by
/// [`crate::drive::sheets::api::SheetsApi::get_spreadsheet_with_pivot_tables`])
/// for a populated `pivotTable` property, computing each one's real A1
/// anchor from its chunk's `startRow`/`startColumn` offset — this lives in
/// the engine rather than the CLI layer because `grid_range` (which
/// `column_index_to_letters` comes from) is private to
/// `crate::drive::sheets`.
pub(crate) fn list_entries(
    workbook: &crate::drive::sheets::types::Spreadsheet,
) -> Vec<PivotListEntry> {
    let mut entries = Vec::new();
    for sheet in &workbook.sheets {
        for chunk in &sheet.data {
            let row0 = chunk.start_row.unwrap_or(0);
            let col0 = chunk.start_column.unwrap_or(0);
            for (row_idx, row) in chunk.row_data.iter().enumerate() {
                for (col_idx, cell) in row.values.iter().enumerate() {
                    let Some(pivot_table) = &cell.pivot_table else {
                        continue;
                    };
                    let anchor = format!(
                        "{}{}",
                        grid_range::column_index_to_letters(col0 + col_idx as i64),
                        row0 + row_idx as i64 + 1
                    );
                    entries.push(PivotListEntry {
                        sheet: sheet.title().to_string(),
                        anchor,
                        summary: describe_pivot_table(pivot_table),
                    });
                }
            }
        }
    }
    entries
}

/// Describes what the anchor cell held before a mutation.
fn describe_anchor_currently(cell: Option<&CellSnapshot>) -> String {
    let Some(cell) = cell else {
        return "empty".to_string();
    };
    if let Some(pivot) = &cell.pivot_table {
        return format!("a pivot table ({})", describe_pivot_table(pivot));
    }
    match cell.formatted_value.as_deref() {
        Some(value) if !value.is_empty() => format!("value {value:?}"),
        _ => "empty".to_string(),
    }
}

fn record_attempt(outcome: &PivotOutcome, opts: &PivotOptions, duration: Duration) {
    let error = match &outcome.result {
        PivotResult::Failed { detail } => Some(detail.clone()),
        _ => None,
    };
    let decided_by = match &outcome.result {
        PivotResult::Blocked { decided_by, .. } => decided_by.as_ref(),
        _ => None,
    };
    let decided_by = write_gate::decided_by_log_fields(decided_by);

    request_log::record_drive_mutation(DriveMutationOutcome {
        operation: opts.verb.log_operation(),
        file_id: outcome.spreadsheet_id.clone(),
        file_name: outcome.file_name.clone().unwrap_or_default(),
        status: outcome.result.log_status().to_string(),
        resolved_folder_id: outcome.resolved_folder_id.clone(),
        decided_by_folder_id: decided_by.folder_id,
        decided_by_depth: decided_by.depth,
        decided_by_file_id: decided_by.file_id,
        error,
        duration,
        ..Default::default()
    });
}

/// Renders the change-specific lines of a `WouldChange`/`Changed` outcome
/// — everything but the leading "Would "/"Applied: " the caller prepends,
/// so both share this one builder.
fn pivot_change_lines(verb: &PivotVerb, change: &PivotChange, book: &str) -> Vec<String> {
    let action = match verb {
        PivotVerb::AddPivotTable { .. } => "add a pivot table",
        PivotVerb::DeletePivotTable { .. } => "delete the pivot table",
    };
    let mut lines = vec![format!("{action} anchored at {} in {book}", change.anchor)];
    if let Some(source) = &change.source {
        lines.push(format!("  source: {source}"));
    }
    if let Some(config) = &change.config {
        lines.push(format!("  {config}"));
    }
    lines.push(format!(
        "  anchor {} currently: {}",
        change.anchor, change.anchor_currently
    ));
    if change.source.is_some() {
        lines.push(
            "  NOTE: the rendered extent is computed by the server from the source data and \
             is not known until the request is sent; cells right of and below the anchor may \
             be overwritten."
                .to_string(),
        );
    }
    lines
}

/// Renders an outcome as human-readable text.
#[must_use]
pub fn describe(outcome: &PivotOutcome) -> String {
    describe_lines(outcome).join("\n")
}

/// Renders an outcome as its individual lines, none of which contains a
/// newline.
#[must_use]
pub fn describe_lines(outcome: &PivotOutcome) -> Vec<String> {
    let verb = &outcome.verb;
    let book = outcome.file_name.as_deref().map_or_else(
        || format!("'{}'", outcome.spreadsheet_id),
        |n| format!("'{n}'"),
    );
    match &outcome.result {
        PivotResult::WouldChange(change) => {
            let mut lines = pivot_change_lines(verb, change, &book);
            lines[0] = format!("Would {}", lines[0]);
            lines
        }
        PivotResult::Changed(change) => {
            let mut lines = pivot_change_lines(verb, change, &book);
            lines[0] = format!("Applied: {}", lines[0]);
            lines
        }
        PivotResult::RefusedNotASpreadsheet { mime_type } => vec![format!(
            "Refused: {book} is not a Google Sheet (mimeType: {mime_type}); `drive sheets {}` \
             only works on spreadsheets",
            verb.label()
        )],
        PivotResult::RefusedShortcut => vec![format!(
            "Refused: {book} is a shortcut; `drive sheets {}` doesn't follow shortcuts",
            verb.label()
        )],
        PivotResult::RefusedNoVisibleParents => {
            let ops = match verb {
                PivotVerb::AddPivotTable { .. } => "\"sheets-write\", \"sheets-structure\"",
                PivotVerb::DeletePivotTable { .. } => "\"sheets-write\"",
            };
            vec![format!(
                "Refused: {book} has no parent folder visible to this account, so no folder \
                 rule can apply to it. Grant it by id instead: add {{\"file_id\": \
                 \"<spreadsheet id>\", \"allow\": [{ops}]}} to write_permissions.rules."
            )]
        }
        PivotResult::RefusedSheetNotFound { title, available } => {
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
        PivotResult::RefusedInvalidRange { detail }
        | PivotResult::RefusedInvalidAnchor { detail }
        | PivotResult::RefusedInvalidSource { detail } => vec![format!("Refused: {detail}")],
        PivotResult::RefusedAnchorHasPivotTable {
            anchor,
            existing_summary,
        } => vec![format!(
            "Refused: {book}'s anchor {anchor} already holds a pivot table \
             ({existing_summary}); delete it first with `drive sheets delete-pivot-table`, or \
             choose a different anchor"
        )],
        PivotResult::RefusedNoPivotTableAtAnchor { anchor } => vec![format!(
            "Refused: {book} has no pivot table anchored at {anchor}. Run `drive sheets \
             list-pivot-tables` to see existing ones."
        )],
        PivotResult::Blocked {
            operation,
            decided_by,
        } => vec![match decided_by {
            Some(rule) => format!(
                "Blocked: {} on {book} refused by rule on {} {}{}",
                verb.label(),
                rule.kind_label(),
                rule.id(),
                rule.depth_suffix()
            ),
            None => format!(
                "Blocked: {} on {book} refused by default policy (no matching rule for \
                 {operation})",
                verb.label()
            ),
        }],
        PivotResult::RefusedNoLease => LeaseGateRefusal::NoLease
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        PivotResult::RefusedLeaseExpired => LeaseGateRefusal::Expired
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        PivotResult::RefusedLeaseWrongFile => LeaseGateRefusal::WrongFile
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        PivotResult::RefusedLeaseStale => LeaseGateRefusal::Stale
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        PivotResult::Failed { detail } => vec![format!("Failed: {detail}")],
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::sheets::client::SHEETS_API_URL;
    use crate::drive::sheets::types::GridRange;
    use crate::drive::test_support::seed_lease;
    use crate::test_support::env::MapEnv;
    use crate::utils::secret::Secret;
    use std::collections::HashSet;

    // ── parse_group_specs / parse_value_specs / parse_filter_specs ──────

    #[test]
    fn parse_group_specs_accepts_a_bare_column_or_with_sort_order() {
        let specs = parse_group_specs(&["0".to_string(), "2:desc".to_string()]).unwrap();
        assert_eq!(
            specs,
            vec![
                PivotGroupSpec {
                    column: 0,
                    sort_order: None
                },
                PivotGroupSpec {
                    column: 2,
                    sort_order: Some(SortOrder::Descending)
                },
            ]
        );
    }

    #[test]
    fn parse_group_specs_rejects_an_unknown_direction() {
        let err = parse_group_specs(&["0:sideways".to_string()]).unwrap_err();
        assert!(err.contains("'sideways'"), "{err}");
    }

    #[test]
    fn parse_group_specs_rejects_a_non_numeric_column() {
        let err = parse_group_specs(&["x:asc".to_string()]).unwrap_err();
        assert!(err.contains("column offset"), "{err}");
    }

    #[test]
    fn parse_value_specs_parses_column_and_function() {
        let specs = parse_value_specs(&["3:sum".to_string(), "1:AVERAGE".to_string()]).unwrap();
        assert_eq!(
            specs,
            vec![
                PivotValueSpec {
                    column: 3,
                    function: SummarizeFunction::Sum
                },
                PivotValueSpec {
                    column: 1,
                    function: SummarizeFunction::Average
                },
            ]
        );
    }

    #[test]
    fn parse_value_specs_rejects_a_missing_colon() {
        let err = parse_value_specs(&["3sum".to_string()]).unwrap_err();
        assert!(err.contains("COLUMN:FUNC"), "{err}");
    }

    #[test]
    fn parse_value_specs_rejects_an_unknown_function() {
        let err = parse_value_specs(&["3:bogus".to_string()]).unwrap_err();
        assert!(err.contains("'bogus'"), "{err}");
    }

    #[test]
    fn parse_filter_specs_splits_values_on_commas() {
        let specs = parse_filter_specs(&["1:Foo,Bar".to_string()]).unwrap();
        assert_eq!(
            specs,
            vec![PivotFilterSpecArg {
                column: 1,
                visible_values: vec!["Foo".to_string(), "Bar".to_string()],
            }]
        );
    }

    #[test]
    fn parse_filter_specs_rejects_an_empty_value_list() {
        let err = parse_filter_specs(&["1:".to_string()]).unwrap_err();
        assert!(err.contains("names no values"), "{err}");
    }

    #[test]
    fn parse_filter_specs_rejects_a_missing_colon() {
        let err = parse_filter_specs(&["1".to_string()]).unwrap_err();
        assert!(err.contains("COLUMN:VALUE"), "{err}");
    }

    // ── to_wire mappings ─────────────────────────────────────────────────

    #[test]
    fn pivot_group_spec_to_wire_maps_sort_order_and_totals() {
        let asc = PivotGroupSpec {
            column: 0,
            sort_order: Some(SortOrder::Ascending),
        }
        .to_wire(true);
        assert_eq!(asc.sort_order.as_deref(), Some("ASCENDING"));
        assert!(asc.show_totals);

        let desc = PivotGroupSpec {
            column: 1,
            sort_order: Some(SortOrder::Descending),
        }
        .to_wire(false);
        assert_eq!(desc.sort_order.as_deref(), Some("DESCENDING"));
        assert!(!desc.show_totals);

        let none = PivotGroupSpec {
            column: 2,
            sort_order: None,
        }
        .to_wire(true);
        assert_eq!(none.sort_order, None);
    }

    #[test]
    fn summarize_function_as_sheets_str_covers_every_variant() {
        let cases = [
            (SummarizeFunction::Sum, "SUM"),
            (SummarizeFunction::Counta, "COUNTA"),
            (SummarizeFunction::Count, "COUNT"),
            (SummarizeFunction::CountUnique, "COUNTUNIQUE"),
            (SummarizeFunction::Average, "AVERAGE"),
            (SummarizeFunction::Max, "MAX"),
            (SummarizeFunction::Min, "MIN"),
            (SummarizeFunction::Median, "MEDIAN"),
            (SummarizeFunction::Product, "PRODUCT"),
            (SummarizeFunction::Stdev, "STDEV"),
            (SummarizeFunction::Stdevp, "STDEVP"),
            (SummarizeFunction::Var, "VAR"),
            (SummarizeFunction::Varp, "VARP"),
        ];
        for (func, expected) in cases {
            assert_eq!(func.as_sheets_str(), expected);
        }
    }

    #[test]
    fn pivot_filter_spec_arg_to_wire_maps_fields() {
        let spec = PivotFilterSpecArg {
            column: 2,
            visible_values: vec!["Open".to_string(), "Closed".to_string()],
        };
        let wire = spec.to_wire();
        assert_eq!(wire.column_offset_index, 2);
        assert_eq!(
            wire.filter_criteria.visible_values,
            vec!["Open".to_string(), "Closed".to_string()]
        );
    }

    // ── describe helpers ─────────────────────────────────────────────────

    #[test]
    fn describe_pivot_config_names_every_axis() {
        let desc = describe_pivot_config(
            &[PivotGroupSpec {
                column: 0,
                sort_order: Some(SortOrder::Ascending),
            }],
            &[],
            &[PivotValueSpec {
                column: 3,
                function: SummarizeFunction::Sum,
            }],
            &[],
            None,
            true,
        );
        assert!(desc.contains("rows: col 0 (asc)"), "{desc}");
        assert!(desc.contains("columns: none"), "{desc}");
        assert!(desc.contains("values: SUM of col 3"), "{desc}");
        assert!(desc.contains("layout: HORIZONTAL (default)"), "{desc}");
        assert!(desc.contains("totals: on"), "{desc}");
    }

    #[test]
    fn describe_pivot_config_appends_filters_only_when_present() {
        let without = describe_pivot_config(&[], &[], &[], &[], None, false);
        assert!(!without.contains("filters:"), "{without}");

        let with = describe_pivot_config(
            &[],
            &[],
            &[],
            &[PivotFilterSpecArg {
                column: 1,
                visible_values: vec!["Foo".to_string()],
            }],
            Some(ValueLayout::Vertical),
            false,
        );
        assert!(with.contains("filters: col 1 in [Foo]"), "{with}");
        assert!(with.contains("layout: VERTICAL"), "{with}");
        assert!(with.contains("totals: off"), "{with}");
    }

    #[test]
    fn describe_pivot_config_covers_every_sort_order_and_an_explicit_horizontal_layout() {
        let desc = describe_pivot_config(
            &[
                PivotGroupSpec {
                    column: 0,
                    sort_order: None,
                },
                PivotGroupSpec {
                    column: 1,
                    sort_order: Some(SortOrder::Descending),
                },
            ],
            &[PivotGroupSpec {
                column: 2,
                sort_order: None,
            }],
            &[PivotValueSpec {
                column: 3,
                function: SummarizeFunction::Sum,
            }],
            &[],
            Some(ValueLayout::Horizontal),
            true,
        );
        assert!(desc.contains("rows: col 0, col 1 (desc)"), "{desc}");
        assert!(desc.contains("columns: col 2"), "{desc}");
        // Explicit `Horizontal`, distinct from the `None` default's
        // "HORIZONTAL (default)" wording.
        assert!(desc.contains("layout: HORIZONTAL"), "{desc}");
        assert!(!desc.contains("(default)"), "{desc}");
    }

    #[test]
    fn describe_anchor_currently_covers_empty_value_and_pivot() {
        assert_eq!(describe_anchor_currently(None), "empty");

        let value_cell = CellSnapshot {
            pivot_table: None,
            formatted_value: Some("Q2 totals".to_string()),
        };
        assert_eq!(
            describe_anchor_currently(Some(&value_cell)),
            "value \"Q2 totals\""
        );

        let pivot_cell = CellSnapshot {
            pivot_table: Some(PivotTable {
                source: GridRange::default(),
                values: vec![PivotValue::default()],
                ..Default::default()
            }),
            formatted_value: None,
        };
        let pivot_desc = describe_anchor_currently(Some(&pivot_cell));
        assert!(pivot_desc.contains("a pivot table"), "{pivot_desc}");

        // A cell present in the response but carrying neither a pivot
        // table nor a (non-empty) formatted value — the catch-all arm.
        let blank_cell = CellSnapshot {
            pivot_table: None,
            formatted_value: None,
        };
        assert_eq!(describe_anchor_currently(Some(&blank_cell)), "empty");
        let empty_string_cell = CellSnapshot {
            pivot_table: None,
            formatted_value: Some(String::new()),
        };
        assert_eq!(describe_anchor_currently(Some(&empty_string_cell)), "empty");
    }

    // ── verb/result structural sanity ────────────────────────────────────

    #[test]
    fn pivot_verb_label_names_each_verb() {
        let add = PivotVerb::AddPivotTable {
            sheet: String::new(),
            anchor: String::new(),
            source: String::new(),
            rows: vec![],
            columns: vec![],
            values: vec![],
            filters: vec![],
            value_layout: None,
            show_totals: true,
        };
        assert_eq!(add.label(), "add-pivot-table");
        let delete = PivotVerb::DeletePivotTable {
            sheet: String::new(),
            anchor: String::new(),
        };
        assert_eq!(delete.label(), "delete-pivot-table");
    }

    #[test]
    fn list_entries_computes_the_real_anchor_from_chunk_offsets() {
        let workbook: crate::drive::sheets::types::Spreadsheet =
            serde_json::from_value(serde_json::json!({
                "sheets": [{
                    "properties": {"sheetId": 0, "title": "Report"},
                    "data": [{
                        "startRow": 2, "startColumn": 1,
                        "rowData": [
                            {"values": [{}, {"pivotTable": existing_pivot_json()}]},
                        ],
                    }],
                }],
            }))
            .unwrap();
        let entries = list_entries(&workbook);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].sheet, "Report");
        // `startColumn` 1 + the pivot's own column index 1 = column C;
        // `startRow` 2 + row index 0, 1-based = row 3.
        assert_eq!(entries[0].anchor, "C3");
        let summary = &entries[0].summary;
        assert!(summary.contains("value(s)"), "{summary}");
    }

    #[test]
    fn pivot_outcome_write_jsonl_emits_the_result_status() {
        let outcome = outcome_with(add_verb(), Some("Budget"), PivotResult::RefusedShortcut);
        let mut buf = Vec::new();
        outcome.write_jsonl(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("refused-shortcut"), "{text}");
    }

    #[test]
    fn log_operations_are_distinct_and_kebab_cased() {
        let ops: HashSet<&str> = [
            PivotVerb::AddPivotTable {
                sheet: String::new(),
                anchor: String::new(),
                source: String::new(),
                rows: vec![],
                columns: vec![],
                values: vec![],
                filters: vec![],
                value_layout: None,
                show_totals: true,
            }
            .log_operation(),
            PivotVerb::DeletePivotTable {
                sheet: String::new(),
                anchor: String::new(),
            }
            .log_operation(),
        ]
        .into_iter()
        .collect();
        assert_eq!(ops.len(), 2);
        for op in ops {
            assert!(op.starts_with("sheets-"), "{op}");
            assert!(!op.contains('_'), "{op}");
        }
    }

    #[test]
    fn gate_operations_put_sheets_write_first_for_add() {
        let verb = PivotVerb::AddPivotTable {
            sheet: String::new(),
            anchor: String::new(),
            source: String::new(),
            rows: vec![],
            columns: vec![],
            values: vec![],
            filters: vec![],
            value_layout: None,
            show_totals: true,
        };
        assert_eq!(
            verb.gate_operations(),
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure]
        );
        let delete = PivotVerb::DeletePivotTable {
            sheet: String::new(),
            anchor: String::new(),
        };
        assert_eq!(delete.gate_operations(), &[DriveOperation::SheetsWrite]);
    }

    // ── end-to-end (wiremock) ────────────────────────────────────────────

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

    fn allow_rule(folder: &str, ops: &[DriveOperation]) -> FolderPermissionRule {
        FolderPermissionRule {
            folder_id: Some(folder.to_string()),
            file_id: None,
            recursive: true,
            allow: ops.iter().copied().collect(),
            deny: HashSet::default(),
            require_lease: true,
        }
    }

    fn leased_opts_for(spreadsheet_id: &str) -> (Option<String>, std::path::PathBuf) {
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        let token = seed_lease(&ledger_path, spreadsheet_id, "1");
        (Some(token), ledger_path)
    }

    /// A workbook with an empty anchor sheet ("Report", sheetId 0) and a
    /// 100x4 source sheet ("Data", sheetId 1). `anchor_pivot`, if given,
    /// is embedded as an existing pivot table at "Report"!A1.
    fn mount_workbook(anchor_pivot: Option<serde_json::Value>) -> wiremock::Mock {
        let report_data = anchor_pivot.map(|pivot| {
            serde_json::json!([{
                "startRow": 0, "startColumn": 0,
                "rowData": [{"values": [{"pivotTable": pivot}]}],
            }])
        });
        let mut report_sheet = serde_json::json!({
            "properties": {"sheetId": 0, "title": "Report", "index": 0},
        });
        if let Some(data) = report_data {
            report_sheet["data"] = data;
        }
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/sheet-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "spreadsheetId": "sheet-1",
                    "properties": {"title": "Budget"},
                    "sheets": [
                        report_sheet,
                        {"properties": {"sheetId": 1, "title": "Data", "index": 1}},
                    ],
                })),
            )
    }

    fn add_verb() -> PivotVerb {
        PivotVerb::AddPivotTable {
            sheet: "Report".to_string(),
            anchor: "A1".to_string(),
            source: "Data!A1:D100".to_string(),
            rows: vec!["0:asc".to_string()],
            columns: vec![],
            values: vec!["3:sum".to_string()],
            filters: vec![],
            value_layout: None,
            show_totals: true,
        }
    }

    fn delete_verb() -> PivotVerb {
        PivotVerb::DeletePivotTable {
            sheet: "Report".to_string(),
            anchor: "A1".to_string(),
        }
    }

    fn existing_pivot_json() -> serde_json::Value {
        serde_json::json!({
            "source": {
                "sheetId": 1, "startRowIndex": 0, "endRowIndex": 100,
                "startColumnIndex": 0, "endColumnIndex": 4,
            },
            "values": [{"sourceColumnOffset": 2, "summarizeFunction": "COUNTA"}],
        })
    }

    #[tokio::test]
    async fn add_denied_when_only_sheets_structure_is_granted_names_sheets_write() {
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
        let rules = vec![allow_rule("folder-1", &[DriveOperation::SheetsStructure])];
        let opts = PivotOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: add_verb(),
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = pivot(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            PivotResult::Blocked { operation, .. } => {
                assert_eq!(operation, DriveOperation::SheetsWrite);
            }
            other => panic!("expected Blocked naming sheets-write, got {other:?}"),
        }
        // No workbook GET is mocked beyond the target/parent metadata —
        // if the gate had been bypassed, the missing mock would 404 and
        // the outcome would be `Failed` instead of `Blocked`.
    }

    #[tokio::test]
    async fn delete_allowed_by_sheets_write_alone() {
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
        mount_workbook(Some(existing_pivot_json()))
            .mount(&server)
            .await;
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
        // Only `sheets-write` is granted — `delete-pivot-table` must not
        // need `sheets-structure`.
        let rules = vec![allow_rule("folder-1", &[DriveOperation::SheetsWrite])];
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
        let opts = PivotOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: delete_verb(),
            dry_run: false,
            lease_token,
            ledger_path,
        };
        let outcome = pivot(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(outcome.result, PivotResult::Changed(_)),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn add_dry_run_reports_the_extent_note_and_calls_no_batch_update() {
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
        mount_workbook(None).mount(&server).await;
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let opts = PivotOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: add_verb(),
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = pivot(&drive, &sheets, &opts, &rules).await;
        let PivotResult::WouldChange(change) = &outcome.result else {
            panic!("expected WouldChange, got {:?}", outcome.result); // omni-dev: coverage ignore-line reason="dry_run plus a fully-allowing gate always reaches WouldChange here; this branch is a safety net against an unexpected refusal, not a coverage gap"
        };
        assert_eq!(change.anchor_currently, "empty");
        let rendered = describe(&outcome);
        assert!(rendered.contains("computed by the server"), "{rendered}");
        assert!(rendered.contains("Data!A1:D100"), "{rendered}");

        assert!(!server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|r| r.url.path().ends_with(":batchUpdate")));
    }

    #[tokio::test]
    async fn add_sends_updatecells_with_no_uservalue_and_the_pivot_field_mask() {
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
        mount_workbook(None).mount(&server).await;
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
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
        let opts = PivotOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: add_verb(),
            dry_run: false,
            lease_token,
            ledger_path,
        };
        let outcome = pivot(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(outcome.result, PivotResult::Changed(_)),
            "{:?}",
            outcome.result
        );

        let requests = server.received_requests().await.unwrap();
        let batch = requests
            .iter()
            .find(|r| r.url.path().ends_with(":batchUpdate"))
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&batch.body).unwrap();
        let update = &body["requests"][0]["updateCells"];
        assert_eq!(update["fields"], "pivotTable");
        assert_eq!(update["start"]["sheetId"], 0);
        assert_eq!(update["start"]["rowIndex"], 0);
        assert_eq!(update["start"]["columnIndex"], 0);
        let cell = &update["rows"][0]["values"][0];
        assert!(
            cell.get("userEnteredValue").is_none(),
            "updateCells must never carry a value: {cell}"
        );
        let pivot = &cell["pivotTable"];
        assert_eq!(pivot["source"]["sheetId"], 1);
        assert_eq!(pivot["rows"][0]["sourceColumnOffset"], 0);
        assert_eq!(pivot["rows"][0]["sortOrder"], "ASCENDING");
        assert_eq!(pivot["values"][0]["sourceColumnOffset"], 3);
        assert_eq!(pivot["values"][0]["summarizeFunction"], "SUM");
    }

    #[tokio::test]
    async fn delete_sends_an_empty_cell_to_clear_the_pivot() {
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
        mount_workbook(Some(existing_pivot_json()))
            .mount(&server)
            .await;
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
        let rules = vec![allow_rule("folder-1", &[DriveOperation::SheetsWrite])];
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
        let opts = PivotOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: delete_verb(),
            dry_run: false,
            lease_token,
            ledger_path,
        };
        let outcome = pivot(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(outcome.result, PivotResult::Changed(_)),
            "{:?}",
            outcome.result
        );

        let requests = server.received_requests().await.unwrap();
        let batch = requests
            .iter()
            .find(|r| r.url.path().ends_with(":batchUpdate"))
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&batch.body).unwrap();
        let update = &body["requests"][0]["updateCells"];
        assert_eq!(update["fields"], "pivotTable");
        assert_eq!(update["rows"][0]["values"][0], serde_json::json!({}));
    }

    #[tokio::test]
    async fn add_refuses_an_anchor_that_already_holds_a_pivot_table() {
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
        mount_workbook(Some(existing_pivot_json()))
            .mount(&server)
            .await;
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let opts = PivotOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: add_verb(),
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = pivot(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(
                outcome.result,
                PivotResult::RefusedAnchorHasPivotTable { .. }
            ),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn delete_refuses_an_anchor_with_no_pivot_table() {
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
        mount_workbook(None).mount(&server).await;
        let rules = vec![allow_rule("folder-1", &[DriveOperation::SheetsWrite])];
        let opts = PivotOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: delete_verb(),
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = pivot(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(
                outcome.result,
                PivotResult::RefusedNoPivotTableAtAnchor { .. }
            ),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn add_refuses_a_column_offset_out_of_bounds_for_the_source() {
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
        mount_workbook(None).mount(&server).await;
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let verb = PivotVerb::AddPivotTable {
            sheet: "Report".to_string(),
            anchor: "A1".to_string(),
            source: "Data!A1:D100".to_string(),
            rows: vec![],
            columns: vec![],
            // Source is 4 columns wide (offsets 0-3); 9 is out of bounds.
            values: vec!["9:sum".to_string()],
            filters: vec![],
            value_layout: None,
            show_totals: true,
        };
        let opts = PivotOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb,
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = pivot(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            PivotResult::RefusedInvalidSource { detail } => {
                assert!(detail.contains("out of bounds"), "{detail}");
            }
            other => panic!("expected RefusedInvalidSource, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn add_refuses_with_no_values_given() {
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
        mount_workbook(None).mount(&server).await;
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let verb = PivotVerb::AddPivotTable {
            sheet: "Report".to_string(),
            anchor: "A1".to_string(),
            source: "Data!A1:D100".to_string(),
            rows: vec![],
            columns: vec![],
            values: vec![],
            filters: vec![],
            value_layout: None,
            show_totals: true,
        };
        let opts = PivotOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb,
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = pivot(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            PivotResult::RefusedInvalidSource { detail } => {
                assert!(detail.contains("at least one --value"), "{detail}");
            }
            other => panic!("expected RefusedInvalidSource, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn add_refuses_an_anchor_that_names_more_than_one_cell() {
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
        mount_workbook(None).mount(&server).await;
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let mut verb = add_verb();
        if let PivotVerb::AddPivotTable { anchor, .. } = &mut verb {
            *anchor = "A1:B2".to_string();
        }
        let opts = PivotOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb,
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = pivot(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(outcome.result, PivotResult::RefusedInvalidAnchor { .. }),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn add_refuses_when_sheet_and_anchor_conflict_on_the_sheet_name() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        // No mocks at all: the ambiguity is caught by `a1::compose` before
        // any HTTP request is ever sent.
        let mut verb = add_verb();
        if let PivotVerb::AddPivotTable { anchor, .. } = &mut verb {
            *anchor = "Other!A1".to_string();
        }
        let opts = PivotOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb,
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = pivot(&drive, &sheets, &opts, &[]).await;
        assert!(
            matches!(outcome.result, PivotResult::RefusedInvalidRange { .. }),
            "{:?}",
            outcome.result
        );
        assert_eq!(outcome.file_name, None);
    }

    #[tokio::test]
    async fn pivot_reports_failed_on_a_metadata_fetch_failure() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/missing"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;
        let opts = PivotOptions {
            spreadsheet_id: "missing".to_string(),
            verb: add_verb(),
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = pivot(&drive, &sheets, &opts, &[]).await;
        assert!(
            matches!(outcome.result, PivotResult::Failed { .. }),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn pivot_refuses_a_shortcut_target() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            "application/vnd.google-apps.shortcut",
            &["folder-1"],
        )
        .mount(&server)
        .await;
        // No mock for folder-1: proves the gate never runs.
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let opts = PivotOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: add_verb(),
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = pivot(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(outcome.result, PivotResult::RefusedShortcut),
            "{:?}",
            outcome.result
        );
        assert_eq!(outcome.file_name.as_deref(), Some("sheet-1"));
    }

    #[tokio::test]
    async fn pivot_refuses_a_non_spreadsheet_target() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", "text/plain", &["folder-1"])
            .mount(&server)
            .await;
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let opts = PivotOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: add_verb(),
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = pivot(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(outcome.result, PivotResult::RefusedNotASpreadsheet { .. }),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn pivot_refuses_a_target_with_no_visible_parents() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", crate::drive::types::GOOGLE_SHEET_MIME_TYPE, &[])
            .mount(&server)
            .await;
        let opts = PivotOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: add_verb(),
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = pivot(&drive, &sheets, &opts, &[]).await;
        assert!(
            matches!(outcome.result, PivotResult::RefusedNoVisibleParents),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn pivot_reports_failed_on_a_gate_ancestor_fetch_failure() {
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
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let opts = PivotOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: add_verb(),
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = pivot(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(outcome.result, PivotResult::Failed { .. }),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn add_reports_failed_when_the_workbook_fetch_fails() {
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
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let opts = PivotOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: add_verb(),
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = pivot(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(outcome.result, PivotResult::Failed { .. }),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn pivot_reports_failed_when_the_cell_pivot_fetch_fails() {
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
        mount_workbook(None).mount(&server).await;
        // A more specific mock, matched only by the `ranges`-scoped
        // `get_cell_pivot` call. wiremock checks equal-priority mocks in
        // mount order, so a higher priority (a *lower* number) is needed
        // for this one to be checked before the general workbook mock
        // above — which otherwise matches first and would serve `200` to
        // every `GET`, `ranges` or not. `get_spreadsheet` (which never
        // sends `ranges`) still falls through to that general mock.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/sheet-1"))
            .and(wiremock::matchers::query_param("ranges", "'Report'!A1"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .with_priority(1)
            .mount(&server)
            .await;
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let opts = PivotOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: add_verb(),
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = pivot(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(outcome.result, PivotResult::Failed { .. }),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn add_refuses_an_anchor_sheet_that_does_not_exist() {
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
        mount_workbook(None).mount(&server).await;
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let mut verb = add_verb();
        if let PivotVerb::AddPivotTable { sheet, .. } = &mut verb {
            *sheet = "Nope".to_string();
        }
        let opts = PivotOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb,
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = pivot(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(outcome.result, PivotResult::RefusedSheetNotFound { .. }),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn add_refuses_an_anchor_with_invalid_range_syntax() {
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
        mount_workbook(None).mount(&server).await;
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let mut verb = add_verb();
        if let PivotVerb::AddPivotTable { anchor, .. } = &mut verb {
            // Row 0 does not exist (rows are 1-based) — a genuinely
            // malformed range, distinct from the multi-cell case above.
            *anchor = "A0".to_string();
        }
        let opts = PivotOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb,
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = pivot(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(outcome.result, PivotResult::RefusedInvalidRange { .. }),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn add_refuses_an_invalid_source_composed_from_sheet_and_bare_range() {
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
        mount_workbook(None).mount(&server).await;
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let mut verb = add_verb();
        if let PivotVerb::AddPivotTable { source, .. } = &mut verb {
            // No `Sheet!` prefix — falls back to composing with `--sheet`.
            // The newline sits *inside* the range (not at either edge, so
            // `compose`'s own `str::trim` can't remove it) and fails
            // `a1::validate_range` inside `compose`.
            *source = "A1\n:D100".to_string();
        }
        let opts = PivotOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb,
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = pivot(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(outcome.result, PivotResult::RefusedInvalidRange { .. }),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn add_refuses_a_source_sheet_that_does_not_exist() {
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
        mount_workbook(None).mount(&server).await;
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let mut verb = add_verb();
        if let PivotVerb::AddPivotTable { source, .. } = &mut verb {
            *source = "Nope!A1:D10".to_string();
        }
        let opts = PivotOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb,
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = pivot(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(outcome.result, PivotResult::RefusedSheetNotFound { .. }),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn add_refuses_a_source_with_invalid_range_syntax() {
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
        mount_workbook(None).mount(&server).await;
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let mut verb = add_verb();
        if let PivotVerb::AddPivotTable { source, .. } = &mut verb {
            *source = "Data!A0:D10".to_string();
        }
        let opts = PivotOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb,
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = pivot(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(outcome.result, PivotResult::RefusedInvalidRange { .. }),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn add_refuses_an_unbounded_source_range() {
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
        mount_workbook(None).mount(&server).await;
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let mut verb = add_verb();
        if let PivotVerb::AddPivotTable { source, .. } = &mut verb {
            // A whole-column span: bounded in columns but not in rows.
            *source = "Data!A:D".to_string();
        }
        let opts = PivotOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb,
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = pivot(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            PivotResult::RefusedInvalidSource { detail } => {
                assert!(detail.contains("bounded range"), "{detail}");
            }
            other => panic!("expected RefusedInvalidSource, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn add_refuses_an_unparseable_row_spec() {
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
        mount_workbook(None).mount(&server).await;
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let mut verb = add_verb();
        if let PivotVerb::AddPivotTable { rows, .. } = &mut verb {
            *rows = vec!["bad".to_string()];
        }
        let opts = PivotOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb,
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = pivot(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(outcome.result, PivotResult::RefusedInvalidSource { .. }),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn add_refuses_an_unparseable_column_spec() {
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
        mount_workbook(None).mount(&server).await;
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let mut verb = add_verb();
        if let PivotVerb::AddPivotTable { columns, .. } = &mut verb {
            *columns = vec!["bad".to_string()];
        }
        let opts = PivotOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb,
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = pivot(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(outcome.result, PivotResult::RefusedInvalidSource { .. }),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn add_refuses_an_unparseable_value_spec() {
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
        mount_workbook(None).mount(&server).await;
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let mut verb = add_verb();
        if let PivotVerb::AddPivotTable { values, .. } = &mut verb {
            *values = vec!["bad".to_string()];
        }
        let opts = PivotOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb,
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = pivot(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(outcome.result, PivotResult::RefusedInvalidSource { .. }),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn add_refuses_an_unparseable_filter_spec() {
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
        mount_workbook(None).mount(&server).await;
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let mut verb = add_verb();
        if let PivotVerb::AddPivotTable { filters, .. } = &mut verb {
            *filters = vec!["bad".to_string()];
        }
        let opts = PivotOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb,
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = pivot(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(outcome.result, PivotResult::RefusedInvalidSource { .. }),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn add_reports_failed_when_batch_update_fails() {
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
        mount_workbook(None).mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
        let opts = PivotOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: add_verb(),
            dry_run: false,
            lease_token,
            ledger_path,
        };
        let outcome = pivot(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(outcome.result, PivotResult::Failed { .. }),
            "{:?}",
            outcome.result
        );
    }

    // ── the Drive write lease (ADR-0080 §9) ─────────────────────────────

    #[tokio::test]
    async fn add_refuses_without_a_lease_when_the_rule_requires_one() {
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
        mount_workbook(None).mount(&server).await;
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let opts = PivotOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: add_verb(),
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = pivot(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, PivotResult::RefusedNoLease));
    }

    #[tokio::test]
    async fn add_refuses_an_unknown_lease_token() {
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
        mount_workbook(None).mount(&server).await;
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        let opts = PivotOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: add_verb(),
            dry_run: false,
            lease_token: Some("bogus-token".to_string()),
            ledger_path,
        };
        let outcome = pivot(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, PivotResult::RefusedLeaseExpired));
    }

    #[tokio::test]
    async fn add_refuses_a_lease_bound_to_a_different_file() {
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
        mount_workbook(None).mount(&server).await;
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        let token = seed_lease(&ledger_path, "some-other-sheet", "1");
        let opts = PivotOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: add_verb(),
            dry_run: false,
            lease_token: Some(token),
            ledger_path,
        };
        let outcome = pivot(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, PivotResult::RefusedLeaseWrongFile));
    }

    #[tokio::test]
    async fn add_refuses_a_stale_lease_when_the_file_has_moved() {
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
        mount_workbook(None).mount(&server).await;
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        let token = seed_lease(&ledger_path, "sheet-1", "0");
        let opts = PivotOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: add_verb(),
            dry_run: false,
            lease_token: Some(token),
            ledger_path,
        };
        let outcome = pivot(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, PivotResult::RefusedLeaseStale));
    }

    #[tokio::test]
    async fn add_reports_failed_when_the_lease_checks_live_version_refetch_fails() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        // The gate's own metadata fetch consumes this mock (it fetches the
        // target's metadata exactly once); the lease check's *separate*
        // live-version refetch (`gate_leased_write`) is a second
        // `files.get` call, served by the always-on 500 mock below.
        mount_file(
            "sheet-1",
            crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
            &["folder-1"],
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/sheet-1"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        mount_folder("folder-1").mount(&server).await;
        mount_workbook(None).mount(&server).await;
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
        let opts = PivotOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: add_verb(),
            dry_run: false,
            lease_token,
            ledger_path,
        };
        let outcome = pivot(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(outcome.result, PivotResult::Failed { .. }),
            "{:?}",
            outcome.result
        );
    }

    // ── describe_lines / describe ────────────────────────────────────────

    fn outcome_with(verb: PivotVerb, file_name: Option<&str>, result: PivotResult) -> PivotOutcome {
        PivotOutcome {
            spreadsheet_id: "sheet-1".to_string(),
            file_name: file_name.map(str::to_string),
            resolved_folder_id: None,
            verb,
            result,
        }
    }

    #[test]
    fn describe_lines_renders_book_fallback_when_file_name_is_absent() {
        let out = outcome_with(add_verb(), None, PivotResult::RefusedShortcut);
        let text = describe(&out);
        assert!(text.contains("'sheet-1'"), "{text}");
    }

    #[test]
    fn describe_lines_renders_not_a_spreadsheet_and_shortcut() {
        let not_a_sheet = outcome_with(
            add_verb(),
            Some("Budget"),
            PivotResult::RefusedNotASpreadsheet {
                mime_type: "text/plain".to_string(),
            },
        );
        let text = describe(&not_a_sheet);
        assert!(text.contains("not a Google Sheet"), "{text}");
        assert!(text.contains("add-pivot-table"), "{text}");

        let shortcut = outcome_with(delete_verb(), Some("Budget"), PivotResult::RefusedShortcut);
        let text = describe(&shortcut);
        assert!(text.contains("shortcut"), "{text}");
        assert!(text.contains("delete-pivot-table"), "{text}");
    }

    #[test]
    fn describe_lines_renders_no_visible_parents_naming_each_verbs_operations() {
        let add = outcome_with(
            add_verb(),
            Some("Budget"),
            PivotResult::RefusedNoVisibleParents,
        );
        let text = describe(&add);
        assert!(text.contains("\"sheets-write\""), "{text}");
        assert!(text.contains("\"sheets-structure\""), "{text}");

        let delete = outcome_with(
            delete_verb(),
            Some("Budget"),
            PivotResult::RefusedNoVisibleParents,
        );
        let text = describe(&delete);
        assert!(text.contains("\"sheets-write\""), "{text}");
        assert!(!text.contains("sheets-structure"), "{text}");
    }

    #[test]
    fn describe_lines_renders_sheet_not_found_with_and_without_available_titles() {
        let none_available = outcome_with(
            add_verb(),
            Some("Budget"),
            PivotResult::RefusedSheetNotFound {
                title: "Nope".to_string(),
                available: vec![],
            },
        );
        let text = describe(&none_available);
        assert!(text.contains("Available: none"), "{text}");

        let some_available = outcome_with(
            add_verb(),
            Some("Budget"),
            PivotResult::RefusedSheetNotFound {
                title: "Nope".to_string(),
                available: vec!["Report".to_string(), "Data".to_string()],
            },
        );
        let text = describe(&some_available);
        assert!(text.contains("'Report', 'Data'"), "{text}");
    }

    #[test]
    fn describe_lines_renders_invalid_range_anchor_and_source() {
        for result in [
            PivotResult::RefusedInvalidRange {
                detail: "bad range".to_string(),
            },
            PivotResult::RefusedInvalidAnchor {
                detail: "bad anchor".to_string(),
            },
            PivotResult::RefusedInvalidSource {
                detail: "bad source".to_string(),
            },
        ] {
            let out = outcome_with(add_verb(), Some("Budget"), result);
            let text = describe(&out);
            assert!(text.starts_with("Refused: bad "), "{text}");
        }
    }

    #[test]
    fn describe_lines_renders_anchor_has_pivot_table_and_no_pivot_at_anchor() {
        let occupied = outcome_with(
            add_verb(),
            Some("Budget"),
            PivotResult::RefusedAnchorHasPivotTable {
                anchor: "'Report'!A1".to_string(),
                existing_summary: "1 row group(s), 0 column group(s), 1 value(s)".to_string(),
            },
        );
        let text = describe(&occupied);
        assert!(text.contains("already holds a pivot table"), "{text}");
        assert!(text.contains("delete-pivot-table"), "{text}");

        let missing = outcome_with(
            delete_verb(),
            Some("Budget"),
            PivotResult::RefusedNoPivotTableAtAnchor {
                anchor: "'Report'!A1".to_string(),
            },
        );
        let text = describe(&missing);
        assert!(text.contains("no pivot table anchored"), "{text}");
        assert!(text.contains("list-pivot-tables"), "{text}");
    }

    #[test]
    fn describe_lines_renders_blocked_with_and_without_a_deciding_rule() {
        let folder_rule = outcome_with(
            add_verb(),
            Some("Budget"),
            PivotResult::Blocked {
                operation: DriveOperation::SheetsWrite,
                decided_by: Some(DecidingRule::Folder {
                    folder_id: "folder-1".to_string(),
                    depth: 2,
                }),
            },
        );
        let text = describe(&folder_rule);
        assert!(text.contains("add-pivot-table"), "{text}");
        assert!(text.contains("folder folder-1 (depth 2)"), "{text}");

        let default_policy = outcome_with(
            delete_verb(),
            Some("Budget"),
            PivotResult::Blocked {
                operation: DriveOperation::SheetsWrite,
                decided_by: None,
            },
        );
        let text = describe(&default_policy);
        assert!(text.contains("default policy"), "{text}");
        assert!(text.contains("sheets-write"), "{text}");
    }

    #[test]
    fn describe_lines_renders_every_lease_refusal() {
        for (result, needle) in [
            (PivotResult::RefusedNoLease, "requires a Drive write lease"),
            (
                PivotResult::RefusedLeaseExpired,
                "expired, released, or unknown",
            ),
            (
                PivotResult::RefusedLeaseWrongFile,
                "acquired for a different file",
            ),
            (
                PivotResult::RefusedLeaseStale,
                "changed since the lease was acquired",
            ),
        ] {
            let out = outcome_with(add_verb(), Some("Budget"), result);
            let text = describe(&out);
            assert!(text.contains(needle), "{text}");
            assert!(text.contains("drive lease acquire sheet-1"), "{text}");
        }
    }

    #[test]
    fn describe_lines_renders_changed_for_delete_and_failed() {
        let change = PivotChange {
            anchor: "'Report'!A1".to_string(),
            source: None,
            config: None,
            anchor_currently: "a pivot table (1 row group(s), 0 column group(s), 1 value(s))"
                .to_string(),
        };
        let changed = outcome_with(delete_verb(), Some("Budget"), PivotResult::Changed(change));
        let text = describe(&changed);
        assert!(text.contains("Applied: delete the pivot table"), "{text}");

        let failed = outcome_with(
            add_verb(),
            Some("Budget"),
            PivotResult::Failed {
                detail: "boom".to_string(),
            },
        );
        assert_eq!(describe(&failed), "Failed: boom");
    }

    #[test]
    fn log_status_covers_every_variant() {
        let decided_by = None;
        let change = PivotChange {
            anchor: "'Report'!A1".to_string(),
            source: None,
            config: None,
            anchor_currently: "empty".to_string(),
        };
        let results = [
            PivotResult::WouldChange(change.clone()),
            PivotResult::RefusedNotASpreadsheet {
                mime_type: "text/plain".to_string(),
            },
            PivotResult::RefusedShortcut,
            PivotResult::RefusedNoVisibleParents,
            PivotResult::RefusedSheetNotFound {
                title: "X".to_string(),
                available: vec![],
            },
            PivotResult::RefusedInvalidRange {
                detail: String::new(),
            },
            PivotResult::RefusedInvalidAnchor {
                detail: String::new(),
            },
            PivotResult::RefusedInvalidSource {
                detail: String::new(),
            },
            PivotResult::RefusedAnchorHasPivotTable {
                anchor: String::new(),
                existing_summary: String::new(),
            },
            PivotResult::RefusedNoPivotTableAtAnchor {
                anchor: String::new(),
            },
            PivotResult::Blocked {
                operation: DriveOperation::SheetsWrite,
                decided_by,
            },
            PivotResult::RefusedNoLease,
            PivotResult::RefusedLeaseExpired,
            PivotResult::RefusedLeaseWrongFile,
            PivotResult::RefusedLeaseStale,
            PivotResult::Changed(change),
            PivotResult::Failed {
                detail: String::new(),
            },
        ];
        let statuses: HashSet<&str> = results.iter().map(PivotResult::log_status).collect();
        assert_eq!(
            statuses.len(),
            results.len(),
            "log_status must be unique per variant"
        );
        for status in statuses {
            assert!(!status.contains('_'), "{status}");
        }
    }
}
