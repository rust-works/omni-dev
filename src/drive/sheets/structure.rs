//! Structural spreadsheet edits via `spreadsheets.batchUpdate`.
//!
//! The `drive sheets add-sheet`/`rename-sheet`/`insert-rows`/`insert-columns`
//! engines, gated by the ADR-0071 folder write-permission rules (issue
//! #1613, [ADR-0075](../../../docs/adrs/adr-0075.md)).
//!
//! [ADR-0073](../../../docs/adrs/adr-0073.md) §12 deferred this surface
//! because `batchUpdate` is where "one call destroys far more than its
//! arguments suggest". Three properties answer that, and each is structural
//! rather than a matter of care:
//!
//! - **Typed verbs, no raw request passthrough.** Every verb builds its own
//!   [`BatchUpdateRequestItem`], so the gate and `--dry-run` can describe the
//!   exact effect. There is no `--requests file.json` and deliberately no
//!   escape hatch, following [ADR-0061](../../../docs/adrs/adr-0061.md)'s
//!   handling of force-push: the dangerous form must be unreachable, not
//!   merely discouraged.
//! - **No destructive request is modelled at all.** `deleteSheet`,
//!   `deleteDimension` and `deleteRange` have no Rust representation, so they
//!   cannot be constructed. The `no_destructive_request_is_reachable` test
//!   pins that.
//! - **A distinct gate operation.** [`DriveOperation::SheetsStructure`], not
//!   `SheetsWrite` — see its doc comment for why reuse would be silent
//!   privilege widening.
//!
//! Shape follows `write.rs` exactly: a public wrapper that logs, an `_inner`
//! that classifies then mutates, and `--dry-run` as an early return *after*
//! the gate so a preview and a real run share one classification by
//! construction. One difference is deliberate and tested: a dry run here does
//! issue a single `spreadsheets.get`, because describing a structural effect
//! honestly requires the sheet's real current dimensions. It still issues no
//! `batchUpdate`, and a gate-blocked attempt still issues no Sheets call at
//! all.

use std::time::{Duration, Instant};

use serde::Serialize;

