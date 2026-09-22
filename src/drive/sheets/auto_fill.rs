//! `auto-fill` — extends a series from source cells into an adjacent
//! destination via `spreadsheets.batchUpdate`'s `autoFill` request (issue
//! #1840, [ADR-0083](../../../docs/adrs/adr-0083.md)).
//!
//! Unblocked by issue #1831. Gated by [`DriveOperation::SheetsWrite`] alone
//! (ADR-0083 §1): the fill writes ordinary cell content into a range the
//! request names or derives, doing nothing a `sheets clear` followed by a
//! `sheets write` of the same range could not already do under the same
//! grant. **This is provisional on one live-verification item ADR-0083 §5
//! names**: if Sheets is found to carry the source cells' *formatting*
//! along with a fill, this module's gate must move to
//! `target_gate::resolve_all([SheetsWrite, SheetsStructure])`, the same
//! union `pivot.rs`'s `add-pivot-table` uses — see that section for the
//! fixed consequence. Until verified, the gate stays `SheetsWrite` alone.
//!
//! **The filled values are never previewable, and this crate never sees
//! them even after a real run.** `autoFill` carries no response object, and
//! which values it writes is entirely Sheets' own series-detection
//! heuristic (dates, numbers, days-of-week, or whatever pattern the source
//! cells show) — the same "server decides, so `--dry-run` cannot fully
//! predict it" limit ADR-0081 §5 names for a pivot table's extent. What
//! `--dry-run` (and the real run) *can* say, per ADR-0083 §6, is the
//! destination range and the **count and A1 locations** of the non-blank
//! cells within it that would be (or were) overwritten — never their
//! values, matching every preview in this tranche but `merge-cells`.
//!
//! ## The two input forms
//!
//! The API's request is a oneof, mirrored here as [`AutoFillForm`] so the
//! two shapes are mutually exclusive by construction rather than by
//! runtime validation of parallel `Option` fields:
//!
//! - **Form A** (`--range`): the whole region. Sheets "examines the range
//!   and detects the location that has data" itself, so which cells are
//!   source and which are destination is server-decided — the destination
//!   this writes is knowable only as an **upper bound** (every non-blank
//!   cell in the named range) before the request is sent.
//! - **Form B** (`--source`/`--dimension`/`--fill-length`): an explicit
//!   source range extended by a caller-chosen length and direction. The
//!   destination is computed locally ([`compute_destination`]) and so is
//!   **exact**. `--fill-length` may be negative (fills backward — up or
//!   left — instead of forward), which needs a two-sided bounds check
//!   (ADR-0077 §6's shape): a negative length is refused if it would reach
//!   before the start of the sheet.
//!
//! Both forms require a **fully bounded** source ([`grid_range::is_bounded`],
//! `merge-cells`'s own requirement, ADR-0078 §4): form B's arithmetic has
//! no fixed edge to extend from against an open-ended source, and form A's
//! preview has no fixed extent to read. A documented cut, not a silent gap.
//!
//! ## The grid edge
//!
//! A destination that runs past the sheet's current `rowCount`/
//! `columnCount` is **not refused client-side** — ADR-0083 §5: growth by
//! writing past the grid's edge is what `sheets append` already does under
//! `sheets-write`. It is surfaced as a caveat in the summary text instead,
//! and the verb never prepends a `updateSheetProperties`/`appendDimension`
//! request to grow the sheet first (ADR-0083 §5's smuggling rule).
//!
//! ## Shape
//!
//! Single-target, `write.rs`/`dimension_group.rs`'s linear shape: resolve
//! the target and gate, resolve the source range, compute the destination,
//! read its current values once (shared by `--dry-run` and the real run,
//! `merge-cells`'s own precedent), dry-run return, build the request,
//! lease, mutate, log.

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
use crate::drive::sheets::api::{SheetsApi, ValueRenderOption};
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::grid_range;
use crate::drive::sheets::target_gate;
use crate::drive::sheets::types::{
    AutoFillRequest, BatchUpdateRequestItem, Dimension, GridRange, Sheet, SourceAndDestination,
};
use crate::drive::types::SheetTargetRefusal;
use crate::drive::write_gate::{self, DecidingRule, DriveOperation, FolderPermissionRule};
use crate::request_log::{self, DriveMutationOutcome};

/// The one operation this module ever logs or leases under — there is
/// only one verb, so `dimension_group.rs`'s per-verb `log_operation()`
/// dispatch would have nothing to dispatch on. Named once so the lease
/// site and the request-log site can't drift.
const LOG_OPERATION: &str = "sheets-auto-fill";

/// Which of the API's two input forms was given — the API's own oneof.
///
/// Modelled as an enum rather than parallel `Option` fields so "both forms
/// at once" is unrepresentable. See the module docs for the two shapes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoFillForm {
    /// `--range`, optionally with `--sheet`: Sheets decides source vs.
    /// destination itself.
    Range {
        /// Sheet (tab) title. Supplies the prefix for a bare `range`.
        sheet: Option<String>,
        /// A1 range, optionally carrying its own `Sheet!` prefix.
        range: Option<String>,
    },
    /// `--source`/`--dimension`/`--fill-length`, `--sheet` supplying the
    /// prefix for a bare `source`.
    SourceAndDestination {
        /// Sheet (tab) title. Supplies the prefix for a bare `source`.
        sheet: Option<String>,
        /// A1 range holding the series to extend, optionally carrying its
        /// own `Sheet!` prefix.
        source: Option<String>,
        /// Which axis `fill_length` extends along.
        dimension: Dimension,
        /// How many rows/columns to fill. Negative fills backward (up or
        /// left) instead of forward (down or right).
        fill_length: i64,
    },
}

impl AutoFillForm {
    /// The `--sheet`/range-or-source pair, for `a1::compose` — the same
    /// shape `format.rs::FormatVerb::sheet_and_range` exposes.
    fn sheet_and_range(&self) -> (Option<&str>, Option<&str>) {
        match self {
            Self::Range { sheet, range } => (sheet.as_deref(), range.as_deref()),
            Self::SourceAndDestination { sheet, source, .. } => {
                (sheet.as_deref(), source.as_deref())
            }
        }
    }
}

