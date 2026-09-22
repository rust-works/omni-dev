//! `text-to-columns` — splits a single column's delimited text into the
//! adjacent columns to its right via `spreadsheets.batchUpdate`'s
//! `textToColumns` request.
//!
//! Issue #1843, [ADR-0083](../../../docs/adrs/adr-0083.md). Unblocked by
//! issue #1831. Gated by **both** [`DriveOperation::SheetsWrite`] and
//! [`DriveOperation::SheetsStructure`], through
//! [`target_gate::resolve_all`] — the same union `pivot.rs`'s
//! `add-pivot-table` uses.
//!
//! ADR-0083 §1 proposed `SheetsWrite` alone, on the argument that the
//! split writes ordinary cell content and so does nothing a `sheets
//! clear` followed by a `sheets write` of the same span could not already
//! do. §5 made that provisional on one live-verification item, and the
//! live run settled it against the proposal: splitting a **bold, pink**
//! source column on a live workbook left every spill cell bold and pink,
//! where they had been unformatted before. A `sheets-write` grant cannot
//! confer that — formatting is exactly what `SheetsStructure` gates — so
//! the union is the honest gate and §5's fixed consequence applies. (The
//! same run also saw Sheets grow the sheet from 26 to 28 columns for a
//! spill past its last column, which `sheets append` already does under
//! `sheets-write` alone and so is not itself the reason.)
//!
//! **How many columns a split writes is never knowable exactly before the
//! request is sent, and this crate never sees the split pieces even after
//! a real run.** `textToColumns` carries no response object, and how many
//! columns the API's own splitting needs for each row is entirely its own
//! decision — the "API decides how many columns a `textToColumns` split
//! needs" limit ADR-0083 §6 names explicitly. What `--dry-run` (and the
//! real run) *can* say is an **upper bound** width, computed locally by a
//! naive, non-quote-aware split of the source's formatted values, and the
//! count and A1 locations of the non-blank cells within that upper bound
//! that would be (or were) overwritten — never their values or the split
//! pieces, matching every preview in this tranche but `merge-cells`.
//!
//! **`--delimiter auto` is the one exception to the bound.** For every
//! other delimiter the local split uses the same separator the API is
//! told to use, so it can only over-count; under `auto` the separator is
//! the API's own choice, and [`Delimiter::local_split_candidates`] can
//! only guess it by trying the four fixed types. Whether Sheets' own
//! detection is confined to those four is undocumented and unverified
//! (ADR-0083 §5's live-verification list), so an `auto` run's width is
//! reported as an estimate — see [`AUTO_DELIMITER_CAVEAT`], the extra
//! line those runs carry.
//!
//! ## `source` must span exactly one column
//!
//! The API's own constraint. This v1 additionally requires `source` to be
//! **fully bounded** (`A2:A100`, not the open-ended `A:A`) —
//! `merge-cells`'/`auto-fill`'s own requirement — since both the local
//! width computation and the preview read need a fixed row extent to work
//! from. The source column itself is excluded from the reported overwrite
//! list: its content is the input the split reads, not a casualty of the
//! request.
//!
//! ## The grid edge
//!
//! A spill that runs past the sheet's current `columnCount` is **not
//! refused client-side** — ADR-0083 §5: growth by writing past the grid's
//! edge is what `sheets append` already does under `sheets-write`. It is
//! surfaced as a caveat in the summary text instead, and the verb never
//! prepends an `updateSheetProperties`/`appendDimension` request to grow
//! the sheet first (ADR-0083 §5's smuggling rule).
//!
//! ## Shape
//!
//! Single-target, `auto_fill.rs`/`write.rs`'s linear shape: resolve the
//! target and gate, resolve the source range, read the source column once
//! to compute the upper-bound width, read the spill span (clamped to the
//! grid) once, dry-run return, build the request, lease, mutate, log.

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
    BatchUpdateRequestItem, DelimiterType, GridRange, Spreadsheet, TextToColumnsRequest, ValueRange,
};
use crate::drive::types::SheetTargetRefusal;
use crate::drive::write_gate::{self, DecidingRule, DriveOperation, FolderPermissionRule};
use crate::request_log::{self, DriveMutationOutcome};

/// The one operation this module ever logs or leases under — there is
/// only one verb, so `dimension_group.rs`'s per-verb `log_operation()`
/// dispatch would have nothing to dispatch on. Named once so the lease
/// site and the request-log site can't drift.
const LOG_OPERATION: &str = "sheets-text-to-columns";

/// The operations this verb's gate is the union of, in the order a
/// refusal reports them. Both are required: `SheetsWrite` for the cell
/// content the split writes, `SheetsStructure` for the source cell's
/// formatting it was measured carrying into the spill cells — see the
/// module docs for the live-run evidence.
const GATE_OPERATIONS: &[DriveOperation] =
    &[DriveOperation::SheetsWrite, DriveOperation::SheetsStructure];

/// Which separator to split on — the CLI/engine's own enum.
///
/// The API's `DELIMITER_TYPE_UNSPECIFIED` is undocumented and has no
/// default this crate resolves to, so every constructible value maps onto
/// exactly one wire-representable choice. [`Self::Custom`] carries its
/// separator text inline, so "custom with no text" and "a fixed delimiter
/// with stray text" are unrepresentable — `AutoFillForm`'s own
/// oneof-as-enum idiom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delimiter {
    /// Split on `,`.
    Comma,
    /// Split on `;`.
    Semicolon,
    /// Split on `.`.
    Period,
    /// Split on a single space.
    Space,
    /// Sheets picks the separator itself.
    Auto,
    /// Split on the carried separator text.
    Custom(String),
}

impl Delimiter {
    /// The wire `delimiterType`.
    fn wire_type(&self) -> DelimiterType {
        match self {
            Self::Comma => DelimiterType::Comma,
            Self::Semicolon => DelimiterType::Semicolon,
            Self::Period => DelimiterType::Period,
            Self::Space => DelimiterType::Space,
            Self::Auto => DelimiterType::Autodetect,
            Self::Custom(_) => DelimiterType::Custom,
        }
    }

    /// The literal separator(s) this module's local, non-quote-aware
    /// preview split tries — one for a fixed delimiter, the four fixed
    /// ones for [`Self::Auto`]. Unlike `auto_fill.rs`'s form-A upper
    /// bound, there is no fixed span to bound `Auto`'s guess against
    /// other than trying every fixed candidate and keeping the widest.
    /// That over-counts against whichever *one* of the four Sheets
    /// settles on, but it is only a bound at all while Sheets' detection
    /// stays within the four — undocumented and unverified, which is why
    /// an `Auto` run carries [`AUTO_DELIMITER_CAVEAT`] instead of
    /// claiming one.
    fn local_split_candidates(&self) -> Vec<&str> {
        match self {
            Self::Comma => vec![","],
            Self::Semicolon => vec![";"],
            Self::Period => vec!["."],
            Self::Space => vec![" "],
            Self::Custom(text) => vec![text.as_str()],
            Self::Auto => vec![",", ";", ".", " "],
        }
    }

    /// Human-readable name for summaries and log text.
    fn describe(&self) -> String {
        match self {
            Self::Comma => "comma".to_string(),
            Self::Semicolon => "semicolon".to_string(),
            Self::Period => "period".to_string(),
            Self::Space => "space".to_string(),
            Self::Auto => "auto-detected".to_string(),
            Self::Custom(text) => format!("custom ({text:?})"),
        }
    }
}

