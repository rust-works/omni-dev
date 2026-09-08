//! Structural and destructive spreadsheet edits via `spreadsheets.batchUpdate`.
//!
//! The `drive sheets add-sheet`/`rename-sheet`/`insert-rows`/`insert-columns`
//! (additive, issue #1613, [ADR-0075](../../../docs/adrs/adr-0075.md)),
//! `duplicate-sheet`/`reorder-sheet`/`hide-sheet`/`show-sheet` (also
//! additive, issue #1643, [ADR-0078](../../../docs/adrs/adr-0078.md)), and
//! `delete-sheet`/`delete-rows`/`delete-columns`/`delete-range` (destructive,
//! issue #1623,
//! [ADR-0077](../../../docs/adrs/adr-0077-sheets-deletion-via-batchupdate.md))
//! engines, gated by the ADR-0071 folder write-permission rules.
//!
//! [ADR-0073](../../../docs/adrs/adr-0073.md) §12 deferred this surface
//! because `batchUpdate` is where "one call destroys far more than its
//! arguments suggest". Two properties answer that, and each is structural
//! rather than a matter of care:
//!
//! - **Typed verbs, no raw request passthrough.** Every verb builds its own
//!   [`BatchUpdateRequestItem`], so the gate and `--dry-run` can describe the
//!   exact effect. There is no `--requests file.json` and deliberately no
//!   escape hatch, following [ADR-0061](../../../docs/adrs/adr-0061.md)'s
//!   handling of force-push: the dangerous form must be unreachable, not
//!   merely discouraged. This still holds for the destructive verbs added by
//!   ADR-0077 — they gained typed variants, not a passthrough.
//! - **A distinct gate operation per risk class.** Additive verbs check
//!   [`DriveOperation::SheetsStructure`]; the four destructive verbs check
//!   [`DriveOperation::SheetsDelete`] instead — never folded together, and
//!   never reusing `SheetsWrite` either. See each variant's doc comment for
//!   why reuse would be silent privilege widening. [`StructureVerb::gate_operation`]
//!   is the single place this split is decided.
//!
//! ADR-0075 §3/§4 originally kept `deleteSheet`/`deleteDimension`/
//! `deleteRange` unreachable at the type level, pinned by a
//! `no_destructive_request_is_reachable` grep-guard test. ADR-0077
//! supersedes that: those requests are now reachable, but only through this
//! module's gated, validated path — never through a raw passthrough, and
//! never under the `sheets-structure` operation an existing `allow` rule may
//! already grant.
//!
//! Shape follows `write.rs` exactly: a public wrapper that logs, an `_inner`
//! that classifies then mutates, and `--dry-run` as an early return *after*
//! the gate so a preview and a real run share one classification by
//! construction. One difference is deliberate and tested: a dry run here does
//! issue a single `spreadsheets.get`, because describing a structural effect
//! honestly requires the sheet's real current dimensions. It still issues no
//! `batchUpdate`, and a gate-blocked attempt still issues no Sheets call at
//! all. `--dry-run` for a destructive verb stays structural-only — no
//! `values.get` read and no cell content in its output or the request log —
//! and instead states a fixed caveat that formulas elsewhere in the workbook
//! may reference what would be deleted, which cannot be checked from the
//! sheet's own dimensions (ADR-0077).

use std::time::{Duration, Instant};

use serde::Serialize;

use crate::cli::drive::format::{write_scalar_jsonl, JsonlSerialize};
use crate::drive::client::DriveClient;
use crate::drive::sheets::api::SheetsApi;
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::target_gate;
use crate::drive::sheets::types::{
    AddSheetRequest, BatchUpdateRequestItem, BatchUpdateResponse, DeleteDimensionRequest,
    DeleteRangeRequest, DeleteSheetRequest, Dimension, DimensionRange, DuplicateSheetRequest,
    GridProperties, GridRange, InsertDimensionRequest, NewSheetProperties, SheetProperties,
    SheetPropertiesUpdate, ShiftDimension, Spreadsheet, UpdateSheetPropertiesRequest,
};
use crate::drive::types::SheetTargetRefusal;
use crate::drive::write_gate::{self, DecidingRule, DriveOperation, FolderPermissionRule};
use crate::request_log::{self, DriveMutationOutcome};

/// Which structural mutation to perform.
///
/// Unlike `write.rs`'s [`WriteVerb`](crate::drive::sheets::write::WriteVerb),
/// which is fieldless with its arguments in the options struct, these verbs
/// take disjoint arguments, so the enum carries them. That also means an
/// impossible combination — a `--title` on an insert, say — is not
/// representable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StructureVerb {
    /// Add a new sheet to the workbook.
    AddSheet {
        /// Title for the new sheet.
        title: String,
        /// Zero-based position; `None` appends.
        index: Option<i64>,
        /// Initial row count; `None` takes Sheets' default.
        rows: Option<i64>,
        /// Initial column count; `None` takes Sheets' default.
        columns: Option<i64>,
    },
    /// Rename an existing sheet.
    RenameSheet {
        /// Current title of the sheet to rename.
        sheet: String,
        /// The new title.
        new_title: String,
    },
    /// Insert empty rows, shifting existing ones down.
    InsertRows {
        /// Title of the sheet to modify.
        sheet: String,
        /// 1-based row to insert before.
        at: i64,
        /// How many rows to insert.
        count: i64,
    },
    /// Insert empty columns, shifting existing ones right.
    InsertColumns {
        /// Title of the sheet to modify.
        sheet: String,
        /// 1-based column to insert before.
        at: i64,
        /// How many columns to insert.
        count: i64,
    },
    /// Delete an entire sheet from the workbook.
    DeleteSheet {
        /// Title of the sheet to delete.
        sheet: String,
    },
    /// Delete whole rows, shifting the remainder up to close the gap.
    DeleteRows {
        /// Title of the sheet to modify.
        sheet: String,
        /// 1-based first row to delete, inclusive.
        at: i64,
        /// How many rows to delete.
        count: i64,
    },
    /// Delete whole columns, shifting the remainder left to close the gap.
    DeleteColumns {
        /// Title of the sheet to modify.
        sheet: String,
        /// 1-based first column to delete, inclusive.
        at: i64,
        /// How many columns to delete.
        count: i64,
    },
    /// Delete a rectangular cell range, shifting the remainder along one
    /// axis to close the gap.
    ///
    /// All four bounds are required together: this crate only ever sends a
    /// fully-bounded [`GridRange`]. An open-ended range on one axis would
    /// degenerate into [`Self::DeleteRows`]/[`Self::DeleteColumns`]
    /// semantics and is out of scope here.
    DeleteRange {
        /// Title of the sheet to modify.
        sheet: String,
        /// 1-based first row, inclusive.
        start_row: i64,
        /// 1-based last row, inclusive.
        end_row: i64,
        /// 1-based first column, inclusive.
        start_column: i64,
        /// 1-based last column, inclusive.
        end_column: i64,
        /// Which way to shift the remaining cells afterward.
        shift: ShiftDimension,
    },
    /// Copy an existing sheet within the same workbook.
    DuplicateSheet {
        /// Title of the sheet to copy.
        sheet: String,
        /// Title for the copy; `None` takes Sheets' own "Copy of X" default.
        title: Option<String>,
        /// Zero-based position for the copy; `None` appends.
        index: Option<i64>,
    },
    /// Move an existing sheet to a new position among its siblings.
    ReorderSheet {
        /// Title of the sheet to move.
        sheet: String,
        /// The new zero-based position.
        index: i64,
    },
    /// Hide or show an existing sheet.
    SetSheetVisibility {
        /// Title of the sheet to modify.
        sheet: String,
        /// `true` hides it, `false` shows it.
        hidden: bool,
    },
}

impl StructureVerb {
    /// The `operation` this verb records in the request log.
    ///
    /// `build_drive_mutation_record` shapes `command` as `["drive",
    /// <operation>]`, so these read as `drive sheets-add-sheet` in the log
    /// even though the CLI spells them `drive sheets add-sheet`.
    const fn log_operation(&self) -> &'static str {
        match self {
            Self::AddSheet { .. } => "sheets-add-sheet",
            Self::RenameSheet { .. } => "sheets-rename-sheet",
            Self::InsertRows { .. } => "sheets-insert-rows",
            Self::InsertColumns { .. } => "sheets-insert-columns",
            Self::DeleteSheet { .. } => "sheets-delete-sheet",
            Self::DeleteRows { .. } => "sheets-delete-rows",
            Self::DeleteColumns { .. } => "sheets-delete-columns",
            Self::DeleteRange { .. } => "sheets-delete-range",
            Self::DuplicateSheet { .. } => "sheets-duplicate-sheet",
            Self::ReorderSheet { .. } => "sheets-reorder-sheet",
            Self::SetSheetVisibility { hidden: true, .. } => "sheets-hide-sheet",
            Self::SetSheetVisibility { hidden: false, .. } => "sheets-show-sheet",
        }
    }

    /// Which [`DriveOperation`] gates this verb.
    ///
    /// The single place the additive/destructive split is decided — see the
    /// module doc comment and [`DriveOperation::SheetsDelete`]'s doc comment
    /// for why the two must never share an operation.
    const fn gate_operation(&self) -> DriveOperation {
        match self {
            Self::AddSheet { .. }
            | Self::RenameSheet { .. }
            | Self::InsertRows { .. }
            | Self::InsertColumns { .. }
            | Self::DuplicateSheet { .. }
            | Self::ReorderSheet { .. }
            | Self::SetSheetVisibility { .. } => DriveOperation::SheetsStructure,
            Self::DeleteSheet { .. }
            | Self::DeleteRows { .. }
            | Self::DeleteColumns { .. }
            | Self::DeleteRange { .. } => DriveOperation::SheetsDelete,
        }
    }

    /// The CLI subcommand that spells this verb, for error messages that
    /// name the command the user actually typed — `write.rs::describe`'s
    /// convention.
    const fn label(&self) -> &'static str {
        match self {
            Self::AddSheet { .. } => "add-sheet",
            Self::RenameSheet { .. } => "rename-sheet",
            Self::InsertRows { .. } => "insert-rows",
            Self::InsertColumns { .. } => "insert-columns",
            Self::DeleteSheet { .. } => "delete-sheet",
            Self::DeleteRows { .. } => "delete-rows",
            Self::DeleteColumns { .. } => "delete-columns",
            Self::DeleteRange { .. } => "delete-range",
            Self::DuplicateSheet { .. } => "duplicate-sheet",
            Self::ReorderSheet { .. } => "reorder-sheet",
            Self::SetSheetVisibility { hidden: true, .. } => "hide-sheet",
            Self::SetSheetVisibility { hidden: false, .. } => "show-sheet",
        }
    }

    /// The title of the sheet this verb acts on: the one being created for
    /// `AddSheet`, the existing target otherwise.
    fn sheet_title(&self) -> &str {
        match self {
            Self::AddSheet { title, .. } => title,
            Self::RenameSheet { sheet, .. }
            | Self::InsertRows { sheet, .. }
            | Self::InsertColumns { sheet, .. }
            | Self::DeleteSheet { sheet }
            | Self::DeleteRows { sheet, .. }
            | Self::DeleteColumns { sheet, .. }
            | Self::DeleteRange { sheet, .. }
            | Self::DuplicateSheet { sheet, .. }
            | Self::ReorderSheet { sheet, .. }
            | Self::SetSheetVisibility { sheet, .. } => sheet,
        }
    }

    /// The title this verb moves the sheet *to*, or `None` for every verb
    /// that renames nothing.
    ///
    /// The companion to [`Self::sheet_title`], which necessarily reports the
    /// title a rename started from — without this the request log could say
    /// which tab was renamed but not to what, the one structural effect a
    /// record could not otherwise reconstruct. `DuplicateSheet`'s explicit
    /// `--title`, when given, is the same kind of fact for the same reason:
    /// the copy's name is not recoverable from `sheet_title` alone, which
    /// necessarily names the *source*.
    fn new_sheet_title(&self) -> Option<&String> {
        match self {
            Self::RenameSheet { new_title, .. } => Some(new_title),
            Self::DuplicateSheet { title, .. } => title.as_ref(),
            Self::AddSheet { .. }
            | Self::InsertRows { .. }
            | Self::InsertColumns { .. }
            | Self::DeleteSheet { .. }
            | Self::DeleteRows { .. }
            | Self::DeleteColumns { .. }
            | Self::DeleteRange { .. }
            | Self::ReorderSheet { .. }
            | Self::SetSheetVisibility { .. } => None,
        }
    }

    /// The axis an insert or a row/column delete runs along, or `None` for
    /// the verbs with no single axis (`add-sheet`, `rename-sheet`,
    /// `delete-sheet`, `delete-range`).
    const fn dimension(&self) -> Option<Dimension> {
        match self {
            Self::InsertRows { .. } | Self::DeleteRows { .. } => Some(Dimension::Rows),
            Self::InsertColumns { .. } | Self::DeleteColumns { .. } => Some(Dimension::Columns),
            Self::AddSheet { .. }
            | Self::RenameSheet { .. }
            | Self::DeleteSheet { .. }
            | Self::DeleteRange { .. }
            | Self::DuplicateSheet { .. }
            | Self::ReorderSheet { .. }
            | Self::SetSheetVisibility { .. } => None,
        }
    }
}

/// Per-call options.
#[derive(Debug, Clone)]
pub struct StructureOptions {
    /// Spreadsheet id.
    pub spreadsheet_id: String,
    /// Which mutation to perform.
    pub verb: StructureVerb,
    /// Classify and describe only; never call `batchUpdate`.
    pub dry_run: bool,
}

/// The sheet a verb resolved to, plus its dimensions at that moment.
///
/// Captured before the mutation so [`describe`] can state the *change*
/// ("1000 rows -> 1003") rather than only the request, which is the whole
/// point of a structural dry run.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct SheetSnapshot {
    /// The sheet's stable numeric id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sheet_id: Option<i64>,
    /// Its title at the time of the attempt.
    pub title: String,
    /// Allocated rows, when the API reported them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row_count: Option<i64>,
    /// Allocated columns, when the API reported them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column_count: Option<i64>,
}

impl SheetSnapshot {
    fn from_properties(props: &SheetProperties) -> Self {
        let grid = props.grid_properties.as_ref();
        Self {
            sheet_id: props.sheet_id,
            title: props.title.clone(),
            row_count: grid.and_then(|g| g.row_count),
            column_count: grid.and_then(|g| g.column_count),
        }
    }
}

/// What happened (or, under `--dry-run`, would happen).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum StructureResult {
    /// `--dry-run`, and the gate would allow it.
    WouldChange {
        /// The target sheet as it stands now. `None` for an `add-sheet`,
        /// which has no existing sheet to snapshot.
        #[serde(skip_serializing_if = "Option::is_none")]
        sheet: Option<SheetSnapshot>,
        /// How many sheets the workbook currently has.
        sheet_count: usize,
    },
    /// The target is not a Google Sheet. Checked client-side before the
    /// gate: restructuring a PDF isn't disallowed, it's meaningless.
    RefusedNotASpreadsheet {
        /// The target's actual MIME type.
        mime_type: String,
    },
    /// The target is a shortcut, which we do not follow.
    RefusedShortcut,
    /// The target has no parents this account can see, so the folder gate
    /// has no ancestor chain to evaluate. Distinct from `Blocked`; see
    /// `write.rs`'s variant of the same name.
    RefusedNoVisibleParents,
    /// The named sheet does not exist in this workbook.
    ///
    /// Not the client-side validation ADR-0073 §7 rejects: §7 is about A1
    /// *grammar*, where a naive validator rejects legal forms. Here the
    /// server's own authoritative sheet list is already in hand, and without
    /// this check a `--dry-run` would promise a change the real run fails.
    RefusedSheetNotFound {
        /// The title that was not found.
        title: String,
        /// The titles that do exist, so the message can be actionable.
        available: Vec<String>,
    },
    /// `add-sheet` was given a title the workbook already uses, or
    /// `rename-sheet`'s new title collides with a *different* existing
    /// sheet. Sheets rejects a duplicate title, and catching it here keeps
    /// the dry run truthful.
    RefusedSheetExists {
        /// The colliding title.
        title: String,
    },
    /// A numeric argument (`--at`, `--count`, `--rows`, `--columns`,
    /// `--index`) is out of range for what the workbook actually contains —
    /// checked against the same `spreadsheets.get` response already fetched
    /// for the dry run, so this costs nothing extra and is exactly as
    /// correct as the server's own eventual rejection would be (ADR-0075
    /// §6's reasoning for the sheet-existence/duplicate-title checks,
    /// applied to a numeric bound instead of a title).
    RefusedInvalidRange {
        /// What was wrong and why.
        detail: String,
    },
    /// The folder write-permission gate refused it.
    Blocked {
        /// The rule that decided the refusal, if any (`None` means the bare
        /// default policy — every write defaults deny).
        decided_by: Option<DecidingRule>,
    },
    /// The mutation succeeded.
    Changed {
        /// The sheet acted on, as it stood *before* the change.
        #[serde(skip_serializing_if = "Option::is_none")]
        sheet: Option<SheetSnapshot>,
        /// The sheet id, which for `add-sheet` the server assigns and is
        /// only knowable from the reply.
        #[serde(skip_serializing_if = "Option::is_none")]
        sheet_id: Option<i64>,
    },
    /// An API or validation error.
    Failed {
        /// A human-readable summary of what failed.
        detail: String,
    },
}

