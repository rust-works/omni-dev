//! Cell/border formatting, merging and auto-resize via
//! `spreadsheets.batchUpdate` (issue #1643,
//! [ADR-0077](../../../docs/adrs/adr-0077.md)).
//!
//! Six verbs, all gated by [`DriveOperation::SheetsStructure`] — none
//! destroy data, with one deliberate exception called out below.
//!
//! Shape follows `structure.rs` exactly: `target_gate::resolve` for the
//! target and gate, then a `spreadsheets.get` (needed to resolve the
//! target sheet's numeric id and, for `merge-cells`, its current values),
//! then a `--dry-run` early return, then the mutation, then one
//! `drivemutation` record. Unlike `structure.rs`'s verbs (each addressing a
//! whole *sheet*), these address a *range*, composed from `--sheet`/
//! `--range` exactly as `write.rs`'s cell verbs do (`a1::compose`), then
//! converted to a numeric [`GridRange`] via `grid_range::parse_grid_range`.
//!
//! **`format-cells` cannot write a value.** It builds a `repeatCell`
//! request whose `cell` is [`RepeatCellData`] — a type with no
//! `userEnteredValue` field at all — so there is nowhere to put one
//! regardless of what a caller asks for. This is `format-cells`'
//! structural answer to "does `updateCells` belong here or in the
//! cell-write surface": it uses `repeatCell` exclusively and never
//! `updateCells`, so the `sheets-structure` grant can never be used to
//! write a value the way `sheets-write` can.
//!
//! **`merge-cells` is the one request here that discards data.** Merging a
//! range keeps only the top-left cell's value; every other non-blank cell
//! in the range is silently dropped. Both the dry run and the real run
//! read the range's current values first (one `values.get`, after the gate
//! like every other read in this module) and the outcome — and its
//! `--dry-run` preview — name every non-top-left, non-blank cell that
//! would be lost, exactly the honesty ADR-0075 §6 requires of a preview
//! whose effect a bounded range can't otherwise express.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::cli::drive::format::{write_scalar_jsonl, JsonlSerialize};
use crate::drive::client::DriveClient;
use crate::drive::files_api::FilesApi;
use crate::drive::lease::check::{
    check_and_lock_lease, refresh_lease_after_native_write, LeaseCheckOutcome,
};
use crate::drive::sheets::a1;
use crate::drive::sheets::api::{SheetsApi, ValueRenderOption};
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::grid_range;
use crate::drive::sheets::target_gate;
use crate::drive::sheets::types::{
    AutoResizeDimensionsRequest, BatchUpdateRequestItem, Border, CellFormat, ColorStyle, Dimension,
    DimensionProperties, DimensionRange, GridRange, MergeCellsRequest, NumberFormat,
    RepeatCellData, RepeatCellRequest, Spreadsheet, TextFormat, UnmergeCellsRequest,
    UpdateBordersRequest, UpdateDimensionPropertiesRequest, ValueRange,
};
use crate::drive::types::SheetTargetRefusal;
use crate::drive::write_gate::{self, DecidingRule, DriveOperation, FolderPermissionRule};
use crate::request_log::{self, DriveMutationOutcome};

/// The `CellFormat` flags `format-cells` exposes.
///
/// Every field optional; [`build_cell_format`] refuses an all-`None` set
/// and builds the `fields` mask from exactly the ones populated, following
/// `SheetPropertiesUpdate`'s discipline.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CellFormatFlags {
    /// Bold.
    pub bold: Option<bool>,
    /// Italic.
    pub italic: Option<bool>,
    /// Strikethrough.
    pub strikethrough: Option<bool>,
    /// Underline.
    pub underline: Option<bool>,
    /// Point size.
    pub font_size: Option<i64>,
    /// `#RRGGBB` text color.
    pub text_color: Option<String>,
    /// `#RRGGBB` background color.
    pub background: Option<String>,
    /// Already wire-shaped (`"LEFT"`/`"CENTER"`/`"RIGHT"`) — the CLI's
    /// `clap::ValueEnum` validates and normalises it, so nothing here can
    /// be a value Sheets would reject as a *grammar* error.
    pub horizontal_align: Option<String>,
    /// Already wire-shaped (`"TOP"`/`"MIDDLE"`/`"BOTTOM"`).
    pub vertical_align: Option<String>,
    /// Display pattern, e.g. `"#,##0.00"`. Must be given together with
    /// `number_format_type`.
    pub number_format_pattern: Option<String>,
    /// Already wire-shaped (`"NUMBER"`, `"CURRENCY"`, …). Must be given
    /// together with `number_format_pattern`.
    pub number_format_type: Option<String>,
    /// Already wire-shaped (`"OVERFLOW_CELL"`/`"CLIP"`/`"WRAP"`).
    pub wrap: Option<String>,
}

/// Which sides an `update-borders` verb sets. At least one must be `true`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BorderSides {
    /// Top edge.
    pub top: bool,
    /// Bottom edge.
    pub bottom: bool,
    /// Left edge.
    pub left: bool,
    /// Right edge.
    pub right: bool,
}

impl BorderSides {
    fn any(self) -> bool {
        self.top || self.bottom || self.left || self.right
    }
}

/// Which mutation to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatVerb {
    /// Apply a cell format across a range.
    FormatCells {
        /// A sheet title, supplying a prefix for a bare `range`.
        sheet: Option<String>,
        /// An explicit A1 range, which may carry its own `Sheet!` prefix.
        range: Option<String>,
        /// The properties to set.
        format: CellFormatFlags,
    },
    /// Set border lines on a range's edges.
    UpdateBorders {
        /// A sheet title, supplying a prefix for a bare `range`.
        sheet: Option<String>,
        /// An explicit A1 range, which may carry its own `Sheet!` prefix.
        range: Option<String>,
        /// Which edges to set.
        sides: BorderSides,
        /// Already wire-shaped (`"SOLID"`, `"DASHED"`, …).
        style: String,
        /// `#RRGGBB`. `None` takes solid black, matching the spreadsheet
        /// UI's own default border color.
        color: Option<String>,
    },
    /// Merge a range into one cell.
    MergeCells {
        /// A sheet title, supplying a prefix for a bare `range`.
        sheet: Option<String>,
        /// An explicit A1 range, which may carry its own `Sheet!` prefix.
        range: Option<String>,
        /// Already wire-shaped (`"MERGE_ALL"`, `"MERGE_ROWS"`,
        /// `"MERGE_COLUMNS"`).
        merge_type: String,
    },
    /// Split a previously merged range back apart.
    UnmergeCells {
        /// A sheet title, supplying a prefix for a bare `range`.
        sheet: Option<String>,
        /// An explicit A1 range, which may carry its own `Sheet!` prefix.
        range: Option<String>,
    },
    /// Resize rows or columns to fit their content.
    AutoResizeDimension {
        /// Title of the sheet to modify.
        sheet: String,
        /// Rows or columns.
        dimension: Dimension,
        /// 1-based first row/column, inclusive.
        start: i64,
        /// 1-based last row/column, inclusive.
        end: i64,
    },
    /// Set an explicit pixel width (columns) or height (rows).
    UpdateDimensionProperties {
        /// Title of the sheet to modify.
        sheet: String,
        /// Rows or columns.
        dimension: Dimension,
        /// 1-based first row/column, inclusive.
        start: i64,
        /// 1-based last row/column, inclusive.
        end: i64,
        /// The new size, in pixels.
        pixel_size: i64,
    },
}

impl FormatVerb {
    /// The `operation` this verb records in the request log.
    const fn log_operation(&self) -> &'static str {
        match self {
            Self::FormatCells { .. } => "sheets-format-cells",
            Self::UpdateBorders { .. } => "sheets-update-borders",
            Self::MergeCells { .. } => "sheets-merge-cells",
            Self::UnmergeCells { .. } => "sheets-unmerge-cells",
            Self::AutoResizeDimension { .. } => "sheets-auto-resize-dimension",
            Self::UpdateDimensionProperties { .. } => "sheets-update-dimension-properties",
        }
    }

    /// The CLI subcommand that spells this verb, for error messages.
    const fn label(&self) -> &'static str {
        match self {
            Self::FormatCells { .. } => "format-cells",
            Self::UpdateBorders { .. } => "update-borders",
            Self::MergeCells { .. } => "merge-cells",
            Self::UnmergeCells { .. } => "unmerge-cells",
            Self::AutoResizeDimension { .. } => "auto-resize-dimension",
            Self::UpdateDimensionProperties { .. } => "update-dimension-properties",
        }
    }

    /// `Some((sheet, range))` for the four range-targeted verbs; `None` for
    /// the two dimension-span verbs, which name their sheet directly.
    fn sheet_and_range(&self) -> Option<(Option<&str>, Option<&str>)> {
        match self {
            Self::FormatCells { sheet, range, .. }
            | Self::UpdateBorders { sheet, range, .. }
            | Self::MergeCells { sheet, range, .. }
            | Self::UnmergeCells { sheet, range, .. } => Some((sheet.as_deref(), range.as_deref())),
            Self::AutoResizeDimension { .. } | Self::UpdateDimensionProperties { .. } => None,
        }
    }
}

/// Per-call options.
#[derive(Debug, Clone)]
pub struct FormatOptions {
    /// Spreadsheet id.
    pub spreadsheet_id: String,
    /// Which mutation to perform.
    pub verb: FormatVerb,
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
pub enum FormatResult {
    /// `--dry-run`, and the gate would allow it.
    WouldChange {
        /// A human-readable summary of the effect — also the request log's
        /// `fields_changed`.
        summary: String,
        /// `merge-cells` only: the non-top-left, non-blank cells that would
        /// be discarded, as `"A1: value"` strings.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        discarded_cells: Vec<String>,
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
    /// The `--sheet`/`--range` pair was invalid (conflicting, empty, or a
    /// bare range naming no sheet at all).
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
        /// Same summary as [`Self::WouldChange`], describing what was
        /// actually done.
        summary: String,
        /// `merge-cells` only: the cells whose values were discarded.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        discarded_cells: Vec<String>,
    },
    /// An API or validation error.
    Failed {
        /// A human-readable summary of what failed.
        detail: String,
    },
}

impl FormatResult {
    fn log_status(&self) -> &'static str {
        match self {
            Self::WouldChange { .. } => "would-change",
            Self::RefusedNotASpreadsheet { .. } => "refused-not-a-spreadsheet",
            Self::RefusedShortcut => "refused-shortcut",
            Self::RefusedNoVisibleParents => "refused-no-visible-parents",
            Self::RefusedSheetNotFound { .. } => "refused-sheet-not-found",
            Self::RefusedInvalidRange { .. } => "refused-invalid-range",
            Self::Blocked { .. } => "blocked",
            Self::RefusedNoLease => "refused-no-lease",
            Self::RefusedLeaseExpired => "refused-lease-expired",
            Self::RefusedLeaseWrongFile => "refused-lease-wrong-file",
            Self::RefusedLeaseStale => "refused-lease-stale",
            Self::Changed { .. } => "changed",
            Self::Failed { .. } => "failed",
        }
    }
}