/// Per-call options.
#[derive(Debug, Clone)]
pub struct AutoFillOptions {
    /// Spreadsheet id.
    pub spreadsheet_id: String,
    /// Which input form was given, and its data.
    pub form: AutoFillForm,
    /// `--alternate-series`: fills using the alternate series Sheets would
    /// not otherwise choose.
    pub use_alternate_series: bool,
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
pub enum AutoFillResult {
    /// `--dry-run`, and the gate would allow it.
    WouldChange {
        /// A human-readable, **tense-neutral** summary of the effect —
        /// also the request log's `fields_changed`. Names the destination,
        /// the source it extends (form B), the direction, and the
        /// alternate-series flag; the overwrite count and the two caveats
        /// are separate, tense-varying lines built by [`describe_lines`],
        /// so this same string reads correctly under both `Would …` and
        /// `Applied: …`.
        summary: String,
        /// The destination range's A1 address.
        destination: String,
        /// The non-blank cells within the destination that would be
        /// overwritten, as bare A1 addresses — **never their values**,
        /// unlike `merge-cells`' own `discarded_cells` (ADR-0083 §6).
        #[serde(skip_serializing_if = "Vec::is_empty")]
        overwritten_cells: Vec<String>,
        /// `true` for form A (`--range`): Sheets decides the source/
        /// destination split itself, so `overwritten_cells` names every
        /// non-blank cell in the named range, an upper bound on what the
        /// fill will actually touch. `false` for form B, where the
        /// destination — and so this list — is exact.
        destination_is_upper_bound: bool,
        /// The destination runs past the sheet's current row/column count.
        /// Never a refusal (ADR-0083 §5) — it only earns the summary's
        /// grid-extent caveat line, which is why it is carried here rather
        /// than baked into the tense-neutral [`Self::WouldChange`] summary.
        past_grid_extent: bool,
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
    /// The `--sheet`/range-or-source pair, `--fill-length`, or the
    /// resulting destination was invalid.
    RefusedInvalidRange {
        /// What was wrong and why.
        detail: String,
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
        /// The same tense-neutral summary [`Self::WouldChange`] carries,
        /// built by the same call — which is why it reads correctly after
        /// the fact as well as before it.
        summary: String,
        /// Same as [`Self::WouldChange`].
        destination: String,
        /// Same as [`Self::WouldChange`] — read *before* the mutating call
        /// (the same read serves both paths), so this names what was
        /// overwritten, not a post-hoc guess.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        overwritten_cells: Vec<String>,
        /// Same as [`Self::WouldChange`].
        destination_is_upper_bound: bool,
        /// Same as [`Self::WouldChange`].
        past_grid_extent: bool,
    },
    /// An API or validation error.
    Failed {
        /// A human-readable summary of what failed.
        detail: String,
    },
}

impl FromLeaseRefusal for AutoFillResult {
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

impl AutoFillResult {
    fn log_status(&self) -> &'static str {
        match self {
            Self::WouldChange { .. } => "would-change",
            Self::RefusedNotASpreadsheet { .. } => "refused-not-a-spreadsheet",
            Self::RefusedShortcut => "refused-shortcut",
            Self::RefusedNoVisibleParents => "refused-no-visible-parents",
            Self::RefusedSheetNotFound { .. } => "refused-sheet-not-found",
            Self::RefusedInvalidRange { .. } => "refused-invalid-range",
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
pub struct AutoFillOutcome {
    /// The spreadsheet acted on.
    pub spreadsheet_id: String,
    /// Its Drive file name, when the metadata fetch got that far.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    /// The folder the gate evaluated against.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_folder_id: Option<String>,
    /// The sheet the fill is (or would be) on, once known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sheet_id: Option<i64>,
    /// Which form/data was requested. Not serialised.
    #[serde(skip)]
    pub form: AutoFillForm,
    /// What happened.
    pub result: AutoFillResult,
}

impl JsonlSerialize for AutoFillOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> anyhow::Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Runs one auto-fill, logging every attempt that isn't a dry run.
pub async fn auto_fill(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &AutoFillOptions,
    rules: &[FolderPermissionRule],
) -> AutoFillOutcome {
    let started = Instant::now();
    let outcome = auto_fill_inner(drive, sheets, opts, rules).await;
    if !opts.dry_run {
        record_attempt(&outcome, started.elapsed());
    }
    outcome
}

async fn auto_fill_inner(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &AutoFillOptions,
    rules: &[FolderPermissionRule],
) -> AutoFillOutcome {
    let bare = |result| AutoFillOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: None,
        resolved_folder_id: None,
        sheet_id: None,
        form: opts.form.clone(),
        result,
    };

    // Compose the range/source first, like `write_inner`/`format_inner`:
    // pure, and a conflicting --sheet/--range(--source) pair, or a
    // --fill-length of 0, should fail without spending a request.
    let (sheet, range) = opts.form.sheet_and_range();
    let composed = match a1::compose(sheet, range) {
        Ok(composed) => composed,
        Err(err) => {
            return bare(AutoFillResult::RefusedInvalidRange {
                detail: err.to_string(),
            })
        }
    };
    if let AutoFillForm::SourceAndDestination { fill_length, .. } = &opts.form {
        if *fill_length == 0 {
            return bare(AutoFillResult::RefusedInvalidRange {
                detail: "--fill-length must not be 0".to_string(),
            });
        }
    }

    // ── Target resolution, pre-gate refusals, and the gate itself ──────
    let (target, decision, resolved_folder_id, requires_lease) = match target_gate::resolve(
        drive,
        &opts.spreadsheet_id,
        DriveOperation::SheetsWrite,
        rules,
    )
    .await
    {
        target_gate::TargetGateOutcome::MetadataFetchFailed { detail } => {
            return bare(AutoFillResult::Failed { detail })
        }
        target_gate::TargetGateOutcome::Refused { target, refusal } => {
            let result = match refusal {
                SheetTargetRefusal::Shortcut => AutoFillResult::RefusedShortcut,
                SheetTargetRefusal::NotASpreadsheet { mime_type } => {
                    AutoFillResult::RefusedNotASpreadsheet { mime_type }
                }
                SheetTargetRefusal::NoVisibleParents => AutoFillResult::RefusedNoVisibleParents,
            };
            return AutoFillOutcome {
                spreadsheet_id: opts.spreadsheet_id.clone(),
                file_name: Some(target.name),
                resolved_folder_id: None,
                sheet_id: None,
                form: opts.form.clone(),
                result,
            };
        }
        target_gate::TargetGateOutcome::GateFetchFailed { target, detail } => {
            return AutoFillOutcome {
                spreadsheet_id: opts.spreadsheet_id.clone(),
                file_name: Some(target.name),
                resolved_folder_id: None,
                sheet_id: None,
                form: opts.form.clone(),
                result: AutoFillResult::Failed { detail },
            };
        }
        target_gate::TargetGateOutcome::Gated {
            target,
            decision,
            resolved_folder_id,
            requires_lease,
        } => (target, decision, resolved_folder_id, requires_lease),
    };

    let pre_gated = |result| AutoFillOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: Some(target.name.clone()),
        resolved_folder_id: resolved_folder_id.clone(),
        sheet_id: None,
        form: opts.form.clone(),
        result,
    };

    if decision.verdict == write_gate::Verdict::Deny {
        return pre_gated(AutoFillResult::Blocked {
            decided_by: decision.decided_by,
        });
    }

    let api = SheetsApi::new(sheets);
    let workbook = match api.get_spreadsheet(&opts.spreadsheet_id).await {
        Ok(workbook) => workbook,
        Err(err) => {
            return pre_gated(AutoFillResult::Failed {
                detail: format!("{err:#}"),
            })
        }
    };

    let (sheet_title, source_grid) = match grid_range::resolve_grid_range(
        &workbook,
        &composed,
        |detail| AutoFillResult::RefusedInvalidRange { detail },
        |title, available| AutoFillResult::RefusedSheetNotFound { title, available },
    ) {
        Ok(resolved) => resolved,
        Err(result) => return pre_gated(result),
    };

    let gated = |result| AutoFillOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: Some(target.name.clone()),
        resolved_folder_id: resolved_folder_id.clone(),
        sheet_id: Some(source_grid.sheet_id),
        form: opts.form.clone(),
        result,
    };

    if !grid_range::is_bounded(&source_grid) {
        return gated(AutoFillResult::RefusedInvalidRange {
            detail: format!(
                "'{composed}' is open-ended; auto-fill needs a fully bounded range (e.g. A1:D10)"
            ),
        });
    }

    let (destination, is_upper_bound) = match &opts.form {
        AutoFillForm::Range { .. } => (source_grid, true),
        AutoFillForm::SourceAndDestination {
            dimension,
            fill_length,
            ..
        } => match compute_destination(source_grid, *dimension, *fill_length) {
            Ok(destination) => (destination, false),
            Err(detail) => return gated(AutoFillResult::RefusedInvalidRange { detail }),
        },
    };
    let destination_a1 = grid_range_to_a1(&sheet_title, &destination);
    // Form B's summary names the source it extends: the request log's
    // `range` key is the *destination*, so without this the record could
    // not say what was extended. Form A's source is the range itself, and
    // its summary says so instead.
    let source_a1 = grid_range_to_a1(&sheet_title, &source_grid);

    let sheet = grid_range::find_sheet_by_id(&workbook, source_grid.sheet_id);
    let past_grid_extent = sheet.is_some_and(|sheet| extends_past_grid(sheet, &destination));
    // The *read* is clamped to the grid; the request is not. A destination
    // running past the sheet's extent is deliberately left to the server
    // (ADR-0083 §5), but reading it back is this crate's own call — and a
    // read that Sheets refuses would return `Failed` before `batchUpdate`
    // was ever attempted, turning §5's "left to the server" into a
    // client-side refusal in all but name. Clamping costs nothing, since a
    // cell past the grid extent cannot hold a value and so can never
    // appear in `overwritten_cells` anyway. `None` means the destination
    // lies wholly past the extent: nothing to read, nothing to overwrite.
    let read_range = sheet.map_or(Some(destination), |sheet| {
        clamp_to_grid(sheet, &destination)
    });

    // The destination's current values are read once, before the
    // --dry-run branch, so both paths report the same overwritten cells
    // from the same read — `merge-cells`' own `read_discarded_cells`
    // precedent.
    let overwritten_cells = match read_range {
        None => Vec::new(),
        Some(read_range) => {
            let read_a1 = grid_range_to_a1(&sheet_title, &read_range);
            match api
                .values_get(&opts.spreadsheet_id, &read_a1, ValueRenderOption::Formatted)
                .await
            {
                // Offsets are the *read* range's own start, not the
                // destination's — clamping can only move the end, but
                // `grid_range::non_blank_locations` is indexed off
                // whichever range was actually read.
                Ok(values) => grid_range::non_blank_locations(
                    &values,
                    read_range.start_row_index.unwrap_or(0),
                    read_range.start_column_index.unwrap_or(0),
                ),
                Err(err) => {
                    return gated(AutoFillResult::Failed {
                        detail: format!("{err:#}"),
                    })
                }
            }
        }
    };

    let summary = describe_effect(
        &opts.form,
        &source_a1,
        &destination_a1,
        opts.use_alternate_series,
    );

    if opts.dry_run {
        return gated(AutoFillResult::WouldChange {
            summary,
            destination: destination_a1,
            overwritten_cells,
            destination_is_upper_bound: is_upper_bound,
            past_grid_extent,
        });
    }

    // Built before the gate, not after — #1688/#1742's ordering invariant,
    // shared verbatim by every leased engine. `build_request` is
    // infallible here (every fallible step already happened above:
    // composing, resolving, bounding, computing the destination), but the
    // shape is kept identical to every sibling engine's so a future
    // fallible step lands in the right place by default.
    let request = build_request(&opts.form, source_grid, opts.use_alternate_series);