use crate::cli::drive::format::{write_scalar_jsonl, JsonlSerialize};
use crate::drive::client::DriveClient;
use crate::drive::files_api::FilesApi;
use crate::drive::folder_ancestry;
use crate::drive::sheets::api::SheetsApi;
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::types::{
    AddSheetRequest, BatchUpdateRequestItem, BatchUpdateResponse, Dimension, DimensionRange,
    GridProperties, InsertDimensionRequest, NewSheetProperties, SheetProperties,
    SheetPropertiesUpdate, Spreadsheet, UpdateSheetPropertiesRequest,
};
use crate::drive::types::{GOOGLE_SHEET_MIME_TYPE, GOOGLE_SHORTCUT_MIME_TYPE};
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
        }
    }

    /// The title of the sheet this verb acts on: the one being created for
    /// `AddSheet`, the existing target otherwise.
    fn sheet_title(&self) -> &str {
        match self {
            Self::AddSheet { title, .. } => title,
            Self::RenameSheet { sheet, .. }
            | Self::InsertRows { sheet, .. }
            | Self::InsertColumns { sheet, .. } => sheet,
        }
    }

    /// The axis an insert runs along, or `None` for the non-insert verbs.
    const fn dimension(&self) -> Option<Dimension> {
        match self {
            Self::InsertRows { .. } => Some(Dimension::Rows),
            Self::InsertColumns { .. } => Some(Dimension::Columns),
            Self::AddSheet { .. } | Self::RenameSheet { .. } => None,
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
    /// `add-sheet` was given a title the workbook already uses. Sheets
    /// rejects a duplicate title, and catching it here keeps the dry run
    /// truthful.
    RefusedSheetExists {
        /// The colliding title.
        title: String,
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
        result,
    };

    let files_api = FilesApi::new(drive);
    let target = match files_api.get_metadata(&opts.spreadsheet_id).await {
        Ok(target) => target,
        Err(err) => {
            return bare(StructureResult::Failed {
                detail: err.to_string(),
            })
        }
    };

    let with_target = |result| StructureOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: Some(target.name.clone()),
        resolved_folder_id: None,
        result,
    };

    // ── Refusals that precede the gate ─────────────────────────────────
    if target.mime_type == GOOGLE_SHORTCUT_MIME_TYPE {
        return with_target(StructureResult::RefusedShortcut);
    }
    if target.mime_type != GOOGLE_SHEET_MIME_TYPE {
        return with_target(StructureResult::RefusedNotASpreadsheet {
            mime_type: target.mime_type.clone(),
        });
    }
    if target.parents.is_empty() {
        return with_target(StructureResult::RefusedNoVisibleParents);
    }

    // ── The gate ───────────────────────────────────────────────────────
    let (decision, resolved_folder_id) = match folder_ancestry::resolve_decision_for_parents(
        &files_api,
        &target.parents,
        DriveOperation::SheetsStructure,
        rules,
    )
    .await
    {
        Ok(pair) => pair,
        // A chain that could not be resolved is a refusal, never a silent
        // allow — ADR-0071 §3's highest-priority invariant.
        Err(err) => {
            return with_target(StructureResult::Failed {
                detail: err.to_string(),
            })
        }
    };

    let gated = |result| StructureOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: Some(target.name.clone()),
        resolved_folder_id: resolved_folder_id.clone(),
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

    if opts.dry_run {
        return gated(StructureResult::WouldChange {
            sheet: sheet.clone(),
            sheet_count,
        });
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
        _ => match found {
            Some(props) => Ok(Some(SheetSnapshot::from_properties(props))),
            None => Err(StructureResult::RefusedSheetNotFound {
                title: wanted.to_string(),
                available: workbook.sheet_titles(),
            }),
        },
    }
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
                    title: new_title.clone(),
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
    }
}

/// The `sheetId` the server assigned to a newly added sheet, if this reply
/// carries one.
fn added_sheet_id(response: &BatchUpdateResponse) -> Option<i64> {
    response
        .replies
        .iter()
        .find_map(|reply| reply.add_sheet.as_ref())
        .and_then(|added| added.properties.as_ref())
        .and_then(|props| props.sheet_id)
}

/// The `dimension_range` context value the request log records, e.g.
/// `"ROWS 5:7"` — the structural analogue of a cell verb's A1 `range`, for
/// effects A1 cannot express. 1-based inclusive, matching the CLI's `--at`.
fn dimension_range_label(verb: &StructureVerb) -> Option<String> {
    let dimension = verb.dimension()?;
    let (at, count) = match verb {
        StructureVerb::InsertRows { at, count, .. }
        | StructureVerb::InsertColumns { at, count, .. } => (*at, *count),
        _ => return None,
    };
    Some(format!("{} {at}:{}", dimension.as_str(), at + count - 1))
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
    let (decided_by_folder_id, decided_by_depth) = write_gate::decided_by_log_fields(decided_by);
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
        decided_by_folder_id,
        decided_by_depth,
        sheet_id,
        sheet_title: Some(opts.verb.sheet_title().to_string()),
        dimension_range: dimension_range_label(&opts.verb),
        error,
        duration,
        ..Default::default()
    });
}