/// The full outcome of one attempt.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FormatOutcome {
    /// The spreadsheet acted on.
    pub spreadsheet_id: String,
    /// Its Drive file name, when the metadata fetch got that far.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    /// The folder the gate evaluated against.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_folder_id: Option<String>,
    /// Which mutation was attempted. Not serialised, like
    /// `StructureOutcome::verb` — carried so `describe` can render from the
    /// outcome alone.
    #[serde(skip)]
    pub verb: FormatVerb,
    /// What happened.
    pub result: FormatResult,
}

impl JsonlSerialize for FormatOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> anyhow::Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Parses `#RRGGBB` (the `#` optional) into a [`crate::drive::sheets::types::Color`].
///
/// The one conversion site: the Sheets API's `Color` is three floats
/// 0.0-1.0, not the 0-255-per-byte a hex string suggests.
fn parse_hex_color(input: &str) -> Result<crate::drive::sheets::types::Color, String> {
    let hex = input.strip_prefix('#').unwrap_or(input);
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "'{input}' is not a color; expected 6 hex digits, optionally prefixed with '#' \
             (e.g. #FF8800)"
        ));
    }
    let byte =
        |slice: &str| -> f32 { f32::from(u8::from_str_radix(slice, 16).unwrap_or(0)) / 255.0 };
    Ok(crate::drive::sheets::types::Color {
        red: byte(&hex[0..2]),
        green: byte(&hex[2..4]),
        blue: byte(&hex[4..6]),
    })
}

/// Builds the `CellFormat` and its `fields` mask (relative to
/// `cell.userEnteredFormat`, per `repeatCell`'s own convention) from
/// exactly the flags populated. Errors if none are, or if only one of
/// `number_format_pattern`/`number_format_type` is given.
fn build_cell_format(flags: &CellFormatFlags) -> Result<(CellFormat, String), String> {
    let mut format = CellFormat::default();
    let mut fields = Vec::new();
    let mut text_format = TextFormat::default();
    let mut text_format_used = false;

    if let Some(bold) = flags.bold {
        text_format.bold = Some(bold);
        text_format_used = true;
        fields.push("userEnteredFormat.textFormat.bold");
    }
    if let Some(italic) = flags.italic {
        text_format.italic = Some(italic);
        text_format_used = true;
        fields.push("userEnteredFormat.textFormat.italic");
    }
    if let Some(strikethrough) = flags.strikethrough {
        text_format.strikethrough = Some(strikethrough);
        text_format_used = true;
        fields.push("userEnteredFormat.textFormat.strikethrough");
    }
    if let Some(underline) = flags.underline {
        text_format.underline = Some(underline);
        text_format_used = true;
        fields.push("userEnteredFormat.textFormat.underline");
    }
    if let Some(font_size) = flags.font_size {
        text_format.font_size = Some(font_size);
        text_format_used = true;
        fields.push("userEnteredFormat.textFormat.fontSize");
    }
    if let Some(hex) = &flags.text_color {
        let color = parse_hex_color(hex)?;
        text_format.foreground_color_style = Some(ColorStyle { rgb_color: color });
        text_format_used = true;
        fields.push("userEnteredFormat.textFormat.foregroundColorStyle");
    }
    if text_format_used {
        format.text_format = Some(text_format);
    }

    if let Some(hex) = &flags.background {
        let color = parse_hex_color(hex)?;
        format.background_color_style = Some(ColorStyle { rgb_color: color });
        fields.push("userEnteredFormat.backgroundColorStyle");
    }
    if let Some(align) = &flags.horizontal_align {
        format.horizontal_alignment = Some(align.clone());
        fields.push("userEnteredFormat.horizontalAlignment");
    }
    if let Some(align) = &flags.vertical_align {
        format.vertical_alignment = Some(align.clone());
        fields.push("userEnteredFormat.verticalAlignment");
    }
    match (&flags.number_format_pattern, &flags.number_format_type) {
        (Some(pattern), Some(format_type)) => {
            format.number_format = Some(NumberFormat {
                format_type: format_type.clone(),
                pattern: pattern.clone(),
            });
            fields.push("userEnteredFormat.numberFormat");
        }
        (None, None) => {}
        _ => {
            return Err(
                "--number-format and --number-format-type must be given together".to_string(),
            )
        }
    }
    if let Some(wrap) = &flags.wrap {
        format.wrap_strategy = Some(wrap.clone());
        fields.push("userEnteredFormat.wrapStrategy");
    }

    if fields.is_empty() {
        return Err(
            "format-cells needs at least one property to set (--bold, --background, …)".to_string(),
        );
    }
    Ok((format, fields.join(",")))
}

/// Runs one formatting mutation, logging every attempt that isn't a dry
/// run. Never returns `Err`, matching `structure.rs::structure`.
pub async fn format(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &FormatOptions,
    rules: &[FolderPermissionRule],
) -> FormatOutcome {
    let started = Instant::now();
    let outcome = format_inner(drive, sheets, opts, rules).await;
    if !opts.dry_run {
        record_attempt(&outcome, opts, started.elapsed());
    }
    outcome
}

async fn format_inner(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &FormatOptions,
    rules: &[FolderPermissionRule],
) -> FormatOutcome {
    let bare = |result| FormatOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: None,
        resolved_folder_id: None,
        verb: opts.verb.clone(),
        result,
    };

    // Compose the range first, like `write_inner`: pure, and a conflicting
    // --sheet/--range pair should fail without spending a request.
    let composed_range = if let Some((sheet, range)) = opts.verb.sheet_and_range() {
        match a1::compose(sheet, range) {
            Ok(composed) => Some(composed),
            Err(err) => {
                return bare(FormatResult::RefusedInvalidRange {
                    detail: err.to_string(),
                })
            }
        }
    } else {
        None
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
            return bare(FormatResult::Failed { detail })
        }
        target_gate::TargetGateOutcome::Refused { target, refusal } => {
            let result = match refusal {
                SheetTargetRefusal::Shortcut => FormatResult::RefusedShortcut,
                SheetTargetRefusal::NotASpreadsheet { mime_type } => {
                    FormatResult::RefusedNotASpreadsheet { mime_type }
                }
                SheetTargetRefusal::NoVisibleParents => FormatResult::RefusedNoVisibleParents,
            };
            return FormatOutcome {
                spreadsheet_id: opts.spreadsheet_id.clone(),
                file_name: Some(target.name),
                resolved_folder_id: None,
                verb: opts.verb.clone(),
                result,
            };
        }
        target_gate::TargetGateOutcome::GateFetchFailed { target, detail } => {
            return FormatOutcome {
                spreadsheet_id: opts.spreadsheet_id.clone(),
                file_name: Some(target.name),
                resolved_folder_id: None,
                verb: opts.verb.clone(),
                result: FormatResult::Failed { detail },
            };
        }
        target_gate::TargetGateOutcome::Gated {
            target,
            decision,
            resolved_folder_id,
            requires_lease,
        } => (target, decision, resolved_folder_id, requires_lease),
    };

    let gated = |result| FormatOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: Some(target.name.clone()),
        resolved_folder_id: resolved_folder_id.clone(),
        verb: opts.verb.clone(),
        result,
    };

    if decision.verdict == write_gate::Verdict::Deny {
        return gated(FormatResult::Blocked {
            decided_by: decision.decided_by,
        });
    }

    let api = SheetsApi::new(sheets);
    let workbook = match api.get_spreadsheet(&opts.spreadsheet_id).await {
        Ok(workbook) => workbook,
        Err(err) => {
            return gated(FormatResult::Failed {
                detail: format!("{err:#}"),
            })
        }
    };

    // ── Resolve the target: a range for four verbs, a sheet+span for two ─
    let resolved = match resolve_target(&workbook, &opts.verb, composed_range.as_deref()) {
        Ok(resolved) => resolved,
        Err(result) => return gated(result),
    };

    // `merge-cells` reads current values before doing anything else, dry
    // run or real: its preview and its record both need to name what would
    // be (or was) discarded, and that fact must come from the same read on
    // both paths.
    let discarded_cells = if matches!(opts.verb, FormatVerb::MergeCells { .. }) {
        match read_discarded_cells(&api, &opts.spreadsheet_id, &resolved).await {
            Ok(cells) => cells,
            Err(detail) => return gated(FormatResult::Failed { detail }),
        }
    } else {
        Vec::new()
    };

    let summary = match describe_effect(&opts.verb, &discarded_cells) {
        Ok(summary) => summary,
        Err(detail) => return gated(FormatResult::RefusedInvalidRange { detail }),
    };

    if opts.dry_run {
        return gated(FormatResult::WouldChange {
            summary,
            discarded_cells,
        });
    }

    // The lease check (ADR-0080 §9) sits here: after the permission gate
    // and the `--dry-run` branch, before the mutating call — see
    // `content_edit.rs::edit_inner`'s doc comment for the full reasoning,
    // shared verbatim by every leased engine. A fresh `files.get` immediately
    // before `batchUpdate`, not a reuse of the metadata `target_gate::resolve`
    // fetched before the (potentially slow) ancestor-chain walk and workbook
    // fetch above.
    let files_api = FilesApi::new(drive);
    let lease_lock = if requires_lease {
        let live_version = match files_api.get_metadata(&opts.spreadsheet_id).await {
            Ok(fresh) => fresh.version,
            Err(err) => {
                return gated(FormatResult::Failed {
                    detail: err.to_string(),
                })
            }
        };
        match check_and_lock_lease(
            "drive sheets format",
            &opts.ledger_path,
            opts.lease_token.as_deref(),
            &opts.spreadsheet_id,
            live_version.as_deref(),
        ) {
            LeaseCheckOutcome::Ok(lock) => Some(lock),
            LeaseCheckOutcome::NoLease => return gated(FormatResult::RefusedNoLease),
            LeaseCheckOutcome::Expired => return gated(FormatResult::RefusedLeaseExpired),
            LeaseCheckOutcome::WrongFile => return gated(FormatResult::RefusedLeaseWrongFile),
            LeaseCheckOutcome::Stale => return gated(FormatResult::RefusedLeaseStale),
            LeaseCheckOutcome::Failed(detail) => return gated(FormatResult::Failed { detail }),
        }
    } else {
        None
    };

    let request = match build_request(&opts.verb, &resolved) {
        Ok(request) => request,
        Err(detail) => return gated(FormatResult::RefusedInvalidRange { detail }),
    };

    let result = match api.batch_update(&opts.spreadsheet_id, vec![request]).await {
        Ok(_response) => {
            if let (Some(token), Some(lock)) = (&opts.lease_token, &lease_lock) {
                refresh_lease_after_native_write(
                    "drive sheets format",
                    lock,
                    &opts.ledger_path,
                    token,
                    &files_api,
                    &opts.spreadsheet_id,
                )
                .await;
            }
            FormatResult::Changed {
                summary,
                discarded_cells,
            }
        }
        Err(err) => FormatResult::Failed {
            detail: format!("{err:#}"),
        },
    };
    drop(lease_lock);
    gated(result)
}

