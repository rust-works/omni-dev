//! Charts and slicers via `spreadsheets.batchUpdate` (issue #1797,
//! [ADR-0081](../../../docs/adrs/adr-0081.md) §3).
//!
//! One module for both, per the issue's own framing: a slicer is an
//! embedded object exactly like a chart, and both are removed by the same
//! `deleteEmbeddedObject` request, addressed by `objectId` alone with no
//! discriminator naming which kind it is.
//!
//! **Gate.** Every mutating verb here — including both deletes — reaches
//! `DriveOperation::SheetsStructure`. ADR-0081 §3 settles this the same way
//! `unmerge-cells`/`clear-data-validation` were settled in ADR-0078: a
//! chart or slicer is a property of the *sheet*, not the sheet's grid
//! *data*, one level further out than a merge or a validation rule but the
//! same kind of thing. The unrecoverability is real, so `delete-chart`/
//! `delete-slicer` read back and report the object's spec — type, title,
//! anchor position — before ever sending the request, in both `--dry-run`
//! and the real run's `drivemutation` record (the `discarded_cells`
//! treatment applied to an object instead of a value). `list-charts`/
//! `list-slicers` are read-only and ungated, like `list-protections`.
//!
//! **Chart subset (v1).** `basicChart` restricted to `chartType` ∈
//! {COLUMN, BAR, LINE, AREA, SCATTER} and `pieChart` — the issue's own
//! chosen first cut. Every other chart type (bubble, candlestick, org,
//! histogram, waterfall, treemap, scorecard, data-source) is a documented
//! cut, not a silent gap.
//!
//! **`update-chart`'s crux: no field mask.** `updateChartSpec` replaces a
//! chart's entire `ChartSpec`, unlike every other `update*` request in this
//! crate. `merge_chart_spec` fetches the existing spec, refuses it outright
//! if it isn't one of the two supported kinds (rather than silently
//! discarding a histogram's configuration), refuses a basic↔pie switch
//! (the two unions carry domain/series too differently to convert), and
//! otherwise applies only the flags the caller actually set — preserving
//! everything else, including every field this crate doesn't model, via
//! each spec type's `#[serde(flatten)] extra` map. `update-slicer` is the
//! opposite case: `updateSlicerSpec` *does* take a field mask, so it only
//! ever names the fields actually set.
//!
//! **Documented cuts, matching this feature's general stance:**
//! `COMBO`/`STEPPED_AREA` basic charts (need a per-series `type` this crate
//! doesn't model), moving or resizing an existing chart/slicer
//! (`updateEmbeddedObjectPosition` — a `move-chart`/`move-slicer` verb is a
//! natural follow-up), chart/slicer borders, and condition-based slicer
//! filter criteria (`FilterCriteria` supports `hiddenValues` only, the same
//! cut `filter.rs` makes).
//!
//! Shape mirrors `filter.rs`: compose/validate, resolve against a
//! freshly-fetched workbook, gate, dry-run, mutate, log.

use std::collections::BTreeMap;
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
    AddChartRequest, AddSlicerRequest, BasicChartAxis, BasicChartDomain, BasicChartSeries,
    BasicChartSpec, BatchUpdateRequestItem, BatchUpdateResponse, ChartData, ChartSourceRange,
    ChartSpec, DeleteEmbeddedObjectRequest, EmbeddedChart, EmbeddedObjectPosition, FilterCriteria,
    GridCoordinate, GridRange, OverlayPosition, PieChartSpec, Sheet, Slicer, SlicerSpec,
    Spreadsheet, UpdateChartSpecRequest, UpdateSlicerSpecRequest,
};
use crate::drive::types::SheetTargetRefusal;
use crate::drive::write_gate::{self, DecidingRule, DriveOperation, FolderPermissionRule};
use crate::request_log::{self, DriveMutationOutcome};

/// The five `basicChart` `chartType` values this crate builds or accepts an
/// update onto. `COMBO` and `STEPPED_AREA` are valid wire values Sheets
/// itself accepts, but neither is offered — see the module docs.
const SUPPORTED_BASIC_CHART_TYPES: &[&str] = &["COLUMN", "BAR", "LINE", "AREA", "SCATTER"];

/// Which mutation to perform.
#[derive(Debug, Clone, PartialEq)]
pub enum EmbeddedObjectVerb {
    /// Add a chart.
    AddChart {
        /// `column`/`bar`/`line`/`area`/`scatter`/`pie`.
        chart_type: String,
        /// The domain (category/x-axis) range.
        domain: String,
        /// The data series ranges — exactly one for a pie chart, one or
        /// more for a basic chart.
        series: Vec<String>,
        /// Sheet title, supplying the prefix for `domain`/`series`/
        /// `anchor` when they don't carry their own.
        sheet: Option<String>,
        /// The chart's title.
        title: Option<String>,
        /// The chart's subtitle.
        subtitle: Option<String>,
        /// `bottom`/`top`/`left`/`right`/`none`.
        legend: Option<String>,
        /// `none`/`stacked`/`percent` — basic charts only.
        stacked: Option<String>,
        /// Leading rows/columns of the source range that are headers —
        /// basic charts only.
        header_count: Option<i64>,
        /// The horizontal (`BOTTOM_AXIS`) axis title — basic charts only.
        horizontal_axis_title: Option<String>,
        /// The vertical (`LEFT_AXIS`) axis title — basic charts only.
        vertical_axis_title: Option<String>,
        /// `0.0`-`1.0` center-hole radius — pie charts only.
        pie_hole: Option<f64>,
        /// The anchor cell, when not `--new-sheet`.
        anchor: Option<String>,
        /// Additional horizontal offset from the anchor cell, in pixels.
        offset_x: Option<i64>,
        /// Additional vertical offset from the anchor cell, in pixels.
        offset_y: Option<i64>,
        /// The chart's width in pixels.
        width: Option<i64>,
        /// The chart's height in pixels.
        height: Option<i64>,
        /// Put the chart on a brand-new sheet of its own, instead of
        /// anchoring it to an existing one. Mutually exclusive with
        /// `anchor`/`offset_x`/`offset_y`/`width`/`height`.
        new_sheet: bool,
    },
    /// Replace an existing chart's spec.
    UpdateChart {
        /// Which chart to update, discovered via `list-charts`.
        chart_id: i64,
        /// Change the chart's type, when set. Refused if it would switch
        /// between a basic chart and a pie chart.
        chart_type: Option<String>,
        /// Replace the domain range, when set (requires `series` too).
        domain: Option<String>,
        /// Replace every series wholesale, when non-empty.
        series: Vec<String>,
        /// Sheet title, supplying the prefix for `domain`/`series` when
        /// they don't carry their own.
        sheet: Option<String>,
        /// Change the title, when set.
        title: Option<String>,
        /// Change the subtitle, when set.
        subtitle: Option<String>,
        /// Change the legend position, when set.
        legend: Option<String>,
        /// Change the stacking mode, when set — basic charts only.
        stacked: Option<String>,
        /// Change the header row/column count, when set — basic charts
        /// only.
        header_count: Option<i64>,
        /// Change the horizontal axis title, when set — basic charts only.
        horizontal_axis_title: Option<String>,
        /// Change the vertical axis title, when set — basic charts only.
        vertical_axis_title: Option<String>,
        /// Change the pie-hole radius, when set — pie charts only.
        pie_hole: Option<f64>,
    },
    /// Remove a chart.
    DeleteChart {
        /// Which chart to remove, discovered via `list-charts`.
        chart_id: i64,
    },
    /// Add a slicer.
    AddSlicer {
        /// Sheet title, supplying the prefix for `range`/`anchor` when
        /// they don't carry their own.
        sheet: Option<String>,
        /// The range the slicer filters.
        range: String,
        /// The 0-based column within `range` the filter criteria apply to.
        column: i64,
        /// Values to hide in `column`.
        hide_values: Vec<String>,
        /// A human-readable name for the slicer.
        title: Option<String>,
        /// Whether the slicer also filters pivot tables built from
        /// `range`.
        apply_to_pivot_tables: Option<bool>,
        /// The anchor cell.
        anchor: String,
        /// Additional horizontal offset from the anchor cell, in pixels.
        offset_x: Option<i64>,
        /// Additional vertical offset from the anchor cell, in pixels.
        offset_y: Option<i64>,
        /// The slicer's width in pixels.
        width: Option<i64>,
        /// The slicer's height in pixels.
        height: Option<i64>,
    },
    /// Change an existing slicer's range, filter column/criteria, title,
    /// or pivot-table linkage.
    UpdateSlicer {
        /// Which slicer to change, discovered via `list-slicers`.
        slicer_id: i64,
        /// Sheet title, supplying the prefix for `range` when it doesn't
        /// carry its own.
        sheet: Option<String>,
        /// Replace the filtered range, when set.
        range: Option<String>,
        /// Replace the filtered column, when set.
        column: Option<i64>,
        /// Replace the hidden-value criteria, when non-empty.
        hide_values: Vec<String>,
        /// Reset the filter criteria to empty.
        clear_criteria: bool,
        /// Change the title, when set.
        title: Option<String>,
        /// Change the pivot-table linkage, when set.
        apply_to_pivot_tables: Option<bool>,
    },
    /// Remove a slicer.
    DeleteSlicer {
        /// Which slicer to remove, discovered via `list-slicers`.
        slicer_id: i64,
    },
}

impl EmbeddedObjectVerb {
    const fn log_operation(&self) -> &'static str {
        match self {
            Self::AddChart { .. } => "sheets-add-chart",
            Self::UpdateChart { .. } => "sheets-update-chart",
            Self::DeleteChart { .. } => "sheets-delete-chart",
            Self::AddSlicer { .. } => "sheets-add-slicer",
            Self::UpdateSlicer { .. } => "sheets-update-slicer",
            Self::DeleteSlicer { .. } => "sheets-delete-slicer",
        }
    }

    const fn label(&self) -> &'static str {
        match self {
            Self::AddChart { .. } => "add-chart",
            Self::UpdateChart { .. } => "update-chart",
            Self::DeleteChart { .. } => "delete-chart",
            Self::AddSlicer { .. } => "add-slicer",
            Self::UpdateSlicer { .. } => "update-slicer",
            Self::DeleteSlicer { .. } => "delete-slicer",
        }
    }
}

/// Per-call options.
#[derive(Debug, Clone)]
pub struct EmbeddedObjectOptions {
    /// Spreadsheet id.
    pub spreadsheet_id: String,
    /// Which mutation to perform.
    pub verb: EmbeddedObjectVerb,
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

/// A chart or slicer's identifying details, as reported in a preview or a
/// completed result — the "type, title or id, anchor position" ADR-0081 §3
/// requires before an unrecoverable delete.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EmbeddedObjectSummary {
    /// The object's stable id.
    pub object_id: i64,
    /// `"chart"` or `"slicer"`.
    pub kind: String,
    /// The chart type (`"COLUMN"`, `"PIE"`, …). `None` for a slicer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chart_type: Option<String>,
    /// The object's title, if it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// A human-readable rendering of where the object is anchored.
    pub position: String,
}