impl StructureResult {
    /// The `status` string the request log records.
    ///
    /// Hand-written rather than derived from the `#[serde(tag)]` shape,
    /// matching `WriteResult`/`MoveResult`'s precedent of keeping the log's
    /// vocabulary decoupled from the wire format.
    fn log_status(&self) -> &'static str {
        match self {
            Self::WouldChange { .. } => "would-change",
            Self::RefusedNotASpreadsheet { .. } => "refused-not-a-spreadsheet",
            Self::RefusedShortcut => "refused-shortcut",
            Self::RefusedNoVisibleParents => "refused-no-visible-parents",
            Self::RefusedSheetNotFound { .. } => "refused-sheet-not-found",
            Self::RefusedSheetExists { .. } => "refused-sheet-exists",
            Self::RefusedInvalidRange { .. } => "refused-invalid-range",
            Self::Blocked { .. } => "blocked",
            Self::Changed { .. } => "changed",
            Self::Failed { .. } => "failed",
        }
    }
}

/// The full outcome of one attempt.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct StructureOutcome {
    /// The spreadsheet acted on.
    pub spreadsheet_id: String,
    /// Its Drive file name, when the metadata fetch got that far.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    /// The folder the gate evaluated against, when exactly one parent
    /// resolved it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_folder_id: Option<String>,
    /// Which mutation was attempted.
    ///
    /// Not serialised: the caller already knows which verb it invoked, and
    /// the JSON shape is a stable output contract. It is carried so
    /// [`describe`] can render an outcome from the outcome alone, rather
    /// than taking a verb a caller could mismatch against it.
    #[serde(skip)]
    pub verb: StructureVerb,
    /// What happened.
    pub result: StructureResult,
}

impl JsonlSerialize for StructureOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> anyhow::Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Converts a 1-based inclusive `--at` into the API's zero-based half-open
/// [`DimensionRange`].
///
/// The single conversion site, on purpose. `--at` is 1-based because that is
/// what A1 and the spreadsheet UI show the user; `DimensionRange` is
/// 0-based half-open because that is the API. Inlining this at a call site
/// is how the obvious off-by-one in this feature would get in.
fn dimension_range(sheet_id: i64, dimension: Dimension, at: i64, count: i64) -> DimensionRange {
    let start = at - 1;
    DimensionRange {
        sheet_id,
        dimension,
        start_index: start,
        end_index: start + count,
    }
}

/// Runs one structural mutation, logging every attempt that isn't a dry run.
///
/// Never returns `Err`: every failure is a [`StructureResult`] variant, so
/// the caller renders one shape and the log records one shape. Exit code
/// stays 0 regardless, matching ADR-0073 §13.
pub async fn structure(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &StructureOptions,
    rules: &[FolderPermissionRule],
) -> StructureOutcome {
    let started = Instant::now();
    let outcome = structure_inner(drive, sheets, opts, rules).await;
    // The single logging site, and the only reason a dry run leaves no
    // record — `structure_inner` never logs.
    if !opts.dry_run {
        record_attempt(&outcome, opts, started.elapsed());
    }
    outcome
}

async fn structure_inner(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &StructureOptions,
    rules: &[FolderPermissionRule],
) -> StructureOutcome {
    let bare = |result| StructureOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: None,
        resolved_folder_id: None,
        verb: opts.verb.clone(),
        result,
    };

    // ── Target resolution, pre-gate refusals, and the gate itself ──────
    // Shared with `write.rs` via `target_gate::resolve`, so this whole
    // shape — the metadata fetch, the shortcut/non-spreadsheet checks, and
    // the file-id-then-ancestor-chain gate lookup — can't quietly drift
    // between the two engines.
    let (target, decision, resolved_folder_id) = match target_gate::resolve(
        drive,
        &opts.spreadsheet_id,
        opts.verb.gate_operation(),
        rules,
    )
    .await
    {
        target_gate::TargetGateOutcome::MetadataFetchFailed { detail } => {
            return bare(StructureResult::Failed { detail })
        }
        target_gate::TargetGateOutcome::Refused { target, refusal } => {
            let result = match refusal {
                SheetTargetRefusal::Shortcut => StructureResult::RefusedShortcut,
                SheetTargetRefusal::NotASpreadsheet { mime_type } => {
                    StructureResult::RefusedNotASpreadsheet { mime_type }
                }
                SheetTargetRefusal::NoVisibleParents => StructureResult::RefusedNoVisibleParents,
            };
            return StructureOutcome {
                spreadsheet_id: opts.spreadsheet_id.clone(),
                file_name: Some(target.name),
                resolved_folder_id: None,
                verb: opts.verb.clone(),
                result,
            };
        }
        // A chain that could not be resolved is a refusal, never a silent
        // allow — ADR-0071 §3's highest-priority invariant.
        target_gate::TargetGateOutcome::GateFetchFailed { target, detail } => {
            return StructureOutcome {
                spreadsheet_id: opts.spreadsheet_id.clone(),
                file_name: Some(target.name),
                resolved_folder_id: None,
                verb: opts.verb.clone(),
                result: StructureResult::Failed { detail },
            };
        }
        target_gate::TargetGateOutcome::Gated {
            target,
            decision,
            resolved_folder_id,
        } => (target, decision, resolved_folder_id),
    };

    let gated = |result| StructureOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: Some(target.name.clone()),
        resolved_folder_id: resolved_folder_id.clone(),
        verb: opts.verb.clone(),
        result,
    };

    if decision.verdict == write_gate::Verdict::Deny {
        return gated(StructureResult::Blocked {
            decided_by: decision.decided_by,
        });
    }

    // ── Resolve the workbook ───────────────────────────────────────────
    // After the gate on purpose: a blocked attempt makes zero Sheets calls,
    // preserving ADR-0073's "a refusal is exactly as auditable as a success"
    // consequence. This *is* a read, so a dry run reaches it — that is what
    // lets `describe` state real dimensions rather than guess.
    let api = SheetsApi::new(sheets);
    let workbook = match api.get_spreadsheet(&opts.spreadsheet_id).await {
        Ok(workbook) => workbook,
        Err(err) => {
            return gated(StructureResult::Failed {
                detail: format!("{err:#}"),
            })
        }
    };

    let sheet = match resolve_sheet(&workbook, &opts.verb) {
        Ok(sheet) => sheet,
        Err(result) => return gated(result),
    };
    let sheet_count = workbook.sheets.len();

    if let Err(result) = validate_verb_args(&workbook, &opts.verb, sheet.as_ref()) {
        return gated(result);
    }

    if opts.dry_run {
        // A move, not a clone: this branch always returns, so `sheet` is
        // never read again on it — the later uses below are only reachable
        // on the disjoint non-dry-run path.
        return gated(StructureResult::WouldChange { sheet, sheet_count });
    }

    // ── The mutation ───────────────────────────────────────────────────
    let request = match build_request(&opts.verb, sheet.as_ref()) {
        Ok(request) => request,
        Err(detail) => return gated(StructureResult::Failed { detail }),
    };

    match api.batch_update(&opts.spreadsheet_id, vec![request]).await {
        Ok(response) => gated(StructureResult::Changed {
            sheet_id: added_sheet_id(&response).or_else(|| sheet.as_ref().and_then(|s| s.sheet_id)),
            sheet,
        }),
        Err(err) => gated(StructureResult::Failed {
            detail: format!("{err:#}"),
        }),
    }
}

/// Finds the sheet a verb targets, or classifies why it cannot.
///
/// `add-sheet` inverts the check: it needs the title *not* to exist, and
/// returns `Ok(None)` because there is no existing sheet to snapshot.
/// `rename-sheet` checks both directions: the *current* title must exist,
/// and the *new* title must not already belong to some other sheet — the
/// same duplicate-title refusal `add-sheet` gets, since Sheets rejects a
/// duplicate title regardless of which verb produced it. Renaming a sheet
/// to the title it already has is not a collision (it names itself, not a
/// different sheet) and is allowed through as a no-op mutation.
fn resolve_sheet(
    workbook: &Spreadsheet,
    verb: &StructureVerb,
) -> Result<Option<SheetSnapshot>, StructureResult> {
    let wanted = verb.sheet_title();
    let found = workbook
        .sheets
        .iter()
        .filter_map(|sheet| sheet.properties.as_ref())
        .find(|props| props.title == wanted);

    match verb {
        StructureVerb::AddSheet { .. } => match found {
            Some(_) => Err(StructureResult::RefusedSheetExists {
                title: wanted.to_string(),
            }),
            None => Ok(None),
        },
        StructureVerb::RenameSheet { new_title, .. } => match found {
            Some(props) => {
                let collides = new_title != wanted
                    && workbook
                        .sheets
                        .iter()
                        .filter_map(|sheet| sheet.properties.as_ref())
                        .any(|other| other.title == *new_title);
                if collides {
                    return Err(StructureResult::RefusedSheetExists {
                        title: new_title.clone(),
                    });
                }
                Ok(Some(SheetSnapshot::from_properties(props)))
            }
            None => Err(StructureResult::RefusedSheetNotFound {
                title: wanted.to_string(),
                available: workbook.sheet_titles(),
            }),
        },
        // Like `add-sheet`, a given `--title` must not already be in use —
        // and unlike `rename-sheet`, colliding with the *source's own*
        // title is still a collision: the source keeps its name, so a copy
        // asking for that same name would land on an existing sheet either
        // way.
        StructureVerb::DuplicateSheet { title, .. } => match found {
            Some(props) => {
                if let Some(new_title) = title {
                    let collides = workbook
                        .sheets
                        .iter()
                        .filter_map(|sheet| sheet.properties.as_ref())
                        .any(|other| other.title == *new_title);
                    if collides {
                        return Err(StructureResult::RefusedSheetExists {
                            title: new_title.clone(),
                        });
                    }
                }
                Ok(Some(SheetSnapshot::from_properties(props)))
            }
            None => Err(StructureResult::RefusedSheetNotFound {
                title: wanted.to_string(),
                available: workbook.sheet_titles(),
            }),
        },
        _ => match found {
            Some(props) => Ok(Some(SheetSnapshot::from_properties(props))),
            None => Err(StructureResult::RefusedSheetNotFound {
                title: wanted.to_string(),
                available: workbook.sheet_titles(),
            }),
        },
    }
}

/// Validates the numeric arguments a verb carries against the workbook's
/// actual state, so a dry run can never promise a change the real run then
/// rejects. Runs after [`resolve_sheet`] (which is why it can read the
/// target sheet's real dimensions) and before the `--dry-run` early return,
/// so both share this classification exactly like the gate above it.
fn validate_verb_args(
    workbook: &Spreadsheet,
    verb: &StructureVerb,
    sheet: Option<&SheetSnapshot>,
) -> Result<(), StructureResult> {
    let invalid = |detail: String| Err(StructureResult::RefusedInvalidRange { detail });

    match verb {
        StructureVerb::AddSheet {
            index,
            rows,
            columns,
            ..
        } => {
            if let Some(rows) = rows {
                if *rows < 1 {
                    return invalid(format!("--rows must be at least 1, got {rows}"));
                }
            }
            if let Some(columns) = columns {
                if *columns < 1 {
                    return invalid(format!("--columns must be at least 1, got {columns}"));
                }
            }
            if let Some(index) = index {
                let max = workbook.sheets.len() as i64;
                if *index < 0 || *index > max {
                    return invalid(format!(
                        "--index must be between 0 and {max} inclusive (the workbook has \
                         {max} sheet(s)), got {index}"
                    ));
                }
            }
            Ok(())
        }
        StructureVerb::RenameSheet { .. }
        | StructureVerb::DeleteSheet { .. }
        | StructureVerb::DuplicateSheet { index: None, .. }
        | StructureVerb::SetSheetVisibility { hidden: false, .. } => Ok(()),
        StructureVerb::InsertRows { at, count, .. } => {
            validate_insert_bounds(Dimension::Rows, *at, *count, sheet)
        }
        StructureVerb::InsertColumns { at, count, .. } => {
            validate_insert_bounds(Dimension::Columns, *at, *count, sheet)
        }
        StructureVerb::DeleteRows { at, count, .. } => {
            validate_delete_dimension_bounds(Dimension::Rows, *at, *count, sheet)
        }
        StructureVerb::DeleteColumns { at, count, .. } => {
            validate_delete_dimension_bounds(Dimension::Columns, *at, *count, sheet)
        }
        StructureVerb::DeleteRange {
            start_row,
            end_row,
            start_column,
            end_column,
            ..
        } => validate_delete_range_bounds(*start_row, *end_row, *start_column, *end_column, sheet),
        StructureVerb::DuplicateSheet {
            index: Some(index), ..
        } => {
            // Same bound as `add-sheet`'s `--index`: a duplicate also adds
            // one sheet, so the valid positions are identical.
            let max = workbook.sheets.len() as i64;
            if *index < 0 || *index > max {
                return invalid(format!(
                    "--index must be between 0 and {max} inclusive (the workbook has \
                     {max} sheet(s)), got {index}"
                ));
            }
            Ok(())
        }
        StructureVerb::ReorderSheet { index, .. } => {
            // Unlike `add-sheet`/`duplicate-sheet`, this repositions an
            // *existing* sheet rather than adding one, so the top of the
            // valid range is one less.
            let len = workbook.sheets.len() as i64;
            let max = len - 1;
            if *index < 0 || *index > max {
                return invalid(format!(
                    "--index must be between 0 and {max} inclusive (the workbook has \
                     {len} sheet(s)), got {index}"
                ));
            }
            Ok(())
        }
        StructureVerb::SetSheetVisibility {
            sheet: title,
            hidden: true,
        } => {
            let target_id = sheet.and_then(|s| s.sheet_id);
            let another_stays_visible = workbook
                .sheets
                .iter()
                .filter_map(|s| s.properties.as_ref())
                .filter(|props| props.sheet_id != target_id)
                .any(|props| !props.hidden.unwrap_or(false));
            if !another_stays_visible {
                return invalid(format!(
                    "hiding '{title}' would leave the workbook with no visible sheets; \
                     Sheets requires at least one"
                ));
            }
            Ok(())
        }
    }
}

/// The `InsertRows`/`InsertColumns` half of [`validate_verb_args`], split
/// out so each caller supplies its own [`Dimension`] directly rather than
/// recovering it from the verb.
fn validate_insert_bounds(
    dimension: Dimension,
    at: i64,
    count: i64,
    sheet: Option<&SheetSnapshot>,
) -> Result<(), StructureResult> {
    let invalid = |detail: String| Err(StructureResult::RefusedInvalidRange { detail });

    if count < 1 {
        return invalid(format!("--count must be at least 1, got {count}"));
    }
    if at < 1 {
        return invalid(format!("--at must be at least 1, got {at}"));
    }
    // Keeps `dimension_range`'s `at - 1 + count` total, so the one pure
    // conversion stays infallible and every value that reaches it is already
    // known to fit. Deliberately an *arithmetic* bound and not a ceiling on
    // how many rows may be added: the workbook's own state implies no upper
    // bound on `--count`, so inventing one would be the client-side
    // validation ADR-0073 §7 rejects, with Sheets the authority on how large
    // a sheet may actually get.
    if at
        .checked_sub(1)
        .and_then(|start| start.checked_add(count))
        .is_none()
    {
        return invalid(format!(
            "--at {at} with --count {count} overflows the {noun} index space",
            noun = dimension.noun(),
        ));
    }
    let current = match dimension {
        Dimension::Rows => sheet.and_then(|s| s.row_count),
        Dimension::Columns => sheet.and_then(|s| s.column_count),
    };
    if let Some(current) = current {
        // `at == current + 1` is a legal append (insert after the last
        // row/column); anything past that names a position that does not
        // exist and is not immediately after one that does.
        let max_at = current + 1;
        if at > max_at {
            return invalid(format!(
                "--at {at} is past the end of the sheet, which has {current} {noun}(s); \
                 the furthest valid position is {max_at}",
                noun = dimension.noun(),
            ));
        }
    }
    Ok(())
}