/// What a verb resolved to: enough to build its request, whichever shape
/// the verb needs.
enum ResolvedTarget {
    /// A numeric range plus the sheet title it's on (for describing).
    Range {
        sheet_title: String,
        grid: GridRange,
    },
    /// A dimension span on a named, resolved sheet.
    Dimension { range: DimensionRange },
}

fn resolve_target(
    workbook: &Spreadsheet,
    verb: &FormatVerb,
    composed_range: Option<&str>,
) -> Result<ResolvedTarget, FormatResult> {
    match verb {
        FormatVerb::AutoResizeDimension {
            sheet,
            dimension,
            start,
            end,
        }
        | FormatVerb::UpdateDimensionProperties {
            sheet,
            dimension,
            start,
            end,
            ..
        } => {
            let sheet_id = find_sheet_id(workbook, sheet)?;
            if *start < 1 || *end < *start {
                return Err(FormatResult::RefusedInvalidRange {
                    detail: format!(
                        "--start must be at least 1 and --end must be >= --start (got \
                         --start {start} --end {end})"
                    ),
                });
            }
            let range = DimensionRange {
                sheet_id,
                dimension: *dimension,
                start_index: start - 1,
                end_index: *end,
            };
            Ok(ResolvedTarget::Dimension { range })
        }
        FormatVerb::FormatCells { .. }
        | FormatVerb::UpdateBorders { .. }
        | FormatVerb::MergeCells { .. }
        | FormatVerb::UnmergeCells { .. } => {
            let composed = composed_range.unwrap_or_default();
            let (title, grid) = grid_range::resolve_grid_range(
                workbook,
                composed,
                |detail| FormatResult::RefusedInvalidRange { detail },
                |title, available| FormatResult::RefusedSheetNotFound { title, available },
            )?;
            // Merging needs a fixed extent: its preview (and the request
            // log) must name exactly which cells would be discarded, and an
            // open-ended column/row range (`A:A`) has no fixed extent to
            // check that against.
            if matches!(verb, FormatVerb::MergeCells { .. }) && !grid_range::is_bounded(&grid) {
                return Err(FormatResult::RefusedInvalidRange {
                    detail: format!(
                        "'{}' is open-ended; merge-cells needs a fully bounded range (e.g. \
                         A1:D20) so its preview can name exactly what would be discarded",
                        a1::split_sheet_prefix(composed)
                            .map_or(composed, |(_, bare_range)| bare_range)
                    ),
                });
            }
            Ok(ResolvedTarget::Range {
                sheet_title: title,
                grid,
            })
        }
    }
}

fn find_sheet_id(workbook: &Spreadsheet, title: &str) -> Result<i64, FormatResult> {
    grid_range::find_sheet_id(workbook, title, |title, available| {
        FormatResult::RefusedSheetNotFound { title, available }
    })
}

/// Reads the current values of a `merge-cells` target range, returning
/// every non-top-left, non-blank cell as an `"A1: value"` string — what
/// would be (or was) discarded.
///
/// Only ever called for [`FormatVerb::MergeCells`], whose target is always
/// [`ResolvedTarget::Range`].
async fn read_discarded_cells(
    api: &SheetsApi<'_>,
    spreadsheet_id: &str,
    resolved: &ResolvedTarget,
) -> Result<Vec<String>, String> {
    let ResolvedTarget::Range { sheet_title, grid } = resolved else {
        return Ok(Vec::new());
    };
    // A single cell can't discard anything by merging — skip the read
    // entirely rather than ask the API to confirm the obvious.
    let is_single_cell = matches!(
        (
            grid.start_row_index,
            grid.end_row_index,
            grid.start_column_index,
            grid.end_column_index,
        ),
        (Some(r0), Some(r1), Some(c0), Some(c1)) if r1 - r0 <= 1 && c1 - c0 <= 1
    );
    if is_single_cell {
        return Ok(Vec::new());
    }
    let a1_range = grid_range_to_a1(sheet_title, grid);
    let values = api
        .values_get(spreadsheet_id, &a1_range, ValueRenderOption::Formatted)
        .await
        .map_err(|err| format!("{err:#}"))?;
    // `values.get` indexes its result relative to the range it was asked
    // for, not the sheet — the discarded-cell addresses must add the
    // range's own start row/column back on.
    Ok(discarded_from_values(
        &values,
        grid.start_row_index.unwrap_or(0),
        grid.start_column_index.unwrap_or(0),
    ))
}

/// Renders a numeric [`GridRange`] back to an A1 string for a `values.get`
/// read. Only ever called on a target `resolve_target` has already refused
/// unless fully bounded (see the `MergeCells` check there) — an unbounded
/// range has no fixed extent this could name.
fn grid_range_to_a1(sheet_title: &str, grid: &GridRange) -> String {
    let (Some(r0), Some(r1), Some(c0), Some(c1)) = (
        grid.start_row_index,
        grid.end_row_index,
        grid.start_column_index,
        grid.end_column_index,
    ) else {
        unreachable!("merge-cells targets are refused earlier unless fully bounded")
    };
    let start = format!("{}{}", grid_range::column_index_to_letters(c0), r0 + 1);
    let end = format!("{}{}", grid_range::column_index_to_letters(c1 - 1), r1);
    a1::compose(Some(sheet_title), Some(&format!("{start}:{end}"))).unwrap_or_default()
}

/// `row_offset`/`col_offset` are the target range's own start row/column —
/// `values.get`'s response is indexed relative to the range it read, not
/// the sheet, so they must be added back to get the true A1 address.
fn discarded_from_values(values: &ValueRange, row_offset: i64, col_offset: i64) -> Vec<String> {
    let mut discarded = Vec::new();
    for (row_idx, row) in values.values.iter().enumerate() {
        for (col_idx, cell) in row.iter().enumerate() {
            if row_idx == 0 && col_idx == 0 {
                continue;
            }
            let is_blank = cell.is_null() || cell.as_str().is_some_and(str::is_empty);
            if is_blank {
                continue;
            }
            let address = format!(
                "{}{}",
                grid_range::column_index_to_letters(col_idx as i64 + col_offset),
                row_idx as i64 + row_offset + 1
            );
            let display = cell
                .as_str()
                .map_or_else(|| cell.to_string(), str::to_string);
            discarded.push(format!("{address}: {display}"));
        }
    }
    discarded
}

fn describe_effect(verb: &FormatVerb, discarded_cells: &[String]) -> Result<String, String> {
    match verb {
        FormatVerb::FormatCells { format, .. } => {
            let (_, fields) = build_cell_format(format)?;
            Ok(format!("set {fields}"))
        }
        FormatVerb::UpdateBorders { sides, style, .. } => {
            if !sides.any() {
                return Err(
                    "update-borders needs at least one side (--top, --bottom, --left, --right, \
                     --all)"
                        .to_string(),
                );
            }
            let mut names = Vec::new();
            if sides.top {
                names.push("top");
            }
            if sides.bottom {
                names.push("bottom");
            }
            if sides.left {
                names.push("left");
            }
            if sides.right {
                names.push("right");
            }
            Ok(format!("{} border(s), style {style}", names.join(", ")))
        }
        FormatVerb::MergeCells { merge_type, .. } => {
            if discarded_cells.is_empty() {
                Ok(format!("merge ({merge_type})"))
            } else {
                Ok(format!(
                    "merge ({merge_type}), discarding {} cell(s): {}",
                    discarded_cells.len(),
                    discarded_cells.join("; ")
                ))
            }
        }
        FormatVerb::UnmergeCells { .. } => Ok("unmerge".to_string()),
        FormatVerb::AutoResizeDimension { dimension, .. } => {
            Ok(format!("auto-resize {}(s)", dimension.noun()))
        }
        FormatVerb::UpdateDimensionProperties {
            dimension,
            pixel_size,
            ..
        } => Ok(format!(
            "set {} pixelSize to {pixel_size}",
            dimension.noun()
        )),
    }
}