/// What happened (or, under `--dry-run`, would happen).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum EmbeddedObjectResult {
    /// `--dry-run`, and the gate would allow it. `object` is populated for
    /// `update-chart`/`update-slicer` (the pre-change state) and for
    /// `delete-chart`/`delete-slicer` (the ADR-0081 §3 preview).
    WouldChange {
        /// A human-readable summary of the effect.
        summary: String,
        /// The object as it stands now, for an update or a delete.
        #[serde(skip_serializing_if = "Option::is_none")]
        object: Option<Box<EmbeddedObjectSummary>>,
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
    /// The verb's own arguments were invalid — a bad range/anchor, an
    /// unsupported flag combination, or nothing to change.
    RefusedInvalidRange {
        /// What was wrong and why.
        detail: String,
    },
    /// `update-chart`/`delete-chart` named a `--chart-id`, or
    /// `update-slicer`/`delete-slicer` a `--slicer-id`, that does not exist
    /// in this workbook.
    RefusedObjectNotFound {
        /// The id that was not found.
        object_id: i64,
    },
    /// `update-chart` named a chart whose existing spec is not one of the
    /// two supported kinds (basic or pie), or would switch between them.
    RefusedUnsupportedChart {
        /// The chart in question.
        chart_id: i64,
        /// The existing (or requested) kind that can't be handled, for the
        /// error message.
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
        /// Same summary as [`Self::WouldChange`].
        summary: String,
        /// The object's stable id — server-assigned for `add-chart`/
        /// `add-slicer`, otherwise the one resolved against.
        #[serde(skip_serializing_if = "Option::is_none")]
        object_id: Option<i64>,
        /// The object as it stood immediately before this change, for an
        /// update or a delete — same as [`Self::WouldChange`]'s `object`.
        #[serde(skip_serializing_if = "Option::is_none")]
        object: Option<Box<EmbeddedObjectSummary>>,
    },
    /// An API or validation error.
    Failed {
        /// A human-readable summary of what failed.
        detail: String,
    },
}

impl FromLeaseRefusal for EmbeddedObjectResult {
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

impl EmbeddedObjectResult {
    fn log_status(&self) -> &'static str {
        match self {
            Self::WouldChange { .. } => "would-change",
            Self::RefusedNotASpreadsheet { .. } => "refused-not-a-spreadsheet",
            Self::RefusedShortcut => "refused-shortcut",
            Self::RefusedNoVisibleParents => "refused-no-visible-parents",
            Self::RefusedSheetNotFound { .. } => "refused-sheet-not-found",
            Self::RefusedInvalidRange { .. } => "refused-invalid-range",
            Self::RefusedObjectNotFound { .. } => "refused-object-not-found",
            Self::RefusedUnsupportedChart { .. } => "refused-unsupported-chart",
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
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct EmbeddedObjectOutcome {
    /// The spreadsheet acted on.
    pub spreadsheet_id: String,
    /// Its Drive file name, when the metadata fetch got that far.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    /// The folder the gate evaluated against.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_folder_id: Option<String>,
    /// The sheet the object is (or would be) anchored on — `None` for a
    /// chart put on its own new sheet, since that sheet doesn't exist yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sheet_id: Option<i64>,
    /// Which mutation was attempted. Not serialised.
    #[serde(skip)]
    pub verb: EmbeddedObjectVerb,
    /// What happened.
    pub result: EmbeddedObjectResult,
}

impl JsonlSerialize for EmbeddedObjectOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> anyhow::Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Runs one embedded-object mutation, logging every attempt that isn't a
/// dry run.
pub async fn embedded_object(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &EmbeddedObjectOptions,
    rules: &[FolderPermissionRule],
) -> EmbeddedObjectOutcome {
    let started = Instant::now();
    let outcome = embedded_object_inner(drive, sheets, opts, rules).await;
    if !opts.dry_run {
        record_attempt(&outcome, opts, started.elapsed());
    }
    outcome
}

async fn embedded_object_inner(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &EmbeddedObjectOptions,
    rules: &[FolderPermissionRule],
) -> EmbeddedObjectOutcome {
    let bare = |result| EmbeddedObjectOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: None,
        resolved_folder_id: None,
        sheet_id: None,
        verb: opts.verb.clone(),
        result,
    };

    if let Err(detail) = validate_verb(&opts.verb) {
        return bare(EmbeddedObjectResult::RefusedInvalidRange { detail });
    }

    let (target, decision, resolved_folder_id, requires_lease) = match target_gate::resolve(
        drive,
        &opts.spreadsheet_id,
        DriveOperation::SheetsStructure,
        rules,
    )
    .await
    {
        target_gate::TargetGateOutcome::MetadataFetchFailed { detail } => {
            return bare(EmbeddedObjectResult::Failed { detail })
        }
        target_gate::TargetGateOutcome::Refused { target, refusal } => {
            let result = match refusal {
                SheetTargetRefusal::Shortcut => EmbeddedObjectResult::RefusedShortcut,
                SheetTargetRefusal::NotASpreadsheet { mime_type } => {
                    EmbeddedObjectResult::RefusedNotASpreadsheet { mime_type }
                }
                SheetTargetRefusal::NoVisibleParents => {
                    EmbeddedObjectResult::RefusedNoVisibleParents
                }
            };
            return EmbeddedObjectOutcome {
                spreadsheet_id: opts.spreadsheet_id.clone(),
                file_name: Some(target.name),
                resolved_folder_id: None,
                sheet_id: None,
                verb: opts.verb.clone(),
                result,
            };
        }
        target_gate::TargetGateOutcome::GateFetchFailed { target, detail } => {
            return EmbeddedObjectOutcome {
                spreadsheet_id: opts.spreadsheet_id.clone(),
                file_name: Some(target.name),
                resolved_folder_id: None,
                sheet_id: None,
                verb: opts.verb.clone(),
                result: EmbeddedObjectResult::Failed { detail },
            };
        }
        target_gate::TargetGateOutcome::Gated {
            target,
            decision,
            resolved_folder_id,
            requires_lease,
        } => (target, decision, resolved_folder_id, requires_lease),
    };

    let pre_gated = |sheet_id, result| EmbeddedObjectOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: Some(target.name.clone()),
        resolved_folder_id: resolved_folder_id.clone(),
        sheet_id,
        verb: opts.verb.clone(),
        result,
    };

    if decision.verdict == write_gate::Verdict::Deny {
        return pre_gated(
            None,
            EmbeddedObjectResult::Blocked {
                decided_by: decision.decided_by,
            },
        );
    }

    let api = SheetsApi::new(sheets);
    let workbook = match api
        .get_spreadsheet_with_embedded_objects(&opts.spreadsheet_id)
        .await
    {
        Ok(workbook) => workbook,
        Err(err) => {
            return pre_gated(
                None,
                EmbeddedObjectResult::Failed {
                    detail: format!("{err:#}"),
                },
            )
        }
    };

    let plan = match build_plan(&workbook, &opts.verb) {
        Ok(plan) => plan,
        Err(result) => return pre_gated(None, result),
    };

    if opts.dry_run {
        return pre_gated(
            plan.sheet_id,
            EmbeddedObjectResult::WouldChange {
                summary: plan.summary.clone(),
                object: plan.before.clone().map(Box::new),
            },
        );
    }

    let gated = |result| pre_gated(plan.sheet_id, result);