/// The `DeleteRows`/`DeleteColumns` half of [`validate_verb_args`].
///
/// Mirrors [`validate_insert_bounds`], but deletion has no append-boundary
/// case: every row/column named must already exist, so the exclusive end
/// index may never exceed the sheet's current size.
fn validate_delete_dimension_bounds(
    dimension: Dimension,
    at: i64,
    count: i64,
    sheet: Option<&SheetSnapshot>,
) -> Result<(), StructureResult> {
    let invalid = |detail: String| Err(StructureResult::RefusedInvalidRange { detail });

    if count < 1 {
        return invalid(format!("--count must be at least 1, got {count}"));
    }
    if at < 1 {
        return invalid(format!("--at must be at least 1, got {at}"));
    }
    let Some(last) = at.checked_sub(1).and_then(|start| start.checked_add(count)) else {
        return invalid(format!(
            "--at {at} with --count {count} overflows the {noun} index space",
            noun = dimension.noun(),
        ));
    };
    let current = match dimension {
        Dimension::Rows => sheet.and_then(|s| s.row_count),
        Dimension::Columns => sheet.and_then(|s| s.column_count),
    };
    if let Some(current) = current {
        if last > current {
            return invalid(format!(
                "--at {at} with --count {count} reaches {noun} {end}, past the end of the \
                 sheet, which has {current} {noun}(s)",
                noun = dimension.noun(),
                end = at + count - 1,
            ));
        }
    }
    Ok(())
}

/// The `DeleteRange` half of [`validate_verb_args`].
///
/// Checks both axes are well-ordered and within the sheet's current bounds —
/// the same "never promise a change the real run then rejects" reasoning as
/// [`validate_insert_bounds`], applied to a rectangle instead of a span.
fn validate_delete_range_bounds(
    start_row: i64,
    end_row: i64,
    start_column: i64,
    end_column: i64,
    sheet: Option<&SheetSnapshot>,
) -> Result<(), StructureResult> {
    let invalid = |detail: String| Err(StructureResult::RefusedInvalidRange { detail });

    if start_row < 1 {
        return invalid(format!("--start-row must be at least 1, got {start_row}"));
    }
    if start_column < 1 {
        return invalid(format!(
            "--start-column must be at least 1, got {start_column}"
        ));
    }
    if end_row < start_row {
        return invalid(format!(
            "--end-row ({end_row}) must be at or after --start-row ({start_row})"
        ));
    }
    if end_column < start_column {
        return invalid(format!(
            "--end-column ({end_column}) must be at or after --start-column ({start_column})"
        ));
    }
    if let Some(current) = sheet.and_then(|s| s.row_count) {
        if end_row > current {
            return invalid(format!(
                "--end-row {end_row} is past the end of the sheet, which has {current} row(s)"
            ));
        }
    }
    if let Some(current) = sheet.and_then(|s| s.column_count) {
        if end_column > current {
            return invalid(format!(
                "--end-column {end_column} is past the end of the sheet, which has {current} \
                 column(s)"
            ));
        }
    }
    Ok(())
}

/// Builds the single `batchUpdate` request a verb sends.
///
/// Every verb produces exactly one request, which is why partial application
/// is not observable here and why one log record per verb is also one record
/// per request.
fn build_request(
    verb: &StructureVerb,
    sheet: Option<&SheetSnapshot>,
) -> Result<BatchUpdateRequestItem, String> {
    // Every verb but `add-sheet` addresses a sheet by its numeric id, and
    // the `fields` mask on `spreadsheets.get` always requests it. A sheet
    // that resolved by title but reported no id is a server contract
    // violation, not a user error — fail rather than guess at 0, which is a
    // real sheet id.
    let sheet_id = |verb_name: &str| -> Result<i64, String> {
        sheet.and_then(|s| s.sheet_id).ok_or_else(|| {
            format!("Sheets did not report a sheetId for the target sheet, so {verb_name} cannot address it")
        })
    };

    match verb {
        StructureVerb::AddSheet {
            title,
            index,
            rows,
            columns,
        } => {
            let grid_properties = (rows.is_some() || columns.is_some()).then_some(GridProperties {
                row_count: *rows,
                column_count: *columns,
            });
            Ok(BatchUpdateRequestItem::AddSheet(AddSheetRequest {
                properties: NewSheetProperties {
                    title: title.clone(),
                    index: *index,
                    grid_properties,
                },
            }))
        }
        StructureVerb::RenameSheet { new_title, .. } => Ok(
            BatchUpdateRequestItem::UpdateSheetProperties(UpdateSheetPropertiesRequest {
                properties: SheetPropertiesUpdate {
                    sheet_id: sheet_id("rename-sheet")?,
                    title: Some(new_title.clone()),
                    ..Default::default()
                },
                // Exactly the one field we set. A wider mask would blank
                // every property it named but we left unpopulated.
                fields: "title".to_string(),
            }),
        ),
        StructureVerb::InsertRows { at, count, .. } => Ok(BatchUpdateRequestItem::InsertDimension(
            InsertDimensionRequest {
                range: dimension_range(sheet_id("insert-rows")?, Dimension::Rows, *at, *count),
                inherit_from_before: false,
            },
        )),
        StructureVerb::InsertColumns { at, count, .. } => Ok(
            BatchUpdateRequestItem::InsertDimension(InsertDimensionRequest {
                range: dimension_range(
                    sheet_id("insert-columns")?,
                    Dimension::Columns,
                    *at,
                    *count,
                ),
                inherit_from_before: false,
            }),
        ),
        StructureVerb::DeleteSheet { .. } => {
            Ok(BatchUpdateRequestItem::DeleteSheet(DeleteSheetRequest {
                sheet_id: sheet_id("delete-sheet")?,
            }))
        }
        StructureVerb::DeleteRows { at, count, .. } => Ok(BatchUpdateRequestItem::DeleteDimension(
            DeleteDimensionRequest {
                range: dimension_range(sheet_id("delete-rows")?, Dimension::Rows, *at, *count),
            },
        )),
        StructureVerb::DeleteColumns { at, count, .. } => Ok(
            BatchUpdateRequestItem::DeleteDimension(DeleteDimensionRequest {
                range: dimension_range(
                    sheet_id("delete-columns")?,
                    Dimension::Columns,
                    *at,
                    *count,
                ),
            }),
        ),
        StructureVerb::DeleteRange {
            start_row,
            end_row,
            start_column,
            end_column,
            shift,
            ..
        } => Ok(BatchUpdateRequestItem::DeleteRange(DeleteRangeRequest {
            range: grid_range(
                sheet_id("delete-range")?,
                *start_row,
                *end_row,
                *start_column,
                *end_column,
            ),
            shift_dimension: *shift,
        })),
        StructureVerb::DuplicateSheet { title, index, .. } => Ok(
            BatchUpdateRequestItem::DuplicateSheet(DuplicateSheetRequest {
                source_sheet_id: sheet_id("duplicate-sheet")?,
                insert_sheet_index: *index,
                new_sheet_name: title.clone(),
            }),
        ),
        StructureVerb::ReorderSheet { index, .. } => Ok(
            BatchUpdateRequestItem::UpdateSheetProperties(UpdateSheetPropertiesRequest {
                properties: SheetPropertiesUpdate {
                    sheet_id: sheet_id("reorder-sheet")?,
                    index: Some(*index),
                    ..Default::default()
                },
                fields: "index".to_string(),
            }),
        ),
        StructureVerb::SetSheetVisibility { hidden, .. } => Ok(
            BatchUpdateRequestItem::UpdateSheetProperties(UpdateSheetPropertiesRequest {
                properties: SheetPropertiesUpdate {
                    sheet_id: sheet_id(if *hidden { "hide-sheet" } else { "show-sheet" })?,
                    hidden: Some(*hidden),
                    ..Default::default()
                },
                fields: "hidden".to_string(),
            }),
        ),
    }
}

/// Converts 1-based inclusive row/column bounds into the API's zero-based
/// half-open [`GridRange`]. The [`dimension_range`] of `DeleteRange`: the
/// single conversion site for the same reason.
fn grid_range(
    sheet_id: i64,
    start_row: i64,
    end_row: i64,
    start_column: i64,
    end_column: i64,
) -> GridRange {
    GridRange {
        sheet_id,
        start_row_index: Some(start_row - 1),
        end_row_index: Some(end_row),
        start_column_index: Some(start_column - 1),
        end_column_index: Some(end_column),
    }
}

/// The `sheetId` the server assigned to a newly added sheet, if this reply
/// carries one — via `addSheet` or `duplicateSheet`, whichever this batch
/// happened to contain (`structure.rs` never sends both in one request, so
/// there is no ambiguity to resolve between them).
fn added_sheet_id(response: &BatchUpdateResponse) -> Option<i64> {
    response
        .replies
        .iter()
        .find_map(|reply| reply.add_sheet.as_ref().or(reply.duplicate_sheet.as_ref()))
        .and_then(|added| added.properties.as_ref())
        .and_then(|props| props.sheet_id)
}

/// The `dimension_range` context value the request log records, e.g.
/// `"ROWS 5:7"` — the structural analogue of a cell verb's A1 `range`, for
/// effects A1 cannot express. 1-based inclusive, matching the CLI's `--at`.
///
/// Total over every `--at`/`--count` the CLI can parse, which is load-bearing
/// rather than defensive: [`record_attempt`] logs *attempts*, so this runs on
/// the refusal path too — with exactly the values
/// [`validate_insert_bounds`] just rejected. Arithmetic that only holds for
/// validated input would panic while recording the refusal of the input that
/// broke it. A span that cannot be computed is omitted (the log key is
/// omit-if-absent) rather than recorded inverted; the `refused-invalid-range`
/// status and its `detail` already carry the numbers the user typed.
fn dimension_range_label(verb: &StructureVerb) -> Option<String> {
    let dimension = verb.dimension()?;
    let (at, count) = match verb {
        StructureVerb::InsertRows { at, count, .. }
        | StructureVerb::InsertColumns { at, count, .. }
        | StructureVerb::DeleteRows { at, count, .. }
        | StructureVerb::DeleteColumns { at, count, .. } => (*at, *count),
        _ => return None,
    };
    // A span is only meaningful for a positive count. `count < 1` is a
    // refusal `validate_insert_bounds` has already classified, and
    // `at + count - 1` computes cleanly for it while meaning nothing — an
    // *inverted* span, which is the misleading answer rather than the
    // missing one.
    if count < 1 {
        return None;
    }
    let end = at.checked_add(count).and_then(|end| end.checked_sub(1))?;
    Some(format!("{} {at}:{end}", dimension.as_str()))
}

/// The `grid_range` context value the request log records for a
/// `delete-range` verb, e.g. `"rows 2-10, columns 2-4"` — 1-based inclusive,
/// matching the CLI's `--start-row`/`--end-row`/`--start-column`/
/// `--end-column`. `None` for every other verb.
fn grid_range_label(verb: &StructureVerb) -> Option<String> {
    match verb {
        StructureVerb::DeleteRange {
            start_row,
            end_row,
            start_column,
            end_column,
            ..
        } => Some(format!(
            "rows {start_row}-{end_row}, columns {start_column}-{end_column}"
        )),
        _ => None,
    }
}

/// Emits the `kind: "drivemutation"` record.
///
/// Inside the engine, never the CLI layer, so a future MCP caller cannot
/// bypass it — and so a `Blocked` outcome, which makes zero Sheets calls,
/// still leaves a trace. Same reasoning as `write.rs::record_attempt`.
fn record_attempt(outcome: &StructureOutcome, opts: &StructureOptions, duration: Duration) {
    let error = match &outcome.result {
        StructureResult::Failed { detail } => Some(detail.clone()),
        _ => None,
    };
    let decided_by = match &outcome.result {
        StructureResult::Blocked { decided_by } => decided_by.as_ref(),
        _ => None,
    };
    let decided_by = write_gate::decided_by_log_fields(decided_by);
    let sheet_id = match &outcome.result {
        StructureResult::Changed { sheet_id, .. } => *sheet_id,
        StructureResult::WouldChange { sheet, .. } => sheet.as_ref().and_then(|s| s.sheet_id),
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
        sheet_id,
        sheet_title: Some(opts.verb.sheet_title().to_string()),
        sheet_new_title: opts.verb.new_sheet_title().map(ToString::to_string),
        dimension_range: dimension_range_label(&opts.verb),
        grid_range: grid_range_label(&opts.verb),
        error,
        duration,
        ..Default::default()
    });
}

/// Renders an outcome as human-readable text.
///
/// Joins [`describe_lines`]; see it for why the line structure, and not just
/// the finished string, is what this module produces.
#[must_use]
pub fn describe(outcome: &StructureOutcome) -> String {
    describe_lines(outcome).join("\n")
}