    let files_api = FilesApi::new(drive);
    let leased = LeasedWrite {
        log_prefix: "drive sheets auto-fill",
        operation: LOG_OPERATION,
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
        Ok(_response) => AutoFillResult::Changed {
            summary,
            destination: destination_a1,
            overwritten_cells,
            destination_is_upper_bound: is_upper_bound,
            past_grid_extent,
        },
        Err(err) => AutoFillResult::Failed {
            detail: format!("{err:#}"),
        },
    };
    drop(lease_grant);
    gated(result)
}

/// Computes form B's destination [`GridRange`] from its bounded `source`,
/// extending `fill_length` rows/columns from `source`'s edge along
/// `dimension`. Positive fills forward (down/right) from `source`'s far
/// edge; negative fills backward (up/left) from `source`'s near edge, and
/// is refused two-sidedly (ADR-0077 §6's shape) if it would reach before
/// the start of the sheet. `fill_length == 0` is refused by the caller
/// before this is ever called.
///
/// Pure and independently tested — the one conversion site, never inlined.
fn compute_destination(
    source: GridRange,
    dimension: Dimension,
    fill_length: i64,
) -> Result<GridRange, String> {
    let (start, end) = match dimension {
        Dimension::Rows => (source.start_row_index, source.end_row_index),
        Dimension::Columns => (source.start_column_index, source.end_column_index),
    };
    // `is_bounded` is checked by the caller before this runs, but each
    // axis is read independently here, so this stays a defined error
    // rather than an `unwrap` if that ever changes.
    let (Some(start), Some(end)) = (start, end) else {
        return Err(format!(
            "--source needs a fully bounded range on the {} axis to compute a destination from",
            dimension.noun()
        ));
    };
    let (new_start, new_end) = if fill_length > 0 {
        let Some(new_end) = end.checked_add(fill_length) else {
            return Err(format!("--fill-length {fill_length} is out of range"));
        };
        (end, new_end)
    } else {
        let Some(new_start) = start.checked_add(fill_length) else {
            return Err(format!("--fill-length {fill_length} is out of range"));
        };
        if new_start < 0 {
            return Err(format!(
                "--fill-length {fill_length} would fill {} {}(s) before the start of the sheet",
                new_start.unsigned_abs(),
                dimension.noun()
            ));
        }
        (new_start, start)
    };
    let mut destination = source;
    match dimension {
        Dimension::Rows => {
            destination.start_row_index = Some(new_start);
            destination.end_row_index = Some(new_end);
        }
        Dimension::Columns => {
            destination.start_column_index = Some(new_start);
            destination.end_column_index = Some(new_end);
        }
    }
    Ok(destination)
}

/// Whether `destination` extends past `sheet`'s current known extent on
/// either axis. `false` when the sheet reports no `gridProperties` for an
/// axis (nothing to compare against), matching `dimension_group.rs`'s
/// `validate_span`'s own "extent unknown ⇒ don't refuse" stance — except
/// this never refuses either way (ADR-0083 §5); it only decides whether
/// the summary carries the caveat.
fn extends_past_grid(sheet: &Sheet, destination: &GridRange) -> bool {
    let grid = sheet
        .properties
        .as_ref()
        .and_then(|props| props.grid_properties.as_ref());
    let past_rows = match (destination.end_row_index, grid.and_then(|g| g.row_count)) {
        (Some(end), Some(current)) => end > current,
        _ => false,
    };
    let past_columns = match (
        destination.end_column_index,
        grid.and_then(|g| g.column_count),
    ) {
        (Some(end), Some(current)) => end > current,
        _ => false,
    };
    past_rows || past_columns
}

/// `destination` with each axis' end clamped to `sheet`'s current extent,
/// or `None` when that leaves nothing — the destination lies wholly past
/// the grid on some axis, so there is no cell to read back.
///
/// An axis the sheet reports no extent for is left alone: there is nothing
/// to clamp against, the same "extent unknown ⇒ don't act on it" stance
/// [`extends_past_grid`] takes.
fn clamp_to_grid(sheet: &Sheet, destination: &GridRange) -> Option<GridRange> {
    let grid = sheet
        .properties
        .as_ref()
        .and_then(|props| props.grid_properties.as_ref());
    let mut clamped = *destination;
    if let (Some(end), Some(count)) = (destination.end_row_index, grid.and_then(|g| g.row_count)) {
        clamped.end_row_index = Some(end.min(count));
    }
    if let (Some(end), Some(count)) = (
        destination.end_column_index,
        grid.and_then(|g| g.column_count),
    ) {
        clamped.end_column_index = Some(end.min(count));
    }
    let empty = |start: Option<i64>, end: Option<i64>| match (start, end) {
        (Some(start), Some(end)) => start >= end,
        _ => false,
    };
    if empty(clamped.start_row_index, clamped.end_row_index)
        || empty(clamped.start_column_index, clamped.end_column_index)
    {
        return None;
    }
    Some(clamped)
}

/// Renders a fully bounded numeric [`GridRange`] back to an A1 string.
/// Only ever called on a `source`/`destination` this module has already
/// refused unless fully bounded — see `is_bounded`'s call site above.
fn grid_range_to_a1(sheet_title: &str, grid: &GridRange) -> String {
    let (Some(r0), Some(r1), Some(c0), Some(c1)) = (
        grid.start_row_index,
        grid.end_row_index,
        grid.start_column_index,
        grid.end_column_index,
    ) else {
        unreachable!("auto-fill ranges are refused earlier unless fully bounded")
        // omni-dev: coverage ignore-line reason="every call site resolves grid from a range already checked with grid_range::is_bounded, or computes it from one via compute_destination, which only ever produces a fully bounded range from a fully bounded source; this else-arm exists only to unwrap the shared Option fields"
    };
    let start = format!("{}{}", grid_range::column_index_to_letters(c0), r0 + 1);
    let end = format!("{}{}", grid_range::column_index_to_letters(c1 - 1), r1);
    match a1::compose(Some(sheet_title), Some(&format!("{start}:{end}"))) {
        Ok(composed) => composed,
        // Not `unwrap_or_default()`: an empty range here would surface as
        // an opaque `values.get` failure rather than as itself. `compose`
        // can only reject a sheet-prefixed or whole-sheet range, and the
        // `{start}:{end}` built just above is neither. The marker is a
        // *trailing* comment because `ignore-line` silences its own line
        // only (`coverage/markers.rs`: `start: line, end: line`), so on a
        // line of its own it would silence nothing.
        Err(err) => unreachable!("auto-fill composes only bounded numeric ranges: {err}"), // omni-dev: coverage ignore-line reason="unreachable by construction: compose rejects only a sheet-prefixed or whole-sheet range, and the numeric {start}:{end} built just above is neither"
    }
}

/// The caveat every auto-fill carries, in both tenses at once — the
/// values are never reported, before *or* after the request, so unlike
/// [`PAST_GRID_EXTENT_CAVEAT_DRY_RUN`] this is one constant rather than
/// two ([`structure.rs`](super::structure)'s `FORMULA_CAVEAT` shape).
const UNPREVIEWABLE_VALUES_CAVEAT: &str =
    "  the filled values are computed by Sheets' own series detection and are never reported, \
     before or after the request";

/// Shared by the `--dry-run` and post-execution lines so the wording
/// can't drift; tense differs, so this is two constants rather than one —
/// `structure.rs`'s `INSERT_RANGE_EDGE_CAVEAT` pair, for the same reason.
const PAST_GRID_EXTENT_CAVEAT_DRY_RUN: &str =
    "  the destination extends past the sheet's current extent — Sheets may grow the sheet or \
     refuse the request";
const PAST_GRID_EXTENT_CAVEAT: &str =
    "  the destination extended past the sheet's current extent, so Sheets may have grown it";

/// The **tense-neutral** head of the summary: the destination, the source
/// it extends and the direction (form B), or the server-decided-split
/// note (form A), plus the alternate-series flag.
///
/// Deliberately carries neither the overwrite count nor either caveat.
/// Both of those vary with tense, and this string is reused verbatim by
/// [`AutoFillResult::Changed`] and by the request log's `fields_changed`,
/// where a conditional ("would be overwritten") would describe a mutation
/// that already happened. They are separate, indented lines instead —
/// which also keeps `describe_lines`' trailing `in {book}` attached to the
/// range clause rather than to a prose caveat.
fn describe_effect(
    form: &AutoFillForm,
    source_a1: &str,
    destination_a1: &str,
    use_alternate_series: bool,
) -> String {
    let mut summary = match form {
        AutoFillForm::Range { .. } => format!(
            "auto-fill within {destination_a1} (Sheets decides which cells are the source and \
             which are filled)"
        ),
        AutoFillForm::SourceAndDestination {
            dimension,
            fill_length,
            ..
        } => {
            let direction = match (dimension, *fill_length >= 0) {
                (Dimension::Rows, true) => "down",
                (Dimension::Rows, false) => "up",
                (Dimension::Columns, true) => "right",
                (Dimension::Columns, false) => "left",
            };
            format!(
                "auto-fill {destination_a1} from source {source_a1}, extending {} {}(s) \
                 {direction}",
                fill_length.abs(),
                dimension.noun(),
            )
        }
    };
    if use_alternate_series {
        summary.push_str(", using the alternate series");
    }
    summary
}

/// The indented overwrite line. `up to` marks form A's upper bound (Sheets
/// picks the source/destination split itself, so some of these cells are
/// the source and will not be touched); the tense follows `dry_run`, the
/// [`PAST_GRID_EXTENT_CAVEAT_DRY_RUN`] pair's own rule.
fn overwritten_line(
    overwritten_cells: &[String],
    destination_is_upper_bound: bool,
    dry_run: bool,
) -> String {
    if overwritten_cells.is_empty() {
        return "  no non-blank cells in the destination".to_string();
    }
    let bound = if destination_is_upper_bound {
        "up to "
    } else {
        ""
    };
    let tense = if dry_run {
        "would be overwritten"
    } else {
        "were overwritten"
    };
    format!(
        "  {bound}{} non-blank cell(s) {tense}: {}",
        overwritten_cells.len(),
        overwritten_cells.join(", ")
    )
}

/// Builds the request for `form`. Placed after the `--dry-run` branch in
/// `auto_fill_inner` deliberately — see `banding.rs::banding_inner`'s
/// comment just before its own call site for the full `#1688` ordering
/// reasoning, shared verbatim by every leased engine.
fn build_request(
    form: &AutoFillForm,
    source_grid: GridRange,
    use_alternate_series: bool,
) -> BatchUpdateRequestItem {
    let request = match form {
        AutoFillForm::Range { .. } => AutoFillRequest {
            range: Some(source_grid),
            source_and_destination: None,
            use_alternate_series,
        },
        AutoFillForm::SourceAndDestination {
            dimension,
            fill_length,
            ..
        } => AutoFillRequest {
            range: None,
            source_and_destination: Some(SourceAndDestination {
                source: source_grid,
                dimension: *dimension,
                fill_length: *fill_length,
            }),
            use_alternate_series,
        },
    };
    BatchUpdateRequestItem::AutoFill(request)
}

fn record_attempt(outcome: &AutoFillOutcome, duration: Duration) {
    let error = match &outcome.result {
        AutoFillResult::Failed { detail } => Some(detail.clone()),
        _ => None,
    };
    let decided_by = match &outcome.result {
        AutoFillResult::Blocked { decided_by } => decided_by.as_ref(),
        _ => None,
    };
    let decided_by = write_gate::decided_by_log_fields(decided_by);
    let (range, fields_changed, overwritten_cells, overwritten_cells_upper_bound) =
        match &outcome.result {
            AutoFillResult::Changed {
                summary,
                destination,
                overwritten_cells,
                destination_is_upper_bound,
                ..
            } => (
                Some(destination.clone()),
                Some(summary.clone()),
                overwritten_cells.clone(),
                *destination_is_upper_bound,
            ),
            _ => (None, None, Vec::new(), false),
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
        sheet_id: outcome.sheet_id,
        range,
        fields_changed,
        overwritten_cells,
        overwritten_cells_upper_bound,
        error,
        duration,
        ..Default::default()
    });
}

/// Renders an outcome as human-readable text.
#[must_use]
pub fn describe(outcome: &AutoFillOutcome) -> String {
    describe_lines(outcome).join("\n")
}

/// Renders an outcome as its individual lines, none of which contains a
/// newline.
#[must_use]
pub fn describe_lines(outcome: &AutoFillOutcome) -> Vec<String> {
    let book = outcome.file_name.as_deref().map_or_else(
        || format!("'{}'", outcome.spreadsheet_id),
        |n| format!("'{n}'"),
    );
    match &outcome.result {
        AutoFillResult::WouldChange {
            summary,
            overwritten_cells,
            destination_is_upper_bound,
            past_grid_extent,
            ..
        } => change_lines(
            &format!("Would {summary} in {book}"),
            overwritten_cells,
            *destination_is_upper_bound,
            *past_grid_extent,
            true,
        ),
        AutoFillResult::RefusedNotASpreadsheet { mime_type } => vec![format!(
            "Refused: {book} is not a Google Sheet (mimeType: {mime_type}); \
             `drive sheets auto-fill` only works on spreadsheets"
        )],
        AutoFillResult::RefusedShortcut => vec![format!(
            "Refused: {book} is a shortcut; `drive sheets auto-fill` doesn't follow shortcuts"
        )],
        AutoFillResult::RefusedNoVisibleParents => vec![format!(
            "Refused: {book} has no parent folder visible to this account, so no folder \
             rule can apply to it. Grant it by id instead: add {{\"file_id\": \"<spreadsheet \
             id>\", \"allow\": [\"sheets-write\"]}} to write_permissions.rules."
        )],
        AutoFillResult::RefusedSheetNotFound { title, available } => {
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
        AutoFillResult::RefusedInvalidRange { detail } => vec![format!("Refused: {detail}")],
        AutoFillResult::Blocked { decided_by } => vec![match decided_by {
            Some(rule) => format!(
                "Blocked: auto-fill on {book} refused by rule on {} {}{}",
                rule.kind_label(),
                rule.id(),
                rule.depth_suffix()
            ),
            None => format!(
                "Blocked: auto-fill on {book} refused by default policy (no matching rule for \
                 sheets-write)"
            ),
        }],
        AutoFillResult::RefusedNoLease => LeaseGateRefusal::NoLease
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        AutoFillResult::RefusedLeaseExpired => LeaseGateRefusal::Expired
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        AutoFillResult::RefusedLeaseWrongFile => LeaseGateRefusal::WrongFile
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        AutoFillResult::RefusedLeaseStale => LeaseGateRefusal::Stale
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        AutoFillResult::Changed {
            summary,
            overwritten_cells,
            destination_is_upper_bound,
            past_grid_extent,
            ..
        } => change_lines(
            &format!("Applied: {summary} in {book}"),
            overwritten_cells,
            *destination_is_upper_bound,
            *past_grid_extent,
            false,
        ),
        AutoFillResult::Failed { detail } => vec![format!("Failed: {detail}")],
    }
}

/// The head line plus its indented detail lines, shared by
/// [`AutoFillResult::WouldChange`] and [`AutoFillResult::Changed`] so the
/// two can only differ in `head` and in the tense `dry_run` selects —
/// `structure.rs`'s `vec![summary, detail]` shape.
fn change_lines(
    head: &str,
    overwritten_cells: &[String],
    destination_is_upper_bound: bool,
    past_grid_extent: bool,
    dry_run: bool,
) -> Vec<String> {
    let mut lines = vec![
        head.to_string(),
        overwritten_line(overwritten_cells, destination_is_upper_bound, dry_run),
    ];
    if past_grid_extent {
        lines.push(
            if dry_run {
                PAST_GRID_EXTENT_CAVEAT_DRY_RUN
            } else {
                PAST_GRID_EXTENT_CAVEAT
            }
            .to_string(),
        );
    }
    lines.push(UNPREVIEWABLE_VALUES_CAVEAT.to_string());
    lines
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::sheets::client::SHEETS_API_URL;
    use crate::drive::sheets::types::{GridProperties, SheetProperties};
    use crate::drive::test_support::seed_lease;
    use crate::test_support::env::MapEnv;
    use crate::utils::secret::Secret;
    use std::collections::HashSet;

    // ── pure helpers ─────────────────────────────────────────────────────

    fn bounded(sheet_id: i64, r0: i64, r1: i64, c0: i64, c1: i64) -> GridRange {
        GridRange {
            sheet_id,
            start_row_index: Some(r0),
            end_row_index: Some(r1),
            start_column_index: Some(c0),
            end_column_index: Some(c1),
        }
    }

    #[test]
    fn compute_destination_extends_rows_forward() {
        let source = bounded(0, 0, 3, 0, 2); // A1:B3
        let dest = compute_destination(source, Dimension::Rows, 4).unwrap();
        assert_eq!(dest, bounded(0, 3, 7, 0, 2)); // A4:B7
    }

    #[test]
    fn compute_destination_extends_rows_backward() {
        let source = bounded(0, 5, 8, 0, 2); // A6:B8
        let dest = compute_destination(source, Dimension::Rows, -3).unwrap();
        assert_eq!(dest, bounded(0, 2, 5, 0, 2)); // A3:B5
    }

    #[test]
    fn compute_destination_extends_columns_forward() {
        let source = bounded(0, 0, 2, 0, 1); // A1:A2
        let dest = compute_destination(source, Dimension::Columns, 2).unwrap();
        assert_eq!(dest, bounded(0, 0, 2, 1, 3)); // B1:C2
    }

    #[test]
    fn compute_destination_extends_columns_backward() {
        let source = bounded(0, 0, 2, 5, 6); // F1:F2
        let dest = compute_destination(source, Dimension::Columns, -5).unwrap();
        assert_eq!(dest, bounded(0, 0, 2, 0, 5)); // A1:E2
    }

    #[test]
    fn compute_destination_refuses_going_before_the_start_of_the_sheet() {
        let source = bounded(0, 2, 5, 0, 2); // A3:B5
        let err = compute_destination(source, Dimension::Rows, -3).unwrap_err();
        assert!(err.contains("before the start"), "{err}");
    }

    #[test]
    fn compute_destination_allows_landing_exactly_at_the_start() {
        let source = bounded(0, 2, 5, 0, 2); // A3:B5
        let dest = compute_destination(source, Dimension::Rows, -2).unwrap();
        assert_eq!(dest, bounded(0, 0, 2, 0, 2)); // A1:B2
    }

    #[test]
    fn compute_destination_refuses_overflow() {
        let source = bounded(0, i64::MAX - 1, i64::MAX, 0, 1);
        let err = compute_destination(source, Dimension::Rows, i64::MAX).unwrap_err();
        assert!(err.contains("out of range"), "{err}");
    }

    #[test]
    fn compute_destination_refuses_an_unbounded_axis_and_backward_overflow() {
        let unbounded = GridRange {
            sheet_id: 0,
            ..Default::default()
        };
        assert!(compute_destination(unbounded, Dimension::Columns, 1)
            .unwrap_err()
            .contains("fully bounded"));
        assert!(
            compute_destination(bounded(0, 0, 1, 0, 1), Dimension::Rows, i64::MIN)
                .unwrap_err()
                .contains("9223372036854775808 row(s) before the start")
        );
        assert!(
            compute_destination(bounded(0, -1, 1, 0, 1), Dimension::Rows, i64::MIN)
                .unwrap_err()
                .contains("out of range")
        );
    }

    #[test]
    fn lease_refusals_map_to_their_auto_fill_results() {
        assert_eq!(
            AutoFillResult::from_lease_expired(),
            AutoFillResult::RefusedLeaseExpired
        );
        assert_eq!(
            AutoFillResult::from_lease_wrong_file(),
            AutoFillResult::RefusedLeaseWrongFile
        );
        assert_eq!(
            AutoFillResult::from_lease_stale(),
            AutoFillResult::RefusedLeaseStale
        );
        assert_eq!(
            AutoFillResult::from_lease_failed("ledger unavailable".into()),
            AutoFillResult::Failed {
                detail: "ledger unavailable".into()
            }
        );
    }

    #[test]
    fn jsonl_outcome_serializes_one_record_without_the_input_form() {
        let outcome = AutoFillOutcome {
            spreadsheet_id: "sheet-1".into(),
            file_name: None,
            resolved_folder_id: None,
            sheet_id: None,
            form: range_form(),
            result: AutoFillResult::RefusedShortcut,
        };
        let mut out = Vec::new();
        outcome.write_jsonl(&mut out).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["result"]["status"], "refused-shortcut");
        assert!(value.get("form").is_none());
        assert_eq!(std::str::from_utf8(&out).unwrap().lines().count(), 1);
    }

    fn sheet_with_extent(row_count: i64, column_count: i64) -> Sheet {
        Sheet {
            properties: Some(SheetProperties {
                sheet_id: Some(0),
                title: "Q1".to_string(),
                index: Some(0),
                hidden: None,
                grid_properties: Some(GridProperties {
                    row_count: Some(row_count),
                    column_count: Some(column_count),
                    frozen_row_count: None,
                    frozen_column_count: None,
                    hide_gridlines: None,
                }),
                right_to_left: None,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn extends_past_grid_is_false_within_the_current_extent() {
        let sheet = sheet_with_extent(1000, 26);
        assert!(!extends_past_grid(&sheet, &bounded(0, 0, 10, 0, 5)));
    }

    #[test]
    fn extends_past_grid_detects_rows_past_the_extent() {
        let sheet = sheet_with_extent(10, 26);
        assert!(extends_past_grid(&sheet, &bounded(0, 5, 15, 0, 5)));
    }

    #[test]
    fn extends_past_grid_detects_columns_past_the_extent() {
        let sheet = sheet_with_extent(1000, 5);
        assert!(extends_past_grid(&sheet, &bounded(0, 0, 10, 0, 10)));
    }

    #[test]
    fn extends_past_grid_is_false_when_the_extent_is_unknown() {
        let sheet = Sheet {
            properties: Some(SheetProperties {
                sheet_id: Some(0),
                title: "Q1".to_string(),
                index: Some(0),
                hidden: None,
                grid_properties: None,
                right_to_left: None,
            }),
            ..Default::default()
        };
        assert!(!extends_past_grid(&sheet, &bounded(0, 0, 1_000_000, 0, 26)));
    }

    #[test]
    fn clamp_to_grid_trims_a_destination_that_overhangs_the_extent() {
        let sheet = sheet_with_extent(10, 5);
        // Rows 8..14 against a 10-row sheet -> 8..10.
        let clamped = clamp_to_grid(&sheet, &bounded(0, 8, 14, 0, 1)).unwrap();
        assert_eq!(clamped.end_row_index, Some(10));
        assert_eq!(clamped.start_row_index, Some(8));
        // Columns 3..9 against a 5-column sheet -> 3..5.
        let clamped = clamp_to_grid(&sheet, &bounded(0, 0, 1, 3, 9)).unwrap();
        assert_eq!(clamped.end_column_index, Some(5));
    }

    #[test]
    fn clamp_to_grid_leaves_a_destination_within_the_extent_untouched() {
        let sheet = sheet_with_extent(10, 5);
        let destination = bounded(0, 2, 5, 1, 3);
        assert_eq!(clamp_to_grid(&sheet, &destination), Some(destination));
    }

    #[test]
    fn clamp_to_grid_is_none_when_the_destination_lies_wholly_past_the_extent() {
        let sheet = sheet_with_extent(10, 5);
        // Starts at row 10 on a 10-row sheet: nothing left to read.
        assert_eq!(clamp_to_grid(&sheet, &bounded(0, 10, 14, 0, 1)), None);
        assert_eq!(clamp_to_grid(&sheet, &bounded(0, 0, 1, 5, 9)), None);
    }

    #[test]
    fn clamp_to_grid_leaves_an_axis_with_no_known_extent_alone() {
        let mut sheet = sheet_with_extent(10, 5);
        sheet
            .properties
            .as_mut()
            .expect("fixture always sets properties")
            .grid_properties = None;
        let destination = bounded(0, 0, 9_999, 0, 1);
        assert_eq!(clamp_to_grid(&sheet, &destination), Some(destination));
    }

    #[test]
    fn grid_range_to_a1_round_trips_a_bounded_range() {
        let grid = bounded(0, 0, 2, 0, 2); // A1:B2
        assert_eq!(grid_range_to_a1("Q1", &grid), "'Q1'!A1:B2");
    }

    #[test]
    #[should_panic(expected = "fully bounded")]
    fn grid_range_to_a1_panics_on_an_unbounded_range() {
        let grid = GridRange {
            sheet_id: 0,
            ..Default::default()
        };
        let _ = grid_range_to_a1("Q1", &grid);
    }

    #[test]
    fn build_request_range_form_carries_the_range_and_no_source_and_destination() {
        let source = bounded(0, 0, 2, 0, 2);
        let form = AutoFillForm::Range {
            sheet: Some("Q1".to_string()),
            range: Some("A1:B2".to_string()),
        };
        let BatchUpdateRequestItem::AutoFill(request) = build_request(&form, source, true) else {
            panic!("expected AutoFill"); // omni-dev: coverage ignore-line reason="guards this test's assumption; build_request always returns AutoFill"
        };
        assert_eq!(request.range, Some(source));
        assert_eq!(request.source_and_destination, None);
        assert!(request.use_alternate_series);
    }

    #[test]
    fn build_request_source_and_destination_form_carries_dimension_and_fill_length() {
        let source = bounded(0, 0, 2, 0, 2);
        let form = AutoFillForm::SourceAndDestination {
            sheet: Some("Q1".to_string()),
            source: Some("A1:B2".to_string()),
            dimension: Dimension::Rows,
            fill_length: 5,
        };
        let BatchUpdateRequestItem::AutoFill(request) = build_request(&form, source, false) else {
            panic!("expected AutoFill"); // omni-dev: coverage ignore-line reason="guards this test's assumption; build_request always returns AutoFill"
        };
        assert_eq!(request.range, None);
        let sad = request.source_and_destination.unwrap();
        assert_eq!(sad.source, source);
        assert_eq!(sad.dimension, Dimension::Rows);
        assert_eq!(sad.fill_length, 5);
        assert!(!request.use_alternate_series);
    }

    #[test]
    fn describe_effect_names_the_direction_for_every_axis_and_sign() {
        let form = |dimension, fill_length| AutoFillForm::SourceAndDestination {
            sheet: None,
            source: None,
            dimension,
            fill_length,
        };
        assert!(
            describe_effect(&form(Dimension::Rows, 3), "A1:A3", "A4:A6", false)
                .contains("3 row(s) down")
        );
        assert!(
            describe_effect(&form(Dimension::Rows, -3), "A4:A6", "A1:A3", false)
                .contains("3 row(s) up")
        );
        assert!(
            describe_effect(&form(Dimension::Columns, 2), "A1:A1", "B1:C1", false)
                .contains("2 column(s) right")
        );
        assert!(
            describe_effect(&form(Dimension::Columns, -2), "C1:C1", "A1:B1", false)
                .contains("2 column(s) left")
        );
    }

    #[test]
    fn describe_effect_names_the_source_it_extends() {
        let form = AutoFillForm::SourceAndDestination {
            sheet: None,
            source: None,
            dimension: Dimension::Rows,
            fill_length: 7,
        };
        // The request log's `range` key is the destination, so without
        // this the record could not say what was extended.
        let summary = describe_effect(&form, "'Q1'!A1:A3", "'Q1'!A4:A10", false);
        assert!(summary.contains("from source 'Q1'!A1:A3"), "{summary}");
    }

    #[test]
    fn describe_effect_names_the_range_form_as_server_decided() {
        let form = AutoFillForm::Range {
            sheet: None,
            range: None,
        };
        let summary = describe_effect(&form, "A1:A10", "A1:A10", false);
        assert!(summary.contains("Sheets decides"), "{summary}");
    }

    #[test]
    fn describe_effect_names_the_alternate_series_flag() {
        let form = AutoFillForm::SourceAndDestination {
            sheet: None,
            source: None,
            dimension: Dimension::Rows,
            fill_length: 1,
        };
        let summary = describe_effect(&form, "A1:A1", "A2:A2", true);
        assert!(summary.contains("alternate series"), "{summary}");
    }

    /// The summary is reused verbatim by `Changed` and by the request
    /// log's `fields_changed`, so it must carry no clause that only reads
    /// correctly before the fact.
    #[test]
    fn describe_effect_is_tense_neutral() {
        let form = AutoFillForm::SourceAndDestination {
            sheet: None,
            source: None,
            dimension: Dimension::Rows,
            fill_length: 1,
        };
        let summary = describe_effect(&form, "A1:A1", "A2:A2", true);
        for conditional in ["would", "cannot", "may "] {
            assert!(!summary.contains(conditional), "{summary}");
        }
    }

    #[test]
    fn overwritten_line_reports_cells_never_their_values() {
        let cells = vec!["A4".to_string(), "A5".to_string()];
        let line = overwritten_line(&cells, false, true);
        assert_eq!(line, "  2 non-blank cell(s) would be overwritten: A4, A5");
    }

    #[test]
    fn overwritten_line_marks_an_upper_bound_count_as_up_to() {
        let cells = vec!["A1".to_string()];
        let line = overwritten_line(&cells, true, true);
        assert_eq!(line, "  up to 1 non-blank cell(s) would be overwritten: A1");
    }

    #[test]
    fn overwritten_line_uses_the_past_tense_after_a_real_run() {
        let cells = vec!["A4".to_string()];
        assert_eq!(
            overwritten_line(&cells, false, false),
            "  1 non-blank cell(s) were overwritten: A4"
        );
        assert_eq!(
            overwritten_line(&[], false, false),
            "  no non-blank cells in the destination"
        );
    }

    #[test]
    fn change_lines_name_the_grid_extent_caveat_in_the_matching_tense() {
        let dry = change_lines("Would x in 'B'", &[], false, true, true);
        assert!(
            dry.contains(&PAST_GRID_EXTENT_CAVEAT_DRY_RUN.to_string()),
            "{dry:?}"
        );
        let real = change_lines("Applied: x in 'B'", &[], false, true, false);
        assert!(
            real.contains(&PAST_GRID_EXTENT_CAVEAT.to_string()),
            "{real:?}"
        );
        // Omitted entirely when the destination fits.
        let within = change_lines("Would x in 'B'", &[], false, false, true);
        assert!(
            !within.iter().any(|l| l.contains("current extent")),
            "{within:?}"
        );
    }

    #[test]
    fn change_lines_always_state_the_server_decides_the_values() {
        for dry_run in [true, false] {
            let lines = change_lines("head", &[], false, false, dry_run);
            assert!(
                lines.contains(&UNPREVIEWABLE_VALUES_CAVEAT.to_string()),
                "{lines:?}"
            );
        }
    }

    #[test]
    fn auto_fill_result_log_status_names_every_variant() {
        assert_eq!(
            AutoFillResult::WouldChange {
                summary: String::new(),
                destination: String::new(),
                overwritten_cells: Vec::new(),
                destination_is_upper_bound: false,
                past_grid_extent: false,
            }
            .log_status(),
            "would-change"
        );
        assert_eq!(
            AutoFillResult::RefusedNotASpreadsheet {
                mime_type: String::new()
            }
            .log_status(),
            "refused-not-a-spreadsheet"
        );
        assert_eq!(
            AutoFillResult::RefusedShortcut.log_status(),
            "refused-shortcut"
        );
        assert_eq!(
            AutoFillResult::RefusedNoVisibleParents.log_status(),
            "refused-no-visible-parents"
        );
        assert_eq!(
            AutoFillResult::RefusedSheetNotFound {
                title: String::new(),
                available: Vec::new(),
            }
            .log_status(),
            "refused-sheet-not-found"
        );
        assert_eq!(
            AutoFillResult::RefusedInvalidRange {
                detail: String::new()
            }
            .log_status(),
            "refused-invalid-range"
        );
        assert_eq!(
            AutoFillResult::Blocked { decided_by: None }.log_status(),
            "blocked"
        );
        assert_eq!(
            AutoFillResult::RefusedNoLease.log_status(),
            LeaseGateRefusal::NoLease.log_status()
        );
        assert_eq!(
            AutoFillResult::RefusedLeaseExpired.log_status(),
            LeaseGateRefusal::Expired.log_status()
        );
        assert_eq!(
            AutoFillResult::RefusedLeaseWrongFile.log_status(),
            LeaseGateRefusal::WrongFile.log_status()
        );
        assert_eq!(
            AutoFillResult::RefusedLeaseStale.log_status(),
            LeaseGateRefusal::Stale.log_status()
        );
        assert_eq!(
            AutoFillResult::Changed {
                summary: String::new(),
                destination: String::new(),
                overwritten_cells: Vec::new(),
                destination_is_upper_bound: false,
                past_grid_extent: false,
            }
            .log_status(),
            "changed"
        );
        assert_eq!(
            AutoFillResult::Failed {
                detail: String::new()
            }
            .log_status(),
            "failed"
        );
    }

    #[test]
    fn sheet_and_range_reads_source_for_the_source_and_destination_form() {
        let form = AutoFillForm::SourceAndDestination {
            sheet: Some("Q1".to_string()),
            source: Some("A1:A3".to_string()),
            dimension: Dimension::Rows,
            fill_length: 1,
        };
        assert_eq!(form.sheet_and_range(), (Some("Q1"), Some("A1:A3")));
    }

    /// No arm ever emits a literal newline of its own — `write.rs`'s own
    /// `every_describe_arm_renders_a_single_line` contract: `describe_lines`
    /// interpolates a Drive-supplied file name and server-supplied A1
    /// strings, and its CLI caller (`sanitize_for_terminal`) can only
    /// sanitize the **whole rendered line** rather than each interpolation.
    /// `WouldChange`/`Changed` return several lines (a head plus indented
    /// detail lines, `structure.rs`'s shape); every other arm returns one.
    #[test]
    fn no_describe_line_contains_an_embedded_newline() {
        let base = AutoFillOutcome {
            spreadsheet_id: "sheet-1".to_string(),
            file_name: Some("Budget".to_string()),
            resolved_folder_id: None,
            sheet_id: Some(0),
            form: range_form(),
            result: AutoFillResult::Failed {
                detail: String::new(),
            },
        };
        let results = vec![
            AutoFillResult::WouldChange {
                summary: "auto-fill within 'Q1'!A1:A10".to_string(),
                destination: "'Q1'!A1:A10".to_string(),
                overwritten_cells: vec!["A1".to_string()],
                destination_is_upper_bound: true,
                past_grid_extent: true,
            },
            AutoFillResult::RefusedNotASpreadsheet {
                mime_type: "application/pdf".to_string(),
            },
            AutoFillResult::RefusedShortcut,
            AutoFillResult::RefusedNoVisibleParents,
            AutoFillResult::RefusedSheetNotFound {
                title: "Q9".to_string(),
                available: vec!["Q1".to_string()],
            },
            AutoFillResult::RefusedInvalidRange {
                detail: "bad range".to_string(),
            },
            AutoFillResult::Blocked { decided_by: None },
            AutoFillResult::RefusedNoLease,
            AutoFillResult::RefusedLeaseExpired,
            AutoFillResult::RefusedLeaseWrongFile,
            AutoFillResult::RefusedLeaseStale,
            AutoFillResult::Changed {
                summary: "auto-fill within 'Q1'!A1:A10".to_string(),
                destination: "'Q1'!A1:A10".to_string(),
                overwritten_cells: Vec::new(),
                destination_is_upper_bound: true,
                past_grid_extent: false,
            },
            AutoFillResult::Failed {
                detail: "boom".to_string(),
            },
        ];
        for result in results {
            let outcome = AutoFillOutcome {
                result,
                ..base.clone()
            };
            let lines = describe_lines(&outcome);
            assert!(!lines.is_empty(), "{lines:?}");
            for line in &lines {
                assert!(!line.contains('\n') && !line.contains('\r'), "{line:?}");
            }
        }
    }

    #[test]
    fn describe_handles_missing_metadata_and_identifies_a_deciding_rule() {
        let base = AutoFillOutcome {
            spreadsheet_id: "sheet-1".into(),
            file_name: None,
            resolved_folder_id: None,
            sheet_id: None,
            form: range_form(),
            result: AutoFillResult::RefusedSheetNotFound {
                title: "Missing".into(),
                available: Vec::new(),
            },
        };
        assert!(describe(&base).contains("Available: none"));
        assert!(describe(&base).contains("'sheet-1'"));

        let blocked = AutoFillOutcome {
            result: AutoFillResult::Blocked {
                decided_by: Some(DecidingRule::Folder {
                    folder_id: "folder-1".into(),
                    depth: 1,
                }),
            },
            ..base
        };
        assert!(describe(&blocked).contains("folder-1"));
    }

    fn range_form() -> AutoFillForm {
        AutoFillForm::Range {
            sheet: Some("Q1".to_string()),
            range: Some("A1:A10".to_string()),
        }
    }

    fn source_and_destination_form(dimension: Dimension, fill_length: i64) -> AutoFillForm {
        AutoFillForm::SourceAndDestination {
            sheet: Some("Q1".to_string()),
            source: Some("A1:A3".to_string()),
            dimension,
            fill_length,
        }
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

    fn mount_workbook() -> wiremock::Mock {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/sheet-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "spreadsheetId": "sheet-1",
                    "properties": {"title": "Budget"},
                    "sheets": [
                        {"properties": {"sheetId": 0, "title": "Q1", "index": 0,
                            "gridProperties": {"rowCount": 1000, "columnCount": 26}}},
                    ],
                })),
            )
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

    fn leased_opts_for(spreadsheet_id: &str) -> (Option<String>, std::path::PathBuf) {
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        let token = seed_lease(&ledger_path, spreadsheet_id, "1");
        (Some(token), ledger_path)
    }

    fn base_opts(form: AutoFillForm, dry_run: bool) -> AutoFillOptions {
        AutoFillOptions {
            spreadsheet_id: "sheet-1".to_string(),
            form,
            use_alternate_series: false,
            dry_run,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        }
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
        let opts = base_opts(range_form(), false);
        let outcome = auto_fill(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, AutoFillResult::Blocked { .. }));
    }

    #[tokio::test]
    async fn invalid_composed_range_is_refused_before_fetching_metadata() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        let form = AutoFillForm::Range {
            sheet: Some("Q1".into()),
            range: Some("Other!A1:A2".into()),
        };
        let outcome = auto_fill_inner(&drive, &sheets, &base_opts(form, true), &[]).await;
        assert!(matches!(
            outcome.result,
            AutoFillResult::RefusedInvalidRange { .. }
        ));
        assert!(server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| r.url.path() != "/drive/v3/files/sheet-1"));
    }