/// Renders an outcome as a single human-readable line.
///
/// Lives here rather than in the CLI layer so the CLI and a future MCP
/// caller describe an outcome identically.
///
/// The `WouldChange` arms are the substance of this module's answer to "what
/// does `--dry-run` show for an effect that isn't a range": the sheet's real
/// current dimensions, the resulting ones, and — for an insert — the shift
/// that no bounded range could express.
#[must_use]
pub fn describe(outcome: &StructureOutcome, verb: &StructureVerb) -> String {
    let book = outcome.file_name.as_deref().map_or_else(
        || format!("'{}'", outcome.spreadsheet_id),
        |n| format!("'{n}'"),
    );

    match &outcome.result {
        StructureResult::WouldChange { sheet, sheet_count } => {
            describe_would_change(verb, sheet.as_ref(), *sheet_count, &book)
        }
        StructureResult::RefusedNotASpreadsheet { mime_type } => format!(
            "Refused: {book} is not a Google Sheet (mimeType: {mime_type}); \
             `drive sheets {}` only works on spreadsheets",
            verb.label()
        ),
        StructureResult::RefusedShortcut => format!(
            "Refused: {book} is a shortcut; `drive sheets {}` doesn't follow shortcuts — \
             resolve the target spreadsheet's id and use that instead",
            verb.label()
        ),
        StructureResult::RefusedNoVisibleParents => format!(
            "Refused: {book} has no parent folder visible to this account, so no \
             write-permission rule can apply to it. This is normal for a Sheet shared by link \
             or email. Add it to a folder in your own Drive, then grant that folder \
             `sheets-structure`."
        ),
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
            format!("Refused: {book} has no sheet titled '{title}'. Available: {list}")
        }
        StructureResult::RefusedSheetExists { title } => {
            format!("Refused: {book} already has a sheet titled '{title}'")
        }
        StructureResult::Blocked { decided_by } => match decided_by {
            Some(rule) => format!(
                "Blocked: structural edits to {book} refused by rule on folder {} (depth {})",
                rule.folder_id, rule.depth
            ),
            None => format!(
                "Blocked: structural edits to {book} refused by default policy (no matching \
                 rule for sheets-structure)"
            ),
        },
        StructureResult::Changed { sheet, sheet_id } => {
            describe_changed(verb, sheet.as_ref(), *sheet_id, &book)
        }
        StructureResult::Failed { detail } => format!("Failed: {detail}"),
    }
}

fn describe_would_change(
    verb: &StructureVerb,
    sheet: Option<&SheetSnapshot>,
    sheet_count: usize,
    book: &str,
) -> String {
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
            format!(
                "Would add sheet '{title}'{size}{position} of {book} \
                 ({sheet_count} sheet(s) -> {})",
                sheet_count + 1
            )
        }
        StructureVerb::RenameSheet {
            sheet: from,
            new_title,
        } => {
            format!("Would rename sheet '{from}'{id} to '{new_title}' in {book}")
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
    }
}

/// The insert arms of [`describe_would_change`].
///
/// This is where a structural dry run earns its keep: it names the resulting
/// dimension *and* the shift, which is the part of the effect no bounded
/// range could express and the reason ADR-0073 §12 called this out.
#[allow(clippy::too_many_arguments)]
fn describe_would_insert(
    dimension: Dimension,
    from: &str,
    id: &str,
    at: i64,
    count: i64,
    sheet: Option<&SheetSnapshot>,
    book: &str,
) -> String {
    let before = match dimension {
        Dimension::Rows => sheet.and_then(|s| s.row_count),
        Dimension::Columns => sheet.and_then(|s| s.column_count),
    };
    let shift = before.map_or_else(String::new, |before| {
        format!(
            "\n  ({before} {plural} -> {}; existing {plural} {at}-{before} shift {direction})",
            before + count,
            plural = plural(dimension),
            direction = match dimension {
                Dimension::Rows => "down",
                Dimension::Columns => "right",
            },
        )
    });
    format!(
        "Would insert {count} {noun}(s) before {noun} {at} of '{from}'{id} in {book}{shift}",
        noun = dimension.noun(),
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
    }
}