fn build_request(
    verb: &FormatVerb,
    resolved: &ResolvedTarget,
) -> Result<BatchUpdateRequestItem, String> {
    // Exhaustive over `FormatVerb`, not `(FormatVerb, ResolvedTarget)`: a
    // new verb forces a new arm here at compile time, the same guarantee
    // `resolve_target` gets. Each arm's `resolved` shape is guaranteed by
    // `resolve_target`, which picks it from the verb itself — an
    // `unreachable!` names that invariant instead of a runtime `Err` that
    // would only fire if `resolve_target` and this fell out of sync.
    match verb {
        FormatVerb::FormatCells { format, .. } => {
            let ResolvedTarget::Range { grid, .. } = resolved else {
                unreachable!("resolve_target always resolves FormatCells to a Range")
            };
            let (cell_format, fields) = build_cell_format(format)?;
            Ok(BatchUpdateRequestItem::RepeatCell(RepeatCellRequest {
                range: *grid,
                cell: RepeatCellData {
                    user_entered_format: cell_format,
                },
                fields,
            }))
        }
        FormatVerb::UpdateBorders {
            sides,
            style,
            color,
            ..
        } => {
            let ResolvedTarget::Range { grid, .. } = resolved else {
                unreachable!("resolve_target always resolves UpdateBorders to a Range")
            };
            let color = match color {
                Some(hex) => parse_hex_color(hex)?,
                None => crate::drive::sheets::types::Color::default(),
            };
            let border = Border {
                style: style.clone(),
                color_style: ColorStyle { rgb_color: color },
            };
            let mut request = UpdateBordersRequest {
                range: *grid,
                ..Default::default()
            };
            if sides.top {
                request.top = Some(border.clone());
            }
            if sides.bottom {
                request.bottom = Some(border.clone());
            }
            if sides.left {
                request.left = Some(border.clone());
            }
            if sides.right {
                request.right = Some(border);
            }
            Ok(BatchUpdateRequestItem::UpdateBorders(request))
        }
        FormatVerb::MergeCells { merge_type, .. } => {
            let ResolvedTarget::Range { grid, .. } = resolved else {
                unreachable!("resolve_target always resolves MergeCells to a Range")
            };
            Ok(BatchUpdateRequestItem::MergeCells(MergeCellsRequest {
                range: *grid,
                merge_type: merge_type.clone(),
            }))
        }
        FormatVerb::UnmergeCells { .. } => {
            let ResolvedTarget::Range { grid, .. } = resolved else {
                unreachable!("resolve_target always resolves UnmergeCells to a Range")
            };
            Ok(BatchUpdateRequestItem::UnmergeCells(UnmergeCellsRequest {
                range: *grid,
            }))
        }
        FormatVerb::AutoResizeDimension { .. } => {
            let ResolvedTarget::Dimension { range } = resolved else {
                unreachable!("resolve_target always resolves AutoResizeDimension to a Dimension")
            };
            Ok(BatchUpdateRequestItem::AutoResizeDimensions(
                AutoResizeDimensionsRequest {
                    dimensions: range.clone(),
                },
            ))
        }
        FormatVerb::UpdateDimensionProperties { pixel_size, .. } => {
            let ResolvedTarget::Dimension { range } = resolved else {
                unreachable!(
                    "resolve_target always resolves UpdateDimensionProperties to a Dimension"
                )
            };
            Ok(BatchUpdateRequestItem::UpdateDimensionProperties(
                UpdateDimensionPropertiesRequest {
                    range: range.clone(),
                    properties: DimensionProperties {
                        pixel_size: *pixel_size,
                    },
                    fields: "pixelSize".to_string(),
                },
            ))
        }
    }
}

fn record_attempt(outcome: &FormatOutcome, opts: &FormatOptions, duration: Duration) {
    let error = match &outcome.result {
        FormatResult::Failed { detail } => Some(detail.clone()),
        _ => None,
    };
    let decided_by = match &outcome.result {
        FormatResult::Blocked { decided_by } => decided_by.as_ref(),
        _ => None,
    };
    let decided_by = write_gate::decided_by_log_fields(decided_by);
    let (fields_changed, discarded_cells) = match &outcome.result {
        FormatResult::Changed {
            summary,
            discarded_cells,
        }
        | FormatResult::WouldChange {
            summary,
            discarded_cells,
        } => (Some(summary.clone()), discarded_cells.clone()),
        _ => (None, Vec::new()),
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
        discarded_cells,
        error,
        duration,
        ..Default::default()
    });
}

/// Renders an outcome as human-readable text.
#[must_use]
pub fn describe(outcome: &FormatOutcome) -> String {
    describe_lines(outcome).join("\n")
}