    let files_api = FilesApi::new(drive);
    let leased = LeasedWrite {
        log_prefix: "drive sheets embedded-object",
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
        api.batch_update(&opts.spreadsheet_id, vec![plan.request])
            .await,
        |err| format!("{err:#}"),
    )
    .await
    {
        Ok(response) => {
            let object_id = added_object_id(&response).or(plan.existing_id);
            EmbeddedObjectResult::Changed {
                summary: plan.summary,
                object_id,
                object: plan.before.map(Box::new),
            }
        }
        Err(err) => EmbeddedObjectResult::Failed {
            detail: format!("{err:#}"),
        },
    };
    drop(lease_grant);
    gated(result)
}

/// Rejects a verb whose own arguments are internally inconsistent, cheaply,
/// before ever fetching the workbook.
fn validate_verb(verb: &EmbeddedObjectVerb) -> Result<(), String> {
    match verb {
        EmbeddedObjectVerb::AddChart { series, .. } => {
            if series.is_empty() {
                return Err("at least one --series is required".to_string());
            }
            Ok(())
        }
        EmbeddedObjectVerb::UpdateChart {
            chart_type,
            domain,
            series,
            title,
            subtitle,
            legend,
            stacked,
            header_count,
            horizontal_axis_title,
            vertical_axis_title,
            pie_hole,
            ..
        } => {
            let nothing_to_change = chart_type.is_none()
                && domain.is_none()
                && series.is_empty()
                && title.is_none()
                && subtitle.is_none()
                && legend.is_none()
                && stacked.is_none()
                && header_count.is_none()
                && horizontal_axis_title.is_none()
                && vertical_axis_title.is_none()
                && pie_hole.is_none();
            if nothing_to_change {
                return Err(
                    "nothing to change: pass --type, --domain, --series, --title, --subtitle, \
                     --legend, --stacked, --header-count, --horizontal-axis-title, \
                     --vertical-axis-title, or --pie-hole"
                        .to_string(),
                );
            }
            if domain.is_some() && series.is_empty() {
                return Err(
                    "--domain requires --series (Sheets replaces both together, \
                    never one alone)"
                        .to_string(),
                );
            }
            Ok(())
        }
        EmbeddedObjectVerb::UpdateSlicer {
            range,
            column,
            hide_values,
            clear_criteria,
            title,
            apply_to_pivot_tables,
            ..
        } => {
            let nothing_to_change = range.is_none()
                && column.is_none()
                && hide_values.is_empty()
                && !clear_criteria
                && title.is_none()
                && apply_to_pivot_tables.is_none();
            if nothing_to_change {
                return Err(
                    "nothing to change: pass --range/--sheet, --column, --hide-values, \
                     --clear-criteria, --title, or --apply-to-pivot-tables"
                        .to_string(),
                );
            }
            Ok(())
        }
        EmbeddedObjectVerb::DeleteChart { .. }
        | EmbeddedObjectVerb::DeleteSlicer { .. }
        | EmbeddedObjectVerb::AddSlicer { .. } => Ok(()),
    }
    .map_err(|detail: String| detail)
    .and(match verb {
        EmbeddedObjectVerb::AddChart {
            anchor,
            new_sheet,
            offset_x,
            offset_y,
            width,
            height,
            ..
        } => {
            if *new_sheet {
                if anchor.is_some()
                    || offset_x.is_some()
                    || offset_y.is_some()
                    || width.is_some()
                    || height.is_some()
                {
                    Err(
                        "--new-sheet cannot be combined with --anchor/--offset-x/--offset-y/\
                         --width/--height"
                            .to_string(),
                    )
                } else {
                    Ok(())
                }
            } else if anchor.is_none() {
                Err("either --anchor or --new-sheet is required".to_string())
            } else {
                Ok(())
            }
        }
        _ => Ok(()),
    })
}

/// A fully-resolved request, computed once and reused for both the
/// `--dry-run` report and the real mutation.
struct Plan {
    request: BatchUpdateRequestItem,
    summary: String,
    sheet_id: Option<i64>,
    before: Option<EmbeddedObjectSummary>,
    existing_id: Option<i64>,
}

fn build_plan(
    workbook: &Spreadsheet,
    verb: &EmbeddedObjectVerb,
) -> Result<Plan, EmbeddedObjectResult> {
    match verb {
        EmbeddedObjectVerb::AddChart { .. } => build_add_chart(workbook, verb),
        EmbeddedObjectVerb::UpdateChart { chart_id, .. } => {
            build_update_chart(workbook, verb, *chart_id)
        }
        EmbeddedObjectVerb::DeleteChart { chart_id } => build_delete_chart(workbook, *chart_id),
        EmbeddedObjectVerb::AddSlicer { .. } => build_add_slicer(workbook, verb),
        EmbeddedObjectVerb::UpdateSlicer { slicer_id, .. } => {
            build_update_slicer(workbook, verb, *slicer_id)
        }
        EmbeddedObjectVerb::DeleteSlicer { slicer_id } => build_delete_slicer(workbook, *slicer_id),
    }
}

// ── Shared range/position resolution ────────────────────────────────────

fn invalid(detail: impl Into<String>) -> EmbeddedObjectResult {
    EmbeddedObjectResult::RefusedInvalidRange {
        detail: detail.into(),
    }
}

fn compose_and_resolve(
    workbook: &Spreadsheet,
    sheet: Option<&str>,
    range: &str,
) -> Result<GridRange, EmbeddedObjectResult> {
    let composed = a1::compose(sheet.filter(|s| !s.is_empty()), Some(range))
        .map_err(|err| invalid(err.to_string()))?;
    let (_, grid) =
        grid_range::resolve_grid_range(workbook, &composed, invalid, |title, available| {
            EmbeddedObjectResult::RefusedSheetNotFound { title, available }
        })?;
    Ok(grid)
}

fn resolve_anchor(
    workbook: &Spreadsheet,
    sheet: Option<&str>,
    anchor: &str,
) -> Result<GridCoordinate, EmbeddedObjectResult> {
    let grid = compose_and_resolve(workbook, sheet, anchor)?;
    let (Some(start_row), Some(end_row), Some(start_col), Some(end_col)) = (
        grid.start_row_index,
        grid.end_row_index,
        grid.start_column_index,
        grid.end_column_index,
    ) else {
        return Err(invalid(format!(
            "'{anchor}' must name a single cell, not an open-ended range"
        )));
    };
    if end_row - start_row != 1 || end_col - start_col != 1 {
        return Err(invalid(format!(
            "'{anchor}' must name a single cell, not a range"
        )));
    }
    Ok(GridCoordinate {
        sheet_id: grid.sheet_id,
        row_index: start_row,
        column_index: start_col,
    })
}

fn overlay_position(
    workbook: &Spreadsheet,
    sheet: Option<&str>,
    anchor: &str,
    offset_x: Option<i64>,
    offset_y: Option<i64>,
    width: Option<i64>,
    height: Option<i64>,
) -> Result<(EmbeddedObjectPosition, i64), EmbeddedObjectResult> {
    let anchor_cell = resolve_anchor(workbook, sheet, anchor)?;
    let sheet_id = anchor_cell.sheet_id;
    Ok((
        EmbeddedObjectPosition {
            overlay_position: Some(OverlayPosition {
                anchor_cell,
                offset_x_pixels: offset_x,
                offset_y_pixels: offset_y,
                width_pixels: width,
                height_pixels: height,
            }),
            ..Default::default()
        },
        sheet_id,
    ))
}

fn chart_data(range: GridRange) -> ChartData {
    ChartData {
        source_range: ChartSourceRange {
            sources: vec![range],
        },
        extra: BTreeMap::new(),
    }
}

fn parse_legend(raw: &str) -> Result<String, String> {
    match raw.to_ascii_lowercase().as_str() {
        "bottom" => Ok("BOTTOM_LEGEND".to_string()),
        "top" => Ok("TOP_LEGEND".to_string()),
        "left" => Ok("LEFT_LEGEND".to_string()),
        "right" => Ok("RIGHT_LEGEND".to_string()),
        "none" => Ok("NO_LEGEND".to_string()),
        other => Err(format!(
            "'{other}' is not a supported legend position; expected one of bottom, top, left, \
             right, none"
        )),
    }
}

fn parse_stacked(raw: &str) -> Result<String, String> {
    match raw.to_ascii_lowercase().as_str() {
        "none" => Ok("NOT_STACKED".to_string()),
        "stacked" => Ok("STACKED".to_string()),
        "percent" => Ok("PERCENT_STACKED".to_string()),
        other => Err(format!(
            "'{other}' is not a supported stacking mode; expected one of none, stacked, percent"
        )),
    }
}

/// Which chart family a `--type` value names.
#[derive(Debug)]
enum ChartKind {
    /// A `basicChart`, carrying its wire `chartType` literal.
    Basic(&'static str),
    /// A `pieChart`.
    Pie,
}

fn parse_chart_type(raw: &str) -> Result<ChartKind, String> {
    match raw.to_ascii_lowercase().as_str() {
        "column" => Ok(ChartKind::Basic("COLUMN")),
        "bar" => Ok(ChartKind::Basic("BAR")),
        "line" => Ok(ChartKind::Basic("LINE")),
        "area" => Ok(ChartKind::Basic("AREA")),
        "scatter" => Ok(ChartKind::Basic("SCATTER")),
        "pie" => Ok(ChartKind::Pie),
        other => Err(format!(
            "'{other}' is not a supported chart type; expected one of column, bar, line, area, \
             scatter, pie"
        )),
    }
}

fn axis_list(horizontal: Option<&str>, vertical: Option<&str>) -> Vec<BasicChartAxis> {
    let mut axis = Vec::new();
    if let Some(title) = horizontal {
        axis.push(BasicChartAxis {
            position: "BOTTOM_AXIS".to_string(),
            title: Some(title.to_string()),
            extra: BTreeMap::new(),
        });
    }
    if let Some(title) = vertical {
        axis.push(BasicChartAxis {
            position: "LEFT_AXIS".to_string(),
            title: Some(title.to_string()),
            extra: BTreeMap::new(),
        });
    }
    axis
}

/// Upserts one axis title by physical position, leaving every other axis
/// entry (and every field an axis carries in `extra`) untouched.
fn upsert_axis_title(axis: &mut Vec<BasicChartAxis>, position: &str, title: &str) {
    if let Some(existing) = axis.iter_mut().find(|a| a.position == position) {
        existing.title = Some(title.to_string());
    } else {
        axis.push(BasicChartAxis {
            position: position.to_string(),
            title: Some(title.to_string()),
            extra: BTreeMap::new(),
        });
    }
}

// ── add-chart ────────────────────────────────────────────────────────────

fn build_add_chart(
    workbook: &Spreadsheet,
    verb: &EmbeddedObjectVerb,
) -> Result<Plan, EmbeddedObjectResult> {
    let EmbeddedObjectVerb::AddChart {
        chart_type,
        domain,
        series,
        sheet,
        title,
        subtitle,
        legend,
        stacked,
        header_count,
        horizontal_axis_title,
        vertical_axis_title,
        pie_hole,
        anchor,
        offset_x,
        offset_y,
        width,
        height,
        new_sheet,
    } = verb
    else {
        unreachable!("build_add_chart is only ever called for AddChart")
    };

    let kind = parse_chart_type(chart_type).map_err(invalid)?;
    let domain_range = compose_and_resolve(workbook, sheet.as_deref(), domain)?;
    let series_ranges = series
        .iter()
        .map(|s| compose_and_resolve(workbook, sheet.as_deref(), s))
        .collect::<Result<Vec<_>, _>>()?;
    let legend = legend
        .as_deref()
        .map(parse_legend)
        .transpose()
        .map_err(invalid)?;

    let (position, position_sheet_id) = if *new_sheet {
        (
            EmbeddedObjectPosition {
                new_sheet: Some(true),
                ..Default::default()
            },
            None,
        )
    } else {
        let Some(anchor) = anchor.as_deref() else {
            return Err(invalid("either --anchor or --new-sheet is required"));
        };
        let (position, sheet_id) = overlay_position(
            workbook,
            sheet.as_deref(),
            anchor,
            *offset_x,
            *offset_y,
            *width,
            *height,
        )?;
        (position, Some(sheet_id))
    };

    let spec = match kind {
        ChartKind::Basic(chart_type) => {
            if pie_hole.is_some() {
                return Err(invalid("--pie-hole only applies to a pie chart"));
            }
            let stacked = stacked
                .as_deref()
                .map(parse_stacked)
                .transpose()
                .map_err(invalid)?;
            ChartSpec {
                title: title.clone(),
                subtitle: subtitle.clone(),
                basic_chart: Some(BasicChartSpec {
                    chart_type: chart_type.to_string(),
                    legend_position: legend,
                    stacked_type: stacked,
                    header_count: *header_count,
                    axis: axis_list(
                        horizontal_axis_title.as_deref(),
                        vertical_axis_title.as_deref(),
                    ),
                    domains: vec![BasicChartDomain {
                        domain: chart_data(domain_range),
                        extra: BTreeMap::new(),
                    }],
                    series: series_ranges
                        .into_iter()
                        .map(|r| BasicChartSeries {
                            series: chart_data(r),
                            target_axis: None,
                            extra: BTreeMap::new(),
                        })
                        .collect(),
                    extra: BTreeMap::new(),
                }),
                pie_chart: None,
                extra: BTreeMap::new(),
            }
        }
        ChartKind::Pie => {
            if header_count.is_some() {
                return Err(invalid(
                    "--header-count only applies to a column/bar/line/area/\
                    scatter chart",
                ));
            }
            if stacked.is_some() {
                return Err(invalid(
                    "--stacked only applies to a column/bar/line/area/scatter \
                    chart",
                ));
            }
            if horizontal_axis_title.is_some() || vertical_axis_title.is_some() {
                return Err(invalid(
                    "--horizontal-axis-title/--vertical-axis-title only apply \
                    to a column/bar/line/area/scatter chart",
                ));
            }
            if series_ranges.len() != 1 {
                return Err(invalid("a pie chart takes exactly one --series"));
            }
            let mut series_ranges = series_ranges;
            let Some(pie_series_range) = series_ranges.pop() else {
                unreachable!("checked series_ranges.len() == 1 above")
            };
            ChartSpec {
                title: title.clone(),
                subtitle: subtitle.clone(),
                basic_chart: None,
                pie_chart: Some(PieChartSpec {
                    domain: chart_data(domain_range),
                    series: chart_data(pie_series_range),
                    legend_position: legend,
                    pie_hole: *pie_hole,
                    extra: BTreeMap::new(),
                }),
                extra: BTreeMap::new(),
            }
        }
    };

    let summary = format!(
        "add {} chart{}",
        chart_type.to_ascii_lowercase(),
        title
            .as_deref()
            .map_or_else(String::new, |t| format!(" '{t}'"))
    );

    Ok(Plan {
        request: BatchUpdateRequestItem::AddChart(AddChartRequest {
            chart: EmbeddedChart {
                chart_id: None,
                spec: Some(spec),
                position: Some(position),
            },
        }),
        summary,
        sheet_id: position_sheet_id,
        before: None,
        existing_id: None,
    })
}

// ── update-chart ─────────────────────────────────────────────────────────

fn find_chart(workbook: &Spreadsheet, chart_id: i64) -> Option<(&Sheet, &EmbeddedChart)> {
    workbook.sheets.iter().find_map(|sheet| {
        sheet
            .charts
            .iter()
            .find(|c| c.chart_id == Some(chart_id))
            .map(|c| (sheet, c))
    })
}

fn find_slicer(workbook: &Spreadsheet, slicer_id: i64) -> Option<(&Sheet, &Slicer)> {
    workbook.sheets.iter().find_map(|sheet| {
        sheet
            .slicers
            .iter()
            .find(|s| s.slicer_id == Some(slicer_id))
            .map(|s| (sheet, s))
    })
}

fn describe_overlay_position(sheet: &Sheet, overlay: &OverlayPosition) -> String {
    format!(
        "{}!{}{}",
        sheet.title(),
        grid_range::column_index_to_letters(overlay.anchor_cell.column_index),
        overlay.anchor_cell.row_index + 1
    )
}

fn describe_position(sheet: &Sheet, position: Option<&EmbeddedObjectPosition>) -> String {
    match position {
        Some(p) if p.new_sheet == Some(true) || p.sheet_id.is_some() => p.sheet_id.map_or_else(
            || "own sheet".to_string(),
            |id| format!("own sheet (id {id})"),
        ),
        Some(p) => p.overlay_position.as_ref().map_or_else(
            || "unknown position".to_string(),
            |o| describe_overlay_position(sheet, o),
        ),
        None => "unknown position".to_string(),
    }
}

fn summarise_chart(sheet: &Sheet, chart: &EmbeddedChart) -> Option<EmbeddedObjectSummary> {
    let object_id = chart.chart_id?;
    let spec = chart.spec.as_ref();
    let chart_type = spec.and_then(|s| {
        if let Some(basic) = &s.basic_chart {
            Some(basic.chart_type.clone())
        } else if s.pie_chart.is_some() {
            Some("PIE".to_string())
        } else {
            None
        }
    });
    Some(EmbeddedObjectSummary {
        object_id,
        kind: "chart".to_string(),
        chart_type,
        title: spec.and_then(|s| s.title.clone()),
        position: describe_position(sheet, chart.position.as_ref()),
    })
}

fn summarise_slicer(sheet: &Sheet, slicer: &Slicer) -> Option<EmbeddedObjectSummary> {
    let object_id = slicer.slicer_id?;
    Some(EmbeddedObjectSummary {
        object_id,
        kind: "slicer".to_string(),
        chart_type: None,
        title: slicer.spec.as_ref().and_then(|s| s.title.clone()),
        position: describe_position(sheet, slicer.position.as_ref()),
    })
}

/// The two chart kinds `merge_chart_spec` accepts as the *existing* state
/// of a chart being updated.
#[derive(Debug)]
enum ExistingChartKind {
    Basic,
    Pie,
}

fn existing_chart_kind(spec: &ChartSpec) -> Result<ExistingChartKind, String> {
    if let Some(basic) = &spec.basic_chart {
        if SUPPORTED_BASIC_CHART_TYPES.contains(&basic.chart_type.as_str()) {
            Ok(ExistingChartKind::Basic)
        } else {
            Err(basic.chart_type.clone())
        }
    } else if spec.pie_chart.is_some() {
        Ok(ExistingChartKind::Pie)
    } else {
        Err(spec
            .extra
            .keys()
            .next()
            .cloned()
            .unwrap_or_else(|| "unknown".to_string()))
    }
}

fn merge_chart_spec(
    workbook: &Spreadsheet,
    existing: &ChartSpec,
    verb: &EmbeddedObjectVerb,
) -> Result<(ChartSpec, String), EmbeddedObjectResult> {
    let EmbeddedObjectVerb::UpdateChart {
        chart_id,
        chart_type,
        title,
        subtitle,
        legend,
        ..
    } = verb
    else {
        unreachable!("merge_chart_spec is only ever called for UpdateChart")
    };

    let existing_kind = existing_chart_kind(existing).map_err(|detail| {
        EmbeddedObjectResult::RefusedUnsupportedChart {
            chart_id: *chart_id,
            detail: format!("chart's existing spec is {detail}, not a supported basic/pie chart"),
        }
    })?;

    let requested_kind = chart_type
        .as_deref()
        .map(parse_chart_type)
        .transpose()
        .map_err(invalid)?;

    let target_is_pie = match &requested_kind {
        Some(ChartKind::Pie) => true,
        Some(ChartKind::Basic(_)) => false,
        None => matches!(existing_kind, ExistingChartKind::Pie),
    };
    if target_is_pie != matches!(existing_kind, ExistingChartKind::Pie) {
        return Err(EmbeddedObjectResult::RefusedUnsupportedChart {
            chart_id: *chart_id,
            detail: "switching between a basic chart and a pie chart is not supported; delete \
                     and re-add"
                .to_string(),
        });
    }

    let mut changed = Vec::new();
    if title.is_some() {
        changed.push("title");
    }
    if subtitle.is_some() {
        changed.push("subtitle");
    }
    if legend.is_some() {
        changed.push("legend");
    }

    let mut spec = existing.clone();
    if let Some(title) = title {
        spec.title = Some(title.clone());
    }
    if let Some(subtitle) = subtitle {
        spec.subtitle = Some(subtitle.clone());
    }

    if target_is_pie {
        merge_pie_chart(workbook, &mut spec, verb, &mut changed)?;
    } else {
        merge_basic_chart(workbook, &mut spec, verb, requested_kind, &mut changed)?;
    }

    let summary = format!("update chart {chart_id} ({})", changed.join(", "));
    Ok((spec, summary))
}

/// The `target_is_pie` branch of [`merge_chart_spec`]: merges every
/// pie-only flag onto `spec.pie_chart`, refusing any basic-only flag.
fn merge_pie_chart(
    workbook: &Spreadsheet,
    spec: &mut ChartSpec,
    verb: &EmbeddedObjectVerb,
    changed: &mut Vec<&'static str>,
) -> Result<(), EmbeddedObjectResult> {
    let EmbeddedObjectVerb::UpdateChart {
        domain,
        series,
        sheet,
        legend,
        stacked,
        header_count,
        horizontal_axis_title,
        vertical_axis_title,
        pie_hole,
        ..
    } = verb
    else {
        unreachable!("merge_pie_chart is only ever called for UpdateChart")
    };

    if header_count.is_some() {
        return Err(invalid(
            "--header-count only applies to a column/bar/line/area/scatter chart",
        ));
    }
    if stacked.is_some() {
        return Err(invalid(
            "--stacked only applies to a column/bar/line/area/scatter chart",
        ));
    }
    if horizontal_axis_title.is_some() || vertical_axis_title.is_some() {
        return Err(invalid(
            "--horizontal-axis-title/--vertical-axis-title only apply to a \
             column/bar/line/area/scatter chart",
        ));
    }

    let mut pie = spec.pie_chart.clone().unwrap_or_default();
    if let Some(domain) = domain {
        pie.domain = chart_data(compose_and_resolve(workbook, sheet.as_deref(), domain)?);
        changed.push("domain");
    }
    if !series.is_empty() {
        let [only_series] = series.as_slice() else {
            return Err(invalid("a pie chart takes exactly one --series"));
        };
        let range = compose_and_resolve(workbook, sheet.as_deref(), only_series)?;
        pie.series = chart_data(range);
        changed.push("series");
    }
    if let Some(legend) = legend
        .as_deref()
        .map(parse_legend)
        .transpose()
        .map_err(invalid)?
    {
        pie.legend_position = Some(legend);
    }
    if let Some(pie_hole) = pie_hole {
        pie.pie_hole = Some(*pie_hole);
        changed.push("pie-hole");
    }
    spec.pie_chart = Some(pie);
    spec.basic_chart = None;
    Ok(())
}

/// The `!target_is_pie` branch of [`merge_chart_spec`]: merges every
/// basic-chart flag onto `spec.basic_chart`. `requested_kind` is passed in
/// (rather than re-parsed) since [`merge_chart_spec`] already needed it to
/// decide `target_is_pie`.
fn merge_basic_chart(
    workbook: &Spreadsheet,
    spec: &mut ChartSpec,
    verb: &EmbeddedObjectVerb,
    requested_kind: Option<ChartKind>,
    changed: &mut Vec<&'static str>,
) -> Result<(), EmbeddedObjectResult> {
    let EmbeddedObjectVerb::UpdateChart {
        domain,
        series,
        sheet,
        legend,
        stacked,
        header_count,
        horizontal_axis_title,
        vertical_axis_title,
        ..
    } = verb
    else {
        unreachable!("merge_basic_chart is only ever called for UpdateChart")
    };

    let mut basic = spec.basic_chart.clone().unwrap_or_default();
    if let Some(ChartKind::Basic(chart_type)) = requested_kind {
        basic.chart_type = chart_type.to_string();
        changed.push("type");
    }
    if let Some(domain) = domain {
        basic.domains = vec![BasicChartDomain {
            domain: chart_data(compose_and_resolve(workbook, sheet.as_deref(), domain)?),
            extra: BTreeMap::new(),
        }];
        changed.push("domain");
    }
    if !series.is_empty() {
        basic.series = series
            .iter()
            .map(|s| {
                compose_and_resolve(workbook, sheet.as_deref(), s).map(|range| BasicChartSeries {
                    series: chart_data(range),
                    target_axis: None,
                    extra: BTreeMap::new(),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        changed.push("series");
    }
    if let Some(legend) = legend
        .as_deref()
        .map(parse_legend)
        .transpose()
        .map_err(invalid)?
    {
        basic.legend_position = Some(legend);
    }
    if let Some(stacked) = stacked
        .as_deref()
        .map(parse_stacked)
        .transpose()
        .map_err(invalid)?
    {
        basic.stacked_type = Some(stacked);
        changed.push("stacked");
    }
    if let Some(header_count) = header_count {
        basic.header_count = Some(*header_count);
        changed.push("header-count");
    }
    if let Some(title) = horizontal_axis_title {
        upsert_axis_title(&mut basic.axis, "BOTTOM_AXIS", title);
        changed.push("horizontal-axis-title");
    }
    if let Some(title) = vertical_axis_title {
        upsert_axis_title(&mut basic.axis, "LEFT_AXIS", title);
        changed.push("vertical-axis-title");
    }
    spec.basic_chart = Some(basic);
    spec.pie_chart = None;
    Ok(())
}

fn build_update_chart(
    workbook: &Spreadsheet,
    verb: &EmbeddedObjectVerb,
    chart_id: i64,
) -> Result<Plan, EmbeddedObjectResult> {
    let Some((sheet, chart)) = find_chart(workbook, chart_id) else {
        return Err(EmbeddedObjectResult::RefusedObjectNotFound {
            object_id: chart_id,
        });
    };
    let Some(existing_spec) = chart.spec.as_ref() else {
        return Err(EmbeddedObjectResult::RefusedUnsupportedChart {
            chart_id,
            detail: "chart has no spec to merge onto".to_string(),
        });
    };
    let before = summarise_chart(sheet, chart);
    let sheet_id = sheet.sheet_id();
    let (spec, summary) = merge_chart_spec(workbook, existing_spec, verb)?;

    Ok(Plan {
        request: BatchUpdateRequestItem::UpdateChartSpec(UpdateChartSpecRequest { chart_id, spec }),
        summary,
        sheet_id,
        before,
        existing_id: Some(chart_id),
    })
}

fn build_delete_chart(workbook: &Spreadsheet, chart_id: i64) -> Result<Plan, EmbeddedObjectResult> {
    let Some((sheet, chart)) = find_chart(workbook, chart_id) else {
        return Err(EmbeddedObjectResult::RefusedObjectNotFound {
            object_id: chart_id,
        });
    };
    let before = summarise_chart(sheet, chart);
    let sheet_id = sheet.sheet_id();
    let summary = format!("delete chart {chart_id}");
    Ok(Plan {
        request: BatchUpdateRequestItem::DeleteEmbeddedObject(DeleteEmbeddedObjectRequest {
            object_id: chart_id,
        }),
        summary,
        sheet_id,
        before,
        existing_id: Some(chart_id),
    })
}

// ── add-slicer / update-slicer / delete-slicer ──────────────────────────

fn build_add_slicer(
    workbook: &Spreadsheet,
    verb: &EmbeddedObjectVerb,
) -> Result<Plan, EmbeddedObjectResult> {
    let EmbeddedObjectVerb::AddSlicer {
        sheet,
        range,
        column,
        hide_values,
        title,
        apply_to_pivot_tables,
        anchor,
        offset_x,
        offset_y,
        width,
        height,
    } = verb
    else {
        unreachable!("build_add_slicer is only ever called for AddSlicer")
    };

    let data_range = compose_and_resolve(workbook, sheet.as_deref(), range)?;
    let (position, position_sheet_id) = overlay_position(
        workbook,
        sheet.as_deref(),
        anchor,
        *offset_x,
        *offset_y,
        *width,
        *height,
    )?;

    let filter_criteria = if hide_values.is_empty() {
        None
    } else {
        Some(FilterCriteria {
            hidden_values: hide_values.clone(),
        })
    };

    let summary = format!(
        "add slicer{}",
        title
            .as_deref()
            .map_or_else(String::new, |t| format!(" '{t}'"))
    );

    Ok(Plan {
        request: BatchUpdateRequestItem::AddSlicer(AddSlicerRequest {
            slicer: Slicer {
                slicer_id: None,
                spec: Some(SlicerSpec {
                    data_range: Some(data_range),
                    filter_criteria,
                    column_index: Some(*column),
                    apply_to_pivot_tables: *apply_to_pivot_tables,
                    title: title.clone(),
                    extra: BTreeMap::new(),
                }),
                position: Some(position),
            },
        }),
        summary,
        sheet_id: Some(position_sheet_id),
        before: None,
        existing_id: None,
    })
}

fn build_update_slicer(
    workbook: &Spreadsheet,
    verb: &EmbeddedObjectVerb,
    slicer_id: i64,
) -> Result<Plan, EmbeddedObjectResult> {
    let EmbeddedObjectVerb::UpdateSlicer {
        sheet,
        range,
        column,
        hide_values,
        clear_criteria,
        title,
        apply_to_pivot_tables,
        ..
    } = verb
    else {
        unreachable!("build_update_slicer is only ever called for UpdateSlicer")
    };

    let Some((host_sheet, slicer)) = find_slicer(workbook, slicer_id) else {
        return Err(EmbeddedObjectResult::RefusedObjectNotFound {
            object_id: slicer_id,
        });
    };
    let before = summarise_slicer(host_sheet, slicer);
    let sheet_id = host_sheet.sheet_id();

    let mut spec = SlicerSpec::default();
    let mut fields = Vec::new();
    let mut changed = Vec::new();
    if let Some(range) = range {
        spec.data_range = Some(compose_and_resolve(workbook, sheet.as_deref(), range)?);
        fields.push("dataRange");
        changed.push("range");
    }
    if *clear_criteria {
        spec.filter_criteria = Some(FilterCriteria::default());
        fields.push("filterCriteria");
        changed.push("clear criteria");
    } else if !hide_values.is_empty() {
        spec.filter_criteria = Some(FilterCriteria {
            hidden_values: hide_values.clone(),
        });
        fields.push("filterCriteria");
        changed.push("criteria");
    }
    if let Some(column) = column {
        spec.column_index = Some(*column);
        fields.push("columnIndex");
        changed.push("column");
    }
    if let Some(title) = title {
        spec.title = Some(title.clone());
        fields.push("title");
        changed.push("title");
    }
    if let Some(apply) = apply_to_pivot_tables {
        spec.apply_to_pivot_tables = Some(*apply);
        fields.push("applyToPivotTables");
        changed.push("apply-to-pivot-tables");
    }

    let summary = format!("update slicer {slicer_id} ({})", changed.join(", "));
    Ok(Plan {
        request: BatchUpdateRequestItem::UpdateSlicerSpec(UpdateSlicerSpecRequest {
            slicer_id,
            spec,
            fields: fields.join(","),
        }),
        summary,
        sheet_id,
        before,
        existing_id: Some(slicer_id),
    })
}

fn build_delete_slicer(
    workbook: &Spreadsheet,
    slicer_id: i64,
) -> Result<Plan, EmbeddedObjectResult> {
    let Some((sheet, slicer)) = find_slicer(workbook, slicer_id) else {
        return Err(EmbeddedObjectResult::RefusedObjectNotFound {
            object_id: slicer_id,
        });
    };
    let before = summarise_slicer(sheet, slicer);
    let sheet_id = sheet.sheet_id();
    let summary = format!("delete slicer {slicer_id}");
    Ok(Plan {
        request: BatchUpdateRequestItem::DeleteEmbeddedObject(DeleteEmbeddedObjectRequest {
            object_id: slicer_id,
        }),
        summary,
        sheet_id,
        before,
        existing_id: Some(slicer_id),
    })
}

// ── list-charts / list-slicers ───────────────────────────────────────────

/// Extracts every chart's summary from an already-fetched workbook, for
/// `list-charts`.
///
/// Takes the workbook rather than fetching it itself — the CLI leaf fetches
/// once and reuses the same `Spreadsheet` for `-o json`/`-o yaml`'s raw
/// dump, matching `list-protections`/`list-filter-views`' own shape.
#[must_use]
pub fn charts_from_workbook(workbook: &Spreadsheet) -> Vec<EmbeddedObjectSummary> {
    workbook
        .sheets
        .iter()
        .flat_map(|sheet| {
            sheet
                .charts
                .iter()
                .filter_map(move |chart| summarise_chart(sheet, chart))
        })
        .collect()
}

/// Extracts every slicer's summary from an already-fetched workbook, for
/// `list-slicers`. See [`charts_from_workbook`]'s doc comment.
#[must_use]
pub fn slicers_from_workbook(workbook: &Spreadsheet) -> Vec<EmbeddedObjectSummary> {
    workbook
        .sheets
        .iter()
        .flat_map(|sheet| {
            sheet
                .slicers
                .iter()
                .filter_map(move |slicer| summarise_slicer(sheet, slicer))
        })
        .collect()
}

// ── logging and rendering ────────────────────────────────────────────────

fn added_object_id(response: &BatchUpdateResponse) -> Option<i64> {
    response.replies.iter().find_map(|reply| {
        reply
            .add_chart
            .as_ref()
            .and_then(|r| r.chart.as_ref())
            .and_then(|c| c.chart_id)
            .or_else(|| {
                reply
                    .add_slicer
                    .as_ref()
                    .and_then(|r| r.slicer.as_ref())
                    .and_then(|s| s.slicer_id)
            })
    })
}

fn record_attempt(
    outcome: &EmbeddedObjectOutcome,
    opts: &EmbeddedObjectOptions,
    duration: Duration,
) {
    let error = match &outcome.result {
        EmbeddedObjectResult::Failed { detail } => Some(detail.clone()),
        _ => None,
    };
    let decided_by = match &outcome.result {
        EmbeddedObjectResult::Blocked { decided_by } => decided_by.as_ref(),
        _ => None,
    };
    let decided_by = write_gate::decided_by_log_fields(decided_by);
    let (embedded_object_id, fields_changed) = match &outcome.result {
        EmbeddedObjectResult::Changed {
            object_id,
            summary,
            object,
        } => {
            // For the two deletes, `object` carries the ADR-0081 §3
            // preview — the spec that is about to become unrecoverable —
            // and it belongs in the audit trail exactly as much as in the
            // `--dry-run` report, so it is appended here rather than left
            // to the summary text alone.
            let fields_changed = object.as_deref().map_or_else(
                || summary.clone(),
                |object| format!("{summary} ({})", object_preview(object)),
            );
            (*object_id, Some(fields_changed))
        }
        _ => (None, None),
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
        sheet_id: outcome.sheet_id,
        embedded_object_id,
        fields_changed,
        error,
        duration,
        ..Default::default()
    });
}

/// Renders an object's type/title/anchor — the ADR-0081 §3 preview text,
/// shared by the human-readable CLI rendering ([`describe_object`]) and the
/// `drivemutation` record's `fields_changed` ([`record_attempt`]).
fn object_preview(object: &EmbeddedObjectSummary) -> String {
    let kind_detail = object
        .chart_type
        .as_deref()
        .map_or_else(|| object.kind.clone(), |t| format!("{} ({t})", object.kind));
    let title = object
        .title
        .as_deref()
        .map_or_else(String::new, |t| format!(" '{t}'"));
    format!(
        "id {}: {kind_detail}{title}, anchored {}",
        object.object_id, object.position
    )
}

fn describe_object(object: &EmbeddedObjectSummary) -> String {
    format!("  {}", object_preview(object))
}

/// Renders an outcome as human-readable text.
#[must_use]
pub fn describe(outcome: &EmbeddedObjectOutcome) -> String {
    describe_lines(outcome).join("\n")
}

/// Renders an outcome as its individual lines, none of which contains a
/// newline — see `structure.rs::describe_lines` for why this shape exists.
#[must_use]
pub fn describe_lines(outcome: &EmbeddedObjectOutcome) -> Vec<String> {
    let verb = &outcome.verb;
    let book = outcome.file_name.as_deref().map_or_else(
        || format!("'{}'", outcome.spreadsheet_id),
        |n| format!("'{n}'"),
    );
    match &outcome.result {
        EmbeddedObjectResult::WouldChange { summary, object } => {
            let mut lines = vec![format!("Would {summary} in {book}")];
            if let Some(object) = object {
                lines.push(describe_object(object));
            }
            lines
        }
        EmbeddedObjectResult::RefusedNotASpreadsheet { mime_type } => vec![format!(
            "Refused: {book} is not a Google Sheet (mimeType: {mime_type}); \
             `drive sheets {}` only works on spreadsheets",
            verb.label()
        )],
        EmbeddedObjectResult::RefusedShortcut => vec![format!(
            "Refused: {book} is a shortcut; `drive sheets {}` doesn't follow shortcuts",
            verb.label()
        )],
        EmbeddedObjectResult::RefusedNoVisibleParents => vec![format!(
            "Refused: {book} has no parent folder visible to this account, so no folder \
             rule can apply to it. Grant it by id instead: add {{\"file_id\": \"<spreadsheet \
             id>\", \"allow\": [\"sheets-structure\"]}} to write_permissions.rules."
        )],
        EmbeddedObjectResult::RefusedSheetNotFound { title, available } => {
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
        EmbeddedObjectResult::RefusedInvalidRange { detail } => vec![format!("Refused: {detail}")],
        EmbeddedObjectResult::RefusedObjectNotFound { object_id } => vec![format!(
            "Refused: {book} has no chart or slicer with id {object_id}; run `drive sheets \
             list-charts`/`list-slicers` to see what exists"
        )],
        EmbeddedObjectResult::RefusedUnsupportedChart { chart_id, detail } => {
            vec![format!("Refused: chart {chart_id} in {book}: {detail}")]
        }
        EmbeddedObjectResult::Blocked { decided_by } => vec![match decided_by {
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
        EmbeddedObjectResult::RefusedNoLease => LeaseGateRefusal::NoLease
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        EmbeddedObjectResult::RefusedLeaseExpired => LeaseGateRefusal::Expired
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        EmbeddedObjectResult::RefusedLeaseWrongFile => LeaseGateRefusal::WrongFile
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        EmbeddedObjectResult::RefusedLeaseStale => LeaseGateRefusal::Stale
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        EmbeddedObjectResult::Changed {
            summary,
            object_id,
            object,
        } => {
            let id = object_id.map_or_else(String::new, |id| format!(" (id {id})"));
            let mut lines = vec![format!("Applied: {summary}{id} in {book}")];
            if let Some(object) = object {
                lines.push(describe_object(object));
            }
            lines
        }
        EmbeddedObjectResult::Failed { detail } => vec![format!("Failed: {detail}")],
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::sheets::client::SHEETS_API_URL;
    use crate::drive::sheets::types::Sheet;
    use crate::drive::test_support::seed_lease;
    use crate::test_support::env::MapEnv;
    use crate::utils::secret::Secret;
    use std::collections::HashSet;

    // ── parse_chart_type / parse_legend / parse_stacked ─────────────────

    #[test]
    fn parse_chart_type_accepts_every_basic_type_and_pie() {
        for (raw, expect_basic) in [
            ("column", Some("COLUMN")),
            ("bar", Some("BAR")),
            ("line", Some("LINE")),
            ("area", Some("AREA")),
            ("scatter", Some("SCATTER")),
            ("PIE", None),
        ] {
            let kind = parse_chart_type(raw).unwrap();
            match (&kind, expect_basic) {
                (ChartKind::Basic(t), Some(expected)) => assert_eq!(*t, expected),
                (ChartKind::Pie, None) => {}
                _ => panic!("unexpected parse for {raw:?}: {kind:?}"),
            }
        }
    }

    #[test]
    fn parse_chart_type_rejects_combo_and_unknown() {
        let err = parse_chart_type("combo").unwrap_err();
        assert!(err.contains("not a supported chart type"), "{err}");
        let err = parse_chart_type("bogus").unwrap_err();
        assert!(
            err.contains("column, bar, line, area, scatter, pie"),
            "{err}"
        );
    }

    #[test]
    fn parse_legend_accepts_every_position() {
        assert_eq!(parse_legend("bottom").unwrap(), "BOTTOM_LEGEND");
        assert_eq!(parse_legend("TOP").unwrap(), "TOP_LEGEND");
        assert_eq!(parse_legend("left").unwrap(), "LEFT_LEGEND");
        assert_eq!(parse_legend("right").unwrap(), "RIGHT_LEGEND");
        assert_eq!(parse_legend("none").unwrap(), "NO_LEGEND");
    }

    #[test]
    fn parse_legend_rejects_unknown() {
        let err = parse_legend("middle").unwrap_err();
        assert!(err.contains("not a supported legend position"), "{err}");
    }

    #[test]
    fn parse_stacked_accepts_every_mode() {
        assert_eq!(parse_stacked("none").unwrap(), "NOT_STACKED");
        assert_eq!(parse_stacked("stacked").unwrap(), "STACKED");
        assert_eq!(parse_stacked("PERCENT").unwrap(), "PERCENT_STACKED");
    }

    #[test]
    fn parse_stacked_rejects_unknown() {
        let err = parse_stacked("half").unwrap_err();
        assert!(err.contains("not a supported stacking mode"), "{err}");
    }

    // ── axis_list / upsert_axis_title ────────────────────────────────────

    #[test]
    fn axis_list_builds_only_the_named_axes() {
        assert!(axis_list(None, None).is_empty());
        let axis = axis_list(Some("Quarter"), None);
        assert_eq!(axis.len(), 1);
        assert_eq!(axis[0].position, "BOTTOM_AXIS");
        assert_eq!(axis[0].title.as_deref(), Some("Quarter"));
        let axis = axis_list(Some("Quarter"), Some("Revenue"));
        assert_eq!(axis.len(), 2);
        assert_eq!(axis[1].position, "LEFT_AXIS");
    }

    #[test]
    fn upsert_axis_title_replaces_an_existing_entry_in_place() {
        let mut axis = vec![
            BasicChartAxis {
                position: "BOTTOM_AXIS".to_string(),
                title: Some("Old".to_string()),
                extra: BTreeMap::new(),
            },
            BasicChartAxis {
                position: "LEFT_AXIS".to_string(),
                title: None,
                extra: BTreeMap::new(),
            },
        ];
        upsert_axis_title(&mut axis, "BOTTOM_AXIS", "New");
        assert_eq!(axis.len(), 2);
        assert_eq!(axis[0].title.as_deref(), Some("New"));
        assert_eq!(axis[1].title, None);
    }

    #[test]
    fn upsert_axis_title_appends_when_absent() {
        let mut axis = Vec::new();
        upsert_axis_title(&mut axis, "LEFT_AXIS", "Revenue");
        assert_eq!(axis.len(), 1);
        assert_eq!(axis[0].position, "LEFT_AXIS");
    }

    // ── log_operation / label ────────────────────────────────────────────

    #[test]
    fn every_verb_has_a_distinct_log_operation_and_label() {
        let verbs = [
            EmbeddedObjectVerb::AddChart {
                chart_type: "column".to_string(),
                domain: String::new(),
                series: vec![String::new()],
                sheet: None,
                title: None,
                subtitle: None,
                legend: None,
                stacked: None,
                header_count: None,
                horizontal_axis_title: None,
                vertical_axis_title: None,
                pie_hole: None,
                anchor: Some(String::new()),
                offset_x: None,
                offset_y: None,
                width: None,
                height: None,
                new_sheet: false,
            },
            EmbeddedObjectVerb::UpdateChart {
                chart_id: 1,
                chart_type: None,
                domain: None,
                series: Vec::new(),
                sheet: None,
                title: None,
                subtitle: None,
                legend: None,
                stacked: None,
                header_count: None,
                horizontal_axis_title: None,
                vertical_axis_title: None,
                pie_hole: None,
            },
            EmbeddedObjectVerb::DeleteChart { chart_id: 1 },
            EmbeddedObjectVerb::AddSlicer {
                sheet: None,
                range: String::new(),
                column: 0,
                hide_values: Vec::new(),
                title: None,
                apply_to_pivot_tables: None,
                anchor: String::new(),
                offset_x: None,
                offset_y: None,
                width: None,
                height: None,
            },
            EmbeddedObjectVerb::UpdateSlicer {
                slicer_id: 1,
                sheet: None,
                range: None,
                column: None,
                hide_values: Vec::new(),
                clear_criteria: false,
                title: None,
                apply_to_pivot_tables: None,
            },
            EmbeddedObjectVerb::DeleteSlicer { slicer_id: 1 },
        ];
        let ops: HashSet<&str> = verbs
            .iter()
            .map(EmbeddedObjectVerb::log_operation)
            .collect();
        let labels: HashSet<&str> = verbs.iter().map(EmbeddedObjectVerb::label).collect();
        assert_eq!(ops.len(), verbs.len());
        assert_eq!(labels.len(), verbs.len());
    }

    // ── validate_verb ────────────────────────────────────────────────────

    fn add_chart_verb() -> EmbeddedObjectVerb {
        EmbeddedObjectVerb::AddChart {
            chart_type: "column".to_string(),
            domain: "A1:A10".to_string(),
            series: vec!["B1:B10".to_string()],
            sheet: Some("Sheet1".to_string()),
            title: None,
            subtitle: None,
            legend: None,
            stacked: None,
            header_count: None,
            horizontal_axis_title: None,
            vertical_axis_title: None,
            pie_hole: None,
            anchor: Some("E2".to_string()),
            offset_x: None,
            offset_y: None,
            width: None,
            height: None,
            new_sheet: false,
        }
    }

    #[test]
    fn validate_verb_rejects_add_chart_with_no_series() {
        let mut verb = add_chart_verb();
        let EmbeddedObjectVerb::AddChart { series, .. } = &mut verb else {
            unreachable!()
        };
        series.clear();
        let err = validate_verb(&verb).unwrap_err();
        assert!(err.contains("at least one --series"), "{err}");
    }

    #[test]
    fn validate_verb_rejects_add_chart_with_neither_anchor_nor_new_sheet() {
        let mut verb = add_chart_verb();
        let EmbeddedObjectVerb::AddChart { anchor, .. } = &mut verb else {
            unreachable!()
        };
        *anchor = None;
        let err = validate_verb(&verb).unwrap_err();
        assert!(err.contains("--anchor or --new-sheet"), "{err}");
    }

    #[test]
    fn validate_verb_rejects_new_sheet_combined_with_anchor() {
        let mut verb = add_chart_verb();
        let EmbeddedObjectVerb::AddChart { new_sheet, .. } = &mut verb else {
            unreachable!()
        };
        *new_sheet = true;
        let err = validate_verb(&verb).unwrap_err();
        assert!(err.contains("--new-sheet cannot be combined"), "{err}");
    }

    #[test]
    fn validate_verb_accepts_new_sheet_alone() {
        let mut verb = add_chart_verb();
        let EmbeddedObjectVerb::AddChart {
            anchor, new_sheet, ..
        } = &mut verb
        else {
            unreachable!()
        };
        *anchor = None;
        *new_sheet = true;
        assert!(validate_verb(&verb).is_ok());
    }

    #[test]
    fn validate_verb_rejects_update_chart_with_nothing_to_change() {
        let verb = EmbeddedObjectVerb::UpdateChart {
            chart_id: 1,
            chart_type: None,
            domain: None,
            series: Vec::new(),
            sheet: None,
            title: None,
            subtitle: None,
            legend: None,
            stacked: None,
            header_count: None,
            horizontal_axis_title: None,
            vertical_axis_title: None,
            pie_hole: None,
        };
        let err = validate_verb(&verb).unwrap_err();
        assert!(err.contains("nothing to change"), "{err}");
    }

    #[test]
    fn validate_verb_rejects_update_chart_domain_without_series() {
        let verb = EmbeddedObjectVerb::UpdateChart {
            chart_id: 1,
            chart_type: None,
            domain: Some("A1:A10".to_string()),
            series: Vec::new(),
            sheet: None,
            title: None,
            subtitle: None,
            legend: None,
            stacked: None,
            header_count: None,
            horizontal_axis_title: None,
            vertical_axis_title: None,
            pie_hole: None,
        };
        let err = validate_verb(&verb).unwrap_err();
        assert!(err.contains("--domain requires --series"), "{err}");
    }

    #[test]
    fn validate_verb_rejects_update_slicer_with_nothing_to_change() {
        let verb = EmbeddedObjectVerb::UpdateSlicer {
            slicer_id: 1,
            sheet: None,
            range: None,
            column: None,
            hide_values: Vec::new(),
            clear_criteria: false,
            title: None,
            apply_to_pivot_tables: None,
        };
        let err = validate_verb(&verb).unwrap_err();
        assert!(err.contains("nothing to change"), "{err}");
    }

    #[test]
    fn validate_verb_accepts_update_slicer_with_only_clear_criteria() {
        let verb = EmbeddedObjectVerb::UpdateSlicer {
            slicer_id: 1,
            sheet: None,
            range: None,
            column: None,
            hide_values: Vec::new(),
            clear_criteria: true,
            title: None,
            apply_to_pivot_tables: None,
        };
        assert!(validate_verb(&verb).is_ok());
    }

    // ── existing_chart_kind ──────────────────────────────────────────────

    #[test]
    fn existing_chart_kind_accepts_a_supported_basic_type() {
        let spec = ChartSpec {
            basic_chart: Some(BasicChartSpec {
                chart_type: "COLUMN".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(matches!(
            existing_chart_kind(&spec),
            Ok(ExistingChartKind::Basic)
        ));
    }

    #[test]
    fn existing_chart_kind_accepts_pie() {
        let spec = ChartSpec {
            pie_chart: Some(PieChartSpec::default()),
            ..Default::default()
        };
        assert!(matches!(
            existing_chart_kind(&spec),
            Ok(ExistingChartKind::Pie)
        ));
    }

    #[test]
    fn existing_chart_kind_refuses_an_unsupported_basic_type() {
        let spec = ChartSpec {
            basic_chart: Some(BasicChartSpec {
                chart_type: "COMBO".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let err = existing_chart_kind(&spec).unwrap_err();
        assert_eq!(err, "COMBO");
    }

    #[test]
    fn existing_chart_kind_refuses_neither_basic_nor_pie() {
        let mut extra = BTreeMap::new();
        extra.insert("histogramChart".to_string(), serde_json::json!({}));
        let spec = ChartSpec {
            extra,
            ..Default::default()
        };
        let err = existing_chart_kind(&spec).unwrap_err();
        assert_eq!(err, "histogramChart");
    }

    // ── merge_chart_spec ─────────────────────────────────────────────────

    fn workbook_with_sheet(sheet: Sheet) -> Spreadsheet {
        Spreadsheet {
            sheets: vec![sheet],
            ..Default::default()
        }
    }

    fn basic_chart_sheet(chart_id: i64, chart_type: &str) -> Sheet {
        let mut extra = BTreeMap::new();
        extra.insert("maximized".to_string(), serde_json::json!(true));
        Sheet {
            properties: Some(crate::drive::sheets::types::SheetProperties {
                sheet_id: Some(0),
                title: "Q1".to_string(),
                ..Default::default()
            }),
            charts: vec![EmbeddedChart {
                chart_id: Some(chart_id),
                spec: Some(ChartSpec {
                    title: Some("Old title".to_string()),
                    basic_chart: Some(BasicChartSpec {
                        chart_type: chart_type.to_string(),
                        domains: vec![BasicChartDomain {
                            domain: chart_data(GridRange {
                                sheet_id: 0,
                                start_row_index: Some(0),
                                end_row_index: Some(10),
                                start_column_index: Some(0),
                                end_column_index: Some(1),
                            }),
                            extra: BTreeMap::new(),
                        }],
                        ..Default::default()
                    }),
                    extra,
                    ..Default::default()
                }),
                position: Some(EmbeddedObjectPosition {
                    overlay_position: Some(OverlayPosition {
                        anchor_cell: GridCoordinate {
                            sheet_id: 0,
                            row_index: 1,
                            column_index: 4,
                        },
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
            }],
            ..Default::default()
        }
    }

    fn update_chart_verb(chart_id: i64) -> EmbeddedObjectVerb {
        EmbeddedObjectVerb::UpdateChart {
            chart_id,
            chart_type: None,
            domain: None,
            series: Vec::new(),
            sheet: None,
            title: Some("New title".to_string()),
            subtitle: None,
            legend: None,
            stacked: None,
            header_count: None,
            horizontal_axis_title: None,
            vertical_axis_title: None,
            pie_hole: None,
        }
    }

    #[test]
    fn merge_chart_spec_preserves_unmodelled_extra_fields() {
        let sheet = basic_chart_sheet(1, "COLUMN");
        let workbook = workbook_with_sheet(sheet);
        let existing = workbook.sheets[0].charts[0].spec.as_ref().unwrap();
        let verb = update_chart_verb(1);
        let (spec, summary) = merge_chart_spec(&workbook, existing, &verb).unwrap();
        assert_eq!(spec.extra.get("maximized"), Some(&serde_json::json!(true)));
        assert_eq!(spec.title.as_deref(), Some("New title"));
        assert!(summary.contains("title"));
    }

    #[test]
    fn merge_chart_spec_refuses_an_unsupported_existing_kind() {
        let sheet = basic_chart_sheet(1, "COMBO");
        let workbook = workbook_with_sheet(sheet);
        let existing = workbook.sheets[0].charts[0].spec.as_ref().unwrap();
        let verb = update_chart_verb(1);
        let err = merge_chart_spec(&workbook, existing, &verb).unwrap_err();
        assert!(matches!(
            err,
            EmbeddedObjectResult::RefusedUnsupportedChart { chart_id: 1, .. }
        ));
    }

    #[test]
    fn merge_chart_spec_refuses_a_basic_to_pie_switch() {
        let sheet = basic_chart_sheet(1, "COLUMN");
        let workbook = workbook_with_sheet(sheet);
        let existing = workbook.sheets[0].charts[0].spec.as_ref().unwrap();
        let mut verb = update_chart_verb(1);
        let EmbeddedObjectVerb::UpdateChart { chart_type, .. } = &mut verb else {
            unreachable!()
        };
        *chart_type = Some("pie".to_string());
        let err = merge_chart_spec(&workbook, existing, &verb).unwrap_err();
        assert!(matches!(
            err,
            EmbeddedObjectResult::RefusedUnsupportedChart { chart_id: 1, .. }
        ));
    }

    #[test]
    fn merge_chart_spec_allows_changing_type_within_the_basic_family() {
        let sheet = basic_chart_sheet(1, "COLUMN");
        let workbook = workbook_with_sheet(sheet);
        let existing = workbook.sheets[0].charts[0].spec.as_ref().unwrap();
        let mut verb = update_chart_verb(1);
        let EmbeddedObjectVerb::UpdateChart { chart_type, .. } = &mut verb else {
            unreachable!()
        };
        *chart_type = Some("bar".to_string());
        let (spec, _) = merge_chart_spec(&workbook, existing, &verb).unwrap();
        assert_eq!(spec.basic_chart.unwrap().chart_type, "BAR");
    }

    #[test]
    fn merge_chart_spec_upserts_an_axis_title_on_a_basic_chart() {
        let sheet = basic_chart_sheet(1, "COLUMN");
        let workbook = workbook_with_sheet(sheet);
        let existing = workbook.sheets[0].charts[0].spec.as_ref().unwrap();
        let mut verb = update_chart_verb(1);
        let EmbeddedObjectVerb::UpdateChart {
            horizontal_axis_title,
            ..
        } = &mut verb
        else {
            unreachable!()
        };
        *horizontal_axis_title = Some("Quarter".to_string());
        let (spec, summary) = merge_chart_spec(&workbook, existing, &verb).unwrap();
        let axis = spec.basic_chart.unwrap().axis;
        assert_eq!(axis.len(), 1);
        assert_eq!(axis[0].position, "BOTTOM_AXIS");
        assert_eq!(axis[0].title.as_deref(), Some("Quarter"));
        assert!(summary.contains("horizontal-axis-title"), "{summary}");
    }

    fn pie_chart_sheet(chart_id: i64) -> Sheet {
        Sheet {
            properties: Some(crate::drive::sheets::types::SheetProperties {
                sheet_id: Some(0),
                title: "Q1".to_string(),
                ..Default::default()
            }),
            charts: vec![EmbeddedChart {
                chart_id: Some(chart_id),
                spec: Some(ChartSpec {
                    pie_chart: Some(PieChartSpec {
                        domain: chart_data(GridRange {
                            sheet_id: 0,
                            start_row_index: Some(0),
                            end_row_index: Some(10),
                            start_column_index: Some(0),
                            end_column_index: Some(1),
                        }),
                        series: chart_data(GridRange {
                            sheet_id: 0,
                            start_row_index: Some(0),
                            end_row_index: Some(10),
                            start_column_index: Some(1),
                            end_column_index: Some(2),
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                position: None,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn merge_chart_spec_refuses_a_basic_only_flag_on_an_existing_pie_chart() {
        let workbook = workbook_with_sheet(pie_chart_sheet(1));
        let existing = workbook.sheets[0].charts[0].spec.as_ref().unwrap();
        let mut verb = update_chart_verb(1);
        let EmbeddedObjectVerb::UpdateChart { header_count, .. } = &mut verb else {
            unreachable!()
        };
        *header_count = Some(1);
        let err = merge_chart_spec(&workbook, existing, &verb).unwrap_err();
        assert!(matches!(
            err,
            EmbeddedObjectResult::RefusedInvalidRange { .. }
        ));
    }

    // ── summarise_chart / summarise_slicer / describe_position ──────────

    #[test]
    fn summarise_chart_reports_overlay_position() {
        let sheet = basic_chart_sheet(7, "LINE");
        let summary = summarise_chart(&sheet, &sheet.charts[0]).unwrap();
        assert_eq!(summary.object_id, 7);
        assert_eq!(summary.kind, "chart");
        assert_eq!(summary.chart_type.as_deref(), Some("LINE"));
        assert_eq!(summary.title.as_deref(), Some("Old title"));
        assert_eq!(summary.position, "Q1!E2");
    }

    #[test]
    fn summarise_chart_reports_own_sheet_for_a_new_sheet_chart() {
        let sheet = Sheet {
            properties: Some(crate::drive::sheets::types::SheetProperties {
                sheet_id: Some(9),
                title: "Charts".to_string(),
                ..Default::default()
            }),
            charts: vec![EmbeddedChart {
                chart_id: Some(3),
                spec: Some(ChartSpec {
                    pie_chart: Some(PieChartSpec::default()),
                    ..Default::default()
                }),
                position: Some(EmbeddedObjectPosition {
                    sheet_id: Some(9),
                    new_sheet: Some(true),
                    ..Default::default()
                }),
            }],
            ..Default::default()
        };
        let summary = summarise_chart(&sheet, &sheet.charts[0]).unwrap();
        assert_eq!(summary.chart_type.as_deref(), Some("PIE"));
        assert_eq!(summary.position, "own sheet (id 9)");
    }

    #[test]
    fn summarise_slicer_reports_title_and_position() {
        let sheet = Sheet {
            properties: Some(crate::drive::sheets::types::SheetProperties {
                sheet_id: Some(0),
                title: "Q1".to_string(),
                ..Default::default()
            }),
            slicers: vec![Slicer {
                slicer_id: Some(4),
                spec: Some(SlicerSpec {
                    title: Some("Region".to_string()),
                    ..Default::default()
                }),
                position: Some(EmbeddedObjectPosition {
                    overlay_position: Some(OverlayPosition {
                        anchor_cell: GridCoordinate {
                            sheet_id: 0,
                            row_index: 0,
                            column_index: 5,
                        },
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
            }],
            ..Default::default()
        };
        let summary = summarise_slicer(&sheet, &sheet.slicers[0]).unwrap();
        assert_eq!(summary.object_id, 4);
        assert_eq!(summary.kind, "slicer");
        assert_eq!(summary.chart_type, None);
        assert_eq!(summary.title.as_deref(), Some("Region"));
        assert_eq!(summary.position, "Q1!F1");
    }

    // ── write_jsonl / log_status ─────────────────────────────────────────

    #[test]
    fn object_preview_names_kind_title_and_position() {
        let object = EmbeddedObjectSummary {
            object_id: 5,
            kind: "chart".to_string(),
            chart_type: Some("PIE".to_string()),
            title: Some("Sales".to_string()),
            position: "Q1!C2".to_string(),
        };
        assert_eq!(
            object_preview(&object),
            "id 5: chart (PIE) 'Sales', anchored Q1!C2"
        );
    }

    #[test]
    fn write_jsonl_emits_one_line_of_json() {
        let outcome = EmbeddedObjectOutcome {
            spreadsheet_id: "sheet-1".to_string(),
            file_name: Some("Budget".to_string()),
            resolved_folder_id: None,
            sheet_id: None,
            verb: EmbeddedObjectVerb::DeleteChart { chart_id: 3 },
            result: EmbeddedObjectResult::Changed {
                summary: "delete chart 3".to_string(),
                object_id: Some(3),
                object: None,
            },
        };
        let mut buf = Vec::new();
        outcome.write_jsonl(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert_eq!(text.matches('\n').count(), 1);
        let parsed: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(parsed["result"]["status"], "changed");
    }

    // ── end-to-end via wiremock ──────────────────────────────────────────

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

    fn mount_workbook(sheets: serde_json::Value) -> wiremock::Mock {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/sheet-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "spreadsheetId": "sheet-1",
                    "properties": {"title": "Budget"},
                    "sheets": sheets,
                })),
            )
    }

    fn leased_opts_for(spreadsheet_id: &str) -> (Option<String>, std::path::PathBuf) {
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        let token = seed_lease(&ledger_path, spreadsheet_id, "1");
        (Some(token), ledger_path)
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
        let opts = EmbeddedObjectOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: add_chart_verb(),
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = embedded_object(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            EmbeddedObjectResult::Blocked { .. }
        ));
    }

    #[tokio::test]
    async fn add_chart_sends_an_add_chart_request_and_reports_the_server_assigned_id() {
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
        mount_workbook(serde_json::json!([
            {"properties": {"sheetId": 0, "title": "Sheet1", "index": 0}},
        ]))
        .mount(&server)
        .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "replies": [{"addChart": {"chart": {"chartId": 42}}}]
                })),
            )
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
        let opts = EmbeddedObjectOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: add_chart_verb(),
            dry_run: false,
            lease_token,
            ledger_path,
        };
        let outcome = embedded_object(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(
                outcome.result,
                EmbeddedObjectResult::Changed {
                    object_id: Some(42),
                    ..
                }
            ),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn add_chart_pie_refuses_more_than_one_series_before_any_batch_update_call() {
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
        mount_workbook(serde_json::json!([
            {"properties": {"sheetId": 0, "title": "Sheet1", "index": 0}},
        ]))
        .mount(&server)
        .await;
        let rules = vec![allow_rule("folder-1")];
        let mut verb = add_chart_verb();
        let EmbeddedObjectVerb::AddChart {
            chart_type, series, ..
        } = &mut verb
        else {
            unreachable!()
        };
        *chart_type = "pie".to_string();
        series.push("C1:C10".to_string());
        let opts = EmbeddedObjectOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb,
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = embedded_object(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(
                outcome.result,
                EmbeddedObjectResult::RefusedInvalidRange { .. }
            ),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn update_chart_refuses_an_unsupported_existing_chart_with_no_batch_update_call() {
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
        mount_workbook(serde_json::json!([
            {
                "properties": {"sheetId": 0, "title": "Q1", "index": 0},
                "charts": [{
                    "chartId": 1,
                    "spec": {"basicChart": {"chartType": "COMBO"}},
                }],
            },
        ]))
        .mount(&server)
        .await;
        let rules = vec![allow_rule("folder-1")];
        let opts = EmbeddedObjectOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: update_chart_verb(1),
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = embedded_object(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(
                outcome.result,
                EmbeddedObjectResult::RefusedUnsupportedChart { chart_id: 1, .. }
            ),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn update_chart_refuses_an_unknown_id() {
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
        mount_workbook(serde_json::json!([
            {"properties": {"sheetId": 0, "title": "Q1", "index": 0}},
        ]))
        .mount(&server)
        .await;
        let rules = vec![allow_rule("folder-1")];
        let opts = EmbeddedObjectOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: update_chart_verb(99),
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = embedded_object(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            EmbeddedObjectResult::RefusedObjectNotFound { object_id: 99 }
        ));
    }

    #[tokio::test]
    async fn delete_chart_reports_a_preview_under_dry_run_with_no_batch_update_call() {
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
        mount_workbook(serde_json::json!([
            {
                "properties": {"sheetId": 0, "title": "Q1", "index": 0},
                "charts": [{
                    "chartId": 5,
                    "spec": {"title": "Sales", "pieChart": {
                        "domain": {"sourceRange": {"sources": [{"sheetId": 0}]}},
                        "series": {"sourceRange": {"sources": [{"sheetId": 0}]}},
                    }},
                    "position": {"overlayPosition": {"anchorCell": {"sheetId": 0, "rowIndex": 1, "columnIndex": 2}}},
                }],
            },
        ]))
        .mount(&server)
        .await;
        let rules = vec![allow_rule("folder-1")];
        let opts = EmbeddedObjectOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: EmbeddedObjectVerb::DeleteChart { chart_id: 5 },
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = embedded_object(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            EmbeddedObjectResult::WouldChange { object, .. } => {
                let object = object.expect("delete-chart previews the object");
                assert_eq!(object.object_id, 5);
                assert_eq!(object.title.as_deref(), Some("Sales"));
                assert_eq!(object.position, "Q1!C2");
            }
            other => panic!("expected WouldChange, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn add_slicer_sends_an_add_slicer_request_and_reports_the_server_assigned_id() {
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
        mount_workbook(serde_json::json!([
            {"properties": {"sheetId": 0, "title": "Q1", "index": 0}},
        ]))
        .mount(&server)
        .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1:batchUpdate",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "replies": [{"addSlicer": {"slicer": {"slicerId": 8}}}]
                })),
            )
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
        let opts = EmbeddedObjectOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: EmbeddedObjectVerb::AddSlicer {
                sheet: Some("Q1".to_string()),
                range: "A1:D10".to_string(),
                column: 1,
                hide_values: vec!["Closed".to_string()],
                title: Some("Status".to_string()),
                apply_to_pivot_tables: None,
                anchor: "F2".to_string(),
                offset_x: None,
                offset_y: None,
                width: None,
                height: None,
            },
            dry_run: false,
            lease_token,
            ledger_path,
        };
        let outcome = embedded_object(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(
                outcome.result,
                EmbeddedObjectResult::Changed {
                    object_id: Some(8),
                    ..
                }
            ),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn update_slicer_names_only_the_changed_fields_in_the_mask() {
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
        mount_workbook(serde_json::json!([
            {
                "properties": {"sheetId": 0, "title": "Q1", "index": 0},
                "slicers": [{
                    "slicerId": 4,
                    "spec": {"title": "Region"},
                    "position": {"overlayPosition": {"anchorCell": {"sheetId": 0, "rowIndex": 0, "columnIndex": 5}}},
                }],
            },
        ]))
        .mount(&server)
        .await;
        let rules = vec![allow_rule("folder-1")];
        let opts = EmbeddedObjectOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: EmbeddedObjectVerb::UpdateSlicer {
                slicer_id: 4,
                sheet: None,
                range: None,
                column: None,
                hide_values: Vec::new(),
                clear_criteria: false,
                title: Some("Territory".to_string()),
                apply_to_pivot_tables: None,
            },
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = embedded_object(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            EmbeddedObjectResult::WouldChange { summary, .. } => {
                assert!(summary.contains("title"), "{summary}");
            }
            other => panic!("expected WouldChange, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn delete_slicer_reports_a_preview_under_dry_run() {
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
        mount_workbook(serde_json::json!([
            {
                "properties": {"sheetId": 0, "title": "Q1", "index": 0},
                "slicers": [{
                    "slicerId": 9,
                    "spec": {"title": "Region"},
                    "position": {"overlayPosition": {"anchorCell": {"sheetId": 0, "rowIndex": 0, "columnIndex": 5}}},
                }],
            },
        ]))
        .mount(&server)
        .await;
        let rules = vec![allow_rule("folder-1")];
        let opts = EmbeddedObjectOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: EmbeddedObjectVerb::DeleteSlicer { slicer_id: 9 },
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = embedded_object(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            EmbeddedObjectResult::WouldChange { object, .. } => {
                let object = object.expect("delete-slicer previews the object");
                assert_eq!(object.object_id, 9);
                assert_eq!(object.kind, "slicer");
                assert_eq!(object.title.as_deref(), Some("Region"));
            }
            other => panic!("expected WouldChange, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dry_run_makes_no_batch_update_call() {
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
        mount_workbook(serde_json::json!([
            {"properties": {"sheetId": 0, "title": "Sheet1", "index": 0}},
        ]))
        .mount(&server)
        .await;
        // No batchUpdate mock is registered at all — a call would 404 and
        // surface as `Failed`, not `WouldChange`.
        let rules = vec![allow_rule("folder-1")];
        let opts = EmbeddedObjectOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: add_chart_verb(),
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = embedded_object(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(outcome.result, EmbeddedObjectResult::WouldChange { .. }),
            "{:?}",
            outcome.result
        );
    }

    #[test]
    fn charts_from_workbook_and_slicers_from_workbook_report_every_object() {
        let mut with_slicer = basic_chart_sheet(1, "COLUMN");
        with_slicer.slicers.push(Slicer {
            slicer_id: Some(2),
            spec: Some(SlicerSpec::default()),
            position: Some(EmbeddedObjectPosition {
                overlay_position: Some(OverlayPosition {
                    anchor_cell: GridCoordinate {
                        sheet_id: 0,
                        row_index: 0,
                        column_index: 0,
                    },
                    ..Default::default()
                }),
                ..Default::default()
            }),
        });
        let workbook = workbook_with_sheet(with_slicer);
        assert_eq!(charts_from_workbook(&workbook).len(), 1);
        assert_eq!(slicers_from_workbook(&workbook).len(), 1);
    }
}