/// Per-call options.
#[derive(Debug, Clone)]
pub struct TextToColumnsOptions {
    /// Spreadsheet id.
    pub spreadsheet_id: String,
    /// Sheet (tab) title. Supplies the prefix for a bare `source`.
    pub sheet: Option<String>,
    /// A1 range holding the column to split, optionally carrying its own
    /// `Sheet!` prefix. Must resolve to a fully bounded, single-column
    /// range.
    pub source: Option<String>,
    /// Which separator to split on.
    pub delimiter: Delimiter,
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
pub enum TextToColumnsResult {
    /// `--dry-run`, and the gate would allow it.
    WouldChange {
        /// A human-readable, **tense-neutral** summary of the effect —
        /// also the request log's `fields_changed`. Names the source, the
        /// delimiter, and the upper-bound width and spill span; the
        /// overwrite count and the grid-extent caveat are separate,
        /// tense-varying lines built by [`describe_lines`], so this same
        /// string reads correctly under both `Would …` and `Applied: …`.
        summary: String,
        /// The source column's A1 address.
        source: String,
        /// The spill span's A1 address — the columns to the right of
        /// `source` a split of `width_upper_bound` columns would reach.
        /// `None` when the local split never produces more than one
        /// column, so nothing would be written beyond `source`.
        #[serde(skip_serializing_if = "Option::is_none")]
        spill: Option<String>,
        /// An upper bound on how many columns the split needs, computed
        /// locally with a naive, non-quote-aware split — never the
        /// server's real answer, which this crate cannot predict. Under
        /// [`Delimiter::Auto`] it is an estimate rather than a bound,
        /// and the rendered output says so
        /// ([`AUTO_DELIMITER_CAVEAT`]).
        width_upper_bound: usize,
        /// The non-blank cells within the spill span that would be
        /// overwritten, as bare A1 addresses — **never their values**,
        /// unlike `merge-cells`' own `discarded_cells` (ADR-0083 §6).
        /// Always an upper bound: the API decides for itself how many
        /// columns the split needs, so some of the listed cells may not
        /// actually be touched — and under [`Delimiter::Auto`] it picks
        /// the separator too, which can reach cells this list does not
        /// name ([`AUTO_DELIMITER_CAVEAT`]).
        #[serde(skip_serializing_if = "Vec::is_empty")]
        overwritten_cells: Vec<String>,
        /// The spill span runs past the sheet's current row/column count.
        /// Never a refusal (ADR-0083 §5) — it only earns the summary's
        /// grid-extent caveat line.
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
    /// The `--sheet`/`--source` pair, or the resolved range, was invalid —
    /// not fully bounded, or spanning more than one column.
    RefusedInvalidRange {
        /// What was wrong and why.
        detail: String,
    },
    /// `--custom-delimiter` was empty.
    RefusedInvalidDelimiter {
        /// What was wrong and why.
        detail: String,
    },
    /// The folder write-permission gate refused it.
    Blocked {
        /// Which of [`GATE_OPERATIONS`] denied first — the union gate
        /// refuses as soon as one of the two does, and which one it was
        /// is the only actionable part of the message.
        operation: DriveOperation,
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
        source: String,
        /// Same as [`Self::WouldChange`].
        #[serde(skip_serializing_if = "Option::is_none")]
        spill: Option<String>,
        /// Same as [`Self::WouldChange`].
        width_upper_bound: usize,
        /// Same as [`Self::WouldChange`] — read *before* the mutating call
        /// (the same read serves both paths), so this names what was
        /// overwritten, not a post-hoc guess.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        overwritten_cells: Vec<String>,
        /// Same as [`Self::WouldChange`].
        past_grid_extent: bool,
    },
    /// An API or validation error.
    Failed {
        /// A human-readable summary of what failed.
        detail: String,
    },
}

impl FromLeaseRefusal for TextToColumnsResult {
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

impl TextToColumnsResult {
    fn log_status(&self) -> &'static str {
        match self {
            Self::WouldChange { .. } => "would-change",
            Self::RefusedNotASpreadsheet { .. } => "refused-not-a-spreadsheet",
            Self::RefusedShortcut => "refused-shortcut",
            Self::RefusedNoVisibleParents => "refused-no-visible-parents",
            Self::RefusedSheetNotFound { .. } => "refused-sheet-not-found",
            Self::RefusedInvalidRange { .. } => "refused-invalid-range",
            Self::RefusedInvalidDelimiter { .. } => "refused-invalid-delimiter",
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
pub struct TextToColumnsOutcome {
    /// The spreadsheet acted on.
    pub spreadsheet_id: String,
    /// Its Drive file name, when the metadata fetch got that far.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    /// The folder the gate evaluated against.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_folder_id: Option<String>,
    /// The sheet the split is (or would be) on, once known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sheet_id: Option<i64>,
    /// Which delimiter was requested. Not serialised.
    #[serde(skip)]
    pub delimiter: Delimiter,
    /// What happened.
    pub result: TextToColumnsResult,
}

impl JsonlSerialize for TextToColumnsOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> anyhow::Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Runs one text-to-columns split, logging every attempt that isn't a dry
/// run.
pub async fn text_to_columns(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &TextToColumnsOptions,
    rules: &[FolderPermissionRule],
) -> TextToColumnsOutcome {
    let started = Instant::now();
    let outcome = text_to_columns_inner(drive, sheets, opts, rules).await;
    if !opts.dry_run {
        record_attempt(&outcome, started.elapsed());
    }
    outcome
}

async fn text_to_columns_inner(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &TextToColumnsOptions,
    rules: &[FolderPermissionRule],
) -> TextToColumnsOutcome {
    let bare = |result| TextToColumnsOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: None,
        resolved_folder_id: None,
        sheet_id: None,
        delimiter: opts.delimiter.clone(),
        result,
    };

    // Compose the range and validate the delimiter first, like
    // `auto_fill_inner`/`write_inner`: pure, and a conflicting
    // --sheet/--source pair or an empty --custom-delimiter should fail
    // without spending a request.
    let composed = match a1::compose(opts.sheet.as_deref(), opts.source.as_deref()) {
        Ok(composed) => composed,
        Err(err) => {
            return bare(TextToColumnsResult::RefusedInvalidRange {
                detail: err.to_string(),
            })
        }
    };
    if let Delimiter::Custom(text) = &opts.delimiter {
        if text.is_empty() {
            return bare(TextToColumnsResult::RefusedInvalidDelimiter {
                detail: "--custom-delimiter must not be empty".to_string(),
            });
        }
    }

    // ── Target resolution, pre-gate refusals, and the gate itself ──────
    let (target, verdict, denied, resolved_folder_id, requires_lease) =
        match target_gate::resolve_all(drive, &opts.spreadsheet_id, GATE_OPERATIONS, rules).await {
            target_gate::TargetGateUnionOutcome::MetadataFetchFailed { detail } => {
                return bare(TextToColumnsResult::Failed { detail })
            }
            target_gate::TargetGateUnionOutcome::Refused { target, refusal } => {
                let result = match refusal {
                    SheetTargetRefusal::Shortcut => TextToColumnsResult::RefusedShortcut,
                    SheetTargetRefusal::NotASpreadsheet { mime_type } => {
                        TextToColumnsResult::RefusedNotASpreadsheet { mime_type }
                    }
                    SheetTargetRefusal::NoVisibleParents => {
                        TextToColumnsResult::RefusedNoVisibleParents
                    }
                };
                return TextToColumnsOutcome {
                    spreadsheet_id: opts.spreadsheet_id.clone(),
                    file_name: Some(target.name),
                    resolved_folder_id: None,
                    sheet_id: None,
                    delimiter: opts.delimiter.clone(),
                    result,
                };
            }
            target_gate::TargetGateUnionOutcome::GateFetchFailed { target, detail } => {
                return TextToColumnsOutcome {
                    spreadsheet_id: opts.spreadsheet_id.clone(),
                    file_name: Some(target.name),
                    resolved_folder_id: None,
                    sheet_id: None,
                    delimiter: opts.delimiter.clone(),
                    result: TextToColumnsResult::Failed { detail },
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

    let pre_gated = |result| TextToColumnsOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: Some(target.name.clone()),
        resolved_folder_id: resolved_folder_id.clone(),
        sheet_id: None,
        delimiter: opts.delimiter.clone(),
        result,
    };

    if verdict == write_gate::Verdict::Deny {
        // `denied` is `Some` whenever the verdict is `Deny`; the fallback
        // names the first operation rather than inventing one, matching
        // `pivot.rs`'s own arm.
        let (operation, decided_by) = denied.unwrap_or((GATE_OPERATIONS[0], None));
        return pre_gated(TextToColumnsResult::Blocked {
            operation,
            decided_by,
        });
    }

    let api = SheetsApi::new(sheets);
    let workbook = match api.get_spreadsheet(&opts.spreadsheet_id).await {
        Ok(workbook) => workbook,
        Err(err) => {
            return pre_gated(TextToColumnsResult::Failed {
                detail: format!("{err:#}"),
            })
        }
    };

    let (sheet_title, source_grid) = match grid_range::resolve_grid_range(
        &workbook,
        &composed,
        |detail| TextToColumnsResult::RefusedInvalidRange { detail },
        |title, available| TextToColumnsResult::RefusedSheetNotFound { title, available },
    ) {
        Ok(resolved) => resolved,
        Err(result) => return pre_gated(result),
    };

    let gated = |result| TextToColumnsOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: Some(target.name.clone()),
        resolved_folder_id: resolved_folder_id.clone(),
        sheet_id: Some(source_grid.sheet_id),
        delimiter: opts.delimiter.clone(),
        result,
    };

    if !grid_range::is_bounded(&source_grid) {
        return gated(TextToColumnsResult::RefusedInvalidRange {
            detail: format!(
                "'{composed}' is open-ended; text-to-columns needs a fully bounded single \
                 column (e.g. A2:A100)"
            ),
        });
    }
    if !is_single_column(&source_grid) {
        return gated(TextToColumnsResult::RefusedInvalidRange {
            detail: format!(
                "'{composed}' spans more than one column; text-to-columns' source must span \
                 exactly one column"
            ),
        });
    }

    // `source_grid` is fully bounded, so this always renders.
    let source_a1 = grid_range::bounded_range_to_a1(&sheet_title, &source_grid).unwrap_or(composed);

    let source_values = match api
        .values_get(
            &opts.spreadsheet_id,
            &source_a1,
            ValueRenderOption::Formatted,
        )
        .await
    {
        Ok(values) => values,
        Err(err) => {
            return gated(TextToColumnsResult::Failed {
                detail: format!("{err:#}"),
            })
        }
    };

    let width_upper_bound = split_width(&source_values, &opts.delimiter);
    let spill_grid = spill_span(source_grid, width_upper_bound);
    let past_grid_extent = spill_grid.is_some_and(|spill| extends_past_grid(&workbook, &spill));

    // The *read* is clamped to the grid; the request is not (ADR-0083
    // §5's "left to the server" stance, `auto_fill.rs`'s own precedent).
    let (spill_a1, overwritten_cells) = match spill_grid {
        None => (None, Vec::new()),
        Some(spill_grid) => {
            let overwritten_cells = match grid_range::clamp_to_sheet(&workbook, &spill_grid) {
                None => Vec::new(),
                Some(read_grid) => {
                    let read_a1 = grid_range::bounded_range_to_a1(&sheet_title, &read_grid)
                        .unwrap_or_else(|| sheet_title.clone());
                    match api
                        .values_get(&opts.spreadsheet_id, &read_a1, ValueRenderOption::Formatted)
                        .await
                    {
                        Ok(values) => grid_range::non_blank_locations(
                            &values,
                            read_grid.start_row_index.unwrap_or(0),
                            read_grid.start_column_index.unwrap_or(0),
                        ),
                        Err(err) => {
                            return gated(TextToColumnsResult::Failed {
                                detail: format!("{err:#}"),
                            })
                        }
                    }
                }
            };
            (
                grid_range::bounded_range_to_a1(&sheet_title, &spill_grid),
                overwritten_cells,
            )
        }
    };

    let summary = describe_effect(
        &source_a1,
        spill_a1.as_deref(),
        width_upper_bound,
        &opts.delimiter,
    );

    if opts.dry_run {
        return gated(TextToColumnsResult::WouldChange {
            summary,
            source: source_a1,
            spill: spill_a1,
            width_upper_bound,
            overwritten_cells,
            past_grid_extent,
        });
    }

    // Built before the gate check that follows, not after — #1688/#1742's
    // ordering invariant, shared verbatim by every leased engine.
    let request = build_request(source_grid, &opts.delimiter);

    let files_api = FilesApi::new(drive);
    let leased = LeasedWrite {
        log_prefix: "drive sheets text-to-columns",
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
        Ok(_response) => TextToColumnsResult::Changed {
            summary,
            source: source_a1,
            spill: spill_a1,
            width_upper_bound,
            overwritten_cells,
            past_grid_extent,
        },
        Err(err) => TextToColumnsResult::Failed {
            detail: format!("{err:#}"),
        },
    };
    drop(lease_grant);
    gated(result)
}

/// Whether `grid` names exactly one column. Only ever called after
/// [`grid_range::is_bounded`] has confirmed all four indices are `Some`.
fn is_single_column(grid: &GridRange) -> bool {
    grid.end_column_index.unwrap_or(0) - grid.start_column_index.unwrap_or(0) == 1
}

/// A cell's text content for the local split: its string form if the API
/// returned one (the normal case — this module always reads with
/// [`ValueRenderOption::Formatted`], which renders every cell as a
/// string), empty for a blank/absent cell, or the raw JSON rendering for
/// any other shape, so a cell this crate does not expect to see still
/// contributes *some* text rather than silently vanishing from the width
/// count — [`split_width`]'s "never under-counts" guarantee depends on
/// this never returning less than the truth.
fn cell_text(cell: &serde_json::Value) -> String {
    match cell.as_str() {
        Some(text) => text.to_string(),
        None if cell.is_null() => String::new(),
        None => cell.to_string(),
    }
}

/// Upper bound on how many columns the split needs, computed locally with
/// a naive, non-quote-aware `str::split` over the source's values. For
/// every delimiter but [`Delimiter::Auto`] this can only ever over-count
/// against the server's real behaviour — a quoted delimiter or a run of
/// consecutive separators splits further locally than Sheets may actually
/// split it — never under-count, which is what makes the destination this
/// computes an upper bound rather than a guess (ADR-0083 §6). Under
/// `Auto` the separator is Sheets' own choice rather than a given, so the
/// same reasoning yields an estimate; see [`AUTO_DELIMITER_CAVEAT`]. A
/// blank source row contributes no width: there is nothing in it to
/// split. An empty source (no rows at all) is width `0`.
fn split_width(values: &ValueRange, delimiter: &Delimiter) -> usize {
    let candidates = delimiter.local_split_candidates();
    values
        .values
        .iter()
        .filter_map(|row| row.first())
        .map(cell_text)
        .filter(|text| !text.is_empty())
        .map(|text| {
            candidates
                .iter()
                .map(|sep| text.split(sep).count())
                .max()
                .unwrap_or(1)
        })
        .max()
        .unwrap_or(0)
}

/// The columns to the right of a bounded, single-column `source` that a
/// split of `width` columns would write into — `None` when `width <= 1`,
/// since a source that never splits writes nothing beyond itself. Pure;
/// `source`'s own column stays out of the result, since its content is
/// the input the split reads, not a cell the request overwrites.
fn spill_span(source: GridRange, width: usize) -> Option<GridRange> {
    if width <= 1 {
        return None;
    }
    let start_column = source.start_column_index?;
    let width = i64::try_from(width).ok()?;
    Some(GridRange {
        sheet_id: source.sheet_id,
        start_row_index: source.start_row_index,
        end_row_index: source.end_row_index,
        start_column_index: Some(start_column + 1),
        end_column_index: Some(start_column + width),
    })
}

/// Whether `extent` runs past `workbook`'s currently allocated rows or
/// columns on `extent`'s sheet — `paste.rs`'s own
/// `sheet_exceeds_dimensions`/`auto_fill.rs`'s own `extends_past_grid`,
/// this module's own copy since each is reached from a different
/// construction pass. `None` on either axis (the API reported no grid
/// dimensions) never counts as exceeding — there's nothing to compare
/// against.
fn extends_past_grid(workbook: &Spreadsheet, extent: &GridRange) -> bool {
    let Some(sheet) = grid_range::find_sheet_by_id(workbook, extent.sheet_id) else {
        return false;
    };
    let grid = sheet
        .properties
        .as_ref()
        .and_then(|props| props.grid_properties.as_ref());
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

/// The caveat every text-to-columns split carries, in both tenses at
/// once — the split pieces are never reported, before *or* after the
/// request, so unlike [`PAST_GRID_EXTENT_CAVEAT_DRY_RUN`] this is one
/// constant rather than two.
const UNPREVIEWABLE_SPLIT_CAVEAT: &str =
    "  the number of columns the split needs, and the values it writes, are computed by \
     Sheets' own splitting and are never reported, before or after the request; the count above \
     is a local upper-bound estimate only";

/// Shared by the `--dry-run` and post-execution lines so the wording
/// can't drift; tense differs, so this is two constants rather than one —
/// `structure.rs`'s `INSERT_RANGE_EDGE_CAVEAT` pair, for the same reason.
const PAST_GRID_EXTENT_CAVEAT_DRY_RUN: &str =
    "  the spill extends past the sheet's current extent — Sheets may grow the sheet or refuse \
     the request";
const PAST_GRID_EXTENT_CAVEAT: &str =
    "  the spill extended past the sheet's current extent, so Sheets may have grown it";

/// The extra caveat [`Delimiter::Auto`] earns, in both tenses at once.
///
/// Every other delimiter's width is a true upper bound: the local split
/// is the same separator the API is told to use, and a naive
/// `str::split` can only ever find more pieces than a quote-aware one
/// (see [`split_width`]). `auto` is the one case where that argument
/// does not close, because the separator itself is the API's choice, and
/// [`Delimiter::local_split_candidates`] can only guess it by trying the
/// four fixed types.
///
/// **This is measured, not hypothetical.** On a live workbook, a column
/// of tab-separated cells previewed under `auto` as "no row spills
/// beyond the source" with an empty overwrite list — and the real
/// request split it into three columns, destroying two cells the preview
/// had just reported as safe. Sheets' detection is *not* confined to the
/// four this preview tries, so under `auto` the overwrite list is
/// neither an upper bound nor a lower one, and nothing rendered for an
/// `auto` run may read as a reassurance. That is what
/// [`overwritten_line`] suppresses and this line replaces.
const AUTO_DELIMITER_CAVEAT: &str =
    "  --delimiter auto lets Sheets detect the separator itself, and it detects separators this \
     preview does not try (a tab-separated column splits under auto, though none of comma, \
     semicolon, period or space appears in it) — so for auto the width above and the cells \
     listed are a guess in both directions, not a bound";

/// The **tense-neutral** head of the summary: the source, the delimiter,
/// and the upper-bound width and spill span (or the "nothing to spill"
/// note when the local split never exceeds one column).
///
/// Deliberately carries neither the overwrite count nor the grid-extent
/// caveat. Both vary with tense, and this string is reused verbatim by
/// [`TextToColumnsResult::Changed`] and by the request log's
/// `fields_changed`, where a conditional ("would be overwritten") would
/// describe a mutation that already happened — `auto_fill.rs::describe_effect`'s
/// own reasoning.
fn describe_effect(
    source_a1: &str,
    spill_a1: Option<&str>,
    width_upper_bound: usize,
    delimiter: &Delimiter,
) -> String {
    match spill_a1 {
        Some(spill_a1) => format!(
            "split {source_a1} on {} into up to {width_upper_bound} column(s), spill {spill_a1}",
            delimiter.describe(),
        ),
        // Under `Auto` the local candidates found nothing to split on,
        // which says nothing about what Sheets will detect — a live run
        // split a tab-separated column this branch had just called
        // single-column. Claiming "no row spills" there would be the
        // same false all-clear `overwritten_line` suppresses.
        None if matches!(delimiter, Delimiter::Auto) => format!(
            "split {source_a1} on {}; no separator this preview tries appears in the source, \
             so the spill span is unknown",
            delimiter.describe(),
        ),
        None => format!(
            "split {source_a1} on {} into a single column each row; no row spills beyond \
             the source under this delimiter",
            delimiter.describe(),
        ),
    }
}

/// The indented overwrite line. Always prefixed `up to`: the API decides
/// for itself how many columns each row's split needs, so the local
/// upper-bound span can list cells the real split never reaches. The
/// tense follows `dry_run`, the [`PAST_GRID_EXTENT_CAVEAT_DRY_RUN`] pair's
/// own rule.
///
/// The empty case is deliberately **not** rendered for
/// [`Delimiter::Auto`]: "no non-blank cells in the spill columns" is an
/// affirmative all-clear, and that is the exact sentence a live `auto`
/// run printed immediately before overwriting two cells (see
/// [`AUTO_DELIMITER_CAVEAT`]). An `auto` run with nothing to list says
/// nothing rather than something false; its caveat line carries the
/// meaning.
fn overwritten_line(
    overwritten_cells: &[String],
    delimiter: &Delimiter,
    dry_run: bool,
) -> Option<String> {
    if overwritten_cells.is_empty() {
        return match delimiter {
            Delimiter::Auto => None,
            _ => Some("  no non-blank cells in the spill columns".to_string()),
        };
    }
    let tense = if dry_run {
        "would be overwritten"
    } else {
        "were overwritten"
    };
    Some(format!(
        "  up to {} non-blank cell(s) {tense}: {}",
        overwritten_cells.len(),
        overwritten_cells.join(", ")
    ))
}

/// Builds the `textToColumns` request. Placed after the `--dry-run`
/// branch in `text_to_columns_inner` deliberately — see
/// `banding.rs::banding_inner`'s comment just before its own call site
/// for the full `#1688` ordering reasoning, shared verbatim by every
/// leased engine.
fn build_request(source: GridRange, delimiter: &Delimiter) -> BatchUpdateRequestItem {
    let delimiter_text = match delimiter {
        Delimiter::Custom(text) => Some(text.clone()),
        _ => None,
    };
    BatchUpdateRequestItem::TextToColumns(TextToColumnsRequest {
        source,
        delimiter: delimiter_text,
        delimiter_type: delimiter.wire_type(),
    })
}

fn record_attempt(outcome: &TextToColumnsOutcome, duration: Duration) {
    let error = match &outcome.result {
        TextToColumnsResult::Failed { detail } => Some(detail.clone()),
        _ => None,
    };
    let decided_by = match &outcome.result {
        TextToColumnsResult::Blocked { decided_by, .. } => decided_by.as_ref(),
        _ => None,
    };
    let decided_by = write_gate::decided_by_log_fields(decided_by);
    let (range, fields_changed, overwritten_cells) = match &outcome.result {
        TextToColumnsResult::Changed {
            summary,
            source,
            overwritten_cells,
            ..
        } => (
            Some(source.clone()),
            Some(summary.clone()),
            overwritten_cells.clone(),
        ),
        _ => (None, None, Vec::new()),
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
        // Always an upper bound for this verb — the API decides for
        // itself how many columns each row's split needs, unlike
        // `auto-fill`'s form B, where the destination is exact. Under
        // `--delimiter auto` it is not even that (the separator is the
        // API's choice too), but the key has no third state and the
        // rendered summary carries `AUTO_DELIMITER_CAVEAT` instead.
        overwritten_cells_upper_bound: true,
        error,
        duration,
        ..Default::default()
    });
}

/// Renders an outcome as human-readable text.
#[must_use]
pub fn describe(outcome: &TextToColumnsOutcome) -> String {
    describe_lines(outcome).join("\n")
}

/// Renders an outcome as its individual lines, none of which contains a
/// newline.
#[must_use]
pub fn describe_lines(outcome: &TextToColumnsOutcome) -> Vec<String> {
    let book = outcome.file_name.as_deref().map_or_else(
        || format!("'{}'", outcome.spreadsheet_id),
        |n| format!("'{n}'"),
    );
    match &outcome.result {
        TextToColumnsResult::WouldChange {
            summary,
            overwritten_cells,
            past_grid_extent,
            ..
        } => change_lines(
            &format!("Would {summary} in {book}"),
            overwritten_cells,
            *past_grid_extent,
            &outcome.delimiter,
            true,
        ),
        TextToColumnsResult::RefusedNotASpreadsheet { mime_type } => vec![format!(
            "Refused: {book} is not a Google Sheet (mimeType: {mime_type}); \
             `drive sheets text-to-columns` only works on spreadsheets"
        )],
        TextToColumnsResult::RefusedShortcut => vec![format!(
            "Refused: {book} is a shortcut; `drive sheets text-to-columns` doesn't follow \
             shortcuts"
        )],
        TextToColumnsResult::RefusedNoVisibleParents => vec![format!(
            "Refused: {book} has no parent folder visible to this account, so no folder \
             rule can apply to it. Grant it by id instead: add {{\"file_id\": \"<spreadsheet \
             id>\", \"allow\": [\"sheets-write\"]}} to write_permissions.rules."
        )],
        TextToColumnsResult::RefusedSheetNotFound { title, available } => {
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
        TextToColumnsResult::RefusedInvalidRange { detail } => vec![format!("Refused: {detail}")],
        TextToColumnsResult::RefusedInvalidDelimiter { detail } => {
            vec![format!("Refused: {detail}")]
        }
        TextToColumnsResult::Blocked {
            operation,
            decided_by,
        } => vec![match decided_by {
            Some(rule) => format!(
                "Blocked: text-to-columns on {book} refused by rule on {} {}{}",
                rule.kind_label(),
                rule.id(),
                rule.depth_suffix()
            ),
            None => format!(
                "Blocked: text-to-columns on {book} refused by default policy (no matching rule \
                 for {operation})"
            ),
        }],
        TextToColumnsResult::RefusedNoLease => LeaseGateRefusal::NoLease
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        TextToColumnsResult::RefusedLeaseExpired => LeaseGateRefusal::Expired
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        TextToColumnsResult::RefusedLeaseWrongFile => LeaseGateRefusal::WrongFile
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        TextToColumnsResult::RefusedLeaseStale => LeaseGateRefusal::Stale
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        TextToColumnsResult::Changed {
            summary,
            overwritten_cells,
            past_grid_extent,
            ..
        } => change_lines(
            &format!("Applied: {summary} in {book}"),
            overwritten_cells,
            *past_grid_extent,
            &outcome.delimiter,
            false,
        ),
        TextToColumnsResult::Failed { detail } => vec![format!("Failed: {detail}")],
    }
}

/// The head line plus its indented detail lines, shared by
/// [`TextToColumnsResult::WouldChange`] and [`TextToColumnsResult::Changed`]
/// so the two can only differ in `head` and in the tense `dry_run`
/// selects — `structure.rs`'s `vec![summary, detail]` shape. `delimiter`
/// is read only to decide whether [`AUTO_DELIMITER_CAVEAT`] applies.
fn change_lines(
    head: &str,
    overwritten_cells: &[String],
    past_grid_extent: bool,
    delimiter: &Delimiter,
    dry_run: bool,
) -> Vec<String> {
    let mut lines = vec![head.to_string()];
    lines.extend(overwritten_line(overwritten_cells, delimiter, dry_run));
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
    if matches!(delimiter, Delimiter::Auto) {
        lines.push(AUTO_DELIMITER_CAVEAT.to_string());
    }
    lines.push(UNPREVIEWABLE_SPLIT_CAVEAT.to_string());
    lines
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

    fn values(rows: &[&str]) -> ValueRange {
        ValueRange {
            range: None,
            values: rows
                .iter()
                .map(|cell| {
                    if cell.is_empty() {
                        vec![]
                    } else {
                        vec![serde_json::json!(cell)]
                    }
                })
                .collect(),
        }
    }

    /// Every constructible [`Delimiter`] maps onto exactly one
    /// wire-representable choice, tries the separator(s) that choice
    /// implies, and has a name for the summary. Table-driven so a new
    /// variant cannot be added with one of the three left behind.
    #[test]
    fn every_delimiter_maps_to_a_wire_type_its_candidates_and_a_name() {
        let cases = [
            (Delimiter::Comma, DelimiterType::Comma, vec![","], "comma"),
            (
                Delimiter::Semicolon,
                DelimiterType::Semicolon,
                vec![";"],
                "semicolon",
            ),
            (
                Delimiter::Period,
                DelimiterType::Period,
                vec!["."],
                "period",
            ),
            (Delimiter::Space, DelimiterType::Space, vec![" "], "space"),
            (
                Delimiter::Auto,
                DelimiterType::Autodetect,
                vec![",", ";", ".", " "],
                "auto-detected",
            ),
            (
                Delimiter::Custom("||".to_string()),
                DelimiterType::Custom,
                vec!["||"],
                "custom (\"||\")",
            ),
        ];
        for (delimiter, wire, candidates, name) in cases {
            assert_eq!(delimiter.wire_type(), wire, "{delimiter:?}");
            assert_eq!(
                delimiter.local_split_candidates(),
                candidates,
                "{delimiter:?}"
            );
            assert_eq!(delimiter.describe(), name, "{delimiter:?}");
        }
    }

    /// The log status is the only field the request log keys refusals
    /// on, and the four lease statuses must be the shared ones rather
    /// than this module's own spelling of them.
    #[test]
    fn every_result_variant_has_its_own_log_status() {
        let statuses = [
            TextToColumnsResult::RefusedNotASpreadsheet {
                mime_type: String::new(),
            },
            TextToColumnsResult::RefusedShortcut,
            TextToColumnsResult::RefusedNoVisibleParents,
            TextToColumnsResult::RefusedSheetNotFound {
                title: String::new(),
                available: Vec::new(),
            },
            TextToColumnsResult::RefusedInvalidRange {
                detail: String::new(),
            },
            TextToColumnsResult::RefusedInvalidDelimiter {
                detail: String::new(),
            },
            TextToColumnsResult::Blocked {
                operation: DriveOperation::SheetsWrite,
                decided_by: None,
            },
            TextToColumnsResult::RefusedNoLease,
            TextToColumnsResult::RefusedLeaseExpired,
            TextToColumnsResult::RefusedLeaseWrongFile,
            TextToColumnsResult::RefusedLeaseStale,
            TextToColumnsResult::Failed {
                detail: String::new(),
            },
        ];
        let mut seen: Vec<&str> = statuses
            .iter()
            .map(TextToColumnsResult::log_status)
            .collect();
        seen.push(would_change_outcome(Vec::new(), false).result.log_status());
        let unique: std::collections::HashSet<&str> = seen.iter().copied().collect();
        assert_eq!(unique.len(), seen.len(), "duplicate log status in {seen:?}");
        assert_eq!(
            TextToColumnsResult::RefusedLeaseStale.log_status(),
            LeaseGateRefusal::Stale.log_status()
        );
    }

    /// The `-o jsonl` renderer writes one scalar record; the delimiter
    /// is `#[serde(skip)]`, so a custom separator never reaches it.
    #[test]
    fn jsonl_writes_one_record_and_omits_the_delimiter() {
        let mut outcome = would_change_outcome(vec!["B2".to_string()], false);
        outcome.delimiter = Delimiter::Custom("not-in-the-record".to_string());
        let mut out = Vec::new();
        outcome.write_jsonl(&mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(!text.contains("not-in-the-record"));
        let parsed: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(parsed["result"]["status"], "would-change");
    }

    #[test]
    fn is_single_column_accepts_a_one_column_span() {
        assert!(is_single_column(&bounded(0, 0, 10, 2, 3)));
    }

    #[test]
    fn is_single_column_rejects_a_wider_span() {
        assert!(!is_single_column(&bounded(0, 0, 10, 2, 4)));
    }

    #[test]
    fn cell_text_renders_a_string_verbatim() {
        assert_eq!(cell_text(&serde_json::json!("a,b")), "a,b");
    }

    #[test]
    fn cell_text_is_empty_for_null() {
        assert_eq!(cell_text(&serde_json::Value::Null), "");
    }

    #[test]
    fn cell_text_falls_back_to_json_rendering_for_a_non_string_non_null_cell() {
        assert_eq!(cell_text(&serde_json::json!(42)), "42");
    }

    #[test]
    fn split_width_is_zero_for_no_rows() {
        assert_eq!(split_width(&values(&[]), &Delimiter::Comma), 0);
    }

    #[test]
    fn split_width_ignores_a_blank_row() {
        assert_eq!(split_width(&values(&[""]), &Delimiter::Comma), 0);
    }

    #[test]
    fn split_width_counts_comma_pieces() {
        assert_eq!(split_width(&values(&["a,b,c"]), &Delimiter::Comma), 3);
    }

    #[test]
    fn split_width_is_the_widest_row() {
        assert_eq!(
            split_width(&values(&["a,b", "x,y,z", "solo"]), &Delimiter::Comma),
            3
        );
    }

    #[test]
    fn split_width_with_a_custom_delimiter() {
        assert_eq!(
            split_width(&values(&["a|b|c|d"]), &Delimiter::Custom("|".to_string())),
            4
        );
    }

    #[test]
    fn split_width_auto_takes_the_widest_of_the_four_fixed_delimiters() {
        // "a.b.c" splits into 3 on PERIOD and 1 on the other three.
        assert_eq!(split_width(&values(&["a.b.c"]), &Delimiter::Auto), 3);
    }

    #[test]
    fn split_width_with_no_delimiter_present_is_one() {
        assert_eq!(split_width(&values(&["solo"]), &Delimiter::Comma), 1);
    }

    #[test]
    fn spill_span_is_none_for_a_width_of_one_or_less() {
        assert_eq!(spill_span(bounded(0, 0, 10, 0, 1), 1), None);
        assert_eq!(spill_span(bounded(0, 0, 10, 0, 1), 0), None);
    }

    #[test]
    fn spill_span_covers_the_columns_to_the_right_of_the_source() {
        // Source A2:A10 (column index 0), width 3 -> spill B2:C10.
        let spill = spill_span(bounded(0, 1, 10, 0, 1), 3).unwrap();
        assert_eq!(spill.start_column_index, Some(1));
        assert_eq!(spill.end_column_index, Some(3));
        assert_eq!(spill.start_row_index, Some(1));
        assert_eq!(spill.end_row_index, Some(10));
    }

    #[test]
    fn extends_past_grid_flags_a_spill_past_the_column_count() {
        let workbook: Spreadsheet = serde_json::from_value(serde_json::json!({
            "spreadsheetId": "sheet-1",
            "sheets": [{"properties": {"sheetId": 0, "title": "Q1",
                "gridProperties": {"rowCount": 100, "columnCount": 3}}}],
        }))
        .unwrap();
        assert!(extends_past_grid(&workbook, &bounded(0, 0, 10, 0, 5)));
        assert!(!extends_past_grid(&workbook, &bounded(0, 0, 10, 0, 3)));
    }

    #[test]
    fn extends_past_grid_is_false_when_the_sheet_is_unknown() {
        let workbook: Spreadsheet = serde_json::from_value(serde_json::json!({
            "spreadsheetId": "sheet-1",
            "sheets": [],
        }))
        .unwrap();
        assert!(!extends_past_grid(&workbook, &bounded(0, 0, 10, 0, 5)));
    }

    #[test]
    fn describe_effect_with_a_spill() {
        let summary = describe_effect("'Q1'!A2:A10", Some("'Q1'!B2:D10"), 3, &Delimiter::Comma);
        assert_eq!(
            summary,
            "split 'Q1'!A2:A10 on comma into up to 3 column(s), spill 'Q1'!B2:D10"
        );
    }

    #[test]
    fn describe_effect_with_no_spill() {
        let summary = describe_effect("'Q1'!A2:A10", None, 1, &Delimiter::Space);
        assert_eq!(
            summary,
            "split 'Q1'!A2:A10 on space into a single column each row; no row spills beyond \
             the source under this delimiter"
        );
    }

    /// `Auto` earns its own no-spill wording: unlike every fixed
    /// delimiter, a local no-match says nothing about what Sheets will
    /// detect, so the summary reports the span as unknown rather than
    /// claiming no row spills.
    #[test]
    fn describe_effect_with_no_spill_under_auto() {
        let summary = describe_effect("'Q1'!A2:A10", None, 1, &Delimiter::Auto);
        assert_eq!(
            summary,
            "split 'Q1'!A2:A10 on auto-detected; no separator this preview tries appears in the \
             source, so the spill span is unknown"
        );
    }

    /// The summary is reused verbatim by `Changed` and by the request
    /// log's `fields_changed`, so it must read correctly *after* the
    /// fact too — a conditional clause here would describe a mutation
    /// that already happened. The no-spill branch is the one that used
    /// to carry a "would".
    #[test]
    fn every_summary_branch_is_tense_neutral() {
        for (spill, width) in [(Some("'Q1'!B2:B10"), 2), (None, 1)] {
            let summary = describe_effect("'Q1'!A2:A10", spill, width, &Delimiter::Comma);
            assert!(
                !summary.contains("would"),
                "summary is reused in the past tense: {summary}"
            );
        }
    }

    #[test]
    fn overwritten_line_is_always_prefixed_up_to() {
        let line = overwritten_line(&["B2".to_string()], &Delimiter::Comma, true).unwrap();
        assert!(line.starts_with("  up to 1 non-blank cell(s) would be overwritten"));
    }

    #[test]
    fn overwritten_line_empty_case() {
        assert_eq!(
            overwritten_line(&[], &Delimiter::Comma, false),
            Some("  no non-blank cells in the spill columns".to_string())
        );
    }

    /// An `auto` run with nothing to list says nothing, rather than
    /// printing the all-clear a live run was measured contradicting.
    #[test]
    fn auto_never_prints_an_all_clear() {
        assert_eq!(overwritten_line(&[], &Delimiter::Auto, true), None);
        // It still lists cells it did find — that half is informative.
        assert!(overwritten_line(&["B2".to_string()], &Delimiter::Auto, true).is_some());
    }

    #[test]
    fn build_request_omits_delimiter_for_a_fixed_type() {
        let request = build_request(bounded(0, 0, 10, 0, 1), &Delimiter::Comma);
        match request {
            BatchUpdateRequestItem::TextToColumns(req) => {
                assert_eq!(req.delimiter, None);
                assert_eq!(req.delimiter_type, DelimiterType::Comma);
            }
            other => panic!("unexpected request: {other:?}"), // omni-dev: coverage ignore-line reason="this match's catch-all only runs if build_request failed to return the request variant this test constructs it to build; that never happens, so the branch never executes"
        }
    }

    #[test]
    fn build_request_carries_the_custom_delimiter_text() {
        let request = build_request(bounded(0, 0, 10, 0, 1), &Delimiter::Custom("|".to_string()));
        match request {
            BatchUpdateRequestItem::TextToColumns(req) => {
                assert_eq!(req.delimiter, Some("|".to_string()));
                assert_eq!(req.delimiter_type, DelimiterType::Custom);
            }
            other => panic!("unexpected request: {other:?}"), // omni-dev: coverage ignore-line reason="this match's catch-all only runs if build_request failed to return the request variant this test constructs it to build; that never happens, so the branch never executes"
        }
    }

    // ── describe_lines ───────────────────────────────────────────────────

    fn would_change_outcome(
        overwritten_cells: Vec<String>,
        past_grid_extent: bool,
    ) -> TextToColumnsOutcome {
        TextToColumnsOutcome {
            spreadsheet_id: "sheet-1".to_string(),
            file_name: Some("Budget".to_string()),
            resolved_folder_id: Some("folder-1".to_string()),
            sheet_id: Some(0),
            delimiter: Delimiter::Comma,
            result: TextToColumnsResult::WouldChange {
                summary: "split 'Q1'!A2:A4 on comma into up to 2 column(s), spill 'Q1'!B2:B4"
                    .to_string(),
                source: "'Q1'!A2:A4".to_string(),
                spill: Some("'Q1'!B2:B4".to_string()),
                width_upper_bound: 2,
                overwritten_cells,
                past_grid_extent,
            },
        }
    }

    #[test]
    fn would_change_lines_carry_the_upper_bound_and_the_unpreviewable_caveat() {
        let outcome = would_change_outcome(vec!["B2".to_string()], false);
        let lines = describe_lines(&outcome);
        assert_eq!(
            lines[0],
            "Would split 'Q1'!A2:A4 on comma into up to 2 column(s), spill 'Q1'!B2:B4 in 'Budget'"
        );
        assert_eq!(
            lines[1],
            "  up to 1 non-blank cell(s) would be overwritten: B2"
        );
        assert_eq!(lines[2], UNPREVIEWABLE_SPLIT_CAVEAT);
    }

    #[test]
    fn past_grid_extent_adds_the_dry_run_caveat_line() {
        let outcome = would_change_outcome(vec![], true);
        let lines = describe_lines(&outcome);
        assert_eq!(lines[2], PAST_GRID_EXTENT_CAVEAT_DRY_RUN);
        assert_eq!(lines[3], UNPREVIEWABLE_SPLIT_CAVEAT);
    }

    #[test]
    fn changed_lines_use_the_past_tense() {
        let mut outcome = would_change_outcome(vec!["B2".to_string()], true);
        outcome.result = match outcome.result {
            TextToColumnsResult::WouldChange {
                summary,
                source,
                spill,
                width_upper_bound,
                overwritten_cells,
                past_grid_extent,
            } => TextToColumnsResult::Changed {
                summary,
                source,
                spill,
                width_upper_bound,
                overwritten_cells,
                past_grid_extent,
            },
            other => other, // omni-dev: coverage ignore-line reason="would_change_outcome always constructs a WouldChange result, so this catch-all identity arm never runs"
        };
        let lines = describe_lines(&outcome);
        assert!(lines[0].starts_with("Applied: "));
        assert!(lines[1].contains("were overwritten"));
        assert_eq!(lines[2], PAST_GRID_EXTENT_CAVEAT);
    }

    /// Every variant renders: non-empty, and never with an embedded
    /// newline, which is [`describe_lines`]' own stated contract (the
    /// `-o table` renderer joins the lines itself, and a `\n` inside one
    /// would silently defeat the per-line terminal sanitising the CLI
    /// applies).
    #[test]
    fn every_result_variant_renders_as_non_empty_newline_free_lines() {
        let results = [
            TextToColumnsResult::RefusedNotASpreadsheet {
                mime_type: "application/pdf".to_string(),
            },
            TextToColumnsResult::RefusedShortcut,
            TextToColumnsResult::RefusedNoVisibleParents,
            TextToColumnsResult::RefusedSheetNotFound {
                title: "Q9".to_string(),
                available: vec!["Q1".to_string()],
            },
            // The workbook has no sheets at all, so there is nothing to
            // suggest — the "none" branch of the same arm.
            TextToColumnsResult::RefusedSheetNotFound {
                title: "Q9".to_string(),
                available: Vec::new(),
            },
            TextToColumnsResult::RefusedInvalidRange {
                detail: "'A1:B2' spans more than one column".to_string(),
            },
            TextToColumnsResult::RefusedInvalidDelimiter {
                detail: "--custom-delimiter must not be empty".to_string(),
            },
            TextToColumnsResult::Blocked {
                operation: DriveOperation::SheetsWrite,
                decided_by: None,
            },
            TextToColumnsResult::RefusedNoLease,
            TextToColumnsResult::RefusedLeaseExpired,
            TextToColumnsResult::RefusedLeaseWrongFile,
            TextToColumnsResult::RefusedLeaseStale,
            TextToColumnsResult::Failed {
                detail: "HTTP 500".to_string(),
            },
        ];
        for result in results {
            let mut outcome = would_change_outcome(Vec::new(), false);
            let status = result.log_status();
            outcome.result = result;
            let lines = describe_lines(&outcome);
            assert!(!lines.is_empty(), "{status} rendered nothing");
            for line in &lines {
                assert!(!line.is_empty(), "{status} rendered an empty line");
                assert!(
                    !line.contains('\n'),
                    "{status} rendered an embedded newline"
                );
            }
            // `describe` is just the joined form of the same lines.
            assert_eq!(describe(&outcome), lines.join("\n"), "{status}");
        }
    }

    /// The lease helper's catch-all maps onto this module's `Failed`,
    /// so a ledger read that blows up is reported like any other error
    /// rather than as a refusal the user could act on.
    #[test]
    fn a_failed_lease_check_maps_to_failed() {
        match TextToColumnsResult::from_lease_failed("ledger unreadable".to_string()) {
            TextToColumnsResult::Failed { detail } => assert_eq!(detail, "ledger unreadable"),
            other => panic!("expected Failed, got {other:?}"), // omni-dev: coverage ignore-line reason="from_lease_failed always returns Failed; this test's catch-all guards that assumption and never runs"
        }
    }

    /// No engine call site ever renders a book-using result with no file
    /// name (every path that resolves one already has the target's
    /// name), but `describe`/`describe_lines` are `pub` and take
    /// whatever `TextToColumnsOutcome` they are given — so this pins the
    /// fallback to the raw spreadsheet id directly.
    #[test]
    fn describe_falls_back_to_the_spreadsheet_id_with_no_file_name() {
        let mut outcome = would_change_outcome(Vec::new(), false);
        outcome.file_name = None;
        assert!(describe(&outcome).contains("'sheet-1'"));
    }

    #[test]
    fn auto_earns_its_own_caveat_line_and_no_other_delimiter_does() {
        let mut outcome = would_change_outcome(vec!["B2".to_string()], false);
        assert!(!describe_lines(&outcome).contains(&AUTO_DELIMITER_CAVEAT.to_string()));
        outcome.delimiter = Delimiter::Auto;
        let lines = describe_lines(&outcome);
        assert_eq!(lines[lines.len() - 2], AUTO_DELIMITER_CAVEAT);
        // Still the last word on the subject: the unpreviewable caveat
        // every run carries stays at the end.
        assert_eq!(lines[lines.len() - 1], UNPREVIEWABLE_SPLIT_CAVEAT);
    }

    #[test]
    fn refused_sheet_not_found_lists_available_titles() {
        let outcome = TextToColumnsOutcome {
            spreadsheet_id: "sheet-1".to_string(),
            file_name: Some("Budget".to_string()),
            resolved_folder_id: Some("folder-1".to_string()),
            sheet_id: None,
            delimiter: Delimiter::Comma,
            result: TextToColumnsResult::RefusedSheetNotFound {
                title: "Q2".to_string(),
                available: vec!["Q1".to_string()],
            },
        };
        assert_eq!(
            describe_lines(&outcome),
            vec!["Refused: 'Budget' has no sheet titled 'Q2'. Available: 'Q1'".to_string()]
        );
    }

    #[test]
    fn blocked_with_no_deciding_rule_names_default_policy() {
        let outcome = TextToColumnsOutcome {
            spreadsheet_id: "sheet-1".to_string(),
            file_name: Some("Budget".to_string()),
            resolved_folder_id: Some("folder-1".to_string()),
            sheet_id: None,
            delimiter: Delimiter::Comma,
            result: TextToColumnsResult::Blocked {
                operation: DriveOperation::SheetsWrite,
                decided_by: None,
            },
        };
        assert_eq!(
            describe_lines(&outcome),
            vec![
                "Blocked: text-to-columns on 'Budget' refused by default policy (no matching \
                 rule for sheets-write)"
                    .to_string()
            ]
        );
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

    /// Builds a `DriveClient`/`SheetsClient` pair pointed at `server`, with
    /// `SHEETS_API_URL` injected via `MapEnv` rather than
    /// `std::env::set_var` — the process environment is global state, and
    /// this module's tests run concurrently against their own mock
    /// servers (STYLE-0028; `auto_fill.rs`'s own precedent).
    async fn client(server: &wiremock::MockServer) -> (DriveClient, SheetsClient) {
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

    fn rule_allowing(ops: &[DriveOperation], require_lease: bool) -> FolderPermissionRule {
        FolderPermissionRule {
            folder_id: Some("parent-1".to_string()),
            file_id: None,
            recursive: true,
            allow: ops.iter().copied().collect(),
            deny: std::collections::HashSet::default(),
            require_lease,
        }
    }

    /// The grant this verb actually needs: both halves of
    /// [`GATE_OPERATIONS`]. Named so a test that means "allowed" does
    /// not have to restate the union, and so the union moving would
    /// break one helper rather than twenty tests.
    fn rule(_op: DriveOperation, require_lease: bool) -> FolderPermissionRule {
        rule_allowing(GATE_OPERATIONS, require_lease)
    }

    fn base_opts(dry_run: bool) -> TextToColumnsOptions {
        TextToColumnsOptions {
            spreadsheet_id: "sheet-1".to_string(),
            sheet: None,
            source: Some("Q1!A2:A4".to_string()),
            delimiter: Delimiter::Comma,
            dry_run,
            lease_token: None,
            ledger_path: PathBuf::default(),
        }
    }

    /// The target's own Drive metadata. Split out of
    /// [`mount_metadata`] so the refusal tests can vary the one field
    /// each of them turns on (`mimeType`, `shortcutDetails`, `parents`)
    /// without restating the rest.
    async fn mount_file_metadata(server: &wiremock::MockServer, body: serde_json::Value) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/sheet-1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    /// The default target: a spreadsheet in `parent-1`, at version `1`
    /// (which is what [`seed_lease`] must record to look fresh).
    fn spreadsheet_metadata() -> serde_json::Value {
        serde_json::json!({
            "id": "sheet-1", "name": "Budget",
            "mimeType": "application/vnd.google-apps.spreadsheet",
            "parents": ["parent-1"], "version": "1",
        })
    }

    async fn mount_parent(server: &wiremock::MockServer) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/parent-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "parent-1", "name": "parent-1",
                    "mimeType": "application/vnd.google-apps.folder", "parents": [],
                })),
            )
            .mount(server)
            .await;
    }

    /// The workbook, with a caller-chosen grid extent — the grid-edge
    /// tests below turn the sheet's `columnCount` down until the spill
    /// runs off it.
    async fn mount_workbook(server: &wiremock::MockServer, row_count: i64, column_count: i64) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/sheet-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "spreadsheetId": "sheet-1",
                    "sheets": [{"properties": {"sheetId": 0, "title": "Q1",
                        "gridProperties": {
                            "rowCount": row_count, "columnCount": column_count,
                        }}}],
                })),
            )
            .mount(server)
            .await;
    }

    async fn mount_metadata(server: &wiremock::MockServer) {
        mount_file_metadata(server, spreadsheet_metadata()).await;
        mount_parent(server).await;
        mount_workbook(server, 1000, 26).await;
    }

    /// Mounts a `values.get` that fails, for the two reads this engine
    /// performs.
    async fn mount_values_failure(server: &wiremock::MockServer, range: &str) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!(
                "/v4/spreadsheets/sheet-1/values/{range}"
            )))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(server)
            .await;
    }

    async fn mount_batch_update(server: &wiremock::MockServer, status: u16) {
        let template = if status == 200 {
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "spreadsheetId": "sheet-1", "replies": [{}],
            }))
        } else {
            wiremock::ResponseTemplate::new(status)
        };
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .respond_with(template)
            .mount(server)
            .await;
    }

    fn batch_update_was_called(requests: &[wiremock::Request]) -> bool {
        requests
            .iter()
            .any(|r| r.url.path() == "/v4/spreadsheets/sheet-1:batchUpdate")
    }

    /// Mounts one `values.get` response for an exact A1 range — the same
    /// literal-path style `auto_fill.rs`'s own tests use (`url::Url`'s
    /// path segment encoding leaves `'`, `!` and `:` untouched, so the
    /// range string can be matched on verbatim).
    async fn mount_values(server: &wiremock::MockServer, range: &str, rows: serde_json::Value) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!(
                "/v4/spreadsheets/sheet-1/values/{range}"
            )))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(rows))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn dry_run_reads_source_and_spill_and_never_calls_batch_update() {
        let server = wiremock::MockServer::start().await;
        let (client, sheets) = client(&server).await;
        mount_metadata(&server).await;
        mount_values(
            &server,
            "'Q1'!A2:A4",
            serde_json::json!({"values": [["a,b"], ["x,y,z"], [""]]}),
        )
        .await;
        // Width 3 (from "x,y,z") means 2 spill columns (B:C) to the right
        // of the source column (A) — the source's own column holds the
        // first piece and stays out of the spill span.
        mount_values(
            &server,
            "'Q1'!B2:C4",
            serde_json::json!({"values": [["", ""], ["old", ""]]}),
        )
        .await;
        let opts = base_opts(true);
        let rules = vec![rule(DriveOperation::SheetsWrite, false)];
        let outcome = text_to_columns(&client, &sheets, &opts, &rules).await;
        match &outcome.result {
            TextToColumnsResult::WouldChange {
                width_upper_bound,
                spill,
                overwritten_cells,
                ..
            } => {
                assert_eq!(*width_upper_bound, 3);
                assert_eq!(spill.as_deref(), Some("'Q1'!B2:C4"));
                assert_eq!(overwritten_cells, &vec!["B3".to_string()]);
            }
            other => panic!("expected WouldChange, got {other:?}"),
        }
        assert!(server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| r.method != wiremock::http::Method::POST
                || r.url.path() != "/v4/spreadsheets/sheet-1:batchUpdate"));
    }

    #[tokio::test]
    async fn real_run_posts_exactly_one_text_to_columns_request() {
        let server = wiremock::MockServer::start().await;
        let (client, sheets) = client(&server).await;
        mount_metadata(&server).await;
        mount_values(
            &server,
            "'Q1'!A2:A4",
            serde_json::json!({"values": [["a,b"]]}),
        )
        .await;
        mount_values(&server, "'Q1'!B2:B4", serde_json::json!({"values": []})).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "spreadsheetId": "sheet-1", "replies": [{}],
                })),
            )
            .mount(&server)
            .await;
        let opts = base_opts(false);
        let rules = vec![rule(DriveOperation::SheetsWrite, false)];
        let outcome = text_to_columns(&client, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            TextToColumnsResult::Changed { .. }
        ));

        let batch_requests: Vec<_> = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.url.path() == "/v4/spreadsheets/sheet-1:batchUpdate")
            .collect();
        assert_eq!(batch_requests.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&batch_requests[0].body).unwrap();
        assert_eq!(body["requests"].as_array().unwrap().len(), 1);
        assert!(body["requests"][0]["textToColumns"].is_object());
    }

    #[tokio::test]
    async fn default_policy_blocks_with_no_configured_rules() {
        let server = wiremock::MockServer::start().await;
        let (client, sheets) = client(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/sheet-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "sheet-1", "name": "Budget",
                    "mimeType": "application/vnd.google-apps.spreadsheet",
                    "parents": ["parent-1"], "version": "1",
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/parent-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "parent-1", "name": "parent-1",
                    "mimeType": "application/vnd.google-apps.folder", "parents": [],
                })),
            )
            .mount(&server)
            .await;
        let opts = base_opts(true);
        let outcome = text_to_columns(&client, &sheets, &opts, &[]).await;
        assert!(matches!(
            outcome.result,
            TextToColumnsResult::Blocked {
                operation: DriveOperation::SheetsWrite,
                decided_by: None,
            }
        ));
    }

    /// The gate is the **union**, so neither half alone opens it —
    /// a live run measured the split carrying the source cell's
    /// formatting into the spill cells, which `sheets-write` does not
    /// confer, and the values it writes are not something
    /// `sheets-structure` confers either. Each half is refused naming
    /// the operation that was missing, which is the only actionable
    /// part of the message.
    #[tokio::test]
    async fn neither_half_of_the_gate_opens_it_alone() {
        for (granted, missing) in [
            (DriveOperation::SheetsWrite, DriveOperation::SheetsStructure),
            (DriveOperation::SheetsStructure, DriveOperation::SheetsWrite),
        ] {
            let server = wiremock::MockServer::start().await;
            let (client, sheets) = client(&server).await;
            mount_metadata(&server).await;
            let rules = vec![rule_allowing(&[granted], false)];
            let outcome = text_to_columns(&client, &sheets, &base_opts(true), &rules).await;
            match &outcome.result {
                TextToColumnsResult::Blocked { operation, .. } => {
                    assert_eq!(*operation, missing, "granted {granted}");
                }
                other => panic!("expected Blocked with {granted} granted, got {other:?}"),
            }
            assert!(describe_lines(&outcome)[0].contains(&missing.to_string()));
        }
    }

    /// …and the union together does.
    #[tokio::test]
    async fn both_operations_together_open_the_gate() {
        let server = wiremock::MockServer::start().await;
        let (client, sheets) = client(&server).await;
        mount_metadata(&server).await;
        mount_values(
            &server,
            "'Q1'!A2:A4",
            serde_json::json!({"values": [["a,b"]]}),
        )
        .await;
        mount_values(&server, "'Q1'!B2:B4", serde_json::json!({"values": []})).await;
        let rules = vec![rule_allowing(GATE_OPERATIONS, false)];
        let outcome = text_to_columns(&client, &sheets, &base_opts(true), &rules).await;
        assert!(matches!(
            outcome.result,
            TextToColumnsResult::WouldChange { .. }
        ));
    }

    /// A `deny` entry — as opposed to the "no matching rule" default
    /// policy every other `Blocked` test exercises — names the deciding
    /// rule in the rendered message. `dry_run: false` also reaches
    /// `record_attempt`'s `Blocked` arm, which a dry run never does.
    #[tokio::test]
    async fn a_blocked_by_rule_names_the_deciding_folder_in_the_message() {
        let server = wiremock::MockServer::start().await;
        let (client, sheets) = client(&server).await;
        mount_metadata(&server).await;
        let deny_by_rule = FolderPermissionRule {
            folder_id: Some("parent-1".to_string()),
            file_id: None,
            recursive: true,
            allow: std::iter::once(DriveOperation::SheetsWrite).collect(),
            deny: std::iter::once(DriveOperation::SheetsStructure).collect(),
            require_lease: false,
        };
        let outcome = text_to_columns(&client, &sheets, &base_opts(false), &[deny_by_rule]).await;
        assert!(
            matches!(
                outcome.result,
                TextToColumnsResult::Blocked {
                    decided_by: Some(_),
                    ..
                }
            ),
            "{:?}",
            outcome.result
        );
        let text = describe(&outcome);
        assert!(
            text.contains("refused by rule on folder parent-1"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn open_ended_source_is_refused() {
        let server = wiremock::MockServer::start().await;
        let (client, sheets) = client(&server).await;
        mount_metadata(&server).await;
        let mut opts = base_opts(true);
        opts.source = Some("Q1!A:A".to_string());
        let rules = vec![rule(DriveOperation::SheetsWrite, false)];
        let outcome = text_to_columns(&client, &sheets, &opts, &rules).await;
        match outcome.result {
            TextToColumnsResult::RefusedInvalidRange { detail } => {
                assert!(detail.contains("open-ended"));
            }
            other => panic!("expected RefusedInvalidRange, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn multi_column_source_is_refused() {
        let server = wiremock::MockServer::start().await;
        let (client, sheets) = client(&server).await;
        mount_metadata(&server).await;
        let mut opts = base_opts(true);
        opts.source = Some("Q1!A2:B4".to_string());
        let rules = vec![rule(DriveOperation::SheetsWrite, false)];
        let outcome = text_to_columns(&client, &sheets, &opts, &rules).await;
        match outcome.result {
            TextToColumnsResult::RefusedInvalidRange { detail } => {
                assert!(detail.contains("exactly one column"));
            }
            other => panic!("expected RefusedInvalidRange, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_custom_delimiter_is_refused_before_any_network_call() {
        let server = wiremock::MockServer::start().await;
        let (client, sheets) = client(&server).await;
        let mut opts = base_opts(true);
        opts.delimiter = Delimiter::Custom(String::new());
        let outcome = text_to_columns(&client, &sheets, &opts, &[]).await;
        assert!(matches!(
            outcome.result,
            TextToColumnsResult::RefusedInvalidDelimiter { .. }
        ));
        assert!(server.received_requests().await.unwrap().is_empty());
        // Refused before any request, so there is no file name yet.
        assert!(outcome.file_name.is_none());
    }

    #[tokio::test]
    async fn lease_required_and_absent_is_refused() {
        let server = wiremock::MockServer::start().await;
        let (client, sheets) = client(&server).await;
        mount_metadata(&server).await;
        mount_values(
            &server,
            "'Q1'!A2:A4",
            serde_json::json!({"values": [["a,b"]]}),
        )
        .await;
        mount_values(&server, "'Q1'!B2:B4", serde_json::json!({"values": []})).await;
        let opts = base_opts(false);
        let rules = vec![rule(DriveOperation::SheetsWrite, true)];
        let outcome = text_to_columns(&client, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            TextToColumnsResult::RefusedNoLease
        ));
    }

    #[tokio::test]
    async fn a_valid_lease_lets_the_real_run_succeed() {
        let ledger_dir = tempfile::tempdir().unwrap();
        let ledger_path = ledger_dir.path().join("leases.json");
        let token = seed_lease(&ledger_path, "sheet-1", "1");

        let server = wiremock::MockServer::start().await;
        let (client, sheets) = client(&server).await;
        mount_metadata(&server).await;
        mount_values(
            &server,
            "'Q1'!A2:A4",
            serde_json::json!({"values": [["a,b"]]}),
        )
        .await;
        mount_values(&server, "'Q1'!B2:B4", serde_json::json!({"values": []})).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "spreadsheetId": "sheet-1", "replies": [{}],
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/drive/v3/files/sheet-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "sheet-1", "name": "Budget", "version": "2",
                })),
            )
            .mount(&server)
            .await;
        let mut opts = base_opts(false);
        opts.lease_token = Some(token);
        opts.ledger_path = ledger_path;
        let rules = vec![rule(DriveOperation::SheetsWrite, true)];
        let outcome = text_to_columns(&client, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            TextToColumnsResult::Changed { .. }
        ));
    }

    /// The lease-holding counterpart of `a_batch_update_failure_is_reported_as_failed`:
    /// with a lease actually granted, a failed `batchUpdate` must reach
    /// `conclude_native_leased_write`'s `Err` arm and record the failure
    /// against the held lease, not just report `Failed`.
    #[tokio::test]
    async fn a_valid_lease_records_the_failure_when_batch_update_fails() {
        let ledger_dir = tempfile::tempdir().unwrap();
        let ledger_path = ledger_dir.path().join("leases.json");
        let token = seed_lease(&ledger_path, "sheet-1", "1");

        let server = wiremock::MockServer::start().await;
        let (client, sheets) = client(&server).await;
        mount_metadata(&server).await;
        mount_values(
            &server,
            "'Q1'!A2:A4",
            serde_json::json!({"values": [["a,b"]]}),
        )
        .await;
        mount_values(&server, "'Q1'!B2:B4", serde_json::json!({"values": []})).await;
        mount_batch_update(&server, 400).await;
        let mut opts = base_opts(false);
        opts.lease_token = Some(token);
        opts.ledger_path = ledger_path;
        let rules = vec![rule(DriveOperation::SheetsWrite, true)];
        let outcome = text_to_columns(&client, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, TextToColumnsResult::Failed { .. }));
    }

    // ── the §6 "never a value" guard, end to end ─────────────────────────

    /// The pinned form of ADR-0083 §6's rule for this verb: a
    /// distinctive value is fed through the *whole* path — the source
    /// read that computes the width, the spill read that names the
    /// overwritten cells, the rendered lines and the serialized
    /// outcome — and must appear in none of them. Asserting it against a
    /// hand-built outcome would only prove that the fields a test
    /// chose to fill are the fields it chose to read.
    #[tokio::test]
    async fn no_cell_value_from_either_read_reaches_the_output_or_the_outcome() {
        const SECRET: &str = "super-secret-cell-value";
        let server = wiremock::MockServer::start().await;
        let (client, sheets) = client(&server).await;
        mount_metadata(&server).await;
        mount_values(
            &server,
            "'Q1'!A2:A4",
            serde_json::json!({"values": [[format!("{SECRET},{SECRET}")]]}),
        )
        .await;
        mount_values(
            &server,
            "'Q1'!B2:B4",
            serde_json::json!({"values": [[SECRET]]}),
        )
        .await;
        let opts = base_opts(true);
        let rules = vec![rule(DriveOperation::SheetsWrite, false)];
        let outcome = text_to_columns(&client, &sheets, &opts, &rules).await;

        // The spill cell was seen — its *address* is reported, so the
        // test is exercising the path that could leak, not an empty one.
        match &outcome.result {
            TextToColumnsResult::WouldChange {
                overwritten_cells, ..
            } => assert_eq!(overwritten_cells, &vec!["B2".to_string()]),
            other => panic!("expected WouldChange, got {other:?}"), // omni-dev: coverage ignore-line reason="this test's mocked responses always drive a WouldChange outcome; this catch-all guards that assumption and never runs"
        }
        let rendered = describe_lines(&outcome).join("\n");
        assert!(
            !rendered.contains(SECRET),
            "leaked into the text: {rendered}"
        );
        let serialized = serde_json::to_string(&outcome).unwrap();
        assert!(
            !serialized.contains(SECRET),
            "leaked into the outcome: {serialized}"
        );
    }

    // ── the grid edge ────────────────────────────────────────────────────

    /// The spill runs off the sheet's last column, so the *read* is
    /// clamped back onto the grid while the request is left alone —
    /// ADR-0083 §5's "left to the server", `auto_fill.rs`'s own
    /// `a_destination_straddling_the_grid_edge_reads_only_the_in_grid_part`.
    #[tokio::test]
    async fn a_spill_straddling_the_grid_edge_reads_only_the_in_grid_part() {
        let server = wiremock::MockServer::start().await;
        let (client, sheets) = client(&server).await;
        mount_file_metadata(&server, spreadsheet_metadata()).await;
        mount_parent(&server).await;
        mount_workbook(&server, 1000, 3).await; // columns A..C only
        mount_values(
            &server,
            "'Q1'!A2:A4",
            serde_json::json!({"values": [["a,b,c,d,e"]]}),
        )
        .await;
        // Width 5 spills into B:E, but only B:C exist — the read is
        // clamped to those, and a cell in the clamped part still counts.
        mount_values(
            &server,
            "'Q1'!B2:C4",
            serde_json::json!({"values": [["", "old"]]}),
        )
        .await;
        let opts = base_opts(true);
        let rules = vec![rule(DriveOperation::SheetsWrite, false)];
        let outcome = text_to_columns(&client, &sheets, &opts, &rules).await;
        match &outcome.result {
            TextToColumnsResult::WouldChange {
                spill,
                width_upper_bound,
                overwritten_cells,
                past_grid_extent,
                ..
            } => {
                // The reported spill is the *unclamped* span: what the
                // request may reach, not what could be read back.
                assert_eq!(spill.as_deref(), Some("'Q1'!B2:E4"));
                assert_eq!(*width_upper_bound, 5);
                assert_eq!(overwritten_cells, &vec!["C2".to_string()]);
                assert!(past_grid_extent);
            }
            other => panic!("expected WouldChange, got {other:?}"),
        }
        assert!(describe_lines(&outcome).contains(&PAST_GRID_EXTENT_CAVEAT_DRY_RUN.to_string()));
    }

    /// Nothing of the spill is inside the grid, so there is no read to
    /// clamp — the caveat still fires and the request is still sent.
    #[tokio::test]
    async fn a_spill_wholly_past_the_grid_extent_skips_the_read_and_still_splits() {
        let server = wiremock::MockServer::start().await;
        let (client, sheets) = client(&server).await;
        mount_file_metadata(&server, spreadsheet_metadata()).await;
        mount_parent(&server).await;
        mount_workbook(&server, 1000, 3).await; // columns A..C only
        mount_values(
            &server,
            "'Q1'!C2:C4",
            serde_json::json!({"values": [["a,b"]]}),
        )
        .await;
        mount_batch_update(&server, 200).await;
        let mut opts = base_opts(false);
        opts.source = Some("Q1!C2:C4".to_string());
        let rules = vec![rule(DriveOperation::SheetsWrite, false)];
        let outcome = text_to_columns(&client, &sheets, &opts, &rules).await;
        match &outcome.result {
            TextToColumnsResult::Changed {
                spill,
                overwritten_cells,
                past_grid_extent,
                ..
            } => {
                assert_eq!(spill.as_deref(), Some("'Q1'!D2:D4"));
                assert!(overwritten_cells.is_empty());
                assert!(past_grid_extent);
            }
            other => panic!("expected Changed, got {other:?}"), // omni-dev: coverage ignore-line reason="this test's mocked responses always drive a Changed outcome; this catch-all guards that assumption and never runs"
        }
        // Only the source was read: there was no in-grid spill to ask about.
        let reads: Vec<_> = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.url.path().contains("/values/"))
            .collect();
        assert_eq!(reads.len(), 1);
        assert!(describe_lines(&outcome).contains(&PAST_GRID_EXTENT_CAVEAT.to_string()));
    }

    // ── failure paths ────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_source_read_failure_is_reported_as_failed() {
        let server = wiremock::MockServer::start().await;
        let (client, sheets) = client(&server).await;
        mount_metadata(&server).await;
        mount_values_failure(&server, "'Q1'!A2:A4").await;
        let outcome = text_to_columns(
            &client,
            &sheets,
            &base_opts(true),
            &[rule(DriveOperation::SheetsWrite, false)],
        )
        .await;
        assert!(matches!(outcome.result, TextToColumnsResult::Failed { .. }));
    }

    #[tokio::test]
    async fn a_spill_read_failure_is_reported_as_failed() {
        let server = wiremock::MockServer::start().await;
        let (client, sheets) = client(&server).await;
        mount_metadata(&server).await;
        mount_values(
            &server,
            "'Q1'!A2:A4",
            serde_json::json!({"values": [["a,b"]]}),
        )
        .await;
        mount_values_failure(&server, "'Q1'!B2:B4").await;
        let outcome = text_to_columns(
            &client,
            &sheets,
            &base_opts(true),
            &[rule(DriveOperation::SheetsWrite, false)],
        )
        .await;
        assert!(matches!(outcome.result, TextToColumnsResult::Failed { .. }));
    }

    #[tokio::test]
    async fn a_batch_update_failure_is_reported_as_failed() {
        let server = wiremock::MockServer::start().await;
        let (client, sheets) = client(&server).await;
        mount_metadata(&server).await;
        mount_values(
            &server,
            "'Q1'!A2:A4",
            serde_json::json!({"values": [["a,b"]]}),
        )
        .await;
        mount_values(&server, "'Q1'!B2:B4", serde_json::json!({"values": []})).await;
        mount_batch_update(&server, 400).await;
        let outcome = text_to_columns(
            &client,
            &sheets,
            &base_opts(false),
            &[rule(DriveOperation::SheetsWrite, false)],
        )
        .await;
        assert!(matches!(outcome.result, TextToColumnsResult::Failed { .. }));
    }

    #[tokio::test]
    async fn a_workbook_fetch_failure_is_reported_as_failed() {
        let server = wiremock::MockServer::start().await;
        let (client, sheets) = client(&server).await;
        mount_file_metadata(&server, spreadsheet_metadata()).await;
        mount_parent(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/sheet-1"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let outcome = text_to_columns(
            &client,
            &sheets,
            &base_opts(true),
            &[rule(DriveOperation::SheetsWrite, false)],
        )
        .await;
        assert!(matches!(outcome.result, TextToColumnsResult::Failed { .. }));
    }

    /// The target's own metadata fetch fails, before there is even a
    /// file name to report.
    #[tokio::test]
    async fn a_metadata_fetch_failure_is_reported_as_failed() {
        let server = wiremock::MockServer::start().await;
        let (client, sheets) = client(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/sheet-1"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let outcome = text_to_columns(&client, &sheets, &base_opts(true), &[]).await;
        assert!(matches!(outcome.result, TextToColumnsResult::Failed { .. }));
        assert!(outcome.file_name.is_none());
    }

    /// The parent lookup the gate needs fails, *after* the target
    /// resolved — so the file name is known and the folder id is not.
    #[tokio::test]
    async fn a_gate_fetch_failure_is_reported_as_failed_and_still_names_the_file() {
        let server = wiremock::MockServer::start().await;
        let (client, sheets) = client(&server).await;
        mount_file_metadata(&server, spreadsheet_metadata()).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/parent-1"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let outcome = text_to_columns(
            &client,
            &sheets,
            &base_opts(true),
            &[rule(DriveOperation::SheetsWrite, false)],
        )
        .await;
        assert!(matches!(outcome.result, TextToColumnsResult::Failed { .. }));
        assert_eq!(outcome.file_name.as_deref(), Some("Budget"));
        assert!(outcome.resolved_folder_id.is_none());
    }

    // ── target refusals ──────────────────────────────────────────────────

    #[tokio::test]
    async fn a_target_that_is_not_a_spreadsheet_is_refused() {
        let server = wiremock::MockServer::start().await;
        let (client, sheets) = client(&server).await;
        mount_file_metadata(
            &server,
            serde_json::json!({
                "id": "sheet-1", "name": "Budget",
                "mimeType": "application/vnd.google-apps.document",
                "parents": ["parent-1"], "version": "1",
            }),
        )
        .await;
        let outcome = text_to_columns(&client, &sheets, &base_opts(true), &[]).await;
        match &outcome.result {
            TextToColumnsResult::RefusedNotASpreadsheet { mime_type } => {
                assert_eq!(mime_type, "application/vnd.google-apps.document");
            }
            other => panic!("expected RefusedNotASpreadsheet, got {other:?}"),
        }
        assert!(describe_lines(&outcome)[0].contains("is not a Google Sheet"));
    }

    #[tokio::test]
    async fn a_shortcut_target_is_refused_rather_than_followed() {
        let server = wiremock::MockServer::start().await;
        let (client, sheets) = client(&server).await;
        mount_file_metadata(
            &server,
            serde_json::json!({
                "id": "sheet-1", "name": "Budget",
                "mimeType": "application/vnd.google-apps.shortcut",
                "parents": ["parent-1"], "version": "1",
                "shortcutDetails": {
                    "targetId": "real-sheet",
                    "targetMimeType": "application/vnd.google-apps.spreadsheet",
                },
            }),
        )
        .await;
        let outcome = text_to_columns(&client, &sheets, &base_opts(true), &[]).await;
        assert!(matches!(
            outcome.result,
            TextToColumnsResult::RefusedShortcut
        ));
        assert!(describe_lines(&outcome)[0].contains("doesn't follow shortcuts"));
    }

    #[tokio::test]
    async fn a_target_with_no_visible_parents_is_refused_with_the_by_id_hint() {
        let server = wiremock::MockServer::start().await;
        let (client, sheets) = client(&server).await;
        mount_file_metadata(
            &server,
            serde_json::json!({
                "id": "sheet-1", "name": "Budget",
                "mimeType": "application/vnd.google-apps.spreadsheet",
                "parents": [], "version": "1",
            }),
        )
        .await;
        let outcome = text_to_columns(&client, &sheets, &base_opts(true), &[]).await;
        assert!(matches!(
            outcome.result,
            TextToColumnsResult::RefusedNoVisibleParents
        ));
        assert!(describe_lines(&outcome)[0].contains("write_permissions.rules"));
    }

    #[tokio::test]
    async fn a_source_on_a_missing_sheet_is_refused_and_names_the_available_titles() {
        let server = wiremock::MockServer::start().await;
        let (client, sheets) = client(&server).await;
        mount_metadata(&server).await;
        let mut opts = base_opts(true);
        opts.source = Some("Q9!A2:A4".to_string());
        let rules = vec![rule(DriveOperation::SheetsWrite, false)];
        let outcome = text_to_columns(&client, &sheets, &opts, &rules).await;
        match &outcome.result {
            TextToColumnsResult::RefusedSheetNotFound { title, available } => {
                assert_eq!(title, "Q9");
                assert_eq!(available, &vec!["Q1".to_string()]);
            }
            other => panic!("expected RefusedSheetNotFound, got {other:?}"),
        }
    }

    // ── lease refusals ───────────────────────────────────────────────────

    fn ledger_in_a_tempdir() -> PathBuf {
        tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl")
    }

    /// Runs a real (non-dry) split against a fixture whose reads all
    /// succeed, so the only thing left to refuse is the lease — and
    /// asserts the refusal happened before `batchUpdate`, which a
    /// mis-ordered gate would not.
    async fn run_with_lease(
        server: &wiremock::MockServer,
        lease_token: Option<String>,
        ledger_path: PathBuf,
    ) -> TextToColumnsOutcome {
        let (client, sheets) = client(server).await;
        mount_metadata(server).await;
        mount_values(
            server,
            "'Q1'!A2:A4",
            serde_json::json!({"values": [["a,b"]]}),
        )
        .await;
        mount_values(server, "'Q1'!B2:B4", serde_json::json!({"values": []})).await;
        mount_batch_update(server, 200).await;
        let opts = TextToColumnsOptions {
            lease_token,
            ledger_path,
            ..base_opts(false)
        };
        let outcome = text_to_columns(
            &client,
            &sheets,
            &opts,
            &[rule(DriveOperation::SheetsWrite, true)],
        )
        .await;
        assert!(
            !batch_update_was_called(&server.received_requests().await.unwrap()),
            "a refused lease must not reach batchUpdate"
        );
        outcome
    }

    /// A `--sheet` that every range self-prefixes past cannot apply to
    /// anything, so it is refused rather than silently ignored — the
    /// pure `a1::compose` check that runs before any request.
    #[tokio::test]
    async fn a_sheet_that_conflicts_with_a_prefixed_source_is_refused_before_any_request() {
        let server = wiremock::MockServer::start().await;
        let (client, sheets) = client(&server).await;
        let mut opts = base_opts(true);
        opts.sheet = Some("Q1".to_string());
        opts.source = Some("'Q2'!A2:A4".to_string());
        let outcome = text_to_columns(&client, &sheets, &opts, &[]).await;
        assert!(matches!(
            outcome.result,
            TextToColumnsResult::RefusedInvalidRange { .. }
        ));
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    /// A range the resolver cannot parse at all, as opposed to one it
    /// parses and this module then refuses for being open-ended or too
    /// wide — a different arm, with the resolver's own message.
    #[tokio::test]
    async fn an_unparseable_source_is_refused_with_the_resolvers_own_detail() {
        let server = wiremock::MockServer::start().await;
        let (client, sheets) = client(&server).await;
        mount_metadata(&server).await;
        let mut opts = base_opts(true);
        opts.source = Some("Q1!not-a-range".to_string());
        let rules = vec![rule(DriveOperation::SheetsWrite, false)];
        let outcome = text_to_columns(&client, &sheets, &opts, &rules).await;
        match &outcome.result {
            TextToColumnsResult::RefusedInvalidRange { detail } => {
                assert!(
                    !detail.is_empty(),
                    "the resolver's detail is passed through"
                );
            }
            other => panic!("expected RefusedInvalidRange, got {other:?}"),
        }
    }

    /// No row splits under the chosen delimiter, so there is no spill
    /// span to read or report — but the request is still sent, since
    /// the server's own splitting rule may differ from this preview's.
    #[tokio::test]
    async fn a_source_that_never_splits_reads_no_spill_and_still_sends_the_request() {
        let server = wiremock::MockServer::start().await;
        let (client, sheets) = client(&server).await;
        mount_metadata(&server).await;
        mount_values(
            &server,
            "'Q1'!A2:A4",
            serde_json::json!({"values": [["no separator here"]]}),
        )
        .await;
        mount_batch_update(&server, 200).await;
        let outcome = text_to_columns(
            &client,
            &sheets,
            &base_opts(false),
            &[rule(DriveOperation::SheetsWrite, false)],
        )
        .await;
        match &outcome.result {
            TextToColumnsResult::Changed {
                summary,
                spill,
                width_upper_bound,
                overwritten_cells,
                past_grid_extent,
                ..
            } => {
                assert_eq!(*width_upper_bound, 1);
                assert!(spill.is_none());
                assert!(overwritten_cells.is_empty());
                assert!(!past_grid_extent);
                assert!(summary.contains("no row spills beyond the source"));
            }
            other => panic!("expected Changed, got {other:?}"), // omni-dev: coverage ignore-line reason="this test's mocked responses always drive a Changed outcome; this catch-all guards that assumption and never runs"
        }
        let reads: Vec<_> = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.url.path().contains("/values/"))
            .collect();
        assert_eq!(reads.len(), 1, "no spill span means no second read");
    }

    #[tokio::test]
    async fn an_expired_or_unknown_lease_is_refused() {
        let server = wiremock::MockServer::start().await;
        // A token this ledger has never heard of is the same refusal as
        // an expired one (ADR-0080 §1).
        let outcome = run_with_lease(
            &server,
            Some("never-issued".to_string()),
            ledger_in_a_tempdir(),
        )
        .await;
        assert!(matches!(
            outcome.result,
            TextToColumnsResult::RefusedLeaseExpired
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
            TextToColumnsResult::RefusedLeaseWrongFile
        ));
    }

    #[tokio::test]
    async fn a_stale_lease_is_refused_when_the_file_has_moved() {
        let server = wiremock::MockServer::start().await;
        let ledger_path = ledger_in_a_tempdir();
        // The fixture reports version "1"; the lease recorded "0", so
        // the file has moved under it — ADR-0080 §6's staleness check.
        let token = seed_lease(&ledger_path, "sheet-1", "0");
        let outcome = run_with_lease(&server, Some(token), ledger_path).await;
        assert!(matches!(
            outcome.result,
            TextToColumnsResult::RefusedLeaseStale
        ));
    }
}