    #[tokio::test]
    async fn target_resolution_refusals_keep_the_target_name() {
        for (mime, parents, expected) in [
            (
                crate::drive::types::GOOGLE_SHORTCUT_MIME_TYPE,
                vec!["folder-1"],
                "shortcut",
            ),
            ("application/pdf", vec!["folder-1"], "not-spreadsheet"),
            (
                crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
                vec![],
                "no-parents",
            ),
        ] {
            let server = wiremock::MockServer::start().await;
            let (drive, sheets) = clients(&server).await;
            mount_file("sheet-1", mime, &parents).mount(&server).await;
            let outcome =
                auto_fill_inner(&drive, &sheets, &base_opts(range_form(), true), &[]).await;
            assert_eq!(outcome.file_name.as_deref(), Some("sheet-1"));
            assert!(outcome.sheet_id.is_none());
            assert!(
                match expected {
                    "shortcut" => matches!(outcome.result, AutoFillResult::RefusedShortcut),
                    "not-spreadsheet" => matches!(
                        outcome.result,
                        AutoFillResult::RefusedNotASpreadsheet { .. }
                    ),
                    _ => matches!(outcome.result, AutoFillResult::RefusedNoVisibleParents),
                },
                "{outcome:?}"
            );
        }
    }

    #[tokio::test]
    async fn metadata_and_workbook_failures_are_reported_at_their_respective_stages() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/sheet-1"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let outcome = auto_fill_inner(&drive, &sheets, &base_opts(range_form(), true), &[]).await;
        assert!(matches!(outcome.result, AutoFillResult::Failed { .. }));
        assert!(outcome.file_name.is_none());

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
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let outcome = auto_fill_inner(
            &drive,
            &sheets,
            &base_opts(range_form(), true),
            &[allow_rule("folder-1")],
        )
        .await;
        assert!(matches!(outcome.result, AutoFillResult::Failed { .. }));
        assert_eq!(outcome.file_name.as_deref(), Some("sheet-1"));
    }

    #[tokio::test]
    async fn a_folder_lookup_failure_preserves_the_target_name() {
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
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let outcome = auto_fill_inner(
            &drive,
            &sheets,
            &base_opts(range_form(), true),
            &[allow_rule("folder-1")],
        )
        .await;
        assert!(matches!(outcome.result, AutoFillResult::Failed { .. }));
        assert_eq!(outcome.file_name.as_deref(), Some("sheet-1"));
        assert!(outcome.sheet_id.is_none());
    }

    #[tokio::test]
    async fn backward_fill_before_the_sheet_start_is_refused_before_values_get() {
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
        mount_workbook().mount(&server).await;
        let outcome = auto_fill_inner(
            &drive,
            &sheets,
            &base_opts(source_and_destination_form(Dimension::Rows, -1), true),
            &[allow_rule("folder-1")],
        )
        .await;
        assert!(matches!(
            outcome.result,
            AutoFillResult::RefusedInvalidRange { .. }
        ));
        assert_eq!(outcome.sheet_id, Some(0));
        assert!(server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| !r.url.path().contains("/values/")));
    }

    #[tokio::test]
    async fn source_and_destination_dry_run_reports_would_change_and_makes_no_batch_update_call() {
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
        mount_workbook().mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/'Q1'!A4:A10",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "values": [["existing"], [], ["also here"]]
                })),
            )
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let opts = base_opts(source_and_destination_form(Dimension::Rows, 7), true);
        let outcome = auto_fill(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            AutoFillResult::WouldChange {
                destination,
                overwritten_cells,
                destination_is_upper_bound,
                ..
            } => {
                assert_eq!(destination, "'Q1'!A4:A10");
                assert_eq!(overwritten_cells, vec!["A4".to_string(), "A6".to_string()]);
                assert!(!destination_is_upper_bound);
            }
            other => panic!("expected WouldChange, got {other:?}"),
        }
        // No mock is registered for POST batchUpdate, so a stray call
        // would fail this test outright.
    }

    #[tokio::test]
    async fn range_form_dry_run_reports_the_destination_as_an_upper_bound() {
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
        mount_workbook().mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/'Q1'!A1:A10",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "values": [["1"], ["2"]]
                })),
            )
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let opts = base_opts(range_form(), true);
        let outcome = auto_fill(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            AutoFillResult::WouldChange {
                destination_is_upper_bound,
                overwritten_cells,
                ..
            } => {
                assert!(destination_is_upper_bound);
                assert_eq!(overwritten_cells, vec!["A1".to_string(), "A2".to_string()]);
            }
            other => panic!("expected WouldChange, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_open_ended_source_is_refused() {
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
        mount_workbook().mount(&server).await;
        let rules = vec![allow_rule("folder-1")];
        let form = AutoFillForm::SourceAndDestination {
            sheet: Some("Q1".to_string()),
            source: Some("A:A".to_string()),
            dimension: Dimension::Rows,
            fill_length: 1,
        };
        let outcome = auto_fill(&drive, &sheets, &base_opts(form, true), &rules).await;
        match outcome.result {
            AutoFillResult::RefusedInvalidRange { detail } => {
                assert!(detail.contains("fully bounded"), "{detail}");
            }
            other => panic!("expected RefusedInvalidRange, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_fill_length_of_zero_is_refused_before_any_request() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        let rules: Vec<FolderPermissionRule> = Vec::new();
        let outcome = auto_fill(
            &drive,
            &sheets,
            &base_opts(source_and_destination_form(Dimension::Rows, 0), true),
            &rules,
        )
        .await;
        match outcome.result {
            AutoFillResult::RefusedInvalidRange { detail } => {
                assert!(detail.contains("--fill-length"), "{detail}");
            }
            other => panic!("expected RefusedInvalidRange, got {other:?}"),
        }
        // No mock server call is registered at all, so a network attempt
        // would fail this test outright.
    }

    #[tokio::test]
    async fn a_real_run_sends_a_source_and_destination_auto_fill_request_and_reports_changed() {
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
        mount_workbook().mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/'Q1'!A4:A10",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "requests": [{
                    "autoFill": {
                        "sourceAndDestination": {
                            "source": {"sheetId": 0, "startRowIndex": 0, "endRowIndex": 3,
                                "startColumnIndex": 0, "endColumnIndex": 1},
                            "dimension": "ROWS",
                            "fillLength": 7
                        },
                        "useAlternateSeries": false
                    }
                }]
            })))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "spreadsheetId": "sheet-1", "replies": [{}]
                })),
            )
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
        let opts = AutoFillOptions {
            lease_token,
            ledger_path,
            ..base_opts(source_and_destination_form(Dimension::Rows, 7), false)
        };
        let outcome = auto_fill(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, AutoFillResult::Changed { .. }));
    }

    /// ADR-0083 §5 leaves a past-extent destination to the server. The
    /// *read* must not pre-empt that: with no mock mounted for any
    /// `values.get` path, a request would 404 and the run would report
    /// `Failed` before `batchUpdate` was ever attempted. Clamping means
    /// the read is skipped entirely, the fill still goes out, and the
    /// grid-extent caveat is what tells the user.
    ///
    /// Only form A can reach this: form B extends from a source range's
    /// own edge, so its destination always *starts* inside the grid and
    /// can only straddle the edge (the test below), never clear it.
    #[tokio::test]
    async fn a_destination_wholly_past_the_grid_extent_skips_the_read_and_still_fills() {
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
        mount_workbook().mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "spreadsheetId": "sheet-1", "replies": [{}]
                })),
            )
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
        // The fixture sheet has 1000 rows; this --range names rows
        // 2000..2010, wholly past the extent.
        let opts = AutoFillOptions {
            lease_token,
            ledger_path,
            form: AutoFillForm::Range {
                sheet: Some("Q1".to_string()),
                range: Some("A2001:A2010".to_string()),
            },
            ..base_opts(range_form(), false)
        };
        let outcome = auto_fill(&drive, &sheets, &opts, &rules).await;
        let AutoFillResult::Changed {
            overwritten_cells,
            past_grid_extent,
            ..
        } = &outcome.result
        else {
            panic!("{:?}", outcome.result);
        };
        assert!(overwritten_cells.is_empty(), "{overwritten_cells:?}");
        assert!(past_grid_extent);
        assert!(
            !server
                .received_requests()
                .await
                .unwrap_or_default()
                .iter()
                .any(|r| r.url.path().contains("/values/")),
            "the read must be skipped, not attempted and failed"
        );
        let lines = describe_lines(&outcome);
        assert_eq!(lines[1], "  no non-blank cells in the destination");
        assert_eq!(lines[2], PAST_GRID_EXTENT_CAVEAT);
    }

    /// The partial case: the destination straddles the grid edge, so the
    /// read is trimmed to the in-grid part rather than skipped.
    #[tokio::test]
    async fn a_destination_straddling_the_grid_edge_reads_only_the_in_grid_part() {
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
        mount_workbook().mount(&server).await;
        // Source A1:A3 filled 1000 rows down lands at rows 4..1003; the
        // 1000-row sheet clamps the read to 'Q1'!A4:A1000.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/'Q1'!A4:A1000",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "values": [["keep"]]
                })),
            )
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let opts = base_opts(source_and_destination_form(Dimension::Rows, 1000), true);
        let outcome = auto_fill(&drive, &sheets, &opts, &rules).await;
        let AutoFillResult::WouldChange {
            destination,
            overwritten_cells,
            past_grid_extent,
            ..
        } = &outcome.result
        else {
            panic!("{:?}", outcome.result);
        };
        // The *destination* reported is the unclamped one — only the read
        // was trimmed.
        assert_eq!(destination, "'Q1'!A4:A1003");
        assert_eq!(overwritten_cells, &vec!["A4".to_string()]);
        assert!(past_grid_extent);
    }

    #[tokio::test]
    async fn a_values_get_failure_is_reported_as_failed() {
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
        mount_workbook().mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/'Q1'!A4:A10",
            ))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let opts = base_opts(source_and_destination_form(Dimension::Rows, 7), true);
        let outcome = auto_fill(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, AutoFillResult::Failed { .. }));
    }

    /// The fixture the three engine-level lease tests below share: an
    /// allowed target whose destination read succeeds, so the only thing
    /// left to refuse is the lease itself.
    ///
    /// Those tests are *not* duplicates of
    /// [`lease_refusals_map_to_their_auto_fill_results`], which checks the
    /// [`FromLeaseRefusal`] impl in isolation. A mis-wired `requires_lease`,
    /// or a gate evaluated in the wrong order, would leave that unit test
    /// green while the engine sailed past the lease into `batchUpdate` —
    /// which is exactly what these assert does not happen.
    async fn mount_leasable_fixture(server: &wiremock::MockServer) {
        mount_file(
            "sheet-1",
            crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
            &["folder-1"],
        )
        .mount(server)
        .await;
        mount_folder("folder-1").mount(server).await;
        mount_workbook().mount(server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/'Q1'!A4:A10",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(server)
            .await;
    }

    /// Runs a real (non-dry) fill against [`mount_leasable_fixture`] with
    /// the given lease, and asserts no `batchUpdate` was ever issued —
    /// every refusal below must happen before the mutating call.
    async fn run_with_lease(
        server: &wiremock::MockServer,
        lease_token: Option<String>,
        ledger_path: std::path::PathBuf,
    ) -> AutoFillOutcome {
        let (drive, sheets) = clients(server).await;
        mount_leasable_fixture(server).await;
        let rules = vec![allow_rule("folder-1")];
        let opts = AutoFillOptions {
            lease_token,
            ledger_path,
            ..base_opts(source_and_destination_form(Dimension::Rows, 7), false)
        };
        let outcome = auto_fill(&drive, &sheets, &opts, &rules).await;
        assert!(
            !server
                .received_requests()
                .await
                .unwrap_or_default()
                .iter()
                .any(|r| r.url.path().ends_with(":batchUpdate")),
            "a refused lease must not reach batchUpdate"
        );
        outcome
    }

    fn ledger_in_a_tempdir() -> std::path::PathBuf {
        tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl")
    }

    #[tokio::test]
    async fn an_expired_or_unknown_lease_is_refused() {
        let server = wiremock::MockServer::start().await;
        // A token this ledger has never heard of is the same refusal as an
        // expired one (ADR-0080 §1).
        let outcome = run_with_lease(
            &server,
            Some("never-issued".to_string()),
            ledger_in_a_tempdir(),
        )
        .await;
        assert!(matches!(
            outcome.result,
            AutoFillResult::RefusedLeaseExpired
        ));
    }

    #[tokio::test]
    async fn a_lease_bound_to_another_file_is_refused() {
        let server = wiremock::MockServer::start().await;
        let ledger_path = ledger_in_a_tempdir();
        let token = seed_lease(&ledger_path, "some-other-sheet", "1");
        let outcome = run_with_lease(&server, Some(token), ledger_path).await;
        assert!(matches!(
            outcome.result,
            AutoFillResult::RefusedLeaseWrongFile
        ));
    }

    #[tokio::test]
    async fn refuses_a_stale_lease_when_the_file_has_moved() {
        let server = wiremock::MockServer::start().await;
        let ledger_path = ledger_in_a_tempdir();
        // `mount_file` reports version "1"; the lease recorded "0", so the
        // file has moved under it — ADR-0080 §6's staleness check.
        let token = seed_lease(&ledger_path, "sheet-1", "0");
        let outcome = run_with_lease(&server, Some(token), ledger_path).await;
        assert!(matches!(outcome.result, AutoFillResult::RefusedLeaseStale));
    }

    /// The rendered `Changed` text, not just the variant — the summary is
    /// shared with `WouldChange` and reused as the request log's
    /// `fields_changed`, so a conditional clause leaking into it would
    /// describe a mutation that already happened.
    #[tokio::test]
    async fn a_real_run_renders_in_the_past_tense_and_names_the_source() {
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
        mount_workbook().mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/'Q1'!A4:A10",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "values": [["keep"], [], ["also"]]
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "spreadsheetId": "sheet-1", "replies": [{}]
                })),
            )
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
        let opts = AutoFillOptions {
            lease_token,
            ledger_path,
            ..base_opts(source_and_destination_form(Dimension::Rows, 7), false)
        };
        let outcome = auto_fill(&drive, &sheets, &opts, &rules).await;
        let lines = describe_lines(&outcome);
        assert_eq!(
            lines[0],
            "Applied: auto-fill 'Q1'!A4:A10 from source 'Q1'!A1:A3, extending 7 row(s) down in \
             'sheet-1'"
        );
        assert_eq!(lines[1], "  2 non-blank cell(s) were overwritten: A4, A6");
        assert_eq!(lines[2], UNPREVIEWABLE_VALUES_CAVEAT);
        let AutoFillResult::Changed { summary, .. } = &outcome.result else {
            panic!("{:?}", outcome.result);
        };
        // The same string lands in the request log's `fields_changed`.
        assert!(!summary.contains("would"), "{summary}");
    }

    #[tokio::test]
    async fn a_batch_update_failure_is_reported_as_failed() {
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
        mount_workbook().mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/'Q1'!A4:A10",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
        let opts = AutoFillOptions {
            lease_token,
            ledger_path,
            ..base_opts(source_and_destination_form(Dimension::Rows, 7), false)
        };
        let outcome = auto_fill(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, AutoFillResult::Failed { .. }));
    }

    #[tokio::test]
    async fn a_missing_lease_is_refused_when_the_deciding_rule_requires_one() {
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
        mount_workbook().mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/'Q1'!A4:A10",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let opts = base_opts(source_and_destination_form(Dimension::Rows, 7), false);
        let outcome = auto_fill(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, AutoFillResult::RefusedNoLease));
    }

    #[tokio::test]
    async fn the_sheet_not_found_refusal_names_the_available_sheets() {
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
        mount_workbook().mount(&server).await;
        let rules = vec![allow_rule("folder-1")];
        let form = AutoFillForm::Range {
            sheet: Some("Missing".to_string()),
            range: Some("A1:A2".to_string()),
        };
        let outcome = auto_fill(&drive, &sheets, &base_opts(form, true), &rules).await;
        match outcome.result {
            AutoFillResult::RefusedSheetNotFound { title, available } => {
                assert_eq!(title, "Missing");
                assert_eq!(available, vec!["Q1".to_string()]);
            }
            other => panic!("expected RefusedSheetNotFound, got {other:?}"),
        }
    }
}