/// The insert arms of [`describe_changed`].
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
    .map_or_else(String::new, |before| {
        format!(" ({} {} now)", before + count, plural(dimension))
    });
    format!(
        "Inserted {count} {noun}(s) before {noun} {at} of '{from}'{id} in {book}{now}",
        noun = dimension.noun(),
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
            folder_id: folder.to_string(),
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

    // ── the safety property this whole module exists for ───────────────

    /// No destructive `batchUpdate` request is reachable from any production
    /// path in the Sheets surface.
    ///
    /// Modelled on `no_force_escape_hatch_exists_in_the_ui_surface`
    /// (`src/cli/worktrees/ui/actions.rs`) and enforcing the same kind of
    /// guarantee: [ADR-0061](../../../docs/adrs/adr-0061.md) established
    /// that an operation's dangerous form must be *unreachable*, not merely
    /// discouraged. `deleteSheet` and `deleteDimension` are what ADR-0073
    /// §12 called the sharp edge, and they are deferred to their own design
    /// pass — so no production line may name one, and a future request type
    /// cannot be added without failing the build.
    #[test]
    fn no_destructive_request_is_reachable() {
        let sources = [
            ("structure.rs", include_str!("structure.rs")),
            ("api.rs", include_str!("api.rs")),
            ("types.rs", include_str!("types.rs")),
        ];
        for (name, source) in sources {
            // Production code only: the prose above and the assertions here
            // deliberately name the requests they forbid.
            let code_only = source.split("#[cfg(test)]").next().unwrap_or(source);
            for (number, line) in code_only.lines().enumerate() {
                let code = line.trim_start();
                if code.starts_with("//") || code.starts_with("///") || code.starts_with("//!") {
                    continue; // prose may discuss deletion; code may not request it
                }
                for forbidden in [
                    "deleteSheet",
                    "deleteDimension",
                    "deleteRange",
                    "DeleteSheet",
                    "DeleteDimension",
                    "DeleteRange",
                ] {
                    assert!(
                        !code.contains(forbidden),
                        "{name}:{}: a destructive batchUpdate request must stay unreachable: \
                         {line}",
                        number + 1
                    );
                }
            }
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
        let text = describe(&outcome, &rename());
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
        let text = describe(&outcome, &rename());
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
        let text = describe(&outcome, &rename());
        assert!(text.contains("no parent folder visible"), "{text}");
        // Names the operation the operator would have to grant, so the
        // message is actionable rather than merely accurate.
        assert!(text.contains("sheets-structure"), "{text}");
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
        let text = describe(&outcome, &rename());
        assert!(text.contains("default policy"), "{text}");
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
            folder_id: "parent-1".to_string(),
            recursive: true,
            allow: std::iter::once(DriveOperation::SheetsWrite).collect(),
            deny: HashSet::default(),
        }];
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
            folder_id: "parent-1".to_string(),
            recursive: true,
            allow: HashSet::default(),
            deny: std::iter::once(DriveOperation::SheetsStructure).collect(),
        }];
        let outcome = structure(&drive, &sheets, &opts(rename(), false), &rules).await;
        let StructureResult::Blocked { decided_by } = &outcome.result else {
            panic!("expected Blocked, got {:?}", outcome.result);
        };
        let rule = decided_by.as_ref().expect("an explicit rule decided this");
        assert_eq!(rule.folder_id, "parent-1");
        assert_eq!(rule.depth, 0);
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
        let text = describe(&outcome, &insert_rows());
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
        let text = describe(&outcome, &verb);
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
        assert!(describe(&outcome, &verb).contains("already has a sheet titled 'Q1'"));
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
        assert!(describe(&outcome, &add_sheet()).contains("sheetId 999"));
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
        ];
        let names: Vec<&str> = verbs.iter().map(StructureVerb::log_operation).collect();
        assert_eq!(
            names,
            vec![
                "sheets-add-sheet",
                "sheets-rename-sheet",
                "sheets-insert-rows",
                "sheets-insert-columns",
            ]
        );
        // One record per user-visible verb means the operations must not
        // collide with each other or with the cell verbs.
        let unique: HashSet<&&str> = names.iter().collect();
        assert_eq!(unique.len(), names.len());
        assert!(!names.contains(&"sheets-write"));
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
}
