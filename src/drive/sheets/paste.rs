//! Clipboard-style range operations — `cutPaste`, `copyPaste` and
//! `pasteData` — via `spreadsheets.batchUpdate` (issue #1839,
//! [ADR-0083](../../../docs/adrs/adr-0083.md) §4).
//!
//! **Gate composition, per `PasteType`.** `cut-paste` always resolves
//! **both** `SheetsWrite` and `SheetsStructure`, whatever `--paste-type`
//! asks for: the source is always cleared in full — values, formats and
//! merges — regardless of what is pasted, and clearing formats/merges is
//! `SheetsStructure`'s territory exactly as clearing values is
//! `SheetsWrite`'s. `copy-paste` and `paste-data` resolve their gate from
//! `--paste-type` alone ([`PasteType::writes_values`]/
//! [`PasteType::writes_presentation`]): a value-only type needs
//! `SheetsWrite` alone, a presentation-only type `SheetsStructure` alone,
//! and `PASTE_NORMAL` needs both, going through
//! [`target_gate::resolve_all`] uniformly like `pivot.rs`'s union gate —
//! this module has one gate-handling code path, not three.
//!
//! **Curated `PasteType`: four of the API's seven variants** —
//! `PASTE_NORMAL`, `PASTE_VALUES`, `PASTE_FORMULA`, `PASTE_FORMAT`. See
//! [`crate::drive::sheets::types::PasteType`]'s doc comment for the
//! deferred three. `paste-data` is `delimiter`-form only; the API's `html`
//! alternative is not modelled anywhere on the wire type, so it cannot be
//! sent.
//!
//! **Defaults, fixed by ADR-0083 §4 so this module doesn't pick its own**:
//! `--paste-type` defaults to `normal` on `cut-paste`/`copy-paste` (so a
//! bare invocation under a `sheets-write`-only grant is refused, naming
//! the missing `sheets-structure` operation rather than silently pasting
//! less than the user copied); `paste-data` defaults to `values`, since its
//! delimited-text input carries no formats, merges or validation for
//! `normal` to add.
//!
//! **`--dry-run` reports counts and A1 locations, never cell values**
//! (ADR-0083 §6 — the posture every verb but `merge-cells` holds to). The
//! written extent is computed from the request plus the workbook's
//! structure alone — [`copy_paste_extent`] for `copy-paste` (the larger of
//! source and destination anchored at the destination's top-left, per the
//! API's own spill/repeat rule), the source's own dimensions for
//! `cut-paste` (a move, never a spill or repeat), and a locally-split
//! upper bound for `paste-data` (ADR-0083 §6 names this an upper bound,
//! not an exact prediction: the API's row-separator convention for
//! `pasteData` is undocumented). A bounded `values.get` then reports the
//! non-blank cells within that extent; `cut-paste` makes a second,
//! independent `values.get` over the source to report what gets cleared,
//! rather than one `values.batchGet` — simpler than matching two results
//! by their echoed `range` field for a preview that already makes one
//! `spreadsheets.get` call per attempt. A presentation-only
//! `--paste-type format` reads no values at all, the same as
//! `format-cells`' preview.
//!
//! **Grid-edge behaviour is undocumented and not verified here.** Whether
//! the API errors or expands the grid when a destination runs past the
//! sheet's current extent is left as an open item (ADR-0083 §5) — this
//! module never prepends a structural request to grow the grid first (that
//! would smuggle `SheetsStructure` into a `SheetsWrite`-gated batch), and a
//! preview whose extent exceeds the sheet's known dimensions carries a
//! fixed caveat naming the uncertainty rather than guessing which way the
//! API will go. The *preview read* is a separate question from the written
//! extent and is clipped to the sheet's current grid
//! ([`grid_range::clamp_to_sheet`]): `values.get` refuses a range past the
//! edge outright, so reading the unclipped extent would turn the very case
//! the caveat exists to report into an opaque failure.
//!
//! **Destination must be fully bounded** for every verb, the same
//! restriction `merge-cells`/`insert-range` place on their own ranges: an
//! open-ended destination (`A:A`) has no fixed size to anchor the extent
//! computation against, and guessing would risk the "preview that lies"
//! ADR-0083 §6 refuses to ship. `cut-paste`'s and `paste-data`'s
//! destination must additionally be a single cell — the API's own
//! `GridCoordinate`/`GridCoordinate` shape for those two requests, unlike
//! `copy-paste`'s `GridRange`.

use std::io::Read as _;
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
    BatchUpdateRequestItem, CopyPasteRequest, CutPasteRequest, GridCoordinate, GridRange,
    PasteDataRequest, PasteOrientation, PasteType, ValueRange,
};
use crate::drive::types::SheetTargetRefusal;
use crate::drive::write_gate::{self, DecidingRule, DriveOperation, FolderPermissionRule};
use crate::request_log::{self, DriveMutationOutcome};

const GATE_WRITE_ONLY: &[DriveOperation] = &[DriveOperation::SheetsWrite];
const GATE_STRUCTURE_ONLY: &[DriveOperation] = &[DriveOperation::SheetsStructure];
// `SheetsWrite` first: `target_gate::TargetGateUnionOutcome::Gated::denied`
// names the first of these that denied, and for a bare `--paste-type
// normal` under a `sheets-structure`-only grant, `sheets-write` is the
// operation actually missing.
const GATE_BOTH: &[DriveOperation] =
    &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure];

/// Which clipboard-style operation to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasteVerb {
    /// Move `source` to `destination` (a single cell), clearing `source`
    /// entirely regardless of `paste_type`.
    CutPaste {
        /// Default sheet for `source`/`destination` when either lacks its
        /// own `'Sheet'!` prefix.
        sheet: Option<String>,
        /// The range to move. May carry its own `'Sheet'!` prefix.
        source: String,
        /// The single-cell destination. May carry its own `'Sheet'!`
        /// prefix.
        destination: String,
        /// What to carry over into the destination. The source is cleared
        /// in full either way.
        paste_type: PasteType,
    },
    /// Copy `source` to `destination`, spilling or repeating per the
    /// API's own rule ([`copy_paste_extent`]).
    CopyPaste {
        /// Default sheet for `source`/`destination` when either lacks its
        /// own `'Sheet'!` prefix.
        sheet: Option<String>,
        /// The range to copy from. May carry its own `'Sheet'!` prefix.
        source: String,
        /// The destination range (a single cell is a valid anchor). May
        /// carry its own `'Sheet'!` prefix.
        destination: String,
        /// What to carry over.
        paste_type: PasteType,
        /// Whether rows/columns are swapped before pasting.
        orientation: PasteOrientation,
    },
    /// Paste delimited text into a range anchored at `destination`.
    PasteData {
        /// Default sheet for `destination` when it lacks its own
        /// `'Sheet'!` prefix.
        sheet: Option<String>,
        /// The single-cell destination. May carry its own `'Sheet'!`
        /// prefix.
        destination: String,
        /// The delimited text to paste.
        data: String,
        /// The delimiter splitting `data` into columns.
        delimiter: String,
        /// What to carry over.
        paste_type: PasteType,
    },
}

impl PasteVerb {
    /// The `operation` this verb records in the request log.
    const fn log_operation(&self) -> &'static str {
        match self {
            Self::CutPaste { .. } => "sheets-cut-paste",
            Self::CopyPaste { .. } => "sheets-copy-paste",
            Self::PasteData { .. } => "sheets-paste-data",
        }
    }

    /// Human-readable verb for CLI output and error messages.
    const fn label(&self) -> &'static str {
        match self {
            Self::CutPaste { .. } => "cut-paste",
            Self::CopyPaste { .. } => "copy-paste",
            Self::PasteData { .. } => "paste-data",
        }
    }

    fn sheet(&self) -> Option<&str> {
        match self {
            Self::CutPaste { sheet, .. }
            | Self::CopyPaste { sheet, .. }
            | Self::PasteData { sheet, .. } => sheet.as_deref(),
        }
    }

    fn source(&self) -> Option<&str> {
        match self {
            Self::CutPaste { source, .. } | Self::CopyPaste { source, .. } => Some(source),
            Self::PasteData { .. } => None,
        }
    }

    fn destination(&self) -> &str {
        match self {
            Self::CutPaste { destination, .. }
            | Self::CopyPaste { destination, .. }
            | Self::PasteData { destination, .. } => destination,
        }
    }

    const fn paste_type(&self) -> PasteType {
        match self {
            Self::CutPaste { paste_type, .. }
            | Self::CopyPaste { paste_type, .. }
            | Self::PasteData { paste_type, .. } => *paste_type,
        }
    }

    /// Which operations must **all** resolve `Allow` (ADR-0083 §4).
    fn gate_operations(&self) -> &'static [DriveOperation] {
        match self {
            // Always both, whatever `paste_type` names — the source is
            // cleared in full regardless (module doc comment).
            Self::CutPaste { .. } => GATE_BOTH,
            Self::CopyPaste { paste_type, .. } | Self::PasteData { paste_type, .. } => {
                match (paste_type.writes_values(), paste_type.writes_presentation()) {
                    (false, true) => GATE_STRUCTURE_ONLY,
                    (true, false) => GATE_WRITE_ONLY,
                    // `(false, false)` is unreachable while every curated
                    // `PasteType` writes at least one of the two, and shares
                    // this arm rather than getting its own so that the
                    // *strictest* gate, not the weakest, is what a future
                    // variant this table has not been taught about falls
                    // into. Merged with `(true, true)` because they must
                    // stay the same answer: splitting them would invite
                    // someone to "simplify" the unreachable one downwards.
                    (true, true) | (false, false) => GATE_BOTH,
                }
            }
        }
    }
}

/// Per-call options.
#[derive(Debug, Clone)]
pub struct PasteOptions {
    /// Spreadsheet id.
    pub spreadsheet_id: String,
    /// Which mutation to perform.
    pub verb: PasteVerb,
    /// Classify and describe only; never call `batchUpdate`.
    pub dry_run: bool,
    /// The lease token presented via `--lease`. Checked only when the
    /// deciding rule requires one.
    pub lease_token: Option<String>,
    /// Path to the lease ledger the token is checked against.
    pub ledger_path: PathBuf,
}

/// The substance of a `WouldChange`/`Changed` outcome.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PasteChange {
    /// The destination, composed with its sheet.
    pub destination: String,
    /// `cut-paste`/`copy-paste` only: the source, composed with its sheet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The A1 extent that would be (or was) written into, computed from
    /// the request plus the workbook's structure alone — always known
    /// (ADR-0083 §6), unlike the values within it.
    pub written_extent: String,
    /// Non-blank cells within `written_extent` that would be (or were)
    /// overwritten, as `"A1"` addresses — never their values. `None` when
    /// `paste_type` writes no cell content, so no values were read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overwritten: Option<Vec<String>>,
    /// `cut-paste` only: the non-blank cells within `source` that get
    /// cleared, as `"A1"` addresses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cleared: Option<Vec<String>>,
    /// Set when `written_extent` runs past the sheet's currently allocated
    /// rows/columns — whether the API errors or silently expands the grid
    /// in that case is undocumented and unverified (ADR-0083 §5); this
    /// names the uncertainty rather than guessing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grid_edge_caveat: Option<String>,
}

/// What happened (or, under `--dry-run`, would happen).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum PasteResult {
    /// `--dry-run`, and the gate would allow it.
    WouldChange(PasteChange),
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
    /// `--sheet`/`--source`/`--destination` failed to compose into a
    /// range, named a sheet no range grammar could resolve, was
    /// open-ended where a bounded range is required, or (for `cut-paste`/
    /// `paste-data`) named more than one cell where a single cell is
    /// required.
    RefusedInvalidRange {
        /// What was wrong and why.
        detail: String,
    },
    /// `paste-data`'s `--data`/`--data-file` resolved to an empty string,
    /// so there is nothing to paste and no extent to preview.
    RefusedEmptyData,
    /// The folder write-permission gate refused it.
    Blocked {
        /// Which of [`PasteVerb::gate_operations`] denied first.
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
    Changed(PasteChange),
    /// An API or validation error.
    Failed {
        /// A human-readable summary of what failed.
        detail: String,
    },
}