/// Renders an outcome as its individual lines, none of which contains a
/// newline — see `structure.rs::describe_lines` for why this shape exists.
#[must_use]
pub fn describe_lines(outcome: &FormatOutcome) -> Vec<String> {
    let verb = &outcome.verb;
    let book = outcome.file_name.as_deref().map_or_else(
        || format!("'{}'", outcome.spreadsheet_id),
        |n| format!("'{n}'"),
    );
    match &outcome.result {
        FormatResult::WouldChange { summary, .. } => {
            vec![format!("Would {summary} in {book}")]
        }
        FormatResult::RefusedNotASpreadsheet { mime_type } => vec![format!(
            "Refused: {book} is not a Google Sheet (mimeType: {mime_type}); \
             `drive sheets {}` only works on spreadsheets",
            verb.label()
        )],
        FormatResult::RefusedShortcut => vec![format!(
            "Refused: {book} is a shortcut; `drive sheets {}` doesn't follow shortcuts",
            verb.label()
        )],
        FormatResult::RefusedNoVisibleParents => vec![format!(
            "Refused: {book} has no parent folder visible to this account, so no folder \
             rule can apply to it. Grant it by id instead: add {{\"file_id\": \"<spreadsheet \
             id>\", \"allow\": [\"sheets-structure\"]}} to write_permissions.rules."
        )],
        FormatResult::RefusedSheetNotFound { title, available } => {
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
        FormatResult::RefusedInvalidRange { detail } => vec![format!("Refused: {detail}")],
        FormatResult::Blocked { decided_by } => vec![match decided_by {
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
        FormatResult::RefusedNoLease => vec![format!(
            "Refused: {book} requires a Drive write lease — run `omni-dev drive lease acquire \
             {}` and pass the printed token via `--lease`.",
            outcome.spreadsheet_id
        )],
        FormatResult::RefusedLeaseExpired => vec![format!(
            "Refused: the presented lease is expired, released, or unknown to this ledger — \
             run `omni-dev drive lease acquire {}` again.",
            outcome.spreadsheet_id
        )],
        FormatResult::RefusedLeaseWrongFile => vec![format!(
            "Refused: the presented lease was acquired for a different file — run `omni-dev \
             drive lease acquire {}` for this one.",
            outcome.spreadsheet_id
        )],
        FormatResult::RefusedLeaseStale => vec![format!(
            "Refused: {book} changed since the lease was acquired (or last written under) — \
             re-run `omni-dev drive lease acquire {}` to lease the current version.",
            outcome.spreadsheet_id
        )],
        FormatResult::Changed { summary, .. } => {
            vec![format!("{}: {summary} in {book}", capitalize(verb.label()))]
        }
        FormatResult::Failed { detail } => vec![format!("Failed: {detail}")],
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::sheets::client::SHEETS_API_URL;
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

    /// `version: "1"` throughout — matches [`format_cells_opts`]'s default
    /// seeded lease, so any test reaching the mutating call has a live,
    /// non-stale lease by construction (ADR-0080 §9).
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

    /// Seeds a fresh, isolated ledger with a live lease for `"sheet-1"` at
    /// version `"1"` (matching [`mount_file`]'s default) and returns options
    /// carrying it. Every existing test built before the lease (ADR-0080 §9)
    /// reaches its mutating call this way by construction — see
    /// `structure.rs::opts_for`'s doc comment for why this doesn't need
    /// touching each test individually. A test exercising the lease
    /// *refusal* paths builds `FormatOptions` directly instead (see the "the
    /// Drive write lease" test section below).
    fn format_cells_opts(dry_run: bool) -> FormatOptions {
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        let token = seed_lease(&ledger_path, "sheet-1", "1");
        FormatOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: FormatVerb::FormatCells {
                sheet: Some("Q1".to_string()),
                range: Some("A1:B2".to_string()),
                format: CellFormatFlags {
                    bold: Some(true),
                    ..Default::default()
                },
            },
            dry_run,
            lease_token: Some(token),
            ledger_path,
        }
    }

    /// Seeds `ledger_path` with a fresh, live lease for `spreadsheet_id` at
    /// `version`, returning its token.
    fn seed_lease(ledger_path: &std::path::Path, spreadsheet_id: &str, version: &str) -> String {
        // A fixed token, not a random one: every call gets its own isolated
        // ledger (a fresh tempdir), so uniqueness across tests is never a
        // concern.
        let token = "test-lease-token".to_string();
        let mut ledger = crate::drive::lease::ledger::LeaseLedger::default();
        ledger.insert(crate::drive::lease::ledger::LeaseRecord {
            token: token.clone(),
            file_id: spreadsheet_id.to_string(),
            version: version.to_string(),
            modified_time: None,
            backup: crate::drive::lease::ledger::LeaseBackup::Bytes {
                path: std::path::PathBuf::from("/tmp/test-backup"),
                sha256: "deadbeef".to_string(),
                size: 0,
            },
            acquired_at: chrono::Utc::now(),
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(30),
            released_at: None,
        });
        ledger.save(ledger_path).unwrap();
        token
    }

    #[tokio::test]
    async fn dry_run_reports_would_change_and_makes_no_batch_update_call() {
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

        let outcome = format(&drive, &sheets, &format_cells_opts(true), &rules).await;
        match outcome.result {
            FormatResult::WouldChange { summary, .. } => {
                assert!(summary.contains("bold"), "{summary}");
            }
            other => panic!("expected WouldChange, got {other:?}"),
        }
        // No batchUpdate mock is mounted, so a stray call would 404 and the
        // outcome would be `Failed` instead — the absence of that failure
        // is itself the assertion that dry-run made no mutating call.
    }

    #[tokio::test]
    async fn a_denied_gate_blocks_before_any_batch_update_call() {
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
        // No workbook or batch-update mock: a blocked attempt must reach
        // neither.
        let rules: Vec<FolderPermissionRule> = Vec::new();

        let outcome = format(&drive, &sheets, &format_cells_opts(false), &rules).await;
        match outcome.result {
            FormatResult::Blocked { decided_by } => assert!(decided_by.is_none()),
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn format_cells_sends_a_repeat_cell_request_with_no_value_field() {
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
        mount_batch_update(serde_json::json!({"spreadsheetId": "sheet-1", "replies": [{}]}))
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];

        let outcome = format(&drive, &sheets, &format_cells_opts(false), &rules).await;
        assert!(matches!(outcome.result, FormatResult::Changed { .. }));

        let requests = server.received_requests().await.unwrap();
        let batch = requests
            .iter()
            .find(|r| r.url.path().ends_with(":batchUpdate"))
            .expect("no batchUpdate request was sent");
        let body: serde_json::Value = serde_json::from_slice(&batch.body).unwrap();
        let repeat_cell = &body["requests"][0]["repeatCell"];
        assert_eq!(
            repeat_cell["cell"]["userEnteredFormat"]["textFormat"]["bold"],
            true
        );
        assert!(
            repeat_cell["cell"].get("userEnteredValue").is_none(),
            "repeatCell must never carry a value: {repeat_cell}"
        );
    }

    #[tokio::test]
    async fn merge_cells_dry_run_lists_discarded_cells() {
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
                "/v4/spreadsheets/sheet-1/values/'Q1'!A1:B2",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "values": [["keep", "gone"]]
                })),
            )
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];

        let opts = FormatOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: FormatVerb::MergeCells {
                sheet: Some("Q1".to_string()),
                range: Some("A1:B2".to_string()),
                merge_type: "MERGE_ALL".to_string(),
            },
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = format(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            FormatResult::WouldChange {
                discarded_cells, ..
            } => {
                assert_eq!(discarded_cells, vec!["B1: gone"]);
            }
            other => panic!("expected WouldChange, got {other:?}"),
        }
    }

    #[test]
    fn parse_hex_color_accepts_with_and_without_hash() {
        let a = parse_hex_color("#FF8800").unwrap();
        let b = parse_hex_color("ff8800").unwrap();
        assert!((a.red - b.red).abs() < f32::EPSILON);
        assert!((a.red - 1.0).abs() < 0.01);
        assert!((a.green - 0x88 as f32 / 255.0).abs() < 0.01);
        assert!((a.blue - 0.0).abs() < 0.01);
    }

    #[test]
    fn parse_hex_color_rejects_wrong_length_and_non_hex() {
        assert!(parse_hex_color("#FFF").is_err());
        assert!(parse_hex_color("#GGGGGG").is_err());
        assert!(parse_hex_color("").is_err());
    }

    #[test]
    fn build_cell_format_refuses_an_empty_flag_set() {
        let err = build_cell_format(&CellFormatFlags::default()).unwrap_err();
        assert!(err.contains("at least one property"), "{err}");
    }

    #[test]
    fn build_cell_format_masks_exactly_the_flags_given() {
        let flags = CellFormatFlags {
            bold: Some(true),
            background: Some("#FF0000".to_string()),
            ..Default::default()
        };
        let (format, fields) = build_cell_format(&flags).unwrap();
        assert_eq!(format.text_format.unwrap().bold, Some(true));
        assert!(format.background_color_style.is_some());
        assert_eq!(
            fields,
            "userEnteredFormat.textFormat.bold,userEnteredFormat.backgroundColorStyle"
        );
    }

    #[test]
    fn build_cell_format_requires_number_format_pattern_and_type_together() {
        let flags = CellFormatFlags {
            number_format_pattern: Some("0.00".to_string()),
            ..Default::default()
        };
        let err = build_cell_format(&flags).unwrap_err();
        assert!(err.contains("together"), "{err}");
    }

    #[test]
    fn discarded_from_values_skips_the_top_left_and_blanks() {
        let values: ValueRange = serde_json::from_value(serde_json::json!({
            "values": [["keep", "b1", ""], ["a2", null, "c2"]]
        }))
        .unwrap();
        let discarded = discarded_from_values(&values, 0, 0);
        assert_eq!(discarded, vec!["B1: b1", "A2: a2", "C2: c2"]);
    }

    #[test]
    fn discarded_from_values_offsets_addresses_by_the_ranges_own_start() {
        // A read of 'Q1'!C5:D10 is 0-indexed within that range, so an
        // address computed with no offset would misreport D5 as B1.
        let values: ValueRange = serde_json::from_value(serde_json::json!({
            "values": [["keep", "d5"]]
        }))
        .unwrap();
        let discarded = discarded_from_values(&values, 4, 2);
        assert_eq!(discarded, vec!["D5: d5"]);
    }

    #[tokio::test]
    async fn merge_cells_refuses_an_open_ended_range() {
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

        let opts = FormatOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: FormatVerb::MergeCells {
                sheet: Some("Q1".to_string()),
                range: Some("A:A".to_string()),
                merge_type: "MERGE_ALL".to_string(),
            },
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = format(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            FormatResult::RefusedInvalidRange { detail } => {
                assert!(detail.contains("open-ended"), "{detail}");
            }
            other => panic!("expected RefusedInvalidRange, got {other:?}"),
        }
        // No values.get mock is mounted, so a stray read would 404 and the
        // outcome would be `Failed` instead — the absence of that failure
        // is itself the assertion that the refusal happened before any
        // read of the (unbounded) target's values.
    }

    #[test]
    fn grid_range_to_a1_round_trips_a_bounded_range() {
        let grid = GridRange {
            sheet_id: 1,
            start_row_index: Some(0),
            end_row_index: Some(3),
            start_column_index: Some(0),
            end_column_index: Some(2),
        };
        assert_eq!(grid_range_to_a1("Q1", &grid), "'Q1'!A1:B3");
    }

    #[test]
    #[should_panic(expected = "merge-cells targets are refused earlier unless fully bounded")]
    fn grid_range_to_a1_panics_on_an_unbounded_range() {
        let grid = GridRange {
            sheet_id: 1,
            start_row_index: None,
            end_row_index: Some(3),
            start_column_index: Some(0),
            end_column_index: Some(2),
        };
        let _ = grid_range_to_a1("Q1", &grid);
    }

    // ── Verb helpers ─────────────────────────────────────────────────

    fn opts(verb: FormatVerb, dry_run: bool) -> FormatOptions {
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        let token = seed_lease(&ledger_path, "sheet-1", "1");
        FormatOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb,
            dry_run,
            lease_token: Some(token),
            ledger_path,
        }
    }

    fn update_borders_verb(sides: BorderSides, style: &str, color: Option<&str>) -> FormatVerb {
        FormatVerb::UpdateBorders {
            sheet: Some("Q1".to_string()),
            range: Some("A1:B2".to_string()),
            sides,
            style: style.to_string(),
            color: color.map(str::to_string),
        }
    }

    fn merge_cells_verb(range: &str) -> FormatVerb {
        FormatVerb::MergeCells {
            sheet: Some("Q1".to_string()),
            range: Some(range.to_string()),
            merge_type: "MERGE_ALL".to_string(),
        }
    }

    fn unmerge_cells_verb() -> FormatVerb {
        FormatVerb::UnmergeCells {
            sheet: Some("Q1".to_string()),
            range: Some("A1:B2".to_string()),
        }
    }

    fn auto_resize_verb(sheet: &str, start: i64, end: i64) -> FormatVerb {
        FormatVerb::AutoResizeDimension {
            sheet: sheet.to_string(),
            dimension: Dimension::Rows,
            start,
            end,
        }
    }

    fn update_dimension_verb(sheet: &str, start: i64, end: i64, pixel_size: i64) -> FormatVerb {
        FormatVerb::UpdateDimensionProperties {
            sheet: sheet.to_string(),
            dimension: Dimension::Columns,
            start,
            end,
            pixel_size,
        }
    }

    fn find_batch_update(requests: &[wiremock::Request]) -> serde_json::Value {
        let batch = requests
            .iter()
            .find(|r| r.url.path().ends_with(":batchUpdate"))
            .expect("no batchUpdate request was sent");
        serde_json::from_slice(&batch.body).unwrap()
    }

    // ── FormatVerb / BorderSides pure-function coverage ─────────────────

    #[test]
    fn border_sides_any_is_true_iff_at_least_one_side_is_set() {
        assert!(!BorderSides::default().any());
        assert!(BorderSides {
            top: true,
            ..Default::default()
        }
        .any());
        assert!(BorderSides {
            bottom: true,
            ..Default::default()
        }
        .any());
        assert!(BorderSides {
            left: true,
            ..Default::default()
        }
        .any());
        assert!(BorderSides {
            right: true,
            ..Default::default()
        }
        .any());
    }

    #[test]
    fn log_operation_and_label_are_distinct_for_every_verb() {
        let verbs = [
            format_cells_opts(false).verb,
            update_borders_verb(BorderSides::default(), "SOLID", None),
            merge_cells_verb("A1:B2"),
            unmerge_cells_verb(),
            auto_resize_verb("Q1", 1, 2),
            update_dimension_verb("Q1", 1, 2, 10),
        ];
        let ops: HashSet<&str> = verbs.iter().map(FormatVerb::log_operation).collect();
        assert_eq!(ops.len(), verbs.len());
        let labels: HashSet<&str> = verbs.iter().map(FormatVerb::label).collect();
        assert_eq!(labels.len(), verbs.len());
    }

    #[test]
    fn sheet_and_range_is_none_only_for_the_dimension_span_verbs() {
        assert_eq!(
            merge_cells_verb("A1:B2").sheet_and_range(),
            Some((Some("Q1"), Some("A1:B2")))
        );
        assert!(auto_resize_verb("Q1", 1, 2).sheet_and_range().is_none());
        assert!(update_dimension_verb("Q1", 1, 2, 10)
            .sheet_and_range()
            .is_none());
    }

    #[test]
    fn log_status_is_distinct_for_every_variant() {
        let statuses = [
            FormatResult::WouldChange {
                summary: "x".to_string(),
                discarded_cells: Vec::new(),
            }
            .log_status(),
            FormatResult::RefusedNotASpreadsheet {
                mime_type: "application/pdf".to_string(),
            }
            .log_status(),
            FormatResult::RefusedShortcut.log_status(),
            FormatResult::RefusedNoVisibleParents.log_status(),
            FormatResult::RefusedSheetNotFound {
                title: "x".to_string(),
                available: Vec::new(),
            }
            .log_status(),
            FormatResult::RefusedInvalidRange {
                detail: "x".to_string(),
            }
            .log_status(),
            FormatResult::Blocked { decided_by: None }.log_status(),
            FormatResult::RefusedNoLease.log_status(),
            FormatResult::RefusedLeaseExpired.log_status(),
            FormatResult::RefusedLeaseWrongFile.log_status(),
            FormatResult::RefusedLeaseStale.log_status(),
            FormatResult::Changed {
                summary: "x".to_string(),
                discarded_cells: Vec::new(),
            }
            .log_status(),
            FormatResult::Failed {
                detail: "x".to_string(),
            }
            .log_status(),
        ];
        let unique: HashSet<&str> = statuses.iter().copied().collect();
        assert_eq!(unique.len(), statuses.len());
        assert!(statuses.iter().all(|s| !s.is_empty()));
    }

    #[test]
    fn record_attempt_handles_a_would_change_outcome_without_panicking() {
        // `record_attempt` is only ever called for `!dry_run`, and
        // `WouldChange` only ever occurs when `dry_run` is true, so the real
        // pipeline never exercises this combination — call it directly, the
        // same bypass `structure.rs`'s `record_attempt` edge-case tests use.
        let opts = format_cells_opts(false);
        let outcome = FormatOutcome {
            spreadsheet_id: "sheet-1".to_string(),
            file_name: Some("Budget".to_string()),
            resolved_folder_id: None,
            verb: opts.verb.clone(),
            result: FormatResult::WouldChange {
                summary: "set bold".to_string(),
                discarded_cells: vec!["B1: gone".to_string()],
            },
        };
        record_attempt(&outcome, &opts, Duration::from_millis(1));
    }

    #[test]
    fn write_jsonl_serializes_the_outcome_as_one_json_line() {
        let outcome = FormatOutcome {
            spreadsheet_id: "sheet-1".to_string(),
            file_name: Some("Budget".to_string()),
            resolved_folder_id: None,
            verb: format_cells_opts(false).verb,
            result: FormatResult::Changed {
                summary: "set bold".to_string(),
                discarded_cells: Vec::new(),
            },
        };
        let mut buf = Vec::new();
        outcome.write_jsonl(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert_eq!(text.matches('\n').count(), 1);
        let value: serde_json::Value = serde_json::from_str(text.trim_end()).unwrap();
        assert_eq!(value["spreadsheet_id"], "sheet-1");
        assert_eq!(value["result"]["status"], "changed");
    }

    // ── build_cell_format: remaining flags ───────────────────────────────

    #[test]
    fn build_cell_format_covers_every_remaining_flag() {
        let flags = CellFormatFlags {
            italic: Some(true),
            strikethrough: Some(true),
            underline: Some(true),
            font_size: Some(14),
            text_color: Some("#112233".to_string()),
            horizontal_align: Some("CENTER".to_string()),
            vertical_align: Some("MIDDLE".to_string()),
            number_format_pattern: Some("0.00".to_string()),
            number_format_type: Some("NUMBER".to_string()),
            wrap: Some("WRAP".to_string()),
            ..Default::default()
        };
        let (format, fields) = build_cell_format(&flags).unwrap();
        let text_format = format.text_format.unwrap();
        assert_eq!(text_format.italic, Some(true));
        assert_eq!(text_format.strikethrough, Some(true));
        assert_eq!(text_format.underline, Some(true));
        assert_eq!(text_format.font_size, Some(14));
        assert!(text_format.foreground_color_style.is_some());
        assert_eq!(format.horizontal_alignment.as_deref(), Some("CENTER"));
        assert_eq!(format.vertical_alignment.as_deref(), Some("MIDDLE"));
        assert!(format.number_format.is_some());
        assert_eq!(format.wrap_strategy.as_deref(), Some("WRAP"));
        for expected in [
            "userEnteredFormat.textFormat.italic",
            "userEnteredFormat.textFormat.strikethrough",
            "userEnteredFormat.textFormat.underline",
            "userEnteredFormat.textFormat.fontSize",
            "userEnteredFormat.textFormat.foregroundColorStyle",
            "userEnteredFormat.horizontalAlignment",
            "userEnteredFormat.verticalAlignment",
            "userEnteredFormat.numberFormat",
            "userEnteredFormat.wrapStrategy",
        ] {
            assert!(fields.contains(expected), "{fields} missing {expected}");
        }
    }

    // ── build_request: defensive unreachable branches ────────────────────

    #[test]
    #[should_panic(expected = "FormatCells to a Range")]
    fn build_request_panics_if_format_cells_resolves_to_a_dimension() {
        let verb = format_cells_opts(false).verb;
        let resolved = ResolvedTarget::Dimension {
            range: DimensionRange {
                sheet_id: 0,
                dimension: Dimension::Rows,
                start_index: 0,
                end_index: 1,
            },
        };
        let _ = build_request(&verb, &resolved);
    }

    #[test]
    #[should_panic(expected = "UpdateBorders to a Range")]
    fn build_request_panics_if_update_borders_resolves_to_a_dimension() {
        let verb = update_borders_verb(
            BorderSides {
                top: true,
                ..Default::default()
            },
            "SOLID",
            None,
        );
        let resolved = ResolvedTarget::Dimension {
            range: DimensionRange {
                sheet_id: 0,
                dimension: Dimension::Rows,
                start_index: 0,
                end_index: 1,
            },
        };
        let _ = build_request(&verb, &resolved);
    }

    #[test]
    #[should_panic(expected = "MergeCells to a Range")]
    fn build_request_panics_if_merge_cells_resolves_to_a_dimension() {
        let resolved = ResolvedTarget::Dimension {
            range: DimensionRange {
                sheet_id: 0,
                dimension: Dimension::Rows,
                start_index: 0,
                end_index: 1,
            },
        };
        let _ = build_request(&merge_cells_verb("A1:B2"), &resolved);
    }

    #[test]
    #[should_panic(expected = "UnmergeCells to a Range")]
    fn build_request_panics_if_unmerge_cells_resolves_to_a_dimension() {
        let resolved = ResolvedTarget::Dimension {
            range: DimensionRange {
                sheet_id: 0,
                dimension: Dimension::Rows,
                start_index: 0,
                end_index: 1,
            },
        };
        let _ = build_request(&unmerge_cells_verb(), &resolved);
    }

    #[test]
    #[should_panic(expected = "AutoResizeDimension to a Dimension")]
    fn build_request_panics_if_auto_resize_resolves_to_a_range() {
        let resolved = ResolvedTarget::Range {
            sheet_title: "Q1".to_string(),
            grid: GridRange::default(),
        };
        let _ = build_request(&auto_resize_verb("Q1", 1, 2), &resolved);
    }

    #[test]
    #[should_panic(expected = "UpdateDimensionProperties to a Dimension")]
    fn build_request_panics_if_update_dimension_properties_resolves_to_a_range() {
        let resolved = ResolvedTarget::Range {
            sheet_title: "Q1".to_string(),
            grid: GridRange::default(),
        };
        let _ = build_request(&update_dimension_verb("Q1", 1, 2, 10), &resolved);
    }

    // ── read_discarded_cells: direct-call edge cases ─────────────────────

    #[tokio::test]
    async fn read_discarded_cells_returns_empty_for_a_non_range_target() {
        let server = wiremock::MockServer::start().await;
        let (_drive, sheets) = clients(&server).await;
        let api = SheetsApi::new(&sheets);
        let resolved = ResolvedTarget::Dimension {
            range: DimensionRange {
                sheet_id: 0,
                dimension: Dimension::Rows,
                start_index: 0,
                end_index: 1,
            },
        };
        let cells = read_discarded_cells(&api, "sheet-1", &resolved)
            .await
            .unwrap();
        assert!(cells.is_empty());
    }

    #[tokio::test]
    async fn read_discarded_cells_skips_the_read_for_a_single_cell_range() {
        let server = wiremock::MockServer::start().await;
        let (_drive, sheets) = clients(&server).await;
        let api = SheetsApi::new(&sheets);
        // No values.get mock mounted: a stray call would 404.
        let resolved = ResolvedTarget::Range {
            sheet_title: "Q1".to_string(),
            grid: GridRange {
                sheet_id: 0,
                start_row_index: Some(0),
                end_row_index: Some(1),
                start_column_index: Some(0),
                end_column_index: Some(1),
            },
        };
        let cells = read_discarded_cells(&api, "sheet-1", &resolved)
            .await
            .unwrap();
        assert!(cells.is_empty());
    }

    // ── format_inner: paths before the workbook fetch ────────────────────

    #[tokio::test]
    async fn a_conflicting_sheet_and_range_is_refused_before_any_drive_call() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        // No file/workbook mock at all: proves the refusal happens first.
        let rules: Vec<FolderPermissionRule> = Vec::new();
        let verb = FormatVerb::FormatCells {
            sheet: Some("Other".to_string()),
            range: Some("Sheet1!A1:B2".to_string()),
            format: CellFormatFlags {
                bold: Some(true),
                ..Default::default()
            },
        };

        let outcome = format(&drive, &sheets, &opts(verb, false), &rules).await;
        match outcome.result {
            FormatResult::RefusedInvalidRange { detail } => {
                assert!(detail.contains("already names a sheet"), "{detail}");
            }
            other => panic!("expected RefusedInvalidRange, got {other:?}"),
        }
        assert!(outcome.file_name.is_none());
    }

    #[tokio::test]
    async fn a_metadata_fetch_failure_is_reported_as_failed() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/sheet-1"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;
        let rules: Vec<FolderPermissionRule> = Vec::new();

        let outcome = format(&drive, &sheets, &format_cells_opts(false), &rules).await;
        assert!(matches!(outcome.result, FormatResult::Failed { .. }));
        assert!(outcome.file_name.is_none());
    }

    #[tokio::test]
    async fn a_non_spreadsheet_target_is_refused_by_mime_type() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", "application/pdf", &["folder-1"])
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];

        let outcome = format(&drive, &sheets, &format_cells_opts(false), &rules).await;
        match outcome.result {
            FormatResult::RefusedNotASpreadsheet { mime_type } => {
                assert_eq!(mime_type, "application/pdf");
            }
            other => panic!("expected RefusedNotASpreadsheet, got {other:?}"),
        }
        assert_eq!(outcome.file_name.as_deref(), Some("sheet-1"));
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
        let rules = vec![allow_rule("folder-1")];

        let outcome = format(&drive, &sheets, &format_cells_opts(false), &rules).await;
        assert!(matches!(outcome.result, FormatResult::RefusedShortcut));
    }

    #[tokio::test]
    async fn a_target_with_no_visible_parents_is_refused() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", crate::drive::types::GOOGLE_SHEET_MIME_TYPE, &[])
            .mount(&server)
            .await;
        let rules: Vec<FolderPermissionRule> = Vec::new();

        let outcome = format(&drive, &sheets, &format_cells_opts(false), &rules).await;
        assert!(matches!(
            outcome.result,
            FormatResult::RefusedNoVisibleParents
        ));
    }

    #[tokio::test]
    async fn a_gate_ancestor_fetch_failure_is_reported_as_failed() {
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
        let rules = vec![allow_rule("folder-1")];

        let outcome = format(&drive, &sheets, &format_cells_opts(false), &rules).await;
        assert!(matches!(outcome.result, FormatResult::Failed { .. }));
        assert_eq!(outcome.file_name.as_deref(), Some("sheet-1"));
    }

    #[tokio::test]
    async fn a_workbook_fetch_failure_is_reported_as_failed() {
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
        let rules = vec![allow_rule("folder-1")];

        let outcome = format(&drive, &sheets, &format_cells_opts(false), &rules).await;
        assert!(matches!(outcome.result, FormatResult::Failed { .. }));
    }

    // ── format_inner: target-resolution errors ───────────────────────────

    #[tokio::test]
    async fn format_cells_reports_sheet_not_found_for_a_range_target() {
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
        let verb = FormatVerb::FormatCells {
            sheet: Some("Nope".to_string()),
            range: Some("A1:B2".to_string()),
            format: CellFormatFlags {
                bold: Some(true),
                ..Default::default()
            },
        };

        let outcome = format(&drive, &sheets, &opts(verb, true), &rules).await;
        match outcome.result {
            FormatResult::RefusedSheetNotFound { title, available } => {
                assert_eq!(title, "Nope");
                assert_eq!(available, vec!["Q1".to_string()]);
            }
            other => panic!("expected RefusedSheetNotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn format_cells_reports_an_invalid_a1_token() {
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
        let verb = FormatVerb::FormatCells {
            sheet: Some("Q1".to_string()),
            range: Some("definitely bogus".to_string()),
            format: CellFormatFlags {
                bold: Some(true),
                ..Default::default()
            },
        };

        let outcome = format(&drive, &sheets, &opts(verb, true), &rules).await;
        match outcome.result {
            FormatResult::RefusedInvalidRange { detail } => {
                assert!(detail.contains("not a recognised A1 range"), "{detail}");
            }
            other => panic!("expected RefusedInvalidRange, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dimension_verb_reports_sheet_not_found() {
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

        let outcome = format(
            &drive,
            &sheets,
            &opts(auto_resize_verb("Nope", 1, 2), true),
            &rules,
        )
        .await;
        match outcome.result {
            FormatResult::RefusedSheetNotFound { title, available } => {
                assert_eq!(title, "Nope");
                assert_eq!(available, vec!["Q1".to_string()]);
            }
            other => panic!("expected RefusedSheetNotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dimension_verbs_reject_an_invalid_start_end_span() {
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

        for verb in [
            auto_resize_verb("Q1", 0, 5),
            update_dimension_verb("Q1", 5, 3, 10),
        ] {
            let outcome = format(&drive, &sheets, &opts(verb, true), &rules).await;
            match outcome.result {
                FormatResult::RefusedInvalidRange { detail } => {
                    assert!(detail.contains("--start"), "{detail}");
                }
                other => panic!("expected RefusedInvalidRange, got {other:?}"),
            }
        }
    }

    // ── format_inner: merge-cells discarded-values read ──────────────────

    #[tokio::test]
    async fn merge_cells_reports_a_values_read_failure_as_failed() {
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
                "/v4/spreadsheets/sheet-1/values/'Q1'!A1:B2",
            ))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];

        let outcome = format(
            &drive,
            &sheets,
            &opts(merge_cells_verb("A1:B2"), true),
            &rules,
        )
        .await;
        assert!(matches!(outcome.result, FormatResult::Failed { .. }));
    }

    #[tokio::test]
    async fn merge_cells_single_cell_range_has_no_discards_and_skips_the_values_read() {
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
        // No values.get mock mounted: a stray call would 404.
        let rules = vec![allow_rule("folder-1")];

        let outcome = format(
            &drive,
            &sheets,
            &opts(merge_cells_verb("A1:A1"), true),
            &rules,
        )
        .await;
        match outcome.result {
            FormatResult::WouldChange {
                summary,
                discarded_cells,
            } => {
                assert!(discarded_cells.is_empty());
                assert_eq!(summary, "merge (MERGE_ALL)");
            }
            other => panic!("expected WouldChange, got {other:?}"),
        }
    }

    // ── format_inner: describe_effect / build_request errors ─────────────

    #[tokio::test]
    async fn update_borders_refuses_when_no_side_is_selected() {
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
        let verb = update_borders_verb(BorderSides::default(), "SOLID", None);

        let outcome = format(&drive, &sheets, &opts(verb, false), &rules).await;
        match outcome.result {
            FormatResult::RefusedInvalidRange { detail } => {
                assert!(detail.contains("at least one side"), "{detail}");
            }
            other => panic!("expected RefusedInvalidRange, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn update_borders_rejects_a_malformed_color_at_request_build_time() {
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
        // No batchUpdate mock mounted: `describe_effect` doesn't validate
        // the color (only `sides.any()`), so this proves the request-build
        // step's own `parse_hex_color` call is the one that catches it.
        let rules = vec![allow_rule("folder-1")];
        let sides = BorderSides {
            top: true,
            ..Default::default()
        };
        let verb = update_borders_verb(sides, "SOLID", Some("ZZZZZZ"));

        let outcome = format(&drive, &sheets, &opts(verb, false), &rules).await;
        match outcome.result {
            FormatResult::RefusedInvalidRange { detail } => {
                assert!(detail.contains("not a color"), "{detail}");
            }
            other => panic!("expected RefusedInvalidRange, got {other:?}"),
        }
    }

    // ── format_inner: full non-dry-run round trips per verb ───────────────

    #[tokio::test]
    async fn update_borders_sets_every_selected_side() {
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
        mount_batch_update(serde_json::json!({"spreadsheetId": "sheet-1", "replies": [{}]}))
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let sides = BorderSides {
            top: true,
            bottom: true,
            left: true,
            right: true,
        };
        let verb = update_borders_verb(sides, "DASHED", Some("00FF00"));

        let outcome = format(&drive, &sheets, &opts(verb, false), &rules).await;
        match &outcome.result {
            FormatResult::Changed { summary, .. } => {
                assert!(summary.contains("top, bottom, left, right"), "{summary}");
            }
            other => panic!("expected Changed, got {other:?}"),
        }

        let requests = server.received_requests().await.unwrap();
        let body = find_batch_update(&requests);
        let update_borders = &body["requests"][0]["updateBorders"];
        for side in ["top", "bottom", "left", "right"] {
            assert_eq!(update_borders[side]["style"], "DASHED", "{update_borders}");
        }
    }

    #[tokio::test]
    async fn update_borders_defaults_to_black_when_no_color_is_given() {
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
        mount_batch_update(serde_json::json!({"spreadsheetId": "sheet-1", "replies": [{}]}))
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let sides = BorderSides {
            top: true,
            ..Default::default()
        };
        let verb = update_borders_verb(sides, "SOLID", None);

        let outcome = format(&drive, &sheets, &opts(verb, false), &rules).await;
        assert!(matches!(outcome.result, FormatResult::Changed { .. }));

        let requests = server.received_requests().await.unwrap();
        let body = find_batch_update(&requests);
        let color = &body["requests"][0]["updateBorders"]["top"]["colorStyle"]["rgbColor"];
        assert_eq!(color["red"], 0.0);
        assert_eq!(color["green"], 0.0);
        assert_eq!(color["blue"], 0.0);
    }

    #[tokio::test]
    async fn merge_cells_real_run_sends_merge_type_and_discards_are_still_reported() {
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
                "/v4/spreadsheets/sheet-1/values/'Q1'!A1:B2",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "values": [["keep", "gone"]]
                })),
            )
            .mount(&server)
            .await;
        mount_batch_update(serde_json::json!({"spreadsheetId": "sheet-1", "replies": [{}]}))
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];

        let outcome = format(
            &drive,
            &sheets,
            &opts(merge_cells_verb("A1:B2"), false),
            &rules,
        )
        .await;
        match &outcome.result {
            FormatResult::Changed {
                summary,
                discarded_cells,
            } => {
                assert_eq!(discarded_cells, &vec!["B1: gone".to_string()]);
                assert!(summary.contains("discarding 1 cell"), "{summary}");
            }
            other => panic!("expected Changed, got {other:?}"),
        }

        let requests = server.received_requests().await.unwrap();
        let body = find_batch_update(&requests);
        assert_eq!(body["requests"][0]["mergeCells"]["mergeType"], "MERGE_ALL");
    }

    #[tokio::test]
    async fn unmerge_cells_sends_the_composed_range() {
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
        mount_batch_update(serde_json::json!({"spreadsheetId": "sheet-1", "replies": [{}]}))
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];

        let outcome = format(&drive, &sheets, &opts(unmerge_cells_verb(), false), &rules).await;
        match &outcome.result {
            FormatResult::Changed { summary, .. } => assert_eq!(summary, "unmerge"),
            other => panic!("expected Changed, got {other:?}"),
        }

        let requests = server.received_requests().await.unwrap();
        let body = find_batch_update(&requests);
        assert!(body["requests"][0].get("unmergeCells").is_some());
    }

    #[tokio::test]
    async fn auto_resize_dimension_sends_the_resolved_sheet_id_and_span() {
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
        mount_batch_update(serde_json::json!({"spreadsheetId": "sheet-1", "replies": [{}]}))
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];

        let outcome = format(
            &drive,
            &sheets,
            &opts(auto_resize_verb("Q1", 2, 5), false),
            &rules,
        )
        .await;
        match &outcome.result {
            FormatResult::Changed { summary, .. } => {
                assert!(summary.contains("row(s)"), "{summary}");
            }
            other => panic!("expected Changed, got {other:?}"),
        }

        let requests = server.received_requests().await.unwrap();
        let body = find_batch_update(&requests);
        let dims = &body["requests"][0]["autoResizeDimensions"]["dimensions"];
        assert_eq!(dims["sheetId"], 0);
        assert_eq!(dims["startIndex"], 1);
        assert_eq!(dims["endIndex"], 5);
    }

    #[tokio::test]
    async fn update_dimension_properties_sends_the_pixel_size() {
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
        mount_batch_update(serde_json::json!({"spreadsheetId": "sheet-1", "replies": [{}]}))
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];

        let outcome = format(
            &drive,
            &sheets,
            &opts(update_dimension_verb("Q1", 1, 3, 120), false),
            &rules,
        )
        .await;
        match &outcome.result {
            FormatResult::Changed { summary, .. } => assert!(summary.contains("120"), "{summary}"),
            other => panic!("expected Changed, got {other:?}"),
        }

        let requests = server.received_requests().await.unwrap();
        let body = find_batch_update(&requests);
        let request = &body["requests"][0]["updateDimensionProperties"];
        assert_eq!(request["properties"]["pixelSize"], 120);
        assert_eq!(request["fields"], "pixelSize");
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
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];

        let outcome = format(&drive, &sheets, &format_cells_opts(false), &rules).await;
        assert!(matches!(outcome.result, FormatResult::Failed { .. }));
    }

    // ── describe_lines ────────────────────────────────────────────────

    fn every_format_result() -> Vec<FormatResult> {
        vec![
            FormatResult::WouldChange {
                summary: "set bold".to_string(),
                discarded_cells: vec!["B1: gone".to_string()],
            },
            FormatResult::RefusedNotASpreadsheet {
                mime_type: "application/pdf".to_string(),
            },
            FormatResult::RefusedShortcut,
            FormatResult::RefusedNoVisibleParents,
            FormatResult::RefusedSheetNotFound {
                title: "Q9".to_string(),
                available: vec!["Q1".to_string()],
            },
            FormatResult::RefusedSheetNotFound {
                title: "Q9".to_string(),
                available: Vec::new(),
            },
            FormatResult::RefusedInvalidRange {
                detail: "bad range".to_string(),
            },
            FormatResult::Blocked {
                decided_by: Some(DecidingRule::Folder {
                    folder_id: "folder-1".to_string(),
                    depth: 0,
                }),
            },
            FormatResult::Blocked { decided_by: None },
            FormatResult::RefusedNoLease,
            FormatResult::RefusedLeaseExpired,
            FormatResult::RefusedLeaseWrongFile,
            FormatResult::RefusedLeaseStale,
            FormatResult::Changed {
                summary: "set bold".to_string(),
                discarded_cells: Vec::new(),
            },
            FormatResult::Failed {
                detail: "boom".to_string(),
            },
        ]
    }

    #[test]
    fn describe_lines_covers_every_result_with_no_control_characters() {
        for file_name in [Some("Budget".to_string()), None] {
            for result in every_format_result() {
                // Compile-time exhaustiveness: a new variant fails to match.
                match &result {
                    FormatResult::WouldChange { .. }
                    | FormatResult::RefusedNotASpreadsheet { .. }
                    | FormatResult::RefusedShortcut
                    | FormatResult::RefusedNoVisibleParents
                    | FormatResult::RefusedSheetNotFound { .. }
                    | FormatResult::RefusedInvalidRange { .. }
                    | FormatResult::Blocked { .. }
                    | FormatResult::RefusedNoLease
                    | FormatResult::RefusedLeaseExpired
                    | FormatResult::RefusedLeaseWrongFile
                    | FormatResult::RefusedLeaseStale
                    | FormatResult::Changed { .. }
                    | FormatResult::Failed { .. } => {}
                }
                let outcome = FormatOutcome {
                    spreadsheet_id: "sheet-1".to_string(),
                    file_name: file_name.clone(),
                    resolved_folder_id: None,
                    verb: format_cells_opts(false).verb,
                    result,
                };
                let lines = describe_lines(&outcome);
                assert_eq!(lines.len(), 1, "{:?}", outcome.result);
                for line in &lines {
                    assert!(
                        !line.chars().any(char::is_control),
                        "{line:?} for {:?}",
                        outcome.result
                    );
                }
                assert_eq!(describe(&outcome), lines.join("\n"));
            }
        }
    }

    // ── the Drive write lease (ADR-0080 §9) ────────────────────────────

    #[tokio::test]
    async fn refuses_without_a_lease_when_the_rule_requires_one() {
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
        // No batchUpdate mock mounted — a refusal must make zero mutating
        // calls.
        let rules = vec![allow_rule("folder-1")];

        let opts = FormatOptions {
            lease_token: None,
            ..format_cells_opts(false)
        };
        let outcome = format(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, FormatResult::RefusedNoLease));
        assert_eq!(outcome.result.log_status(), "refused-no-lease");
    }

    #[tokio::test]
    async fn reports_a_lock_acquisition_failure_as_failed() {
        // A pre-existing lock file simulates another `drive lease`
        // operation genuinely in progress — reported as an operational
        // failure, not folded into `RefusedLeaseExpired`.
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

        let opts = format_cells_opts(false);
        let mut lock_path = opts.ledger_path.clone().into_os_string();
        lock_path.push(".lock");
        std::fs::write(std::path::PathBuf::from(lock_path), b"").unwrap();

        let outcome = format(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, FormatResult::Failed { .. }));
    }

    #[tokio::test]
    async fn a_failed_pre_lease_refetch_is_reported_as_failed_with_no_batch_update_call() {
        // The gate's own resolve step succeeds off the first `files.get`,
        // but the fresh re-fetch feeding the staleness check (ADR-0080 §6)
        // fails — the change must report `Failed` and never reach
        // `batchUpdate`.
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
            &["folder-1"],
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/sheet-1"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .with_priority(2)
            .mount(&server)
            .await;
        mount_folder("folder-1").mount(&server).await;
        mount_workbook().mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];

        let outcome = format(&drive, &sheets, &format_cells_opts(false), &rules).await;
        assert!(matches!(outcome.result, FormatResult::Failed { .. }));
    }

    #[tokio::test]
    async fn refuses_an_unknown_lease_token() {
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
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        // Never seeded — the ledger exists nowhere near this token.

        let opts = FormatOptions {
            lease_token: Some("bogus-token".to_string()),
            ledger_path,
            ..format_cells_opts(false)
        };
        let outcome = format(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, FormatResult::RefusedLeaseExpired));
    }

    #[tokio::test]
    async fn refuses_an_expired_lease() {
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
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        let token = "expired-token".to_string();
        let mut ledger = crate::drive::lease::ledger::LeaseLedger::default();
        ledger.insert(crate::drive::lease::ledger::LeaseRecord {
            token: token.clone(),
            file_id: "sheet-1".to_string(),
            version: "1".to_string(),
            modified_time: None,
            backup: crate::drive::lease::ledger::LeaseBackup::Bytes {
                path: std::path::PathBuf::from("/tmp/test-backup"),
                sha256: "deadbeef".to_string(),
                size: 0,
            },
            acquired_at: chrono::Utc::now() - chrono::Duration::hours(2),
            expires_at: chrono::Utc::now() - chrono::Duration::hours(1),
            released_at: None,
        });
        ledger.save(&ledger_path).unwrap();

        let opts = FormatOptions {
            lease_token: Some(token),
            ledger_path,
            ..format_cells_opts(false)
        };
        let outcome = format(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, FormatResult::RefusedLeaseExpired));
    }

    #[tokio::test]
    async fn refuses_a_lease_bound_to_a_different_file() {
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
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        // Seeded for a *different* spreadsheet id.
        let token = seed_lease(&ledger_path, "some-other-sheet", "1");

        let opts = FormatOptions {
            lease_token: Some(token),
            ledger_path,
            ..format_cells_opts(false)
        };
        let outcome = format(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            FormatResult::RefusedLeaseWrongFile
        ));
    }

    #[tokio::test]
    async fn refuses_a_stale_lease_when_the_file_has_moved() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        // `mount_file` always returns version "1"; the lease below was
        // acquired against version "0" — a foreign edit landed since.
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
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        let token = seed_lease(&ledger_path, "sheet-1", "0");

        let opts = FormatOptions {
            lease_token: Some(token),
            ledger_path,
            ..format_cells_opts(false)
        };
        let outcome = format(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, FormatResult::RefusedLeaseStale));
    }

    #[tokio::test]
    async fn require_lease_false_skips_the_lease_check_entirely() {
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
        mount_batch_update(serde_json::json!({"spreadsheetId": "sheet-1", "replies": [{}]}))
            .mount(&server)
            .await;
        let rule = FolderPermissionRule {
            folder_id: Some("folder-1".to_string()),
            file_id: None,
            recursive: true,
            allow: std::iter::once(DriveOperation::SheetsStructure).collect(),
            deny: HashSet::default(),
            require_lease: false,
        };

        // No lease token presented at all, and no ledger exists.
        let opts = FormatOptions {
            lease_token: None,
            ledger_path: std::path::PathBuf::from("/nonexistent/lease-ledger.jsonl"),
            ..format_cells_opts(false)
        };
        let outcome = format(&drive, &sheets, &opts, &[rule]).await;
        assert!(matches!(outcome.result, FormatResult::Changed { .. }));
    }
}