/// Renders an outcome as its individual lines, **none of which contains a
/// newline**.
///
/// Lives here rather than in the CLI layer so the CLI and a future MCP
/// caller describe an outcome identically.
///
/// The `WouldChange` arms are the substance of this module's answer to "what
/// does `--dry-run` show for an effect that isn't a range": the sheet's real
/// current dimensions, the resulting ones, and — for an insert — the shift
/// that no bounded range could express. That shift is a second line, which
/// is why this is the line list and `describe` the convenience over it.
///
/// Returning the lines rather than one pre-joined string is what lets the
/// CLI sanitize each one and then supply the separators itself. Every line
/// here interpolates untrusted text — a Drive-supplied file name, sheet
/// titles read out of the workbook, raw API error details — so a newline
/// arriving inside one of those values must not be able to pass for a line
/// break this module chose. Filtering a joined string cannot tell the two
/// apart; filtering the parts before joining them cannot confuse them.
/// `no_describe_line_contains_a_newline` pins the guarantee.
#[must_use]
pub fn describe_lines(outcome: &StructureOutcome) -> Vec<String> {
    let verb = &outcome.verb;
    let book = outcome.file_name.as_deref().map_or_else(
        || format!("'{}'", outcome.spreadsheet_id),
        |n| format!("'{n}'"),
    );

    match &outcome.result {
        StructureResult::WouldChange { sheet, sheet_count } => {
            describe_would_change(verb, sheet.as_ref(), *sheet_count, &book)
        }
        StructureResult::RefusedNotASpreadsheet { mime_type } => vec![format!(
            "Refused: {book} is not a Google Sheet (mimeType: {mime_type}); \
             `drive sheets {}` only works on spreadsheets",
            verb.label()
        )],
        StructureResult::RefusedShortcut => vec![format!(
            "Refused: {book} is a shortcut; `drive sheets {}` doesn't follow shortcuts — \
             resolve the target spreadsheet's id and use that instead",
            verb.label()
        )],
        StructureResult::RefusedNoVisibleParents => {
            let op = verb.gate_operation();
            vec![format!(
                "Refused: {book} has no parent folder visible to this account, so no folder \
                 rule can apply to it. This is normal for a Sheet shared by link or email. \
                 Grant it by id instead: add {{\"file_id\": \"<spreadsheet id>\", \"allow\": \
                 [\"{op}\"]}} to write_permissions.rules. (Adding it to a folder in your own \
                 Drive and granting that folder `{op}` also works.)"
            )]
        }
        StructureResult::RefusedSheetNotFound { title, available } => {
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
        StructureResult::RefusedSheetExists { title } => {
            vec![format!(
                "Refused: {book} already has a sheet titled '{title}'"
            )]
        }
        StructureResult::RefusedInvalidRange { detail } => vec![format!("Refused: {detail}")],
        StructureResult::Blocked { decided_by } => {
            let op = verb.gate_operation();
            let action = if matches!(op, DriveOperation::SheetsDelete) {
                "destructive edits"
            } else {
                "structural edits"
            };
            vec![match decided_by {
                Some(rule) => format!(
                    "Blocked: {action} to {book} refused by rule on {} {}{}",
                    rule.kind_label(),
                    rule.id(),
                    rule.depth_suffix()
                ),
                None => format!(
                    "Blocked: {action} to {book} refused by default policy (no matching rule \
                     for {op})"
                ),
            }]
        }
        StructureResult::Changed { sheet, sheet_id } => {
            vec![describe_changed(verb, sheet.as_ref(), *sheet_id, &book)]
        }
        StructureResult::Failed { detail } => vec![format!("Failed: {detail}")],
    }
}

fn describe_would_change(
    verb: &StructureVerb,
    sheet: Option<&SheetSnapshot>,
    sheet_count: usize,
    book: &str,
) -> Vec<String> {
    let id = sheet
        .and_then(|s| s.sheet_id)
        .map_or_else(String::new, |id| format!(" (sheetId {id})"));

    match verb {
        StructureVerb::AddSheet {
            title,
            index,
            rows,
            columns,
        } => {
            let size = match (rows, columns) {
                (Some(r), Some(c)) => format!(" ({r} x {c})"),
                (Some(r), None) => format!(" ({r} rows)"),
                (None, Some(c)) => format!(" ({c} columns)"),
                (None, None) => String::new(),
            };
            let position = index.map_or_else(
                || " at the end".to_string(),
                |index| format!(" at index {index}"),
            );
            vec![format!(
                "Would add sheet '{title}'{size}{position} of {book} \
                 ({sheet_count} sheet(s) -> {})",
                sheet_count + 1
            )]
        }
        StructureVerb::RenameSheet {
            sheet: from,
            new_title,
        } => {
            vec![format!(
                "Would rename sheet '{from}'{id} to '{new_title}' in {book}"
            )]
        }
        StructureVerb::InsertRows {
            sheet: from,
            at,
            count,
        } => describe_would_insert(Dimension::Rows, from, &id, *at, *count, sheet, book),
        StructureVerb::InsertColumns {
            sheet: from,
            at,
            count,
        } => describe_would_insert(Dimension::Columns, from, &id, *at, *count, sheet, book),
        StructureVerb::DeleteSheet { sheet: from } => {
            vec![format!(
                "Would delete sheet '{from}'{id} from {book} ({sheet_count} sheet(s) -> {}); \
                 {FORMULA_CAVEAT}",
                sheet_count.saturating_sub(1)
            )]
        }
        StructureVerb::DeleteRows {
            sheet: from,
            at,
            count,
        } => describe_would_delete_dimension(Dimension::Rows, from, &id, *at, *count, sheet, book),
        StructureVerb::DeleteColumns {
            sheet: from,
            at,
            count,
        } => {
            describe_would_delete_dimension(Dimension::Columns, from, &id, *at, *count, sheet, book)
        }
        StructureVerb::DeleteRange {
            sheet: from,
            start_row,
            end_row,
            start_column,
            end_column,
            shift,
        } => vec![describe_would_delete_range(
            from,
            &id,
            *start_row,
            *end_row,
            *start_column,
            *end_column,
            *shift,
            book,
        )],
        StructureVerb::DuplicateSheet {
            sheet: from,
            title,
            index,
        } => {
            let to = title.as_deref().map_or_else(
                || " as a copy Sheets names automatically".to_string(),
                |title| format!(" as '{title}'"),
            );
            let position = index.map_or_else(
                // Confirmed against the live API: unlike `add-sheet`,
                // `duplicateSheetRequest` does not default to appending —
                // an omitted index lands the copy at the *front* (index 0).
                || " at the front (Sheets' default; not the end)".to_string(),
                |index| format!(" at index {index}"),
            );
            vec![format!(
                "Would duplicate sheet '{from}'{id}{to}{position} of {book} \
                 ({sheet_count} sheet(s) -> {})",
                sheet_count + 1
            )]
        }
        StructureVerb::ReorderSheet { sheet: from, index } => {
            vec![format!(
                "Would move sheet '{from}'{id} to index {index} in {book}"
            )]
        }
        StructureVerb::SetSheetVisibility {
            sheet: from,
            hidden,
        } => {
            let verb_word = if *hidden { "hide" } else { "show" };
            vec![format!("Would {verb_word} sheet '{from}'{id} in {book}")]
        }
    }
}

/// The insert arms of [`describe_would_change`].
///
/// This is where a structural dry run earns its keep: it names the resulting
/// dimension *and* the shift, which is the part of the effect no bounded
/// range could express and the reason ADR-0073 §12 called this out.
// One parameter per fact the message needs (dimension, the two labels
// already formatted by the caller, the insert's own `at`/`count`, the
// target's dimensions, and the workbook name) — bundling them into a struct
// would just move the same fields one level out for a private helper with a
// single call site.
#[allow(clippy::too_many_arguments)]
fn describe_would_insert(
    dimension: Dimension,
    from: &str,
    id: &str,
    at: i64,
    count: i64,
    sheet: Option<&SheetSnapshot>,
    book: &str,
) -> Vec<String> {
    let summary = format!(
        "Would insert {count} {noun}(s) before {noun} {at} of '{from}'{id} in {book}",
        noun = dimension.noun(),
    );
    let before = match dimension {
        Dimension::Rows => sheet.and_then(|s| s.row_count),
        Dimension::Columns => sheet.and_then(|s| s.column_count),
    };
    // The shift line is the trustworthy half of a structural preview, so it
    // is omitted outright when it cannot be stated — an unknown current size,
    // or a resulting size that does not fit. A wrong number here would be
    // worse than a missing one.
    let Some(before) = before else {
        return vec![summary];
    };
    let Some(after) = before.checked_add(count) else {
        return vec![summary];
    };
    // `at > before` only at the append boundary (`at == before + 1`,
    // enforced by `validate_verb_args`): there is nothing after the last
    // row/column to shift, so "existing N-M shift" would print an inverted,
    // self-contradictory range instead of the truth.
    let existing = if at <= before {
        format!(
            "; existing {plural} {at}-{before} shift {direction}",
            plural = plural(dimension),
            direction = match dimension {
                Dimension::Rows => "down",
                Dimension::Columns => "right",
            },
        )
    } else {
        format!(
            "; appended at the end, no existing {plural} shift",
            plural = plural(dimension)
        )
    };
    vec![
        summary,
        format!(
            "  ({before} {plural} -> {after}{existing})",
            plural = plural(dimension),
        ),
    ]
}

/// A destructive `--dry-run` cannot check whether some formula elsewhere in
/// the workbook references what would be deleted — that would need reading
/// every other sheet's formulas, not just this verb's own target, and ADR-0077
/// keeps a destructive dry run structural-only (no extra `values.get` read,
/// no cell content in its output or the request log). This fixed caveat is
/// the honest substitute.
const FORMULA_CAVEAT: &str =
    "formulas elsewhere in the workbook that reference this may break, which cannot be \
     checked automatically";

/// The `DeleteRows`/`DeleteColumns` arm of [`describe_would_change`].
///
/// Mirrors [`describe_would_insert`]'s shape (a summary line plus a shift
/// line), shrinking instead of growing and shifting the opposite direction.
/// Unlike insert, deletion has no append-boundary case — every row/column
/// named already exists — so the "nothing remains after" branch replaces
/// insert's "appended at the end" one.
#[allow(clippy::too_many_arguments)]
fn describe_would_delete_dimension(
    dimension: Dimension,
    from: &str,
    id: &str,
    at: i64,
    count: i64,
    sheet: Option<&SheetSnapshot>,
    book: &str,
) -> Vec<String> {
    let Some(last) = at.checked_add(count).and_then(|end| end.checked_sub(1)) else {
        return vec![format!(
            "Would delete {count} {noun}(s) from {noun} {at} of '{from}'{id} in {book}",
            noun = dimension.noun(),
        )];
    };
    let summary = format!(
        "Would delete {count} {noun}(s) {at}-{last} of '{from}'{id} in {book}",
        noun = dimension.noun(),
    );
    let before = match dimension {
        Dimension::Rows => sheet.and_then(|s| s.row_count),
        Dimension::Columns => sheet.and_then(|s| s.column_count),
    };
    let Some(before) = before else {
        return vec![summary];
    };
    let Some(after) = before.checked_sub(count) else {
        return vec![summary];
    };
    let existing = if last < before {
        format!(
            "; existing {plural} {next}-{before} shift {direction}",
            plural = plural(dimension),
            next = last + 1,
            direction = match dimension {
                Dimension::Rows => "up",
                Dimension::Columns => "left",
            },
        )
    } else {
        format!(
            "; nothing remains after the deleted {plural}",
            plural = plural(dimension)
        )
    };
    vec![
        summary,
        format!(
            "  ({before} {plural} -> {after}{existing}; {FORMULA_CAVEAT})",
            plural = plural(dimension),
        ),
    ]
}

/// The `DeleteRange` arm of [`describe_would_change`].
///
/// `deleteRange` never changes the sheet's `rowCount`/`columnCount` — it
/// shifts cells within the same bounded grid — so unlike the dimension
/// deletes there is no "before -> after" size line to add; the rectangle and
/// shift direction already say the whole effect in one line.
#[allow(clippy::too_many_arguments)]
fn describe_would_delete_range(
    from: &str,
    id: &str,
    start_row: i64,
    end_row: i64,
    start_column: i64,
    end_column: i64,
    shift: ShiftDimension,
    book: &str,
) -> String {
    let direction = match shift {
        ShiftDimension::Rows => "up",
        ShiftDimension::Columns => "left",
    };
    format!(
        "Would delete rows {start_row}-{end_row}, columns {start_column}-{end_column} of \
         '{from}'{id} in {book}, shifting remaining cells {direction} to close the gap; \
         {FORMULA_CAVEAT}"
    )
}

fn describe_changed(
    verb: &StructureVerb,
    sheet: Option<&SheetSnapshot>,
    sheet_id: Option<i64>,
    book: &str,
) -> String {
    let id = sheet_id.map_or_else(String::new, |id| format!(" (sheetId {id})"));
    match verb {
        StructureVerb::AddSheet { title, .. } => {
            format!("Added sheet '{title}'{id} to {book}")
        }
        StructureVerb::RenameSheet {
            sheet: from,
            new_title,
        } => {
            format!("Renamed sheet '{from}' to '{new_title}'{id} in {book}")
        }
        StructureVerb::InsertRows {
            sheet: from,
            at,
            count,
        } => describe_inserted(Dimension::Rows, from, &id, *at, *count, sheet, book),
        StructureVerb::InsertColumns {
            sheet: from,
            at,
            count,
        } => describe_inserted(Dimension::Columns, from, &id, *at, *count, sheet, book),
        StructureVerb::DeleteSheet { sheet: from } => {
            format!("Deleted sheet '{from}'{id} from {book}; {RECOVERY_NOTE}")
        }
        StructureVerb::DeleteRows {
            sheet: from,
            at,
            count,
        } => describe_deleted_dimension(Dimension::Rows, from, &id, *at, *count, sheet, book),
        StructureVerb::DeleteColumns {
            sheet: from,
            at,
            count,
        } => describe_deleted_dimension(Dimension::Columns, from, &id, *at, *count, sheet, book),
        StructureVerb::DeleteRange {
            sheet: from,
            start_row,
            end_row,
            start_column,
            end_column,
            shift,
        } => describe_deleted_range(
            from,
            &id,
            *start_row,
            *end_row,
            *start_column,
            *end_column,
            *shift,
            book,
        ),
        StructureVerb::DuplicateSheet {
            sheet: from, title, ..
        } => {
            let to = title
                .as_deref()
                .map_or_else(String::new, |title| format!(" as '{title}'"));
            format!("Duplicated sheet '{from}'{to}{id} in {book}")
        }
        StructureVerb::ReorderSheet { sheet: from, index } => {
            format!("Moved sheet '{from}'{id} to index {index} in {book}")
        }
        StructureVerb::SetSheetVisibility {
            sheet: from,
            hidden,
        } => {
            let verb_word = if *hidden { "Hid" } else { "Showed" };
            format!("{verb_word} sheet '{from}'{id} in {book}")
        }
    }
}

/// The insert arms of [`describe_changed`].
// Same shape as `describe_would_insert`, for the same reason: one parameter
// per fact the message needs, with a single call site.
#[allow(clippy::too_many_arguments)]
fn describe_inserted(
    dimension: Dimension,
    from: &str,
    id: &str,
    at: i64,
    count: i64,
    sheet: Option<&SheetSnapshot>,
    book: &str,
) -> String {
    let now = match dimension {
        Dimension::Rows => sheet.and_then(|s| s.row_count),
        Dimension::Columns => sheet.and_then(|s| s.column_count),
    }
    .and_then(|before| before.checked_add(count))
    .map_or_else(String::new, |after| {
        format!(" ({after} {} now)", plural(dimension))
    });
    format!(
        "Inserted {count} {noun}(s) before {noun} {at} of '{from}'{id} in {book}{now}",
        noun = dimension.noun(),
    )
}

/// There is no `files.delete` or undo anywhere in this integration (ADR-0077):
/// Drive's own version history is the only recovery path, so every
/// destructive real-run message says so.
const RECOVERY_NOTE: &str =
    "this cannot be undone through omni-dev — use Google Drive's version history to recover it \
     if needed";

/// The `DeleteRows`/`DeleteColumns` arm of [`describe_changed`].
#[allow(clippy::too_many_arguments)]
fn describe_deleted_dimension(
    dimension: Dimension,
    from: &str,
    id: &str,
    at: i64,
    count: i64,
    sheet: Option<&SheetSnapshot>,
    book: &str,
) -> String {
    let last = at.checked_add(count).and_then(|end| end.checked_sub(1));
    let range = last.map_or_else(|| at.to_string(), |last| format!("{at}-{last}"));
    let now = match dimension {
        Dimension::Rows => sheet.and_then(|s| s.row_count),
        Dimension::Columns => sheet.and_then(|s| s.column_count),
    }
    .and_then(|before| before.checked_sub(count))
    .map_or_else(String::new, |after| {
        format!(" ({after} {} now)", plural(dimension))
    });
    format!(
        "Deleted {count} {noun}(s) {range} of '{from}'{id} in {book}{now}; {RECOVERY_NOTE}",
        noun = dimension.noun(),
    )
}

/// The `DeleteRange` arm of [`describe_changed`].
#[allow(clippy::too_many_arguments)]
fn describe_deleted_range(
    from: &str,
    id: &str,
    start_row: i64,
    end_row: i64,
    start_column: i64,
    end_column: i64,
    shift: ShiftDimension,
    book: &str,
) -> String {
    let direction = match shift {
        ShiftDimension::Rows => "up",
        ShiftDimension::Columns => "left",
    };
    format!(
        "Deleted rows {start_row}-{end_row}, columns {start_column}-{end_column} of '{from}'{id} \
         in {book}, shifted remaining cells {direction}; {RECOVERY_NOTE}"
    )
}

const fn plural(dimension: Dimension) -> &'static str {
    match dimension {
        Dimension::Rows => "rows",
        Dimension::Columns => "columns",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::sheets::client::SHEETS_API_URL;
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
    ///
    /// `replace_session` must run before the derive: it swaps the Drive
    /// client's whole transport, so deriving first would leave the Sheets
    /// client pointed at the real `oauth2.googleapis.com`.
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
        }
    }

    /// A `spreadsheets.get` reply with two sheets, `Q1` (1000 x 26) and
    /// `Q2` (500 x 10).
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
                            "sheetId": 118_293, "title": "Q2", "index": 1,
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

    fn opts(verb: StructureVerb, dry_run: bool) -> StructureOptions {
        StructureOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb,
            dry_run,
        }
    }

    fn rename() -> StructureVerb {
        StructureVerb::RenameSheet {
            sheet: "Q2".to_string(),
            new_title: "Q3".to_string(),
        }
    }

    fn insert_rows() -> StructureVerb {
        StructureVerb::InsertRows {
            sheet: "Q2".to_string(),
            at: 5,
            count: 3,
        }
    }

    fn add_sheet() -> StructureVerb {
        StructureVerb::AddSheet {
            title: "Q3".to_string(),
            index: None,
            rows: None,
            columns: None,
        }
    }

    fn delete_sheet() -> StructureVerb {
        StructureVerb::DeleteSheet {
            sheet: "Q2".to_string(),
        }
    }

    fn delete_rows() -> StructureVerb {
        StructureVerb::DeleteRows {
            sheet: "Q2".to_string(),
            at: 5,
            count: 3,
        }
    }

    fn delete_columns() -> StructureVerb {
        StructureVerb::DeleteColumns {
            sheet: "Q2".to_string(),
            at: 3,
            count: 2,
        }
    }

    fn delete_range() -> StructureVerb {
        StructureVerb::DeleteRange {
            sheet: "Q2".to_string(),
            start_row: 2,
            end_row: 4,
            start_column: 2,
            end_column: 3,
            shift: ShiftDimension::Rows,
        }
    }

    /// [`delete_range`]'s twin with the other shift axis, so the `COLUMNS`
    /// wire value and "shift ... left" wording get exercised too — `Rows` is
    /// otherwise the only [`ShiftDimension`] any test ever names.
    fn delete_range_columns_shift() -> StructureVerb {
        StructureVerb::DeleteRange {
            sheet: "Q2".to_string(),
            start_row: 2,
            end_row: 4,
            start_column: 2,
            end_column: 3,
            shift: ShiftDimension::Columns,
        }
    }

    fn duplicate_sheet() -> StructureVerb {
        StructureVerb::DuplicateSheet {
            sheet: "Q2".to_string(),
            title: Some("Q2 Copy".to_string()),
            index: Some(1),
        }
    }

    fn duplicate_sheet_default() -> StructureVerb {
        StructureVerb::DuplicateSheet {
            sheet: "Q2".to_string(),
            title: None,
            index: None,
        }
    }

    fn reorder_sheet() -> StructureVerb {
        StructureVerb::ReorderSheet {
            sheet: "Q2".to_string(),
            index: 0,
        }
    }

    fn hide_sheet() -> StructureVerb {
        StructureVerb::SetSheetVisibility {
            sheet: "Q2".to_string(),
            hidden: true,
        }
    }

    fn show_sheet() -> StructureVerb {
        StructureVerb::SetSheetVisibility {
            sheet: "Q2".to_string(),
            hidden: false,
        }
    }

    fn delete_allow_rule(folder: &str) -> FolderPermissionRule {
        FolderPermissionRule {
            folder_id: Some(folder.to_string()),
            file_id: None,
            recursive: true,
            allow: std::iter::once(DriveOperation::SheetsDelete).collect(),
            deny: HashSet::default(),
        }
    }

    // ── the safety property this module now enforces ───────────────────
    //
    // ADR-0075 §3/§4 kept every destructive `batchUpdate` request
    // unreachable at the type level, pinned by a
    // `no_destructive_request_is_reachable` grep-guard test. ADR-0077
    // (issue #1623) supersedes that: the requests are now reachable, but
    // only through `StructureVerb::gate_operation` routing them to
    // `DriveOperation::SheetsDelete` — never `SheetsStructure`, and never
    // through a raw passthrough (there still isn't one). The tests below are
    // what replace the old grep-guard: they pin the *new* invariant instead
    // of the old absence.

    #[test]
    fn every_delete_verb_gates_on_sheets_delete_not_sheets_structure() {
        for verb in [
            StructureVerb::DeleteSheet {
                sheet: "Q1".to_string(),
            },
            StructureVerb::DeleteRows {
                sheet: "Q1".to_string(),
                at: 1,
                count: 1,
            },
            StructureVerb::DeleteColumns {
                sheet: "Q1".to_string(),
                at: 1,
                count: 1,
            },
            StructureVerb::DeleteRange {
                sheet: "Q1".to_string(),
                start_row: 1,
                end_row: 2,
                start_column: 1,
                end_column: 2,
                shift: ShiftDimension::Rows,
            },
        ] {
            assert_eq!(verb.gate_operation(), DriveOperation::SheetsDelete);
        }
    }

    #[test]
    fn every_additive_verb_still_gates_on_sheets_structure() {
        for verb in [
            add_sheet(),
            StructureVerb::RenameSheet {
                sheet: "Q1".to_string(),
                new_title: "Q2".to_string(),
            },
            StructureVerb::InsertRows {
                sheet: "Q1".to_string(),
                at: 1,
                count: 1,
            },
            StructureVerb::InsertColumns {
                sheet: "Q1".to_string(),
                at: 1,
                count: 1,
            },
            StructureVerb::DuplicateSheet {
                sheet: "Q1".to_string(),
                title: None,
                index: None,
            },
            StructureVerb::ReorderSheet {
                sheet: "Q1".to_string(),
                index: 0,
            },
            StructureVerb::SetSheetVisibility {
                sheet: "Q1".to_string(),
                hidden: true,
            },
        ] {
            assert_eq!(verb.gate_operation(), DriveOperation::SheetsStructure);
        }
    }

    // ── the 1-based/0-based conversion, the obvious off-by-one ─────────

    #[test]
    fn at_is_one_based_inclusive_and_becomes_zero_based_half_open() {
        // "insert 3 rows before row 5" == indices [4, 7).
        let range = dimension_range(7, Dimension::Rows, 5, 3);
        assert_eq!(range.start_index, 4);
        assert_eq!(range.end_index, 7);
        assert_eq!(range.sheet_id, 7);
        assert_eq!(range.dimension, Dimension::Rows);
    }

    #[test]
    fn inserting_at_row_one_starts_at_index_zero() {
        // The boundary case, and the reason `inherit_from_before` is fixed
        // at false: the API rejects `true` when start_index is 0.
        let range = dimension_range(0, Dimension::Rows, 1, 1);
        assert_eq!(range.start_index, 0);
        assert_eq!(range.end_index, 1);
    }

    #[test]
    fn a_single_insert_spans_exactly_one_index() {
        let range = dimension_range(0, Dimension::Columns, 3, 1);
        assert_eq!((range.start_index, range.end_index), (2, 3));
    }

    #[test]
    fn dimension_range_label_is_one_based_inclusive_like_the_flag() {
        // Insert 3 rows at row 5 => rows 5, 6, 7 are the new ones.
        assert_eq!(
            dimension_range_label(&insert_rows()).as_deref(),
            Some("ROWS 5:7")
        );
        assert_eq!(
            dimension_range_label(&StructureVerb::InsertColumns {
                sheet: "Q2".to_string(),
                at: 2,
                count: 1,
            })
            .as_deref(),
            Some("COLUMNS 2:2")
        );
        // Verbs that span no dimension record none.
        assert_eq!(dimension_range_label(&rename()), None);
        assert_eq!(dimension_range_label(&add_sheet()), None);
    }

    #[test]
    fn dimension_range_label_omits_a_span_it_cannot_compute() {
        // `record_attempt` logs attempts, refused ones included, so this is
        // reached with arguments no validation let through. An uncomputable
        // span is omitted rather than recorded inverted — the omit-if-absent
        // log key already expresses "no span", and the refusal's own detail
        // carries the numbers.
        for (at, count) in [(i64::MAX, 5), (1, i64::MIN), (-1, i64::MIN)] {
            assert_eq!(
                dimension_range_label(&StructureVerb::InsertRows {
                    sheet: "Q2".to_string(),
                    at,
                    count,
                }),
                None,
                "--at {at} --count {count} must not produce a span"
            );
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
        // No mock for parent-1 (the gate never runs) and none for any Sheets
        // endpoint, even though the rule below would otherwise permit this.
        let outcome = structure(
            &drive,
            &sheets,
            &opts(rename(), false),
            &[allow_rule("parent-1")],
        )
        .await;
        assert!(matches!(
            outcome.result,
            StructureResult::RefusedNotASpreadsheet { .. }
        ));
        let text = describe(&outcome);
        assert!(text.contains("is not a Google Sheet"), "{text}");
        assert!(text.contains("drive sheets rename-sheet"), "{text}");
    }

    #[tokio::test]
    async fn shortcut_is_refused_with_its_own_message() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            "application/vnd.google-apps.shortcut",
            &["parent-1"],
        )
        .mount(&server)
        .await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(rename(), false),
            &[allow_rule("parent-1")],
        )
        .await;
        assert!(matches!(outcome.result, StructureResult::RefusedShortcut));
        let text = describe(&outcome);
        assert!(text.contains("is a shortcut"), "{text}");
    }

    #[tokio::test]
    async fn a_sheet_with_no_visible_parents_is_refused_distinctly_from_blocked() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &[])
            .mount(&server)
            .await;
        let outcome = structure(&drive, &sheets, &opts(rename(), false), &[]).await;
        assert!(matches!(
            outcome.result,
            StructureResult::RefusedNoVisibleParents
        ));
        let text = describe(&outcome);
        assert!(text.contains("no parent folder visible"), "{text}");
        // Names the operation the operator would have to grant, so the
        // message is actionable rather than merely accurate.
        assert!(text.contains("sheets-structure"), "{text}");
    }

    /// The same message for a delete verb must name `sheets-delete`, not
    /// `sheets-structure` — suggesting the wrong operation here would send an
    /// operator to grant a capability that does not cover what they asked
    /// for.
    #[tokio::test]
    async fn a_delete_verbs_no_visible_parents_message_names_sheets_delete() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &[])
            .mount(&server)
            .await;
        let outcome = structure(&drive, &sheets, &opts(delete_sheet(), false), &[]).await;
        let text = describe(&outcome);
        assert!(text.contains("sheets-delete"), "{text}");
        assert!(!text.contains("sheets-structure"), "{text}");
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
        // Deliberately no `spreadsheets.get` mock and no `batchUpdate` mock:
        // a blocked attempt must not reach the Sheets API at all, which is
        // what keeps a refusal exactly as auditable as a success.
        let outcome = structure(&drive, &sheets, &opts(rename(), false), &[]).await;
        assert!(matches!(
            outcome.result,
            StructureResult::Blocked { decided_by: None }
        ));
        let text = describe(&outcome);
        assert!(text.contains("default policy"), "{text}");
    }

    #[tokio::test]
    async fn a_blocked_delete_names_destructive_edits_and_sheets_delete() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        let outcome = structure(&drive, &sheets, &opts(delete_sheet(), false), &[]).await;
        assert!(matches!(
            outcome.result,
            StructureResult::Blocked { decided_by: None }
        ));
        let text = describe(&outcome);
        assert!(text.contains("destructive edits"), "{text}");
        assert!(text.contains("default policy"), "{text}");
        assert!(text.contains("sheets-delete"), "{text}");
        assert!(!text.contains("structural edits"), "{text}");
    }

    #[tokio::test]
    async fn a_sheets_write_rule_alone_does_not_permit_a_structural_edit() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        // The non-widening property, observed end to end rather than only in
        // the gate's unit tests: a folder granted cell writes must not gain
        // the power to restructure the workbook.
        let rules = [FolderPermissionRule {
            folder_id: Some("parent-1".to_string()),
            file_id: None,
            recursive: true,
            allow: std::iter::once(DriveOperation::SheetsWrite).collect(),
            deny: HashSet::default(),
        }];
        let outcome = structure(&drive, &sheets, &opts(rename(), false), &rules).await;
        assert!(matches!(outcome.result, StructureResult::Blocked { .. }));
    }

    /// The property ADR-0077 exists to protect: a folder granted
    /// `sheets-structure` before deletion existed must not silently gain the
    /// power to destroy data now that it does.
    #[tokio::test]
    async fn a_sheets_structure_rule_alone_does_not_permit_deletion() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        let rules = [allow_rule("parent-1")]; // grants SheetsStructure only
        for verb in [delete_sheet(), delete_rows(), delete_range()] {
            let outcome = structure(&drive, &sheets, &opts(verb, false), &rules).await;
            assert!(matches!(outcome.result, StructureResult::Blocked { .. }));
        }
    }

    /// The converse: `sheets-delete` must not grant the additive verbs
    /// either — the two operations are siblings, not a hierarchy.
    #[tokio::test]
    async fn a_sheets_delete_rule_alone_does_not_permit_a_structural_edit() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        let rules = [delete_allow_rule("parent-1")];
        let outcome = structure(&drive, &sheets, &opts(rename(), false), &rules).await;
        assert!(matches!(outcome.result, StructureResult::Blocked { .. }));
    }

    #[tokio::test]
    async fn an_explicit_deny_is_reported_with_the_deciding_rule() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        let rules = [FolderPermissionRule {
            folder_id: Some("parent-1".to_string()),
            file_id: None,
            recursive: true,
            allow: HashSet::default(),
            deny: std::iter::once(DriveOperation::SheetsStructure).collect(),
        }];
        let outcome = structure(&drive, &sheets, &opts(rename(), false), &rules).await;
        let StructureResult::Blocked { decided_by } = &outcome.result else {
            panic!("expected Blocked, got {:?}", outcome.result);
        };
        let rule = decided_by.as_ref().expect("an explicit rule decided this");
        assert_eq!(rule.kind_label(), "folder");
        assert_eq!(rule.id(), "parent-1");
        assert_eq!(rule.depth_suffix(), " (depth 0)");
    }

    #[tokio::test]
    async fn ancestor_chain_fetch_failure_produces_failed_not_allow() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/parent-1"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(rename(), false),
            &[allow_rule("parent-1")],
        )
        .await;
        assert!(matches!(outcome.result, StructureResult::Failed { .. }));
    }

    // ── --dry-run ──────────────────────────────────────────────────────

    /// A dry run must never issue `batchUpdate`.
    ///
    /// It *does* issue one `spreadsheets.get` — deliberately, and unlike
    /// `sheets write`'s dry run, which touches Sheets not at all. Describing
    /// a structural effect honestly needs the sheet's real dimensions, and a
    /// read is not a mutation. The absent `batchUpdate` mock is what proves
    /// the distinction: wiremock fails an unmatched request.
    #[tokio::test]
    async fn dry_run_reads_the_workbook_but_never_calls_batch_update() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().expect(1).mount(&server).await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(insert_rows(), true),
            &[allow_rule("parent-1")],
        )
        .await;
        assert!(matches!(
            outcome.result,
            StructureResult::WouldChange { .. }
        ));
    }

    #[tokio::test]
    async fn dry_run_names_the_resulting_dimensions_and_the_shift() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(insert_rows(), true),
            &[allow_rule("parent-1")],
        )
        .await;
        let text = describe(&outcome);
        // The answer to "what does --dry-run show when the effect isn't a
        // range": the real before/after, plus the shift a range can't express.
        assert!(
            text.contains("Would insert 3 row(s) before row 5"),
            "{text}"
        );
        assert!(text.contains("500 rows -> 503"), "{text}");
        assert!(text.contains("shift down"), "{text}");
        assert!(text.contains("sheetId 118293"), "{text}");
    }

    /// A destructive dry run must never issue `batchUpdate` either, and stays
    /// structural-only (ADR-0077): no `values.get`, no cell content, just the
    /// same single `spreadsheets.get` an additive dry run already issues.
    #[tokio::test]
    async fn dry_run_never_calls_batch_update_for_a_delete_verb() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        for verb in [delete_sheet(), delete_rows(), delete_range()] {
            let outcome = structure(
                &drive,
                &sheets,
                &opts(verb, true),
                &[delete_allow_rule("parent-1")],
            )
            .await;
            assert!(matches!(
                outcome.result,
                StructureResult::WouldChange { .. }
            ));
        }
    }

    #[tokio::test]
    async fn dry_run_names_the_deleted_dimensions_and_the_shift_plus_the_caveat() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(delete_rows(), true),
            &[delete_allow_rule("parent-1")],
        )
        .await;
        let text = describe(&outcome);
        assert!(text.contains("Would delete 3 row(s) 5-7"), "{text}");
        assert!(text.contains("500 rows -> 497"), "{text}");
        assert!(text.contains("shift up"), "{text}");
        assert!(
            text.contains("formulas elsewhere in the workbook"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn dry_run_names_the_deleted_sheet_and_the_caveat() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(delete_sheet(), true),
            &[delete_allow_rule("parent-1")],
        )
        .await;
        let text = describe(&outcome);
        assert!(text.contains("Would delete sheet 'Q2'"), "{text}");
        assert!(text.contains("2 sheet(s) -> 1"), "{text}");
        assert!(
            text.contains("formulas elsewhere in the workbook"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn dry_run_names_the_deleted_range_and_the_shift() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(delete_range(), true),
            &[delete_allow_rule("parent-1")],
        )
        .await;
        let text = describe(&outcome);
        assert!(
            text.contains("Would delete rows 2-4, columns 2-3"),
            "{text}"
        );
        assert!(text.contains("shifting remaining cells up"), "{text}");
    }

    #[tokio::test]
    async fn dry_run_surfaces_the_same_blocked_reasoning_as_a_real_denied_run() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        // The gate precedes the dry-run early return, so a preview of a
        // denied edit reports the denial rather than a rosy preview.
        let dry = structure(&drive, &sheets, &opts(rename(), true), &[]).await;
        let wet = structure(&drive, &sheets, &opts(rename(), false), &[]).await;
        assert_eq!(dry.result, wet.result);
        assert!(matches!(dry.result, StructureResult::Blocked { .. }));
    }

    // ── resolving the target sheet ─────────────────────────────────────

    #[tokio::test]
    async fn a_missing_sheet_is_refused_and_lists_what_does_exist() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        let verb = StructureVerb::RenameSheet {
            sheet: "Nope".to_string(),
            new_title: "Q3".to_string(),
        };
        let outcome = structure(
            &drive,
            &sheets,
            &opts(verb.clone(), false),
            &[allow_rule("parent-1")],
        )
        .await;
        assert!(matches!(
            outcome.result,
            StructureResult::RefusedSheetNotFound { .. }
        ));
        let text = describe(&outcome);
        assert!(text.contains("no sheet titled 'Nope'"), "{text}");
        assert!(text.contains("'Q1'"), "{text}");
        assert!(text.contains("'Q2'"), "{text}");
    }

    /// A duplicate title is caught before `batchUpdate` rather than left to
    /// the server, so a dry run cannot promise a change the real run fails.
    #[tokio::test]
    async fn add_sheet_refuses_a_title_the_workbook_already_uses() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        let verb = StructureVerb::AddSheet {
            title: "Q1".to_string(),
            index: None,
            rows: None,
            columns: None,
        };
        // No batchUpdate mock: the refusal must short-circuit the mutation.
        let outcome = structure(
            &drive,
            &sheets,
            &opts(verb.clone(), false),
            &[allow_rule("parent-1")],
        )
        .await;
        assert!(matches!(
            outcome.result,
            StructureResult::RefusedSheetExists { .. }
        ));
        assert!(describe(&outcome).contains("already has a sheet titled 'Q1'"));
    }

    /// The rename analogue of the `add-sheet` test above: renaming to a
    /// title a *different* sheet already has must be caught before
    /// `batchUpdate`, not left to a dry run that then can't be trusted.
    #[tokio::test]
    async fn rename_sheet_refuses_a_title_another_sheet_already_uses() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        let verb = StructureVerb::RenameSheet {
            sheet: "Q2".to_string(),
            new_title: "Q1".to_string(),
        };
        // No batchUpdate mock: the refusal must short-circuit the mutation,
        // and the same must hold for a --dry-run preview of it.
        let outcome = structure(
            &drive,
            &sheets,
            &opts(verb.clone(), false),
            &[allow_rule("parent-1")],
        )
        .await;
        assert!(matches!(
            outcome.result,
            StructureResult::RefusedSheetExists { .. }
        ));
        assert!(describe(&outcome).contains("already has a sheet titled 'Q1'"));

        let dry = structure(
            &drive,
            &sheets,
            &opts(verb.clone(), true),
            &[allow_rule("parent-1")],
        )
        .await;
        assert_eq!(dry.result, outcome.result);
    }

    /// Renaming a sheet to the title it already has names itself, not a
    /// different sheet, so it must not be refused as a duplicate.
    #[tokio::test]
    async fn rename_sheet_to_its_own_current_title_is_not_a_duplicate() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        mount_batch_update(serde_json::json!({"spreadsheetId": "sheet-1", "replies": [{}]}))
            .mount(&server)
            .await;
        let verb = StructureVerb::RenameSheet {
            sheet: "Q2".to_string(),
            new_title: "Q2".to_string(),
        };
        let outcome = structure(
            &drive,
            &sheets,
            &opts(verb, false),
            &[allow_rule("parent-1")],
        )
        .await;
        assert!(matches!(outcome.result, StructureResult::Changed { .. }));
    }

    // ── argument validation ──────────────────────────────────────────────

    #[tokio::test]
    async fn insert_rows_refuses_a_count_below_one() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        let verb = StructureVerb::InsertRows {
            sheet: "Q2".to_string(),
            at: 5,
            count: 0,
        };
        // No batchUpdate mock: the refusal must short-circuit the mutation.
        let outcome = structure(
            &drive,
            &sheets,
            &opts(verb.clone(), false),
            &[allow_rule("parent-1")],
        )
        .await;
        let StructureResult::RefusedInvalidRange { detail } = &outcome.result else {
            panic!("expected RefusedInvalidRange, got {:?}", outcome.result);
        };
        assert!(detail.contains("--count"), "{detail}");
        assert!(describe(&outcome).starts_with("Refused: --count"));
    }

    #[tokio::test]
    async fn insert_columns_refuses_an_at_below_one() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        let verb = StructureVerb::InsertColumns {
            sheet: "Q2".to_string(),
            at: 0,
            count: 1,
        };
        let outcome = structure(
            &drive,
            &sheets,
            &opts(verb, false),
            &[allow_rule("parent-1")],
        )
        .await;
        let StructureResult::RefusedInvalidRange { detail } = &outcome.result else {
            panic!("expected RefusedInvalidRange, got {:?}", outcome.result);
        };
        assert!(detail.contains("--at"), "{detail}");
    }

    /// `Q2` has 500 rows (from `mount_workbook`); `--at 502` names a
    /// position that does not exist and is not immediately after one that
    /// does, so it must be refused rather than sent to `batchUpdate` or
    /// promised by a `--dry-run`.
    #[tokio::test]
    async fn insert_rows_refuses_an_at_past_the_sheets_end() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        let verb = StructureVerb::InsertRows {
            sheet: "Q2".to_string(),
            at: 502,
            count: 1,
        };
        let outcome = structure(
            &drive,
            &sheets,
            &opts(verb.clone(), false),
            &[allow_rule("parent-1")],
        )
        .await;
        let StructureResult::RefusedInvalidRange { detail } = &outcome.result else {
            panic!("expected RefusedInvalidRange, got {:?}", outcome.result);
        };
        assert!(detail.contains("500 row(s)"), "{detail}");
        assert!(detail.contains("501"), "{detail}");

        let dry = structure(
            &drive,
            &sheets,
            &opts(verb, true),
            &[allow_rule("parent-1")],
        )
        .await;
        assert_eq!(dry.result, outcome.result);
    }

    #[test]
    fn an_at_and_count_that_would_overflow_the_index_space_are_refused() {
        let sheet = SheetSnapshot {
            sheet_id: Some(1),
            title: "Q2".to_string(),
            row_count: Some(500),
            column_count: Some(26),
        };
        // Both halves of `at - 1 + count`, since either can be the one that
        // overflows: a huge `--at` (refused for being past the end too, but
        // only *after* this check) and a huge `--count` at a legal `--at`
        // (which nothing else bounds — see the comment on the guard).
        for (at, count) in [(i64::MAX, 5), (500, i64::MAX)] {
            let Err(StructureResult::RefusedInvalidRange { detail }) =
                validate_insert_bounds(Dimension::Rows, at, count, Some(&sheet))
            else {
                panic!("expected RefusedInvalidRange for --at {at} --count {count}");
            };
            assert!(detail.contains("overflows the row index space"), "{detail}");
        }
    }

    #[tokio::test]
    async fn a_refused_out_of_range_insert_still_records_without_panicking() {
        // The regression that matters for `dimension_range_label`: this runs
        // with `dry_run: false`, so `record_attempt` builds a log record from
        // the very arguments `validate_insert_bounds` just rejected. Computing
        // the span with plain arithmetic panics here rather than in the
        // mutation it refused to make.
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;

        let outcome = structure(
            &drive,
            &sheets,
            &opts(
                StructureVerb::InsertRows {
                    sheet: "Q2".to_string(),
                    at: i64::MAX,
                    count: 5,
                },
                false,
            ),
            &[allow_rule("parent-1")],
        )
        .await;
        assert!(
            matches!(outcome.result, StructureResult::RefusedInvalidRange { .. }),
            "{:?}",
            outcome.result
        );
    }

    /// `--at` equal to `row_count + 1` is a legal append (insert after the
    /// last row), the one boundary value the check above must not reject —
    /// and the dry-run message must not print an inverted "existing rows
    /// 501-500 shift down".
    #[tokio::test]
    async fn insert_rows_allows_at_equal_to_row_count_plus_one_as_an_append() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        let verb = StructureVerb::InsertRows {
            sheet: "Q2".to_string(),
            at: 501,
            count: 2,
        };
        let outcome = structure(
            &drive,
            &sheets,
            &opts(verb.clone(), true),
            &[allow_rule("parent-1")],
        )
        .await;
        assert!(matches!(
            outcome.result,
            StructureResult::WouldChange { .. }
        ));
        let text = describe(&outcome);
        assert!(text.contains("500 rows -> 502"), "{text}");
        assert!(text.contains("appended at the end"), "{text}");
        assert!(!text.contains("501-500"), "{text}");
    }

    #[tokio::test]
    async fn add_sheet_refuses_non_positive_rows_and_columns() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        let verb = StructureVerb::AddSheet {
            title: "Q3".to_string(),
            index: None,
            rows: Some(0),
            columns: None,
        };
        let outcome = structure(
            &drive,
            &sheets,
            &opts(verb, false),
            &[allow_rule("parent-1")],
        )
        .await;
        let StructureResult::RefusedInvalidRange { detail } = &outcome.result else {
            panic!("expected RefusedInvalidRange, got {:?}", outcome.result);
        };
        assert!(detail.contains("--rows"), "{detail}");
    }

    /// `mount_workbook` has 2 sheets, so 0/1/2 are the only valid indices
    /// (2 means "append", the same as omitting `--index`).
    #[tokio::test]
    async fn add_sheet_refuses_an_out_of_range_index() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        let verb = StructureVerb::AddSheet {
            title: "Q3".to_string(),
            index: Some(3),
            rows: None,
            columns: None,
        };
        let outcome = structure(
            &drive,
            &sheets,
            &opts(verb, false),
            &[allow_rule("parent-1")],
        )
        .await;
        let StructureResult::RefusedInvalidRange { detail } = &outcome.result else {
            panic!("expected RefusedInvalidRange, got {:?}", outcome.result);
        };
        assert!(detail.contains("--index"), "{detail}");
    }

    #[tokio::test]
    async fn add_sheet_allows_an_index_equal_to_the_sheet_count() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        let verb = StructureVerb::AddSheet {
            title: "Q3".to_string(),
            index: Some(2),
            rows: None,
            columns: None,
        };
        let outcome = structure(
            &drive,
            &sheets,
            &opts(verb, true),
            &[allow_rule("parent-1")],
        )
        .await;
        assert!(matches!(
            outcome.result,
            StructureResult::WouldChange { .. }
        ));
    }

    // ── the mutation ───────────────────────────────────────────────────

    #[tokio::test]
    async fn rename_sends_a_title_masked_update_for_the_resolved_sheet_id() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        mount_batch_update(serde_json::json!({"spreadsheetId": "sheet-1", "replies": [{}]}))
            .expect(1)
            .mount(&server)
            .await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(rename(), false),
            &[allow_rule("parent-1")],
        )
        .await;
        assert!(matches!(outcome.result, StructureResult::Changed { .. }));

        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = requests
            .iter()
            .find(|r| r.url.path().ends_with(":batchUpdate"))
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .expect("a batchUpdate request");
        let update = &body["requests"][0]["updateSheetProperties"];
        // Resolved by title to the numeric id the API addresses.
        assert_eq!(update["properties"]["sheetId"], 118_293);
        assert_eq!(update["properties"]["title"], "Q3");
        // The mask names exactly the one field we set; a wider mask would
        // blank every property it named but we left unpopulated.
        assert_eq!(update["fields"], "title");
        assert_eq!(body["requests"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn insert_rows_sends_a_zero_based_half_open_range() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        mount_batch_update(serde_json::json!({"spreadsheetId": "sheet-1", "replies": [{}]}))
            .mount(&server)
            .await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(insert_rows(), false),
            &[allow_rule("parent-1")],
        )
        .await;
        assert!(matches!(outcome.result, StructureResult::Changed { .. }));

        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = requests
            .iter()
            .find(|r| r.url.path().ends_with(":batchUpdate"))
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .expect("a batchUpdate request");
        let insert = &body["requests"][0]["insertDimension"];
        assert_eq!(insert["range"]["dimension"], "ROWS");
        assert_eq!(insert["range"]["sheetId"], 118_293);
        // `--at 5 --count 3` on the wire, 1-based inclusive to 0-based
        // half-open.
        assert_eq!(insert["range"]["startIndex"], 4);
        assert_eq!(insert["range"]["endIndex"], 7);
        assert_eq!(insert["inheritFromBefore"], false);
    }

    #[tokio::test]
    async fn delete_sheet_sends_the_resolved_sheet_id() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        mount_batch_update(serde_json::json!({"spreadsheetId": "sheet-1", "replies": [{}]}))
            .expect(1)
            .mount(&server)
            .await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(delete_sheet(), false),
            &[delete_allow_rule("parent-1")],
        )
        .await;
        assert!(matches!(outcome.result, StructureResult::Changed { .. }));
        assert!(describe(&outcome).contains("Deleted sheet 'Q2'"));
        assert!(describe(&outcome).contains("Google Drive's version history"));

        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = requests
            .iter()
            .find(|r| r.url.path().ends_with(":batchUpdate"))
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .expect("a batchUpdate request");
        assert_eq!(body["requests"][0]["deleteSheet"]["sheetId"], 118_293);
        assert_eq!(body["requests"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn delete_rows_sends_a_zero_based_half_open_range() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        mount_batch_update(serde_json::json!({"spreadsheetId": "sheet-1", "replies": [{}]}))
            .mount(&server)
            .await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(delete_rows(), false),
            &[delete_allow_rule("parent-1")],
        )
        .await;
        assert!(matches!(outcome.result, StructureResult::Changed { .. }));

        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = requests
            .iter()
            .find(|r| r.url.path().ends_with(":batchUpdate"))
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .expect("a batchUpdate request");
        let delete = &body["requests"][0]["deleteDimension"];
        assert_eq!(delete["range"]["dimension"], "ROWS");
        assert_eq!(delete["range"]["sheetId"], 118_293);
        // `--at 5 --count 3` on the wire, 1-based inclusive to 0-based
        // half-open — same conversion as insert, same site.
        assert_eq!(delete["range"]["startIndex"], 4);
        assert_eq!(delete["range"]["endIndex"], 7);
    }

    #[tokio::test]
    async fn delete_columns_sends_a_zero_based_half_open_range() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        mount_batch_update(serde_json::json!({"spreadsheetId": "sheet-1", "replies": [{}]}))
            .mount(&server)
            .await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(delete_columns(), false),
            &[delete_allow_rule("parent-1")],
        )
        .await;
        assert!(matches!(outcome.result, StructureResult::Changed { .. }));
        assert!(describe(&outcome).contains("Deleted 2 column(s) 3-4"));

        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = requests
            .iter()
            .find(|r| r.url.path().ends_with(":batchUpdate"))
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .expect("a batchUpdate request");
        let delete = &body["requests"][0]["deleteDimension"];
        assert_eq!(delete["range"]["dimension"], "COLUMNS");
        assert_eq!(delete["range"]["sheetId"], 118_293);
        // `--at 3 --count 2` on the wire, 1-based inclusive to 0-based
        // half-open.
        assert_eq!(delete["range"]["startIndex"], 2);
        assert_eq!(delete["range"]["endIndex"], 4);
    }

    #[tokio::test]
    async fn delete_range_sends_a_zero_based_half_open_grid_range() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        mount_batch_update(serde_json::json!({"spreadsheetId": "sheet-1", "replies": [{}]}))
            .mount(&server)
            .await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(delete_range(), false),
            &[delete_allow_rule("parent-1")],
        )
        .await;
        assert!(matches!(outcome.result, StructureResult::Changed { .. }));

        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = requests
            .iter()
            .find(|r| r.url.path().ends_with(":batchUpdate"))
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .expect("a batchUpdate request");
        let delete = &body["requests"][0]["deleteRange"];
        assert_eq!(delete["range"]["sheetId"], 118_293);
        // `--start-row 2 --end-row 4 --start-column 2 --end-column 3` on the
        // wire, 1-based inclusive to 0-based half-open.
        assert_eq!(delete["range"]["startRowIndex"], 1);
        assert_eq!(delete["range"]["endRowIndex"], 4);
        assert_eq!(delete["range"]["startColumnIndex"], 1);
        assert_eq!(delete["range"]["endColumnIndex"], 3);
        assert_eq!(delete["shiftDimension"], "ROWS");
    }

    #[tokio::test]
    async fn delete_range_with_columns_shift_sends_the_column_direction() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        mount_batch_update(serde_json::json!({"spreadsheetId": "sheet-1", "replies": [{}]}))
            .mount(&server)
            .await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(delete_range_columns_shift(), false),
            &[delete_allow_rule("parent-1")],
        )
        .await;
        assert!(matches!(outcome.result, StructureResult::Changed { .. }));
        assert!(describe(&outcome).contains("shifted remaining cells left"));

        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = requests
            .iter()
            .find(|r| r.url.path().ends_with(":batchUpdate"))
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .expect("a batchUpdate request");
        assert_eq!(
            body["requests"][0]["deleteRange"]["shiftDimension"],
            "COLUMNS"
        );
    }

    #[tokio::test]
    async fn delete_dimension_past_the_end_of_the_sheet_is_refused() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        // Q2 has 500 rows; deleting 5 rows starting at 499 reaches row 503.
        let outcome = structure(
            &drive,
            &sheets,
            &opts(
                StructureVerb::DeleteRows {
                    sheet: "Q2".to_string(),
                    at: 499,
                    count: 5,
                },
                true,
            ),
            &[delete_allow_rule("parent-1")],
        )
        .await;
        assert!(matches!(
            outcome.result,
            StructureResult::RefusedInvalidRange { .. }
        ));
    }

    #[tokio::test]
    async fn delete_range_with_end_before_start_is_refused() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(
                StructureVerb::DeleteRange {
                    sheet: "Q2".to_string(),
                    start_row: 5,
                    end_row: 2,
                    start_column: 1,
                    end_column: 2,
                    shift: ShiftDimension::Rows,
                },
                true,
            ),
            &[delete_allow_rule("parent-1")],
        )
        .await;
        assert!(matches!(
            outcome.result,
            StructureResult::RefusedInvalidRange { .. }
        ));
    }

    #[tokio::test]
    async fn delete_dimension_refuses_a_non_positive_count() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(
                StructureVerb::DeleteRows {
                    sheet: "Q2".to_string(),
                    at: 1,
                    count: 0,
                },
                true,
            ),
            &[delete_allow_rule("parent-1")],
        )
        .await;
        let StructureResult::RefusedInvalidRange { detail } = &outcome.result else {
            panic!("expected RefusedInvalidRange, got {:?}", outcome.result);
        };
        assert!(detail.contains("--count must be at least 1"), "{detail}");
    }

    #[tokio::test]
    async fn delete_dimension_refuses_an_at_below_one() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(
                StructureVerb::DeleteColumns {
                    sheet: "Q2".to_string(),
                    at: 0,
                    count: 1,
                },
                true,
            ),
            &[delete_allow_rule("parent-1")],
        )
        .await;
        let StructureResult::RefusedInvalidRange { detail } = &outcome.result else {
            panic!("expected RefusedInvalidRange, got {:?}", outcome.result);
        };
        assert!(detail.contains("--at must be at least 1"), "{detail}");
    }

    #[tokio::test]
    async fn delete_dimension_refuses_an_at_and_count_that_overflow_the_index_space() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(
                StructureVerb::DeleteRows {
                    sheet: "Q2".to_string(),
                    at: i64::MAX,
                    count: 2,
                },
                true,
            ),
            &[delete_allow_rule("parent-1")],
        )
        .await;
        let StructureResult::RefusedInvalidRange { detail } = &outcome.result else {
            panic!("expected RefusedInvalidRange, got {:?}", outcome.result);
        };
        assert!(detail.contains("overflows the row index space"), "{detail}");
    }

    #[tokio::test]
    async fn delete_range_refuses_a_start_row_below_one() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(
                StructureVerb::DeleteRange {
                    sheet: "Q2".to_string(),
                    start_row: 0,
                    end_row: 2,
                    start_column: 1,
                    end_column: 2,
                    shift: ShiftDimension::Rows,
                },
                true,
            ),
            &[delete_allow_rule("parent-1")],
        )
        .await;
        let StructureResult::RefusedInvalidRange { detail } = &outcome.result else {
            panic!("expected RefusedInvalidRange, got {:?}", outcome.result);
        };
        assert!(
            detail.contains("--start-row must be at least 1"),
            "{detail}"
        );
    }

    #[tokio::test]
    async fn delete_range_refuses_a_start_column_below_one() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(
                StructureVerb::DeleteRange {
                    sheet: "Q2".to_string(),
                    start_row: 1,
                    end_row: 2,
                    start_column: 0,
                    end_column: 2,
                    shift: ShiftDimension::Rows,
                },
                true,
            ),
            &[delete_allow_rule("parent-1")],
        )
        .await;
        let StructureResult::RefusedInvalidRange { detail } = &outcome.result else {
            panic!("expected RefusedInvalidRange, got {:?}", outcome.result);
        };
        assert!(
            detail.contains("--start-column must be at least 1"),
            "{detail}"
        );
    }

    #[tokio::test]
    async fn delete_range_refuses_an_end_column_before_start_column() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        // Rows are well-ordered on their own; only the columns are inverted,
        // so this is refused for a different reason than
        // `delete_range_with_end_before_start_is_refused`.
        let outcome = structure(
            &drive,
            &sheets,
            &opts(
                StructureVerb::DeleteRange {
                    sheet: "Q2".to_string(),
                    start_row: 1,
                    end_row: 2,
                    start_column: 5,
                    end_column: 2,
                    shift: ShiftDimension::Rows,
                },
                true,
            ),
            &[delete_allow_rule("parent-1")],
        )
        .await;
        let StructureResult::RefusedInvalidRange { detail } = &outcome.result else {
            panic!("expected RefusedInvalidRange, got {:?}", outcome.result);
        };
        assert!(
            detail.contains("--end-column (2) must be at or after --start-column (5)"),
            "{detail}"
        );
    }

    #[tokio::test]
    async fn delete_range_refuses_an_end_row_past_the_end_of_the_sheet() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        // Q2 has 500 rows.
        let outcome = structure(
            &drive,
            &sheets,
            &opts(
                StructureVerb::DeleteRange {
                    sheet: "Q2".to_string(),
                    start_row: 1,
                    end_row: 501,
                    start_column: 1,
                    end_column: 2,
                    shift: ShiftDimension::Rows,
                },
                true,
            ),
            &[delete_allow_rule("parent-1")],
        )
        .await;
        let StructureResult::RefusedInvalidRange { detail } = &outcome.result else {
            panic!("expected RefusedInvalidRange, got {:?}", outcome.result);
        };
        assert!(
            detail.contains("--end-row 501 is past the end of the sheet"),
            "{detail}"
        );
    }

    #[tokio::test]
    async fn delete_range_refuses_an_end_column_past_the_end_of_the_sheet() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        // Q2 has 10 columns.
        let outcome = structure(
            &drive,
            &sheets,
            &opts(
                StructureVerb::DeleteRange {
                    sheet: "Q2".to_string(),
                    start_row: 1,
                    end_row: 2,
                    start_column: 1,
                    end_column: 11,
                    shift: ShiftDimension::Rows,
                },
                true,
            ),
            &[delete_allow_rule("parent-1")],
        )
        .await;
        let StructureResult::RefusedInvalidRange { detail } = &outcome.result else {
            panic!("expected RefusedInvalidRange, got {:?}", outcome.result);
        };
        assert!(
            detail.contains("--end-column 11 is past the end of the sheet"),
            "{detail}"
        );
    }

    #[tokio::test]
    async fn add_sheet_reports_the_server_assigned_sheet_id() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        mount_batch_update(serde_json::json!({
            "spreadsheetId": "sheet-1",
            "replies": [{"addSheet": {"properties": {"sheetId": 999, "title": "Q3"}}}],
        }))
        .mount(&server)
        .await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(add_sheet(), false),
            &[allow_rule("parent-1")],
        )
        .await;
        // The one fact the reply carries that the request did not know.
        assert!(matches!(
            outcome.result,
            StructureResult::Changed {
                sheet_id: Some(999),
                ..
            }
        ));
        assert!(describe(&outcome).contains("sheetId 999"));
    }

    #[tokio::test]
    async fn add_sheet_omits_grid_properties_when_no_size_was_asked_for() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        mount_batch_update(serde_json::json!({"spreadsheetId": "sheet-1", "replies": [{}]}))
            .mount(&server)
            .await;
        let _ = structure(
            &drive,
            &sheets,
            &opts(add_sheet(), false),
            &[allow_rule("parent-1")],
        )
        .await;
        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = requests
            .iter()
            .find(|r| r.url.path().ends_with(":batchUpdate"))
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .expect("a batchUpdate request");
        let props = &body["requests"][0]["addSheet"]["properties"];
        // Omitted, not zeroed: Sheets' own default (1000 x 26) should apply
        // rather than a size we invented.
        assert!(props.get("gridProperties").is_none(), "{props}");
        assert!(props.get("index").is_none(), "{props}");
        assert_eq!(props["title"], "Q3");
    }

    #[tokio::test]
    async fn a_batch_update_failure_is_reported_as_failed_not_changed() {
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
            .respond_with(
                wiremock::ResponseTemplate::new(403).set_body_json(serde_json::json!({
                    "error": {
                        "code": 403,
                        "message": "The caller does not have permission",
                        "status": "PERMISSION_DENIED",
                    }
                })),
            )
            .mount(&server)
            .await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(rename(), false),
            &[allow_rule("parent-1")],
        )
        .await;
        let StructureResult::Failed { detail } = &outcome.result else {
            panic!("expected Failed, got {:?}", outcome.result);
        };
        // The `google.rpc` envelope has no `errors[]`, so the scope hint has
        // to match on `status` — ADR-0073 §2.
        assert!(detail.contains("drive auth login"), "{detail}");
    }

    // ── duplicate-sheet / reorder-sheet / hide-show-sheet (issue #1643) ─

    #[tokio::test]
    async fn duplicate_sheet_apply_sends_source_title_and_index() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        mount_batch_update(serde_json::json!({"spreadsheetId": "sheet-1", "replies": [{}]}))
            .mount(&server)
            .await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(duplicate_sheet(), false),
            &[allow_rule("parent-1")],
        )
        .await;
        assert!(matches!(outcome.result, StructureResult::Changed { .. }));

        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = requests
            .iter()
            .find(|r| r.url.path().ends_with(":batchUpdate"))
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .expect("a batchUpdate request");
        let dup = &body["requests"][0]["duplicateSheet"];
        assert_eq!(dup["sourceSheetId"], 118_293);
        assert_eq!(dup["insertSheetIndex"], 1);
        assert_eq!(dup["newSheetName"], "Q2 Copy");
    }

    #[tokio::test]
    async fn duplicate_sheet_apply_with_default_title_and_index() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        mount_batch_update(serde_json::json!({"spreadsheetId": "sheet-1", "replies": [{}]}))
            .mount(&server)
            .await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(duplicate_sheet_default(), false),
            &[allow_rule("parent-1")],
        )
        .await;
        assert!(matches!(outcome.result, StructureResult::Changed { .. }));

        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = requests
            .iter()
            .find(|r| r.url.path().ends_with(":batchUpdate"))
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .expect("a batchUpdate request");
        let dup = &body["requests"][0]["duplicateSheet"];
        assert!(dup.get("insertSheetIndex").is_none());
        assert!(dup.get("newSheetName").is_none());
    }

    #[tokio::test]
    async fn duplicate_sheet_dry_run_names_title_and_index() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(duplicate_sheet(), true),
            &[allow_rule("parent-1")],
        )
        .await;
        let text = describe(&outcome);
        assert!(text.contains("Would duplicate sheet 'Q2'"), "{text}");
        assert!(text.contains("as 'Q2 Copy'"), "{text}");
        assert!(text.contains("at index 1"), "{text}");
        let requests = server.received_requests().await.unwrap();
        assert!(!requests
            .iter()
            .any(|r| r.url.path().ends_with(":batchUpdate")));
    }

    #[tokio::test]
    async fn duplicate_sheet_dry_run_uses_defaults_when_omitted() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(duplicate_sheet_default(), true),
            &[allow_rule("parent-1")],
        )
        .await;
        let text = describe(&outcome);
        assert!(
            text.contains("as a copy Sheets names automatically"),
            "{text}"
        );
        assert!(text.contains("at the front"), "{text}");
    }

    #[tokio::test]
    async fn duplicate_sheet_refuses_a_missing_source_sheet() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(
                StructureVerb::DuplicateSheet {
                    sheet: "Nope".to_string(),
                    title: None,
                    index: None,
                },
                false,
            ),
            &[allow_rule("parent-1")],
        )
        .await;
        assert!(matches!(
            outcome.result,
            StructureResult::RefusedSheetNotFound { .. }
        ));
    }

    #[tokio::test]
    async fn duplicate_sheet_refuses_a_colliding_new_title() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(
                StructureVerb::DuplicateSheet {
                    sheet: "Q2".to_string(),
                    title: Some("Q1".to_string()),
                    index: None,
                },
                false,
            ),
            &[allow_rule("parent-1")],
        )
        .await;
        assert!(matches!(
            outcome.result,
            StructureResult::RefusedSheetExists { ref title } if title == "Q1"
        ));
    }

    #[tokio::test]
    async fn duplicate_sheet_refuses_an_out_of_range_index() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(
                StructureVerb::DuplicateSheet {
                    sheet: "Q2".to_string(),
                    title: None,
                    index: Some(99),
                },
                false,
            ),
            &[allow_rule("parent-1")],
        )
        .await;
        let StructureResult::RefusedInvalidRange { detail } = &outcome.result else {
            panic!("expected RefusedInvalidRange, got {:?}", outcome.result);
        };
        assert!(
            detail.contains("--index must be between 0 and 2"),
            "{detail}"
        );
    }

    #[tokio::test]
    async fn reorder_sheet_apply_sends_index_masked_update() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        mount_batch_update(serde_json::json!({"spreadsheetId": "sheet-1", "replies": [{}]}))
            .mount(&server)
            .await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(reorder_sheet(), false),
            &[allow_rule("parent-1")],
        )
        .await;
        assert!(matches!(outcome.result, StructureResult::Changed { .. }));

        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = requests
            .iter()
            .find(|r| r.url.path().ends_with(":batchUpdate"))
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .expect("a batchUpdate request");
        let update = &body["requests"][0]["updateSheetProperties"];
        assert_eq!(update["properties"]["sheetId"], 118_293);
        assert_eq!(update["properties"]["index"], 0);
        assert_eq!(update["fields"], "index");
    }

    #[tokio::test]
    async fn reorder_sheet_dry_run_names_the_target_index() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(reorder_sheet(), true),
            &[allow_rule("parent-1")],
        )
        .await;
        let text = describe(&outcome);
        assert!(text.contains("Would move sheet 'Q2'"), "{text}");
        assert!(text.contains("to index 0"), "{text}");
    }

    #[tokio::test]
    async fn reorder_sheet_refuses_an_out_of_range_index() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(
                StructureVerb::ReorderSheet {
                    sheet: "Q2".to_string(),
                    index: 5,
                },
                false,
            ),
            &[allow_rule("parent-1")],
        )
        .await;
        let StructureResult::RefusedInvalidRange { detail } = &outcome.result else {
            panic!("expected RefusedInvalidRange, got {:?}", outcome.result);
        };
        assert!(
            detail.contains("--index must be between 0 and 1"),
            "{detail}"
        );
    }

    #[tokio::test]
    async fn hide_sheet_apply_sends_hidden_true() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        mount_batch_update(serde_json::json!({"spreadsheetId": "sheet-1", "replies": [{}]}))
            .mount(&server)
            .await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(hide_sheet(), false),
            &[allow_rule("parent-1")],
        )
        .await;
        assert!(matches!(outcome.result, StructureResult::Changed { .. }));

        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = requests
            .iter()
            .find(|r| r.url.path().ends_with(":batchUpdate"))
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .expect("a batchUpdate request");
        let update = &body["requests"][0]["updateSheetProperties"];
        assert_eq!(update["properties"]["sheetId"], 118_293);
        assert_eq!(update["properties"]["hidden"], true);
        assert_eq!(update["fields"], "hidden");
    }

    #[tokio::test]
    async fn show_sheet_apply_sends_hidden_false() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        mount_batch_update(serde_json::json!({"spreadsheetId": "sheet-1", "replies": [{}]}))
            .mount(&server)
            .await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(show_sheet(), false),
            &[allow_rule("parent-1")],
        )
        .await;
        assert!(matches!(outcome.result, StructureResult::Changed { .. }));

        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = requests
            .iter()
            .find(|r| r.url.path().ends_with(":batchUpdate"))
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .expect("a batchUpdate request");
        let update = &body["requests"][0]["updateSheetProperties"];
        assert_eq!(update["properties"]["hidden"], false);
    }

    #[tokio::test]
    async fn hide_sheet_dry_run_names_the_hide() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(hide_sheet(), true),
            &[allow_rule("parent-1")],
        )
        .await;
        let text = describe(&outcome);
        assert!(text.contains("Would hide sheet 'Q2'"), "{text}");
    }

    #[tokio::test]
    async fn show_sheet_dry_run_names_the_show() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(show_sheet(), true),
            &[allow_rule("parent-1")],
        )
        .await;
        let text = describe(&outcome);
        assert!(text.contains("Would show sheet 'Q2'"), "{text}");
    }

    #[tokio::test]
    async fn hide_sheet_refuses_leaving_no_visible_sheets() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/sheet-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "spreadsheetId": "sheet-1",
                    "properties": {"title": "Budget"},
                    "sheets": [
                        {"properties": {"sheetId": 0, "title": "Q1", "index": 0, "hidden": true}},
                        {"properties": {"sheetId": 118_293, "title": "Q2", "index": 1}},
                    ],
                })),
            )
            .mount(&server)
            .await;
        let outcome = structure(
            &drive,
            &sheets,
            &opts(hide_sheet(), false),
            &[allow_rule("parent-1")],
        )
        .await;
        let StructureResult::RefusedInvalidRange { detail } = &outcome.result else {
            panic!("expected RefusedInvalidRange, got {:?}", outcome.result);
        };
        assert!(
            detail.contains("would leave the workbook with no visible sheets"),
            "{detail}"
        );
    }

    // ── plumbing ───────────────────────────────────────────────────────

    #[test]
    fn log_operation_is_distinct_per_verb() {
        let verbs = [
            add_sheet(),
            rename(),
            insert_rows(),
            StructureVerb::InsertColumns {
                sheet: "Q2".to_string(),
                at: 1,
                count: 1,
            },
            delete_sheet(),
            delete_rows(),
            StructureVerb::DeleteColumns {
                sheet: "Q2".to_string(),
                at: 1,
                count: 1,
            },
            delete_range(),
            duplicate_sheet(),
            reorder_sheet(),
            hide_sheet(),
            show_sheet(),
        ];
        let names: Vec<&str> = verbs.iter().map(StructureVerb::log_operation).collect();
        assert_eq!(
            names,
            vec![
                "sheets-add-sheet",
                "sheets-rename-sheet",
                "sheets-insert-rows",
                "sheets-insert-columns",
                "sheets-delete-sheet",
                "sheets-delete-rows",
                "sheets-delete-columns",
                "sheets-delete-range",
                "sheets-duplicate-sheet",
                "sheets-reorder-sheet",
                "sheets-hide-sheet",
                "sheets-show-sheet",
            ]
        );
        // One record per user-visible verb means the operations must not
        // collide with each other or with the cell verbs.
        let unique: HashSet<&&str> = names.iter().collect();
        assert_eq!(unique.len(), names.len());
        assert!(!names.contains(&"sheets-write"));
    }

    /// One of every [`StructureResult`] variant.
    ///
    /// Exhaustive and wildcard-free on purpose, mirroring `write.rs`'s
    /// `every_write_result`: adding a variant breaks this build, which is
    /// what forces the new arm through the newline check below.
    fn every_structure_result() -> Vec<StructureResult> {
        let all = vec![
            StructureResult::WouldChange {
                sheet: Some(SheetSnapshot {
                    sheet_id: Some(7),
                    title: "Q2".to_string(),
                    row_count: Some(500),
                    column_count: Some(26),
                }),
                sheet_count: 2,
            },
            StructureResult::RefusedNotASpreadsheet {
                mime_type: "application/pdf".to_string(),
            },
            StructureResult::RefusedShortcut,
            StructureResult::RefusedNoVisibleParents,
            StructureResult::RefusedSheetNotFound {
                title: "Nope".to_string(),
                available: vec!["Q1".to_string(), "Q2".to_string()],
            },
            StructureResult::RefusedSheetExists {
                title: "Q1".to_string(),
            },
            StructureResult::RefusedInvalidRange {
                detail: "--count must be at least 1, got 0".to_string(),
            },
            StructureResult::Blocked { decided_by: None },
            StructureResult::Changed {
                sheet: Some(SheetSnapshot {
                    sheet_id: Some(7),
                    title: "Q2".to_string(),
                    row_count: Some(500),
                    column_count: Some(26),
                }),
                sheet_id: Some(7),
            },
            StructureResult::Failed {
                detail: "boom".to_string(),
            },
        ];
        // Compile-time exhaustiveness: a new variant fails to match here.
        for result in &all {
            match result {
                StructureResult::WouldChange { .. }
                | StructureResult::RefusedNotASpreadsheet { .. }
                | StructureResult::RefusedShortcut
                | StructureResult::RefusedNoVisibleParents
                | StructureResult::RefusedSheetNotFound { .. }
                | StructureResult::RefusedSheetExists { .. }
                | StructureResult::RefusedInvalidRange { .. }
                | StructureResult::Blocked { .. }
                | StructureResult::Changed { .. }
                | StructureResult::Failed { .. } => {}
            }
        }
        all
    }

    /// No `describe_lines` line ever contains a newline, and the insert
    /// preview is the only arm that emits a second line.
    ///
    /// Both halves are load-bearing, and they are different claims. The
    /// second is presentation: `write.rs`/`create.rs` pin every arm to one
    /// line, and a structural insert earns its extra one because the shift is
    /// the substance of the dry run (ADR-0075 §6).
    ///
    /// The first is what makes `cli::drive::sheets::structure`'s sanitizing
    /// sound. Every line here interpolates untrusted text — a Drive-supplied
    /// file name, workbook sheet titles, raw API error details — and the CLI
    /// strips control characters from each line before supplying the
    /// separators itself. That is only equivalent to filtering each
    /// interpolation while this module contributes no newline of its own: a
    /// line built with an embedded `\n` would smuggle a separator past the
    /// filter and let an injected one pass for a real one.
    ///
    /// The wildcard-free `every_structure_result` is the other half — a new
    /// variant will not compile until it is listed there, so it cannot reach
    /// the terminal without passing through this check.
    #[test]
    fn no_describe_line_contains_a_newline() {
        for verb in [
            add_sheet(),
            rename(),
            insert_rows(),
            StructureVerb::InsertColumns {
                sheet: "Q2".to_string(),
                at: 2,
                count: 1,
            },
            delete_sheet(),
            delete_rows(),
            delete_columns(),
            delete_range(),
            delete_range_columns_shift(),
            duplicate_sheet(),
            duplicate_sheet_default(),
            reorder_sheet(),
            hide_sheet(),
            show_sheet(),
        ] {
            // A verb with a single axis (insert or delete-dimension) earns a
            // second `WouldChange` line for the shift; every other verb,
            // additive or destructive, stays one line.
            let has_dimension_shift = verb.dimension().is_some();
            for result in every_structure_result() {
                let previews_an_insert =
                    has_dimension_shift && matches!(result, StructureResult::WouldChange { .. });
                let outcome = StructureOutcome {
                    spreadsheet_id: "sheet-1".to_string(),
                    file_name: Some("Budget".to_string()),
                    resolved_folder_id: None,
                    verb: verb.clone(),
                    result,
                };
                let lines = describe_lines(&outcome);
                assert_eq!(
                    lines.len(),
                    usize::from(previews_an_insert) + 1,
                    "unexpected line count for {:?}/{:?}: {lines:?}",
                    outcome.verb,
                    outcome.result
                );
                for rendered in &lines {
                    assert!(
                        !rendered.chars().any(char::is_control),
                        "describe_lines emitted a control character for {:?}/{:?}: \
                         {rendered:?}",
                        outcome.verb,
                        outcome.result
                    );
                }
                // `describe` is the join, so it reconstructs exactly the
                // separators the CLI would supply and nothing else.
                assert_eq!(describe(&outcome), lines.join("\n"));
            }
        }
    }

    /// [`describe_would_delete_dimension`]'s own defensive branches, reached
    /// with arguments `validate_verb_args` would already have refused (an
    /// overflowing `--at`/`--count`) or a workbook snapshot the real pipeline
    /// never hands it (`sheet: None`, or a size too small to subtract
    /// `count` from) — the `describe_lines(&outcome)` bypass used above lets
    /// a test construct exactly that, the same way
    /// `dimension_range_label_omits_a_span_it_cannot_compute` does for
    /// `record_attempt`'s logging path.
    #[test]
    fn describe_would_delete_dimension_handles_unvalidated_edge_cases() {
        let would_change = |verb: StructureVerb, sheet: Option<SheetSnapshot>| StructureOutcome {
            spreadsheet_id: "sheet-1".to_string(),
            file_name: Some("Budget".to_string()),
            resolved_folder_id: None,
            verb,
            result: StructureResult::WouldChange {
                sheet,
                sheet_count: 2,
            },
        };
        // `sheet_id: None` keeps every expected string free of the
        // `(sheetId N)` suffix, which is orthogonal to what this test checks.
        let sheet_with_columns = |column_count| SheetSnapshot {
            sheet_id: None,
            title: "Q2".to_string(),
            row_count: None,
            column_count: Some(column_count),
        };

        // `--at`/`--count` overflowing the `last` computation: no dash-range
        // or shift line, just the plain summary.
        let overflow = describe(&would_change(
            StructureVerb::DeleteColumns {
                sheet: "Q2".to_string(),
                at: i64::MAX,
                count: 2,
            },
            Some(sheet_with_columns(26)),
        ));
        assert!(
            overflow.contains("Would delete 2 column(s) from column"),
            "{overflow}"
        );
        assert!(!overflow.contains('-'), "{overflow}");

        // No sheet snapshot at all: the shift line needs a real "before"
        // size, so it is omitted rather than guessed.
        let no_sheet = describe(&would_change(delete_columns(), None));
        assert_eq!(no_sheet, "Would delete 2 column(s) 3-4 of 'Q2' in 'Budget'");

        // A `before` too small for `count` to subtract from without
        // underflowing `i64::MIN`: also omitted rather than reported wrong.
        let after_underflows = describe(&would_change(
            StructureVerb::DeleteColumns {
                sheet: "Q2".to_string(),
                at: 0,
                count: i64::MAX,
            },
            Some(sheet_with_columns(-2)),
        ));
        assert_eq!(
            after_underflows,
            format!(
                "Would delete {} column(s) 0-{} of 'Q2' in 'Budget'",
                i64::MAX,
                i64::MAX - 1
            )
        );

        // Deleting exactly through the sheet's last column: "nothing
        // remains", not an inverted "existing N-M shift".
        let to_the_end = describe(&would_change(
            StructureVerb::DeleteColumns {
                sheet: "Q2".to_string(),
                at: 25,
                count: 2,
            },
            Some(sheet_with_columns(26)),
        ));
        assert!(
            to_the_end.contains("nothing remains after the deleted columns"),
            "{to_the_end}"
        );
    }

    #[test]
    fn log_status_covers_every_variant() {
        let statuses = [
            StructureResult::WouldChange {
                sheet: None,
                sheet_count: 1,
            }
            .log_status(),
            StructureResult::RefusedNotASpreadsheet {
                mime_type: "application/pdf".to_string(),
            }
            .log_status(),
            StructureResult::RefusedShortcut.log_status(),
            StructureResult::RefusedNoVisibleParents.log_status(),
            StructureResult::RefusedSheetNotFound {
                title: "x".to_string(),
                available: Vec::new(),
            }
            .log_status(),
            StructureResult::RefusedSheetExists {
                title: "x".to_string(),
            }
            .log_status(),
            StructureResult::RefusedInvalidRange {
                detail: "x".to_string(),
            }
            .log_status(),
            StructureResult::Blocked { decided_by: None }.log_status(),
            StructureResult::Changed {
                sheet: None,
                sheet_id: None,
            }
            .log_status(),
            StructureResult::Failed {
                detail: "x".to_string(),
            }
            .log_status(),
        ];
        let unique: HashSet<&&str> = statuses.iter().collect();
        assert_eq!(unique.len(), statuses.len(), "statuses must be distinct");
        assert!(statuses.iter().all(|s| !s.is_empty()));
    }

    #[test]
    fn a_verdict_of_allow_is_what_lets_a_run_proceed() {
        // Pins the gate's vocabulary this module depends on, so a rename of
        // the enum can't silently invert the check above.
        assert_ne!(Verdict::Allow, Verdict::Deny);
    }

    #[test]
    fn build_request_fails_rather_than_guessing_a_missing_sheet_id() {
        // Sheet id 0 is a real sheet (the first tab), so defaulting would
        // aim a rename at the wrong tab.
        let sheet = SheetSnapshot {
            sheet_id: None,
            title: "Q2".to_string(),
            row_count: None,
            column_count: None,
        };
        let err = build_request(&rename(), Some(&sheet)).unwrap_err();
        assert!(err.contains("sheetId"), "{err}");
    }

    #[test]
    fn added_sheet_id_reads_the_add_sheet_reply_only() {
        let empty: BatchUpdateResponse =
            serde_json::from_value(serde_json::json!({"spreadsheetId": "s", "replies": [{}]}))
                .unwrap();
        assert_eq!(added_sheet_id(&empty), None);
        let added: BatchUpdateResponse = serde_json::from_value(serde_json::json!({
            "replies": [{"addSheet": {"properties": {"sheetId": 42, "title": "New"}}}]
        }))
        .unwrap();
        assert_eq!(added_sheet_id(&added), Some(42));
    }

    // ── file-id rules (issue #1612) ────────────────────────────────────

    #[tokio::test]
    async fn a_file_rule_grants_a_sheet_with_no_visible_parents() {
        // The case issue #1612 exists for: before file rules there was no
        // rule an operator could write that would permit this at all.
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &[])
            .mount(&server)
            .await;
        mount_workbook().mount(&server).await;
        mount_batch_update(serde_json::json!({"spreadsheetId": "sheet-1", "replies": [{}]}))
            .expect(1)
            .mount(&server)
            .await;

        let outcome = structure(
            &drive,
            &sheets,
            &opts(rename(), false),
            &[FolderPermissionRule::file("sheet-1").allowing([DriveOperation::SheetsStructure])],
        )
        .await;

        assert!(
            matches!(outcome.result, StructureResult::Changed { .. }),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn a_file_rule_denies_even_when_a_parent_folder_would_allow() {
        // Depth −1 beats depth 0 in the restrictive direction too.
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        // No mock for parent-1 and none for any Sheets endpoint: the file
        // rule must decide before either is reached.

        let outcome = structure(
            &drive,
            &sheets,
            &opts(rename(), false),
            &[
                allow_rule("parent-1"),
                FolderPermissionRule::file("sheet-1").denying([DriveOperation::SheetsStructure]),
            ],
        )
        .await;

        match &outcome.result {
            StructureResult::Blocked { decided_by } => {
                let rule = decided_by.as_ref().expect("a file rule decided this");
                assert_eq!(rule.kind_label(), "file");
                assert_eq!(rule.id(), "sheet-1");
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
        let text = describe(&outcome);
        assert!(text.contains("refused by rule on file sheet-1"), "{text}");
        assert!(!text.contains("depth"), "a file rule has no depth: {text}");
    }

    #[tokio::test]
    async fn the_no_visible_parents_message_names_a_file_rule_as_the_fix() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &[])
            .mount(&server)
            .await;
        let outcome = structure(&drive, &sheets, &opts(rename(), false), &[]).await;
        let text = describe(&outcome);
        assert!(text.contains("file_id"), "{text}");
        assert!(text.contains("sheets-structure"), "{text}");
    }
}