impl FromLeaseRefusal for PasteResult {
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

impl PasteResult {
    fn log_status(&self) -> &'static str {
        match self {
            Self::WouldChange(_) => "would-change",
            Self::RefusedNotASpreadsheet { .. } => "refused-not-a-spreadsheet",
            Self::RefusedShortcut => "refused-shortcut",
            Self::RefusedNoVisibleParents => "refused-no-visible-parents",
            Self::RefusedSheetNotFound { .. } => "refused-sheet-not-found",
            Self::RefusedInvalidRange { .. } => "refused-invalid-range",
            Self::RefusedEmptyData => "refused-empty-data",
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
pub struct PasteOutcome {
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
    pub verb: PasteVerb,
    /// What happened.
    pub result: PasteResult,
}

impl JsonlSerialize for PasteOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> anyhow::Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Runs one clipboard-style mutation, logging every attempt that isn't a
/// dry run.
pub async fn paste(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &PasteOptions,
    rules: &[FolderPermissionRule],
) -> PasteOutcome {
    let started = Instant::now();
    let outcome = paste_inner(drive, sheets, opts, rules).await;
    if !opts.dry_run {
        record_attempt(&outcome, opts, started.elapsed());
    }
    outcome
}

async fn paste_inner(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &PasteOptions,
    rules: &[FolderPermissionRule],
) -> PasteOutcome {
    let bare = |result| PasteOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: None,
        resolved_folder_id: None,
        verb: opts.verb.clone(),
        result,
    };

    if let PasteVerb::PasteData { data, .. } = &opts.verb {
        if data.is_empty() {
            return bare(PasteResult::RefusedEmptyData);
        }
    }
    if let Some(sheet) = unusable_sheet_default(&opts.verb) {
        return bare(PasteResult::RefusedInvalidRange { detail: sheet });
    }

    let destination_composed = match compose_with_default_sheet(
        opts.verb.sheet(),
        opts.verb.destination(),
        "--destination",
    ) {
        Ok(composed) => composed,
        Err(detail) => return bare(PasteResult::RefusedInvalidRange { detail }),
    };
    let source_composed = match opts.verb.source() {
        Some(source) => match compose_with_default_sheet(opts.verb.sheet(), source, "--source") {
            Ok(composed) => Some(composed),
            Err(detail) => return bare(PasteResult::RefusedInvalidRange { detail }),
        },
        None => None,
    };

    let operations = opts.verb.gate_operations();

    let (target, verdict, denied, resolved_folder_id, requires_lease) =
        match target_gate::resolve_all(drive, &opts.spreadsheet_id, operations, rules).await {
            target_gate::TargetGateUnionOutcome::MetadataFetchFailed { detail } => {
                return bare(PasteResult::Failed { detail })
            }
            target_gate::TargetGateUnionOutcome::Refused { target, refusal } => {
                let result = match refusal {
                    SheetTargetRefusal::Shortcut => PasteResult::RefusedShortcut,
                    SheetTargetRefusal::NotASpreadsheet { mime_type } => {
                        PasteResult::RefusedNotASpreadsheet { mime_type }
                    }
                    SheetTargetRefusal::NoVisibleParents => PasteResult::RefusedNoVisibleParents,
                };
                return PasteOutcome {
                    spreadsheet_id: opts.spreadsheet_id.clone(),
                    file_name: Some(target.name),
                    resolved_folder_id: None,
                    verb: opts.verb.clone(),
                    result,
                };
            }
            target_gate::TargetGateUnionOutcome::GateFetchFailed { target, detail } => {
                return PasteOutcome {
                    spreadsheet_id: opts.spreadsheet_id.clone(),
                    file_name: Some(target.name),
                    resolved_folder_id: None,
                    verb: opts.verb.clone(),
                    result: PasteResult::Failed { detail },
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

    let gated = |result| PasteOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: Some(target.name.clone()),
        resolved_folder_id: resolved_folder_id.clone(),
        verb: opts.verb.clone(),
        result,
    };

    if verdict == write_gate::Verdict::Deny {
        let (operation, decided_by) = denied.unwrap_or((operations[0], None));
        return gated(PasteResult::Blocked {
            operation,
            decided_by,
        });
    }

    let api = SheetsApi::new(sheets);
    let workbook = match api.get_spreadsheet(&opts.spreadsheet_id).await {
        Ok(workbook) => workbook,
        Err(err) => {
            return gated(PasteResult::Failed {
                detail: format!("{err:#}"),
            })
        }
    };

    let (dest_title, dest_grid) = match grid_range::resolve_grid_range(
        &workbook,
        &destination_composed,
        |detail| PasteResult::RefusedInvalidRange { detail },
        |title, available| PasteResult::RefusedSheetNotFound { title, available },
    ) {
        Ok(resolved) => resolved,
        Err(result) => return gated(result),
    };
    if !grid_range::is_bounded(&dest_grid) {
        return gated(PasteResult::RefusedInvalidRange {
            detail: format!(
                "--destination '{}' must be a bounded range, not an open-ended column/row span",
                opts.verb.destination()
            ),
        });
    }
    let needs_single_cell_destination = matches!(
        opts.verb,
        PasteVerb::CutPaste { .. } | PasteVerb::PasteData { .. }
    );
    if needs_single_cell_destination && !grid_range::is_single_cell(&dest_grid) {
        return gated(PasteResult::RefusedInvalidRange {
            detail: format!(
                "--destination '{}' must name a single cell, not a range",
                opts.verb.destination()
            ),
        });
    }

    let source_resolved = match &source_composed {
        Some(source_composed) => {
            match grid_range::resolve_grid_range(
                &workbook,
                source_composed,
                |detail| PasteResult::RefusedInvalidRange { detail },
                |title, available| PasteResult::RefusedSheetNotFound { title, available },
            ) {
                Ok((source_title, grid)) => {
                    if !grid_range::is_bounded(&grid) {
                        return gated(PasteResult::RefusedInvalidRange {
                            detail: format!(
                                "--source '{}' must be a bounded range, not an open-ended \
                                 column/row span",
                                opts.verb.source().unwrap_or_default()
                            ),
                        });
                    }
                    Some((source_title, grid))
                }
                Err(result) => return gated(result),
            }
        }
        None => None,
    };
    let source_grid = source_resolved.as_ref().map(|(_, grid)| *grid);

    let extent = match &opts.verb {
        PasteVerb::CutPaste { .. } => {
            #[allow(clippy::unwrap_used)] // `source_grid` is always `Some` for `CutPaste`.
            let source_grid = source_grid.unwrap();
            let (rows, cols) = grid_dims(&source_grid);
            anchored_extent(&dest_grid, rows, cols)
        }
        PasteVerb::CopyPaste { orientation, .. } => {
            #[allow(clippy::unwrap_used)] // `source_grid` is always `Some` for `CopyPaste`.
            let source_grid = source_grid.unwrap();
            copy_paste_extent(&source_grid, &dest_grid, *orientation)
        }
        PasteVerb::PasteData {
            data, delimiter, ..
        } => {
            let (rows, cols) = paste_data_upper_bound(data, delimiter);
            anchored_extent(&dest_grid, rows, cols)
        }
    };

    let grid_edge_caveat = sheet_exceeds_dimensions(&workbook, &extent).then(|| {
        "written extent runs past the sheet's currently allocated rows/columns; whether the API \
         errors or expands the grid here is undocumented and unverified"
            .to_string()
    });

    // The extent is reported unclamped — it is what the *request* writes,
    // and `grid_edge_caveat` above says so when that runs past the sheet.
    // The read below is a different question, and is clamped: see
    // `read_non_blank`.
    let extent_a1 = grid_range::bounded_range_to_a1(&dest_title, &extent)
        // Unreachable: every extent is anchored on a bounded destination and
        // sized from a bounded source or a non-empty `--data`, so it is never
        // empty. Falling back to the destination beats an `unwrap` on a value
        // only invariants — not types — keep `Some`.
        .unwrap_or_else(|| destination_composed.clone()); // omni-dev: coverage ignore-line reason="extents are bounded and non-empty by construction; the fallback exists so an invariant break degrades instead of panicking"

    let overwritten = if opts.verb.paste_type().writes_values() {
        match read_non_blank(&api, &opts.spreadsheet_id, &workbook, &dest_title, &extent).await {
            Ok(locations) => Some(locations),
            Err(detail) => return gated(PasteResult::Failed { detail }),
        }
    } else {
        None
    };

    let cleared = if let (PasteVerb::CutPaste { .. }, Some((source_title, source_grid))) =
        (&opts.verb, &source_resolved)
    {
        match read_non_blank(
            &api,
            &opts.spreadsheet_id,
            &workbook,
            source_title,
            source_grid,
        )
        .await
        {
            Ok(locations) => Some(locations),
            Err(detail) => return gated(PasteResult::Failed { detail }),
        }
    } else {
        None
    };

    let change = PasteChange {
        destination: destination_composed.clone(),
        source: source_composed.clone(),
        written_extent: extent_a1,
        overwritten,
        cleared,
        grid_edge_caveat,
    };

    if opts.dry_run {
        return gated(PasteResult::WouldChange(change));
    }

    let request = match &opts.verb {
        PasteVerb::CutPaste { paste_type, .. } => {
            #[allow(clippy::unwrap_used)] // `source_grid` is always `Some` for `CutPaste`.
            let source_grid = source_grid.unwrap();
            BatchUpdateRequestItem::CutPaste(CutPasteRequest {
                source: source_grid,
                destination: GridCoordinate {
                    sheet_id: dest_grid.sheet_id,
                    row_index: dest_grid.start_row_index.unwrap_or(0),
                    column_index: dest_grid.start_column_index.unwrap_or(0),
                },
                paste_type: *paste_type,
            })
        }
        PasteVerb::CopyPaste {
            paste_type,
            orientation,
            ..
        } => {
            #[allow(clippy::unwrap_used)] // `source_grid` is always `Some` for `CopyPaste`.
            let source_grid = source_grid.unwrap();
            BatchUpdateRequestItem::CopyPaste(CopyPasteRequest {
                source: source_grid,
                destination: dest_grid,
                paste_type: *paste_type,
                paste_orientation: *orientation,
            })
        }
        PasteVerb::PasteData {
            data,
            delimiter,
            paste_type,
            ..
        } => BatchUpdateRequestItem::PasteData(PasteDataRequest {
            coordinate: GridCoordinate {
                sheet_id: dest_grid.sheet_id,
                row_index: dest_grid.start_row_index.unwrap_or(0),
                column_index: dest_grid.start_column_index.unwrap_or(0),
            },
            data: data.clone(),
            delimiter: delimiter.clone(),
            r#type: *paste_type,
        }),
    };

    // The lease check sits here: after the permission gate and the
    // `--dry-run` branch, before the mutating call — see
    // `content_edit.rs::edit_inner`'s doc comment for the full reasoning,
    // shared verbatim by every leased engine.
    let files_api = FilesApi::new(drive);
    let leased = LeasedWrite {
        log_prefix: "drive sheets paste",
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
        Ok(_response) => PasteResult::Changed(change),
        Err(err) => PasteResult::Failed {
            detail: format!("{err:#}"),
        },
    };
    drop(lease_grant);
    gated(result)
}

/// Composes a `--source`/`--destination` A1 reference against an optional
/// default `--sheet`: a reference already carrying its own `'Sheet'!`
/// prefix is used as-is — source and destination may sit on different
/// sheets, and the API allows it — otherwise `sheet` supplies the prefix.
/// Mirrors `pivot.rs::compose_source`'s stance for the same shape of
/// two-range command, but `sheet` is optional here since either field may
/// self-prefix independently; `flag` names which one this call is for, so
/// the "neither given" error names the flag the caller actually typed.
fn compose_with_default_sheet(
    sheet: Option<&str>,
    range: &str,
    flag: &str,
) -> Result<String, String> {
    if a1::split_sheet_prefix(range).is_some() {
        a1::validate_range(range)
            .map(|()| range.to_string())
            .map_err(|err| err.to_string())
    } else {
        match sheet {
            Some(sheet) => a1::compose(Some(sheet), Some(range)).map_err(|err| err.to_string()),
            None => Err(format!(
                "{flag} '{range}' does not name a sheet; pass --sheet, or give {flag} its own \
                 'Sheet'! prefix"
            )),
        }
    }
}

/// The `--sheet` a caller passed that no flag can use, as a refusal
/// message — `None` when it is used, or was not passed at all.
///
/// `--sheet` is a *default* here, not the target's sheet: each of
/// `--source`/`--destination` may carry its own `'Sheet'!` prefix and a
/// paste may legitimately straddle two sheets, so a prefix on one flag
/// beside a `--sheet` for the other is correct and stays allowed. What is
/// refused is the case where every flag self-prefixes, leaving `--sheet`
/// with nothing to apply to: silently ignoring it would hide a typo in
/// whichever prefix the caller meant it to correct. The narrower cousin of
/// `a1::compose`'s blanket "already names a sheet, so --sheet would be
/// ambiguous" refusal, which this module cannot use as-is without losing
/// the cross-sheet paste (issue #1839).
fn unusable_sheet_default(verb: &PasteVerb) -> Option<String> {
    let sheet = verb.sheet()?;
    let prefixed = |range: &str| a1::split_sheet_prefix(range).is_some();
    let every_range_self_prefixes =
        prefixed(verb.destination()) && verb.source().is_none_or(prefixed);
    every_range_self_prefixes.then(|| {
        format!(
            "--sheet '{sheet}' has nothing to apply to: every range already carries its own \
             'Sheet'! prefix. Drop --sheet, or drop the prefix from the range it was meant for"
        )
    })
}

/// Every non-blank cell of `grid`, as A1 addresses, from one `values.get`.
///
/// The read range is **clipped to the sheet's current grid**: a paste's
/// written extent is a property of the request, so it can name rows or
/// columns the sheet does not have yet, and `values.get` refuses such a
/// range outright ("exceeds grid limits") — which would turn the one case
/// `grid_edge_caveat` exists to report into an opaque `Failed`. A range
/// with nothing inside the grid at all reads as no cells, because that is
/// the truth: cells that do not exist hold nothing to overwrite.
async fn read_non_blank(
    api: &SheetsApi<'_>,
    spreadsheet_id: &str,
    workbook: &crate::drive::sheets::types::Spreadsheet,
    sheet_title: &str,
    grid: &GridRange,
) -> Result<Vec<String>, String> {
    let Some(clamped) = grid_range::clamp_to_sheet(workbook, grid) else {
        return Ok(Vec::new());
    };
    let Some(range) = grid_range::bounded_range_to_a1(sheet_title, &clamped) else {
        return Ok(Vec::new());
    };
    let values = api
        .values_get(spreadsheet_id, &range, ValueRenderOption::Formatted)
        .await
        .map_err(|err| format!("{err:#}"))?;
    Ok(non_blank_locations(
        &values,
        clamped.start_row_index.unwrap_or(0),
        clamped.start_column_index.unwrap_or(0),
    ))
}

/// `(rows, columns)` of a fully-bounded [`GridRange`]. Only ever called
/// after `grid_range::is_bounded` has confirmed all four indices are
/// `Some`.
fn grid_dims(grid: &GridRange) -> (i64, i64) {
    (
        grid.end_row_index.unwrap_or(0) - grid.start_row_index.unwrap_or(0),
        grid.end_column_index.unwrap_or(0) - grid.start_column_index.unwrap_or(0),
    )
}

/// A `rows` x `cols` region anchored at `dest`'s top-left corner, on
/// `dest`'s sheet.
fn anchored_extent(dest: &GridRange, rows: i64, cols: i64) -> GridRange {
    let start_row = dest.start_row_index.unwrap_or(0);
    let start_col = dest.start_column_index.unwrap_or(0);
    GridRange {
        sheet_id: dest.sheet_id,
        start_row_index: Some(start_row),
        end_row_index: Some(start_row + rows),
        start_column_index: Some(start_col),
        end_column_index: Some(start_col + cols),
    }
}

/// One axis of [`copy_paste_extent`]: the Sheets API repeats a source that
/// evenly divides a *larger* destination to fill it exactly; otherwise the
/// source is copied at its own length, spilling past a smaller destination
/// or leaving a larger, non-multiple one only partly filled — the
/// documented behaviour ADR-0083 §6 verifies for #1839.
fn extent_len(src_len: i64, dst_len: i64) -> i64 {
    if src_len > 0 && dst_len > src_len && dst_len % src_len == 0 {
        dst_len
    } else {
        src_len
    }
}

/// The region a `copy-paste` writes: the larger of `source` and
/// `destination` per axis, anchored at `destination`'s top-left corner,
/// with `orientation` swapping `source`'s dimensions first (ADR-0083 §4,
/// §6). Both `source` and `destination` must already be fully bounded —
/// callers check `grid_range::is_bounded` before reaching here.
pub(crate) fn copy_paste_extent(
    source: &GridRange,
    destination: &GridRange,
    orientation: PasteOrientation,
) -> GridRange {
    let (mut src_rows, mut src_cols) = grid_dims(source);
    if orientation == PasteOrientation::Transpose {
        std::mem::swap(&mut src_rows, &mut src_cols);
    }
    let (dst_rows, dst_cols) = grid_dims(destination);
    let rows = extent_len(src_rows, dst_rows);
    let cols = extent_len(src_cols, dst_cols);
    anchored_extent(destination, rows, cols)
}

/// A local, upper-bound estimate of the rows/columns a `paste-data` write
/// reaches: `data` split on `\n` (tolerating a trailing `\r`) for rows,
/// then each row split on `delimiter` for columns, taking the widest row.
/// The API's own row-separator convention for `pasteData` is undocumented
/// (ADR-0083 §6, a #1839 live-verification item) — this is a preview
/// input, never sent on the wire, so an imprecise split costs nothing but
/// preview accuracy.
///
/// One *terminating* newline is not a row separator: every POSIX text file
/// and every `printf` ends with one, so counting it would add a phantom
/// row to the extent on the commonest input there is, and the preview
/// would then name cells in that row as overwritten when nothing touches
/// them. A blank line in the middle, or a second trailing one, is still a
/// row — that is data, not a terminator.
pub(crate) fn paste_data_upper_bound(data: &str, delimiter: &str) -> (i64, i64) {
    if data.is_empty() {
        return (0, 0);
    }
    let body = data.strip_suffix('\n').unwrap_or(data);
    if body.is_empty() {
        // `data` was exactly one line terminator: one empty row, not zero.
        return (1, 1);
    }
    let rows: Vec<&str> = body
        .split('\n')
        .map(|line| line.trim_end_matches('\r'))
        .collect();
    let cols = rows
        .iter()
        .map(|row| {
            if delimiter.is_empty() {
                1
            } else {
                row.split(delimiter).count()
            }
        })
        .max()
        .unwrap_or(0);
    (rows.len() as i64, cols as i64)
}

/// Whether `extent` runs past `workbook`'s currently allocated rows or
/// columns on `extent`'s sheet. `None` on either axis (the API reported no
/// grid dimensions at all) never counts as exceeding — there's nothing to
/// compare against.
fn sheet_exceeds_dimensions(
    workbook: &crate::drive::sheets::types::Spreadsheet,
    extent: &GridRange,
) -> bool {
    let Some(sheet) = grid_range::find_sheet_by_id(workbook, extent.sheet_id) else {
        return false;
    };
    let grid = sheet
        .properties
        .as_ref()
        .and_then(|p| p.grid_properties.as_ref());
    let row_count = grid.and_then(|g| g.row_count);
    let column_count = grid.and_then(|g| g.column_count);
    let rows_exceed = matches!(
        (extent.end_row_index, row_count),
        (Some(end), Some(count)) if end > count
    );
    let cols_exceed = matches!(
        (extent.end_column_index, column_count),
        (Some(end), Some(count)) if end > count
    );
    rows_exceed || cols_exceed
}

/// Every non-blank cell within a `values.get` read, as its A1 address —
/// never its value (ADR-0083 §6: "counts and A1 locations, never
/// contents"). Unlike `format.rs::discarded_from_values`,
/// the top-left cell is **not** skipped: every cell within a paste's
/// written extent is overwritten by construction, including the one at
/// its anchor, whereas a merge alone keeps its top-left value in place.
/// `row_offset`/`col_offset` are the read range's own start row/column,
/// since the API indexes a read relative to the range it was asked for,
/// not the sheet.
fn non_blank_locations(values: &ValueRange, row_offset: i64, col_offset: i64) -> Vec<String> {
    let mut locations = Vec::new();
    for (row_idx, row) in values.values.iter().enumerate() {
        for (col_idx, cell) in row.iter().enumerate() {
            let is_blank = cell.is_null() || cell.as_str().is_some_and(str::is_empty);
            if is_blank {
                continue;
            }
            locations.push(format!(
                "{}{}",
                grid_range::column_index_to_letters(col_idx as i64 + col_offset),
                row_idx as i64 + row_offset + 1
            ));
        }
    }
    locations
}

fn record_attempt(outcome: &PasteOutcome, opts: &PasteOptions, duration: Duration) {
    let error = match &outcome.result {
        PasteResult::Failed { detail } => Some(detail.clone()),
        _ => None,
    };
    let decided_by = match &outcome.result {
        PasteResult::Blocked { decided_by, .. } => decided_by.as_ref(),
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
/// so both share this one builder. `applied` picks the tense: a real run
/// reports what it *did*, not what it would do.
fn paste_change_lines(
    verb: &PasteVerb,
    change: &PasteChange,
    book: &str,
    applied: bool,
) -> Vec<String> {
    let mut lines = vec![format!(
        "{} into {} in {book}, writing {}",
        verb.label(),
        change.destination,
        change.written_extent
    )];
    if let Some(source) = &change.source {
        lines.push(format!("  source: {source}"));
    }
    // Named on every line of output, not just in the flag that set it: the
    // paste type decides both what lands in the destination and which
    // operations the gate consumed, so an outcome that doesn't say which
    // one ran can't be read back afterwards.
    lines.push(format!("  paste type: {}", verb.paste_type().as_str()));
    if let PasteVerb::CopyPaste { orientation, .. } = verb {
        lines.push(format!("  orientation: {}", orientation.as_str()));
    }
    let overwritten_verb = if applied {
        "were overwritten"
    } else {
        "would be overwritten"
    };
    match &change.overwritten {
        Some(locations) if locations.is_empty() => {
            lines.push(format!(
                "  no non-blank cells in the destination {overwritten_verb}"
            ));
        }
        Some(locations) => {
            lines.push(format!(
                "  {} non-blank cell(s) in the destination {overwritten_verb}:",
                locations.len()
            ));
            lines.extend(locations.iter().map(|loc| format!("    {loc}")));
        }
        None => lines.push(
            "  destination overwrite: not previewed (this paste type writes no cell values)"
                .to_string(),
        ),
    }
    if let Some(locations) = &change.cleared {
        if locations.is_empty() {
            lines.push(format!(
                "  the source {} no non-blank cells to clear",
                if applied { "had" } else { "has" }
            ));
        } else {
            lines.push(format!(
                "  {} non-blank cell(s) in the source {}:",
                locations.len(),
                if applied {
                    "were cleared"
                } else {
                    "will be cleared"
                }
            ));
            lines.extend(locations.iter().map(|loc| format!("    {loc}")));
        }
    }
    if let Some(caveat) = &change.grid_edge_caveat {
        lines.push(format!("  note: {caveat}"));
    }
    lines
}

/// Renders one [`PasteOutcome`] as human-readable lines, for both
/// `--dry-run` and a real run.
pub fn describe_lines(outcome: &PasteOutcome) -> Vec<String> {
    let verb = &outcome.verb;
    let book = outcome.file_name.as_deref().map_or_else(
        || format!("'{}'", outcome.spreadsheet_id),
        |n| format!("'{n}'"),
    );
    match &outcome.result {
        PasteResult::WouldChange(change) => {
            let mut lines = paste_change_lines(verb, change, &book, false);
            lines[0] = format!("Would {}", lines[0]);
            lines
        }
        PasteResult::Changed(change) => {
            let mut lines = paste_change_lines(verb, change, &book, true);
            lines[0] = format!("Applied: {}", lines[0]);
            lines
        }
        PasteResult::RefusedNotASpreadsheet { mime_type } => vec![format!(
            "Refused: {book} is not a Google Sheet (mimeType: {mime_type}); `drive sheets {}` \
             only works on spreadsheets",
            verb.label()
        )],
        PasteResult::RefusedShortcut => vec![format!(
            "Refused: {book} is a shortcut; `drive sheets {}` doesn't follow shortcuts",
            verb.label()
        )],
        PasteResult::RefusedNoVisibleParents => {
            let ops = match verb.gate_operations() {
                [DriveOperation::SheetsWrite] => "\"sheets-write\"",
                [DriveOperation::SheetsStructure] => "\"sheets-structure\"",
                _ => "\"sheets-write\", \"sheets-structure\"",
            };
            vec![format!(
                "Refused: {book} has no parent folder visible to this account, so no folder \
                 rule can apply to it. Grant it by id instead: add {{\"file_id\": \
                 \"<spreadsheet id>\", \"allow\": [{ops}]}} to write_permissions.rules."
            )]
        }
        PasteResult::RefusedSheetNotFound { title, available } => {
            let list = if available.is_empty() {
                "none".to_string()
            } else {
                available.join(", ")
            };
            vec![format!(
                "Refused: {book} has no sheet named '{title}' (available: {list})"
            )]
        }
        PasteResult::RefusedInvalidRange { detail } => vec![format!("Refused: {detail}")],
        PasteResult::RefusedEmptyData => vec![format!(
            "Refused: `drive sheets {}` was given no data to paste (--data/--data-file is empty)",
            verb.label()
        )],
        PasteResult::Blocked {
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
        PasteResult::RefusedNoLease => LeaseGateRefusal::NoLease
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        PasteResult::RefusedLeaseExpired => LeaseGateRefusal::Expired
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        PasteResult::RefusedLeaseWrongFile => LeaseGateRefusal::WrongFile
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        PasteResult::RefusedLeaseStale => LeaseGateRefusal::Stale
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        PasteResult::Failed { detail } => vec![format!("Failed: {detail}")],
    }
}

/// Reads `--data-file`: a local file path, or `-` for stdin, capped and
/// UTF-8-validated exactly like `write.rs::read_values`'s stdin/file
/// branches, but returning the raw text untouched — `pasteData` sends the
/// caller's content verbatim, unlike `write`'s CSV/JSON cell parse.
///
/// Empty content is *not* an error here: `paste_inner` refuses it as
/// [`PasteResult::RefusedEmptyData`], so an empty file and a literal
/// `--data ''` are refused the same way and in the same shape as every
/// other refusal, rather than one of them being an `anyhow` error.
pub(crate) fn read_data_text(source: &str) -> anyhow::Result<String> {
    use anyhow::Context;

    if source == "-" {
        let mut buf = Vec::new();
        std::io::stdin()
            .take(crate::drive::files_api::MAX_UPLOAD_BYTES + 1)
            .read_to_end(&mut buf)
            .context("Failed to read --data-file from stdin")?;
        anyhow::ensure!(
            buf.len() as u64 <= crate::drive::files_api::MAX_UPLOAD_BYTES,
            "--data-file from stdin is over the {} byte cap",
            crate::drive::files_api::MAX_UPLOAD_BYTES
        );
        String::from_utf8(buf).context("--data-file from stdin is not valid UTF-8")
    } else {
        let metadata = std::fs::metadata(source)
            .with_context(|| format!("Failed to stat --data-file {source}"))?;
        anyhow::ensure!(
            metadata.len() <= crate::drive::files_api::MAX_UPLOAD_BYTES,
            "--data-file {source} is {} bytes, over the {} byte cap",
            metadata.len(),
            crate::drive::files_api::MAX_UPLOAD_BYTES
        );
        std::fs::read_to_string(source)
            .with_context(|| format!("Failed to read --data-file {source}"))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // ── pure functions ──

    #[test]
    fn extent_len_repeats_a_source_that_evenly_divides_a_larger_destination() {
        assert_eq!(extent_len(2, 6), 6);
        assert_eq!(extent_len(3, 9), 9);
    }

    #[test]
    fn extent_len_spills_a_source_larger_than_its_destination() {
        assert_eq!(extent_len(5, 2), 5);
    }

    #[test]
    fn extent_len_copies_once_when_the_destination_is_not_a_clean_multiple() {
        assert_eq!(extent_len(2, 5), 2);
    }

    #[test]
    fn extent_len_treats_equal_lengths_as_a_single_copy() {
        assert_eq!(extent_len(3, 3), 3);
    }

    fn range(sheet_id: i64, r0: i64, r1: i64, c0: i64, c1: i64) -> GridRange {
        GridRange {
            sheet_id,
            start_row_index: Some(r0),
            end_row_index: Some(r1),
            start_column_index: Some(c0),
            end_column_index: Some(c1),
        }
    }

    #[test]
    fn copy_paste_extent_anchors_at_the_destinations_top_left() {
        let source = range(1, 0, 2, 0, 2); // 2x2
        let destination = range(1, 10, 11, 10, 11); // 1x1 anchor
        let extent = copy_paste_extent(&source, &destination, PasteOrientation::Normal);
        assert_eq!(extent, range(1, 10, 12, 10, 12));
    }

    #[test]
    fn copy_paste_extent_repeats_to_fill_a_multiple_destination() {
        let source = range(1, 0, 1, 0, 1); // 1x1
        let destination = range(1, 0, 3, 0, 3); // 3x3
        let extent = copy_paste_extent(&source, &destination, PasteOrientation::Normal);
        assert_eq!(extent, range(1, 0, 3, 0, 3));
    }

    #[test]
    fn copy_paste_extent_transposes_the_source_before_computing() {
        let source = range(1, 0, 1, 0, 3); // 1 row x 3 cols
        let destination = range(1, 5, 6, 5, 6); // 1x1 anchor
        let extent = copy_paste_extent(&source, &destination, PasteOrientation::Transpose);
        // Transposed source is 3 rows x 1 col.
        assert_eq!(extent, range(1, 5, 8, 5, 6));
    }

    #[test]
    fn paste_data_upper_bound_counts_rows_and_the_widest_row() {
        assert_eq!(paste_data_upper_bound("a\tb\nc\td\te", "\t"), (2, 3));
    }

    #[test]
    fn paste_data_upper_bound_tolerates_crlf_line_endings() {
        assert_eq!(paste_data_upper_bound("a,b\r\nc,d\r\n", ","), (2, 2));
    }

    #[test]
    fn paste_data_upper_bound_does_not_count_a_terminating_newline_as_a_row() {
        // What `printf '1\t2\n3\t4\n'` produces, and what every text file
        // ends with: two rows, not three. Counting the terminator would
        // name a row of cells as overwritten that nothing writes to.
        assert_eq!(paste_data_upper_bound("1\t2\n3\t4\n", "\t"), (2, 2));
        assert_eq!(paste_data_upper_bound("1\t2\n3\t4", "\t"), (2, 2));
    }

    #[test]
    fn paste_data_upper_bound_counts_a_second_trailing_newline_as_a_blank_row() {
        // Only the *terminator* is dropped: a blank line before it is data.
        assert_eq!(paste_data_upper_bound("a\nb\n\n", "\t"), (3, 1));
        assert_eq!(paste_data_upper_bound("a\n\nb\n", "\t"), (3, 1));
    }

    #[test]
    fn paste_data_upper_bound_of_a_lone_newline_is_one_empty_row() {
        assert_eq!(paste_data_upper_bound("\n", "\t"), (1, 1));
    }

    #[test]
    fn paste_data_upper_bound_of_empty_data_is_zero_by_zero() {
        assert_eq!(paste_data_upper_bound("", "\t"), (0, 0));
    }

    #[test]
    fn non_blank_locations_skips_blanks_and_offsets_addresses() {
        let values: ValueRange = serde_json::from_value(serde_json::json!({
            "values": [["a", "", "c"], [null, "d2"]]
        }))
        .unwrap();
        let locations = non_blank_locations(&values, 4, 2);
        assert_eq!(locations, vec!["C5", "E5", "D6"]);
    }

    #[test]
    fn non_blank_locations_includes_the_top_left_cell() {
        // Unlike `format.rs::discarded_from_values`, a paste preview must
        // not skip the anchor — the whole extent is overwritten.
        let values: ValueRange = serde_json::from_value(serde_json::json!({
            "values": [["kept"]]
        }))
        .unwrap();
        assert_eq!(non_blank_locations(&values, 0, 0), vec!["A1"]);
    }

    #[test]
    fn compose_with_default_sheet_prefers_the_ranges_own_prefix() {
        let composed = compose_with_default_sheet(Some("Q1"), "'Q2'!A1:B2", "--source").unwrap();
        assert_eq!(composed, "'Q2'!A1:B2");
    }

    #[test]
    fn compose_with_default_sheet_falls_back_to_sheet() {
        let composed = compose_with_default_sheet(Some("Q1"), "A1:B2", "--source").unwrap();
        assert_eq!(composed, "'Q1'!A1:B2");
    }

    #[test]
    fn compose_with_default_sheet_refuses_neither() {
        let err = compose_with_default_sheet(None, "A1:B2", "--destination").unwrap_err();
        assert!(err.contains("--destination"), "{err}");
        assert!(err.contains("--sheet"), "{err}");
    }

    #[test]
    fn unusable_sheet_default_is_none_when_sheet_is_the_only_source_of_a_prefix() {
        let verb = PasteVerb::CutPaste {
            sheet: Some("Q1".to_string()),
            source: "A1:B2".to_string(),
            destination: "D1".to_string(),
            paste_type: PasteType::Normal,
        };
        assert_eq!(unusable_sheet_default(&verb), None);
    }

    #[test]
    fn unusable_sheet_default_is_none_for_a_cross_sheet_paste_that_still_uses_sheet() {
        // The case a blanket refusal would have cost: one end prefixed,
        // the other leaning on `--sheet`.
        let verb = PasteVerb::CutPaste {
            sheet: Some("Q1".to_string()),
            source: "A1:B2".to_string(),
            destination: "'Q2'!D1".to_string(),
            paste_type: PasteType::Normal,
        };
        assert_eq!(unusable_sheet_default(&verb), None);
    }

    #[test]
    fn unusable_sheet_default_refuses_a_sheet_no_range_can_use() {
        let verb = PasteVerb::CutPaste {
            sheet: Some("Q1".to_string()),
            source: "'Q2'!A1:B2".to_string(),
            destination: "'Q3'!D1".to_string(),
            paste_type: PasteType::Normal,
        };
        let detail = unusable_sheet_default(&verb).expect("refusal");
        assert!(detail.contains("--sheet 'Q1'"), "{detail}");
        assert!(detail.contains("nothing to apply to"), "{detail}");
    }

    #[test]
    fn unusable_sheet_default_refuses_a_sheet_beside_a_prefixed_paste_data_destination() {
        // `paste-data` has only one range, so a prefix on it leaves
        // `--sheet` with nothing at all.
        let verb = PasteVerb::PasteData {
            sheet: Some("Q1".to_string()),
            destination: "'Q2'!A1".to_string(),
            data: "1".to_string(),
            delimiter: "\t".to_string(),
            paste_type: PasteType::Values,
        };
        assert!(unusable_sheet_default(&verb).is_some());
    }

    #[test]
    fn unusable_sheet_default_is_none_without_a_sheet_flag() {
        let verb = PasteVerb::PasteData {
            sheet: None,
            destination: "'Q2'!A1".to_string(),
            data: "1".to_string(),
            delimiter: "\t".to_string(),
            paste_type: PasteType::Values,
        };
        assert_eq!(unusable_sheet_default(&verb), None);
    }

    #[test]
    fn gate_operations_for_cut_paste_is_always_both() {
        let verb = PasteVerb::CutPaste {
            sheet: None,
            source: "A1".to_string(),
            destination: "B1".to_string(),
            paste_type: PasteType::Values,
        };
        assert_eq!(verb.gate_operations(), GATE_BOTH);
    }

    #[test]
    fn gate_operations_for_copy_paste_follows_the_paste_type_table() {
        let verb_of = |paste_type| PasteVerb::CopyPaste {
            sheet: None,
            source: "A1".to_string(),
            destination: "B1".to_string(),
            paste_type,
            orientation: PasteOrientation::Normal,
        };
        assert_eq!(verb_of(PasteType::Normal).gate_operations(), GATE_BOTH);
        assert_eq!(
            verb_of(PasteType::Values).gate_operations(),
            GATE_WRITE_ONLY
        );
        assert_eq!(
            verb_of(PasteType::Formula).gate_operations(),
            GATE_WRITE_ONLY
        );
        assert_eq!(
            verb_of(PasteType::Format).gate_operations(),
            GATE_STRUCTURE_ONLY
        );
    }

    #[test]
    fn sheet_exceeds_dimensions_flags_an_extent_past_the_grid() {
        let workbook: crate::drive::sheets::types::Spreadsheet =
            serde_json::from_value(serde_json::json!({
                "sheets": [
                    {"properties": {"sheetId": 1, "title": "Q1",
                                     "gridProperties": {"rowCount": 10, "columnCount": 5}}},
                ],
            }))
            .unwrap();
        assert!(sheet_exceeds_dimensions(&workbook, &range(1, 0, 11, 0, 3)));
        assert!(sheet_exceeds_dimensions(&workbook, &range(1, 0, 3, 0, 6)));
        assert!(!sheet_exceeds_dimensions(&workbook, &range(1, 0, 10, 0, 5)));
    }

    #[test]
    fn sheet_exceeds_dimensions_is_false_when_the_extents_sheet_is_unknown() {
        let workbook: crate::drive::sheets::types::Spreadsheet =
            serde_json::from_value(serde_json::json!({
                "sheets": [
                    {"properties": {"sheetId": 1, "title": "Q1",
                                     "gridProperties": {"rowCount": 10, "columnCount": 5}}},
                ],
            }))
            .unwrap();
        // `extent.sheet_id` names a sheet this workbook doesn't have —
        // nothing to compare against, so this is never "exceeds".
        assert!(!sheet_exceeds_dimensions(
            &workbook,
            &range(99, 0, 20, 0, 20)
        ));
    }

    #[test]
    fn paste_data_upper_bound_with_an_empty_delimiter_counts_one_column() {
        assert_eq!(paste_data_upper_bound("ab\ncd", ""), (2, 1));
    }

    #[test]
    fn log_operation_is_defined_for_every_verb() {
        for verb in every_paste_verb() {
            assert!(!verb.log_operation().is_empty());
        }
    }

    #[test]
    fn log_status_is_defined_for_every_result_variant() {
        for result in every_paste_result() {
            assert!(!result.log_status().is_empty());
        }
    }

    #[test]
    fn from_lease_refusal_maps_every_refusal_kind() {
        assert_eq!(PasteResult::from_no_lease(), PasteResult::RefusedNoLease);
        assert_eq!(
            PasteResult::from_lease_expired(),
            PasteResult::RefusedLeaseExpired
        );
        assert_eq!(
            PasteResult::from_lease_wrong_file(),
            PasteResult::RefusedLeaseWrongFile
        );
        assert_eq!(
            PasteResult::from_lease_stale(),
            PasteResult::RefusedLeaseStale
        );
        assert_eq!(
            PasteResult::from_lease_failed("boom".to_string()),
            PasteResult::Failed {
                detail: "boom".to_string()
            }
        );
    }

    #[test]
    fn write_jsonl_serializes_the_outcome_as_one_json_line() {
        let outcome = PasteOutcome {
            spreadsheet_id: "sheet-1".to_string(),
            file_name: Some("Budget".to_string()),
            resolved_folder_id: None,
            verb: every_paste_verb().remove(0),
            result: PasteResult::RefusedShortcut,
        };
        let mut buf = Vec::new();
        outcome.write_jsonl(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("refused-shortcut"), "{text}");
    }

    #[test]
    fn read_data_text_reads_a_local_file_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clip.tsv");
        std::fs::write(&path, "a\tb\nc\td").unwrap();
        let text = read_data_text(path.to_str().unwrap()).unwrap();
        assert_eq!(text, "a\tb\nc\td");
    }

    #[test]
    fn read_data_text_reports_a_missing_file_clearly() {
        let err = read_data_text("/definitely/not/here.tsv").unwrap_err();
        assert!(err.to_string().contains("Failed to stat"), "{err}");
    }

    #[test]
    fn read_data_text_refuses_a_file_over_the_upload_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.tsv");
        let oversize = vec![b'a'; (crate::drive::files_api::MAX_UPLOAD_BYTES + 1) as usize];
        std::fs::write(&path, oversize).unwrap();
        let err = read_data_text(path.to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("over the"), "{err}");
    }

    // ── async flow, against a wiremock Drive+Sheets backend ──

    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::sheets::client::SHEETS_API_URL;
    use crate::test_support::env::MapEnv;
    use crate::utils::secret::Secret;

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

    async fn mount_workbook(server: &wiremock::MockServer) {
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
            .mount(server)
            .await;
    }

    /// Every sheets operation allowed on every folder — for the tests that
    /// must be refused *before* the gate is ever consulted, where the rule
    /// set is deliberately not the reason.
    fn rules_allowing_everything() -> Vec<FolderPermissionRule> {
        vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )]
    }

    /// `require_lease: false` by default — most tests here exercise the
    /// permission gate or the dry-run/real-run split, not the lease gate,
    /// which has its own dedicated test.
    fn allow_rule(folder_id: &str, ops: &[DriveOperation]) -> FolderPermissionRule {
        FolderPermissionRule::folder(folder_id)
            .allowing(ops.iter().copied())
            .requiring_lease(false)
    }

    #[tokio::test]
    async fn copy_paste_default_normal_is_blocked_naming_sheets_structure_under_write_only() {
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
        let rules = vec![allow_rule("folder-1", &[DriveOperation::SheetsWrite])];

        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::CopyPaste {
                sheet: Some("Q1".to_string()),
                source: "A1:B2".to_string(),
                destination: "D1:E2".to_string(),
                paste_type: PasteType::Normal,
                orientation: PasteOrientation::Normal,
            },
            dry_run: true,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            PasteResult::Blocked { operation, .. } => {
                assert_eq!(operation, DriveOperation::SheetsStructure);
            }
            other => panic!("expected Blocked naming sheets-structure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn copy_paste_values_only_proceeds_under_a_write_only_grant() {
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
        mount_workbook(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/'Q1'!D1:E2",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"range": "'Q1'!D1:E2", "values": []})),
            )
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1", &[DriveOperation::SheetsWrite])];

        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::CopyPaste {
                sheet: Some("Q1".to_string()),
                source: "A1:B2".to_string(),
                destination: "D1:E2".to_string(),
                paste_type: PasteType::Values,
                orientation: PasteOrientation::Normal,
            },
            dry_run: true,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(outcome.result, PasteResult::WouldChange(_)),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn format_only_copy_paste_dry_run_reads_no_values() {
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
        mount_workbook(&server).await;
        // No `values.get` mock at all — a `format`-only dry run must never
        // call it, or this test's request would 404 and the outcome would
        // be `Failed` instead of `WouldChange`.
        let rules = vec![allow_rule("folder-1", &[DriveOperation::SheetsStructure])];

        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::CopyPaste {
                sheet: Some("Q1".to_string()),
                source: "A1:B2".to_string(),
                destination: "D1:E2".to_string(),
                paste_type: PasteType::Format,
                orientation: PasteOrientation::Normal,
            },
            dry_run: true,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            PasteResult::WouldChange(change) => assert_eq!(change.overwritten, None),
            other => panic!("expected WouldChange with no overwritten preview, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dry_run_never_calls_batch_update() {
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
        mount_workbook(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/'Q1'!D1:E2",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"range": "'Q1'!D1:E2", "values": []})),
            )
            .mount(&server)
            .await;
        // No `batchUpdate` mock — a dry run calling it would 404.
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];

        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::CopyPaste {
                sheet: Some("Q1".to_string()),
                source: "A1:B2".to_string(),
                destination: "D1:E2".to_string(),
                paste_type: PasteType::Normal,
                orientation: PasteOrientation::Normal,
            },
            dry_run: true,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, PasteResult::WouldChange(_)));
    }

    #[tokio::test]
    async fn cut_paste_always_needs_both_operations_even_for_values_only() {
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
        let rules = vec![allow_rule("folder-1", &[DriveOperation::SheetsWrite])];

        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::CutPaste {
                sheet: Some("Q1".to_string()),
                source: "A1:B2".to_string(),
                destination: "D1".to_string(),
                paste_type: PasteType::Values,
            },
            dry_run: true,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            PasteResult::Blocked { operation, .. } => {
                assert_eq!(operation, DriveOperation::SheetsStructure);
            }
            other => panic!("expected Blocked naming sheets-structure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cut_paste_refuses_a_multi_cell_destination() {
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
        mount_workbook(&server).await;
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];

        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::CutPaste {
                sheet: Some("Q1".to_string()),
                source: "A1:B2".to_string(),
                destination: "D1:E2".to_string(),
                paste_type: PasteType::Normal,
            },
            dry_run: true,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            PasteResult::RefusedInvalidRange { detail } => {
                assert!(detail.contains("single cell"), "{detail}");
            }
            other => panic!("expected RefusedInvalidRange, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cut_paste_reports_destination_overwrite_and_source_clear_separately() {
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
        mount_workbook(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/'Q1'!D1:E2",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "range": "'Q1'!D1:E2", "values": [["x", ""], ["", "y"]],
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/'Q1'!A1:B2",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "range": "'Q1'!A1:B2", "values": [["1", "2"], ["3", "4"]],
                })),
            )
            .mount(&server)
            .await;
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];

        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::CutPaste {
                sheet: Some("Q1".to_string()),
                source: "A1:B2".to_string(),
                destination: "D1".to_string(),
                paste_type: PasteType::Normal,
            },
            dry_run: true,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            PasteResult::WouldChange(change) => {
                assert_eq!(
                    change.overwritten,
                    Some(vec!["D1".to_string(), "E2".to_string()])
                );
                assert_eq!(
                    change.cleared,
                    Some(vec![
                        "A1".to_string(),
                        "B1".to_string(),
                        "A2".to_string(),
                        "B2".to_string()
                    ])
                );
            }
            other => panic!("expected WouldChange, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn paste_data_defaults_and_real_run_succeeds() {
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
        mount_workbook(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/'Q1'!A1:B1",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"range": "'Q1'!A1:B1", "values": []})),
            )
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

        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::PasteData {
                sheet: Some("Q1".to_string()),
                destination: "A1".to_string(),
                data: "1\t2".to_string(),
                delimiter: "\t".to_string(),
                paste_type: PasteType::Values,
            },
            dry_run: false,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(outcome.result, PasteResult::Changed(_)),
            "{:?}",
            outcome.result
        );
    }

    // ── early refusals, before any gate/workbook call ──

    #[tokio::test]
    async fn a_destination_with_no_sheet_and_no_prefix_is_refused_before_any_network_call() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        // No file/folder/workbook mock is mounted at all — reaching any of
        // them would 404 and mask this as `Failed` rather than
        // `RefusedInvalidRange`.
        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::CopyPaste {
                sheet: None,
                source: "'Q1'!A1:B2".to_string(),
                destination: "D1:E2".to_string(),
                paste_type: PasteType::Values,
                orientation: PasteOrientation::Normal,
            },
            dry_run: true,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &[]).await;
        match outcome.result {
            PasteResult::RefusedInvalidRange { detail } => {
                assert!(detail.contains("--destination"), "{detail}");
            }
            other => panic!("expected RefusedInvalidRange, got {other:?}"),
        }
        assert_eq!(outcome.file_name, None);
    }

    #[tokio::test]
    async fn a_source_with_no_sheet_and_no_prefix_is_refused_before_any_network_call() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::CopyPaste {
                sheet: None,
                source: "A1:B2".to_string(),
                destination: "'Q1'!D1:E2".to_string(),
                paste_type: PasteType::Values,
                orientation: PasteOrientation::Normal,
            },
            dry_run: true,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &[]).await;
        match outcome.result {
            PasteResult::RefusedInvalidRange { detail } => {
                assert!(detail.contains("--source"), "{detail}");
            }
            other => panic!("expected RefusedInvalidRange, got {other:?}"),
        }
    }

    // ── target-gate refusals/failures ──

    #[tokio::test]
    async fn a_metadata_fetch_failure_surfaces_as_failed() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/sheet-1"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;
        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::CopyPaste {
                sheet: Some("Q1".to_string()),
                source: "A1:B2".to_string(),
                destination: "D1:E2".to_string(),
                paste_type: PasteType::Normal,
                orientation: PasteOrientation::Normal,
            },
            // `false` so this also drives `record_attempt`'s `Failed` arm.
            dry_run: false,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &[]).await;
        assert!(matches!(outcome.result, PasteResult::Failed { .. }));
    }

    #[tokio::test]
    async fn a_shortcut_target_is_refused() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            "application/vnd.google-apps.shortcut",
            &["folder-1"],
        )
        .mount(&server)
        .await;
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::CopyPaste {
                sheet: Some("Q1".to_string()),
                source: "A1:B2".to_string(),
                destination: "D1:E2".to_string(),
                paste_type: PasteType::Normal,
                orientation: PasteOrientation::Normal,
            },
            dry_run: true,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, PasteResult::RefusedShortcut));
    }

    #[tokio::test]
    async fn a_non_spreadsheet_target_is_refused() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            "application/vnd.google-apps.document",
            &["folder-1"],
        )
        .mount(&server)
        .await;
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::CopyPaste {
                sheet: Some("Q1".to_string()),
                source: "A1:B2".to_string(),
                destination: "D1:E2".to_string(),
                paste_type: PasteType::Normal,
                orientation: PasteOrientation::Normal,
            },
            dry_run: true,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            PasteResult::RefusedNotASpreadsheet { .. }
        ));
    }

    #[tokio::test]
    async fn a_target_with_no_visible_parents_is_refused() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", crate::drive::types::GOOGLE_SHEET_MIME_TYPE, &[])
            .mount(&server)
            .await;
        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::CopyPaste {
                sheet: Some("Q1".to_string()),
                source: "A1:B2".to_string(),
                destination: "D1:E2".to_string(),
                paste_type: PasteType::Normal,
                orientation: PasteOrientation::Normal,
            },
            dry_run: true,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &[]).await;
        assert!(matches!(
            outcome.result,
            PasteResult::RefusedNoVisibleParents
        ));
    }

    #[tokio::test]
    async fn a_gate_ancestor_fetch_failure_surfaces_as_failed() {
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
        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::CopyPaste {
                sheet: Some("Q1".to_string()),
                source: "A1:B2".to_string(),
                destination: "D1:E2".to_string(),
                paste_type: PasteType::Normal,
                orientation: PasteOrientation::Normal,
            },
            dry_run: true,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, PasteResult::Failed { .. }));
    }

    #[tokio::test]
    async fn a_workbook_fetch_failure_surfaces_as_failed() {
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
        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::CopyPaste {
                sheet: Some("Q1".to_string()),
                source: "A1:B2".to_string(),
                destination: "D1:E2".to_string(),
                paste_type: PasteType::Normal,
                orientation: PasteOrientation::Normal,
            },
            dry_run: true,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, PasteResult::Failed { .. }));
    }

    // ── destination/source range resolution ──

    #[tokio::test]
    async fn a_destination_naming_an_unknown_sheet_is_refused() {
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
        mount_workbook(&server).await;
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::CopyPaste {
                sheet: None,
                source: "'Q1'!A1:B2".to_string(),
                destination: "'Ghost'!D1:E2".to_string(),
                paste_type: PasteType::Normal,
                orientation: PasteOrientation::Normal,
            },
            dry_run: true,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            PasteResult::RefusedSheetNotFound { title, .. } => assert_eq!(title, "Ghost"),
            other => panic!("expected RefusedSheetNotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_destination_with_an_unparseable_range_is_refused() {
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
        mount_workbook(&server).await;
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::CopyPaste {
                sheet: None,
                source: "'Q1'!A1:B2".to_string(),
                destination: "'Q1'!not a range".to_string(),
                paste_type: PasteType::Normal,
                orientation: PasteOrientation::Normal,
            },
            dry_run: true,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(outcome.result, PasteResult::RefusedInvalidRange { .. }),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn an_unbounded_destination_is_refused() {
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
        mount_workbook(&server).await;
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::CopyPaste {
                sheet: None,
                source: "'Q1'!A1:B2".to_string(),
                destination: "'Q1'!A:A".to_string(),
                paste_type: PasteType::Normal,
                orientation: PasteOrientation::Normal,
            },
            dry_run: true,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            PasteResult::RefusedInvalidRange { detail } => {
                assert!(detail.contains("bounded"), "{detail}");
            }
            other => panic!("expected RefusedInvalidRange, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_source_naming_an_unknown_sheet_is_refused() {
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
        mount_workbook(&server).await;
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::CopyPaste {
                sheet: None,
                source: "'Ghost'!A1:B2".to_string(),
                destination: "'Q1'!D1:E2".to_string(),
                paste_type: PasteType::Normal,
                orientation: PasteOrientation::Normal,
            },
            dry_run: true,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            PasteResult::RefusedSheetNotFound { title, .. } => assert_eq!(title, "Ghost"),
            other => panic!("expected RefusedSheetNotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_source_with_an_unparseable_range_is_refused() {
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
        mount_workbook(&server).await;
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::CopyPaste {
                sheet: None,
                source: "'Q1'!not a range".to_string(),
                destination: "'Q1'!D1:E2".to_string(),
                paste_type: PasteType::Normal,
                orientation: PasteOrientation::Normal,
            },
            dry_run: true,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(outcome.result, PasteResult::RefusedInvalidRange { .. }),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn an_unbounded_source_is_refused() {
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
        mount_workbook(&server).await;
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::CopyPaste {
                sheet: None,
                source: "'Q1'!A:A".to_string(),
                destination: "'Q1'!D1:E2".to_string(),
                paste_type: PasteType::Normal,
                orientation: PasteOrientation::Normal,
            },
            dry_run: true,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            PasteResult::RefusedInvalidRange { detail } => {
                assert!(
                    detail.contains("bounded") && detail.contains("--source"),
                    "{detail}"
                );
            }
            other => panic!("expected RefusedInvalidRange, got {other:?}"),
        }
    }

    // ── grid-edge caveat, values.get failures ──

    #[tokio::test]
    async fn a_destination_past_the_sheets_allocated_grid_gets_a_caveat() {
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
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "spreadsheetId": "sheet-1",
                    "properties": {"title": "Budget"},
                    "sheets": [
                        {"properties": {"sheetId": 0, "title": "Q1", "index": 0,
                                         "gridProperties": {"rowCount": 5, "columnCount": 5}}},
                    ],
                })),
            )
            .mount(&server)
            .await;
        // Format-only: no `values.get` mock is needed to reach `WouldChange`.
        let rules = vec![allow_rule("folder-1", &[DriveOperation::SheetsStructure])];
        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::CopyPaste {
                sheet: Some("Q1".to_string()),
                source: "A1:F1".to_string(),
                destination: "A1:B1".to_string(),
                paste_type: PasteType::Format,
                orientation: PasteOrientation::Normal,
            },
            dry_run: true,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            PasteResult::WouldChange(change) => {
                assert!(change.grid_edge_caveat.is_some(), "{change:?}");
            }
            other => panic!("expected WouldChange, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_values_get_failure_for_the_destination_preview_surfaces_as_failed() {
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
        mount_workbook(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/'Q1'!D1:E2",
            ))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1", &[DriveOperation::SheetsWrite])];
        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::CopyPaste {
                sheet: Some("Q1".to_string()),
                source: "A1:B2".to_string(),
                destination: "D1:E2".to_string(),
                paste_type: PasteType::Values,
                orientation: PasteOrientation::Normal,
            },
            dry_run: true,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, PasteResult::Failed { .. }));
    }

    #[tokio::test]
    async fn a_values_get_failure_for_the_cut_paste_source_clear_preview_surfaces_as_failed() {
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
        mount_workbook(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/'Q1'!D1:E2",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"range": "'Q1'!D1:E2", "values": []})),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/'Q1'!A1:B2",
            ))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::CutPaste {
                sheet: Some("Q1".to_string()),
                source: "A1:B2".to_string(),
                destination: "D1".to_string(),
                paste_type: PasteType::Normal,
            },
            dry_run: true,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, PasteResult::Failed { .. }));
    }

    #[tokio::test]
    async fn paste_data_with_empty_data_is_refused_before_any_network_call() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        // No mocks at all: an empty paste must be refused before the
        // metadata fetch, let alone before an extent is computed from it
        // (a zero-by-zero extent names no range a read could describe).
        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::PasteData {
                sheet: Some("Q1".to_string()),
                destination: "A1".to_string(),
                data: String::new(),
                delimiter: "\t".to_string(),
                paste_type: PasteType::Values,
            },
            dry_run: true,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &rules_allowing_everything()).await;
        assert_eq!(outcome.result, PasteResult::RefusedEmptyData);
        let lines = describe_lines(&outcome).join("\n");
        assert!(lines.contains("no data to paste"), "{lines}");
    }

    #[tokio::test]
    async fn a_sheet_flag_no_range_can_use_is_refused_before_any_network_call() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::CopyPaste {
                sheet: Some("Q1".to_string()),
                source: "'Q2'!A1:B2".to_string(),
                destination: "'Q3'!D1:E2".to_string(),
                paste_type: PasteType::Values,
                orientation: PasteOrientation::Normal,
            },
            dry_run: true,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &rules_allowing_everything()).await;
        match outcome.result {
            PasteResult::RefusedInvalidRange { detail } => {
                assert!(detail.contains("--sheet 'Q1'"), "{detail}");
            }
            other => panic!("expected RefusedInvalidRange, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_preview_read_is_clipped_to_the_grid_so_the_caveat_survives() {
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
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "spreadsheetId": "sheet-1",
                    "properties": {"title": "Budget"},
                    "sheets": [
                        {"properties": {"sheetId": 0, "title": "Q1", "index": 0,
                                         "gridProperties": {"rowCount": 5, "columnCount": 5}}},
                    ],
                })),
            )
            .mount(&server)
            .await;
        // The only `values.get` mock is the *clipped* range: a read of the
        // full `'Q1'!D1:I1` extent would 404 here, exactly as the real API
        // refuses a range past the grid's edge ("exceeds grid limits").
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/'Q1'!D1:E1",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"range": "'Q1'!D1:E1", "values": [["x"]]})),
            )
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1", &[DriveOperation::SheetsWrite])];
        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::CopyPaste {
                // A 6-wide source onto a 5-column sheet, anchored at D1:
                // the extent runs to column I, three columns past the edge.
                sheet: Some("Q1".to_string()),
                source: "A1:F1".to_string(),
                destination: "D1".to_string(),
                paste_type: PasteType::Values,
                orientation: PasteOrientation::Normal,
            },
            dry_run: true,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            PasteResult::WouldChange(change) => {
                // The extent is reported unclipped — it is what the request
                // writes — and the caveat says it runs past the grid.
                assert_eq!(change.written_extent, "'Q1'!D1:I1");
                assert!(change.grid_edge_caveat.is_some(), "{change:?}");
                // …while the read that backs it saw only the cells that exist.
                assert_eq!(change.overwritten, Some(vec!["D1".to_string()]));
            }
            other => panic!("expected WouldChange, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_destination_entirely_past_the_grid_previews_no_overwrite_without_reading() {
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
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "spreadsheetId": "sheet-1",
                    "properties": {"title": "Budget"},
                    "sheets": [
                        {"properties": {"sheetId": 0, "title": "Q1", "index": 0,
                                         "gridProperties": {"rowCount": 5, "columnCount": 5}}},
                    ],
                })),
            )
            .mount(&server)
            .await;
        // No `values.get` mock: nothing of the extent lies inside the grid,
        // so there is nothing to read and no cell that could be overwritten.
        let rules = vec![allow_rule("folder-1", &[DriveOperation::SheetsWrite])];
        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::CopyPaste {
                sheet: Some("Q1".to_string()),
                source: "A1:A1".to_string(),
                destination: "A20".to_string(),
                paste_type: PasteType::Values,
                orientation: PasteOrientation::Normal,
            },
            dry_run: true,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            PasteResult::WouldChange(change) => {
                assert_eq!(change.overwritten, Some(Vec::new()));
                assert!(change.grid_edge_caveat.is_some(), "{change:?}");
            }
            other => panic!("expected WouldChange, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_cell_value_ever_reaches_the_rendered_output() {
        // ADR-0083 §6: a paste preview reports counts and A1 locations, and
        // `merge-cells` stays the one verb in this crate that prints cell
        // contents. Asserted on the rendered lines, not on the struct, since
        // the rendering is where a value would leak.
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
        mount_workbook(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/'Q1'!D1:E2",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "range": "'Q1'!D1:E2",
                    "values": [["destination-secret", ""], ["", "another-secret"]],
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/'Q1'!A1:B2",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "range": "'Q1'!A1:B2",
                    "values": [["source-secret", "x"], ["y", "z"]],
                })),
            )
            .mount(&server)
            .await;
        let rules = rules_allowing_everything();
        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::CutPaste {
                sheet: Some("Q1".to_string()),
                source: "A1:B2".to_string(),
                destination: "D1".to_string(),
                paste_type: PasteType::Normal,
            },
            dry_run: true,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &rules).await;
        let rendered = describe_lines(&outcome).join("\n");
        for secret in ["destination-secret", "another-secret", "source-secret"] {
            assert!(
                !rendered.contains(secret),
                "{secret} leaked into: {rendered}"
            );
        }
        // The counts and locations are there instead.
        assert!(
            rendered.contains("2 non-blank cell(s) in the destination"),
            "{rendered}"
        );
        assert!(rendered.contains("    D1"), "{rendered}");
        assert!(
            rendered.contains("4 non-blank cell(s) in the source"),
            "{rendered}"
        );
    }

    // ── real (non-dry-run) mutations ──

    #[tokio::test]
    async fn cut_paste_real_run_succeeds_and_reports_changed() {
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
        mount_workbook(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/'Q1'!D1:E2",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"range": "'Q1'!D1:E2", "values": []})),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/'Q1'!A1:B2",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"range": "'Q1'!A1:B2", "values": []})),
            )
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
        let rules = vec![allow_rule(
            "folder-1",
            &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure],
        )];
        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::CutPaste {
                sheet: Some("Q1".to_string()),
                source: "A1:B2".to_string(),
                destination: "D1".to_string(),
                paste_type: PasteType::Normal,
            },
            dry_run: false,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(outcome.result, PasteResult::Changed(_)),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn copy_paste_real_run_succeeds_and_reports_changed() {
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
        mount_workbook(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/'Q1'!D1:E2",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"range": "'Q1'!D1:E2", "values": []})),
            )
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
        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::CopyPaste {
                sheet: Some("Q1".to_string()),
                source: "A1:B2".to_string(),
                destination: "D1:E2".to_string(),
                paste_type: PasteType::Values,
                orientation: PasteOrientation::Normal,
            },
            dry_run: false,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(outcome.result, PasteResult::Changed(_)),
            "{:?}",
            outcome.result
        );
    }

    /// The single `batchUpdate` body the server received, as JSON.
    async fn sent_batch_update(server: &wiremock::MockServer) -> serde_json::Value {
        let requests = server.received_requests().await.expect("recorded requests");
        let body = requests
            .iter()
            .find(|r| r.url.path().ends_with(":batchUpdate"))
            .map(|r| r.body.clone())
            .expect("a batchUpdate request");
        serde_json::from_slice(&body).expect("a JSON body")
    }

    #[tokio::test]
    async fn cut_paste_sends_the_source_range_and_a_destination_coordinate() {
        // The engine→wire mapping: `types.rs` pins how a `CutPasteRequest`
        // serialises, this pins that the right one is built — that the
        // destination becomes a `GridCoordinate` at the range's top-left
        // and the two ends are not transposed.
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
        mount_workbook(&server).await;
        for range in ["'Q1'!D1:E2", "'Q1'!A1:B2"] {
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path(format!(
                    "/v4/spreadsheets/sheet-1/values/{range}"
                )))
                .respond_with(
                    wiremock::ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({"range": range, "values": []})),
                )
                .mount(&server)
                .await;
        }
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
        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::CutPaste {
                sheet: Some("Q1".to_string()),
                source: "A1:B2".to_string(),
                destination: "D1".to_string(),
                paste_type: PasteType::Values,
            },
            dry_run: false,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &rules_allowing_everything()).await;
        assert!(
            matches!(outcome.result, PasteResult::Changed(_)),
            "{:?}",
            outcome.result
        );
        assert_eq!(
            sent_batch_update(&server).await,
            serde_json::json!({
                "requests": [{
                    "cutPaste": {
                        "source": {
                            "sheetId": 0,
                            "startRowIndex": 0, "endRowIndex": 2,
                            "startColumnIndex": 0, "endColumnIndex": 2,
                        },
                        "destination": {"sheetId": 0, "rowIndex": 0, "columnIndex": 3},
                        "pasteType": "PASTE_VALUES",
                    },
                }],
            })
        );
    }

    #[tokio::test]
    async fn copy_paste_sends_both_ranges_with_the_requested_orientation() {
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
        mount_workbook(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/'Q1'!D1:E2",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"range": "'Q1'!D1:E2", "values": []})),
            )
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
        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::CopyPaste {
                sheet: Some("Q1".to_string()),
                source: "A1:B2".to_string(),
                destination: "D1:E2".to_string(),
                paste_type: PasteType::Values,
                orientation: PasteOrientation::Transpose,
            },
            dry_run: false,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &rules_allowing_everything()).await;
        assert!(
            matches!(outcome.result, PasteResult::Changed(_)),
            "{:?}",
            outcome.result
        );
        assert_eq!(
            sent_batch_update(&server).await,
            serde_json::json!({
                "requests": [{
                    "copyPaste": {
                        "source": {
                            "sheetId": 0,
                            "startRowIndex": 0, "endRowIndex": 2,
                            "startColumnIndex": 0, "endColumnIndex": 2,
                        },
                        "destination": {
                            "sheetId": 0,
                            "startRowIndex": 0, "endRowIndex": 2,
                            "startColumnIndex": 3, "endColumnIndex": 5,
                        },
                        "pasteType": "PASTE_VALUES",
                        "pasteOrientation": "TRANSPOSE",
                    },
                }],
            })
        );
    }

    #[tokio::test]
    async fn paste_data_sends_the_delimiter_form_verbatim() {
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
        mount_workbook(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/'Q1'!A1:B2",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"range": "'Q1'!A1:B2", "values": []})),
            )
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
        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::PasteData {
                sheet: Some("Q1".to_string()),
                destination: "A1".to_string(),
                // The trailing terminator is sent as the caller gave it —
                // only the *preview's* row count ignores it.
                data: "1\t2\n3\t4\n".to_string(),
                delimiter: "\t".to_string(),
                paste_type: PasteType::Values,
            },
            dry_run: false,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &rules_allowing_everything()).await;
        assert!(
            matches!(outcome.result, PasteResult::Changed(_)),
            "{:?}",
            outcome.result
        );
        assert_eq!(
            sent_batch_update(&server).await,
            serde_json::json!({
                "requests": [{
                    "pasteData": {
                        "coordinate": {"sheetId": 0, "rowIndex": 0, "columnIndex": 0},
                        "data": "1\t2\n3\t4\n",
                        "delimiter": "\t",
                        "type": "PASTE_VALUES",
                    },
                }],
            })
        );
    }

    #[tokio::test]
    async fn a_batch_update_failure_is_reported_as_failed_not_changed() {
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
        mount_workbook(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/'Q1'!D1:E2",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"range": "'Q1'!D1:E2", "values": []})),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1", &[DriveOperation::SheetsWrite])];
        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::CopyPaste {
                sheet: Some("Q1".to_string()),
                source: "A1:B2".to_string(),
                destination: "D1:E2".to_string(),
                paste_type: PasteType::Values,
                orientation: PasteOrientation::Normal,
            },
            dry_run: false,
            lease_token: None,
            ledger_path: PathBuf::new(),
        };
        let outcome = paste(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(outcome.result, PasteResult::Failed { .. }),
            "{:?}",
            outcome.result
        );
    }

    // ── lease refusals ──

    #[tokio::test]
    async fn paste_data_refuses_an_unknown_lease_token() {
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
        mount_workbook(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/'Q1'!A1:B1",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"range": "'Q1'!A1:B1", "values": []})),
            )
            .mount(&server)
            .await;
        let rules = vec![FolderPermissionRule::folder("folder-1")
            .allowing([DriveOperation::SheetsWrite])
            .requiring_lease(true)];
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::PasteData {
                sheet: Some("Q1".to_string()),
                destination: "A1".to_string(),
                data: "1\t2".to_string(),
                delimiter: "\t".to_string(),
                paste_type: PasteType::Values,
            },
            dry_run: false,
            lease_token: Some("bogus-token".to_string()),
            ledger_path,
        };
        let outcome = paste(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, PasteResult::RefusedLeaseExpired));
    }

    #[tokio::test]
    async fn paste_data_refuses_a_lease_bound_to_a_different_file() {
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
        mount_workbook(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/'Q1'!A1:B1",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"range": "'Q1'!A1:B1", "values": []})),
            )
            .mount(&server)
            .await;
        let rules = vec![FolderPermissionRule::folder("folder-1")
            .allowing([DriveOperation::SheetsWrite])
            .requiring_lease(true)];
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        let token = crate::drive::test_support::seed_lease(&ledger_path, "some-other-sheet", "1");
        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::PasteData {
                sheet: Some("Q1".to_string()),
                destination: "A1".to_string(),
                data: "1\t2".to_string(),
                delimiter: "\t".to_string(),
                paste_type: PasteType::Values,
            },
            dry_run: false,
            lease_token: Some(token),
            ledger_path,
        };
        let outcome = paste(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, PasteResult::RefusedLeaseWrongFile));
    }

    #[tokio::test]
    async fn paste_data_refuses_a_stale_lease_when_the_file_has_moved() {
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
        mount_workbook(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/'Q1'!A1:B1",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"range": "'Q1'!A1:B1", "values": []})),
            )
            .mount(&server)
            .await;
        let rules = vec![FolderPermissionRule::folder("folder-1")
            .allowing([DriveOperation::SheetsWrite])
            .requiring_lease(true)];
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        let token = crate::drive::test_support::seed_lease(&ledger_path, "sheet-1", "0");
        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::PasteData {
                sheet: Some("Q1".to_string()),
                destination: "A1".to_string(),
                data: "1\t2".to_string(),
                delimiter: "\t".to_string(),
                paste_type: PasteType::Values,
            },
            dry_run: false,
            lease_token: Some(token),
            ledger_path,
        };
        let outcome = paste(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, PasteResult::RefusedLeaseStale));
    }

    #[tokio::test]
    async fn a_required_lease_without_one_refuses_before_batch_update() {
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
        mount_workbook(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/'Q1'!A1:B1",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"range": "'Q1'!A1:B1", "values": []})),
            )
            .mount(&server)
            .await;
        // No `batchUpdate` mock — reaching it would 404 and mask the real
        // failure as `Failed` rather than `RefusedNoLease`.
        let rules = vec![FolderPermissionRule::folder("folder-1")
            .allowing([DriveOperation::SheetsWrite])
            .requiring_lease(true)];

        let opts = PasteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: PasteVerb::PasteData {
                sheet: Some("Q1".to_string()),
                destination: "A1".to_string(),
                data: "1\t2".to_string(),
                delimiter: "\t".to_string(),
                paste_type: PasteType::Values,
            },
            dry_run: false,
            lease_token: None,
            ledger_path: PathBuf::from("/nonexistent/ledger.yaml"),
        };
        let outcome = paste(&drive, &sheets, &opts, &rules).await;
        assert_eq!(outcome.result, PasteResult::RefusedNoLease);
    }

    #[tokio::test]
    async fn dry_run_surfaces_the_same_blocked_reasoning_as_a_real_denied_run() {
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
        let verb = PasteVerb::CopyPaste {
            sheet: Some("Q1".to_string()),
            source: "A1:B2".to_string(),
            destination: "D1:E2".to_string(),
            paste_type: PasteType::Values,
            orientation: PasteOrientation::Normal,
        };

        let dry = paste(
            &drive,
            &sheets,
            &PasteOptions {
                spreadsheet_id: "sheet-1".to_string(),
                verb: verb.clone(),
                dry_run: true,
                lease_token: None,
                ledger_path: PathBuf::new(),
            },
            &rules,
        )
        .await;
        let real = paste(
            &drive,
            &sheets,
            &PasteOptions {
                spreadsheet_id: "sheet-1".to_string(),
                verb,
                dry_run: false,
                lease_token: None,
                ledger_path: PathBuf::new(),
            },
            &rules,
        )
        .await;
        assert_eq!(dry.result, real.result);
    }

    // ── `describe_lines` over every result variant, every verb ──

    fn every_paste_result() -> Vec<PasteResult> {
        let change = PasteChange {
            destination: "'Q1'!D1".to_string(),
            source: Some("'Q1'!A1:B2".to_string()),
            written_extent: "'Q1'!D1:E2".to_string(),
            overwritten: Some(vec!["D1".to_string()]),
            cleared: Some(vec!["A1".to_string()]),
            grid_edge_caveat: Some("runs past the sheet's allocated rows".to_string()),
        };
        vec![
            PasteResult::WouldChange(change.clone()),
            PasteResult::Changed(change),
            PasteResult::RefusedNotASpreadsheet {
                mime_type: "text/plain".to_string(),
            },
            PasteResult::RefusedShortcut,
            PasteResult::RefusedNoVisibleParents,
            PasteResult::RefusedSheetNotFound {
                title: "Ghost".to_string(),
                available: vec!["Q1".to_string()],
            },
            PasteResult::RefusedInvalidRange {
                detail: "bad range".to_string(),
            },
            PasteResult::RefusedEmptyData,
            PasteResult::Blocked {
                operation: DriveOperation::SheetsWrite,
                decided_by: None,
            },
            PasteResult::RefusedNoLease,
            PasteResult::RefusedLeaseExpired,
            PasteResult::RefusedLeaseWrongFile,
            PasteResult::RefusedLeaseStale,
            PasteResult::Failed {
                detail: "boom".to_string(),
            },
        ]
    }

    fn every_paste_verb() -> Vec<PasteVerb> {
        vec![
            PasteVerb::CutPaste {
                sheet: Some("Q1".to_string()),
                source: "A1:B2".to_string(),
                destination: "D1".to_string(),
                paste_type: PasteType::Normal,
            },
            PasteVerb::CopyPaste {
                sheet: Some("Q1".to_string()),
                source: "A1:B2".to_string(),
                destination: "D1:E2".to_string(),
                paste_type: PasteType::Values,
                orientation: PasteOrientation::Transpose,
            },
            PasteVerb::PasteData {
                sheet: Some("Q1".to_string()),
                destination: "D1".to_string(),
                data: "1\t2".to_string(),
                delimiter: "\t".to_string(),
                paste_type: PasteType::Values,
            },
        ]
    }

    #[test]
    fn no_describe_line_contains_a_control_character() {
        for verb in every_paste_verb() {
            for result in every_paste_result() {
                let outcome = PasteOutcome {
                    spreadsheet_id: "sheet-1".to_string(),
                    file_name: Some("Budget".to_string()),
                    resolved_folder_id: None,
                    verb: verb.clone(),
                    result,
                };
                for rendered in describe_lines(&outcome) {
                    assert!(
                        !rendered.chars().any(char::is_control),
                        "describe_lines emitted a control character for {:?}/{:?}: {rendered:?}",
                        outcome.verb,
                        outcome.result
                    );
                }
            }
        }
    }

    #[test]
    fn describe_lines_reports_no_overwritten_cells_when_the_extent_is_all_blank() {
        let change = PasteChange {
            destination: "'Q1'!D1".to_string(),
            source: None,
            written_extent: "'Q1'!D1:E2".to_string(),
            overwritten: Some(vec![]),
            cleared: None,
            grid_edge_caveat: None,
        };
        let outcome = PasteOutcome {
            spreadsheet_id: "sheet-1".to_string(),
            file_name: Some("Budget".to_string()),
            resolved_folder_id: None,
            verb: PasteVerb::PasteData {
                sheet: Some("Q1".to_string()),
                destination: "D1".to_string(),
                data: "1".to_string(),
                delimiter: "\t".to_string(),
                paste_type: PasteType::Values,
            },
            result: PasteResult::WouldChange(change),
        };
        let lines = describe_lines(&outcome);
        assert!(
            lines.iter().any(|l| l.contains("no non-blank cells")),
            "{lines:?}"
        );
    }

    #[test]
    fn describe_lines_notes_that_a_format_only_paste_previews_no_overwrite() {
        let change = PasteChange {
            destination: "'Q1'!D1:E2".to_string(),
            source: Some("'Q1'!A1:B2".to_string()),
            written_extent: "'Q1'!D1:E2".to_string(),
            overwritten: None,
            cleared: None,
            grid_edge_caveat: None,
        };
        let outcome = PasteOutcome {
            spreadsheet_id: "sheet-1".to_string(),
            file_name: Some("Budget".to_string()),
            resolved_folder_id: None,
            verb: PasteVerb::CopyPaste {
                sheet: Some("Q1".to_string()),
                source: "A1:B2".to_string(),
                destination: "D1:E2".to_string(),
                paste_type: PasteType::Format,
                orientation: PasteOrientation::Normal,
            },
            result: PasteResult::WouldChange(change),
        };
        let lines = describe_lines(&outcome);
        assert!(
            lines.iter().any(|l| l.contains("not previewed")),
            "{lines:?}"
        );
    }

    #[test]
    fn describe_lines_reports_no_cells_to_clear_when_the_source_is_blank() {
        let change = PasteChange {
            destination: "'Q1'!D1".to_string(),
            source: Some("'Q1'!A1:B2".to_string()),
            written_extent: "'Q1'!D1:E2".to_string(),
            overwritten: Some(vec![]),
            cleared: Some(vec![]),
            grid_edge_caveat: None,
        };
        let outcome = PasteOutcome {
            spreadsheet_id: "sheet-1".to_string(),
            file_name: Some("Budget".to_string()),
            resolved_folder_id: None,
            verb: PasteVerb::CutPaste {
                sheet: Some("Q1".to_string()),
                source: "A1:B2".to_string(),
                destination: "D1".to_string(),
                paste_type: PasteType::Normal,
            },
            result: PasteResult::WouldChange(change),
        };
        let lines = describe_lines(&outcome);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("no non-blank cells to clear")),
            "{lines:?}"
        );
    }

    #[test]
    fn describe_lines_falls_back_to_the_spreadsheet_id_when_no_file_name_is_known() {
        let outcome = PasteOutcome {
            spreadsheet_id: "sheet-1".to_string(),
            file_name: None,
            resolved_folder_id: None,
            verb: PasteVerb::CopyPaste {
                sheet: Some("Q1".to_string()),
                source: "A1:B2".to_string(),
                destination: "D1:E2".to_string(),
                paste_type: PasteType::Values,
                orientation: PasteOrientation::Normal,
            },
            result: PasteResult::RefusedShortcut,
        };
        let lines = describe_lines(&outcome);
        assert!(lines[0].contains("'sheet-1'"), "{lines:?}");
    }

    #[test]
    fn describe_lines_names_sheets_structure_alone_for_a_no_visible_parents_refusal_on_a_format_only_verb(
    ) {
        let outcome = PasteOutcome {
            spreadsheet_id: "sheet-1".to_string(),
            file_name: Some("Budget".to_string()),
            resolved_folder_id: None,
            verb: PasteVerb::CopyPaste {
                sheet: Some("Q1".to_string()),
                source: "A1:B2".to_string(),
                destination: "D1:E2".to_string(),
                paste_type: PasteType::Format,
                orientation: PasteOrientation::Normal,
            },
            result: PasteResult::RefusedNoVisibleParents,
        };
        let lines = describe_lines(&outcome);
        assert!(lines[0].contains("\"sheets-structure\""), "{lines:?}");
    }

    #[test]
    fn describe_lines_reports_no_available_sheets_when_the_workbook_has_none() {
        let outcome = PasteOutcome {
            spreadsheet_id: "sheet-1".to_string(),
            file_name: Some("Budget".to_string()),
            resolved_folder_id: None,
            verb: PasteVerb::PasteData {
                sheet: Some("Q1".to_string()),
                destination: "D1".to_string(),
                data: "1".to_string(),
                delimiter: "\t".to_string(),
                paste_type: PasteType::Values,
            },
            result: PasteResult::RefusedSheetNotFound {
                title: "Ghost".to_string(),
                available: vec![],
            },
        };
        let lines = describe_lines(&outcome);
        assert!(lines[0].contains("available: none"), "{lines:?}");
    }

    #[test]
    fn describe_lines_names_the_deciding_folder_rule_when_present() {
        let outcome = PasteOutcome {
            spreadsheet_id: "sheet-1".to_string(),
            file_name: Some("Budget".to_string()),
            resolved_folder_id: None,
            verb: PasteVerb::CutPaste {
                sheet: Some("Q1".to_string()),
                source: "A1:B2".to_string(),
                destination: "D1".to_string(),
                paste_type: PasteType::Normal,
            },
            result: PasteResult::Blocked {
                operation: DriveOperation::SheetsWrite,
                decided_by: Some(DecidingRule::Folder {
                    folder_id: "folder-1".to_string(),
                    depth: 2,
                }),
            },
        };
        let lines = describe_lines(&outcome);
        assert!(lines[0].contains("folder folder-1 (depth 2)"), "{lines:?}");
    }
}
