//! Conditional formatting via `spreadsheets.batchUpdate` (issue #1793,
//! [ADR-0081](../../../docs/adrs/adr-0081.md) §1).
//!
//! Three mutating verbs, all gated by [`DriveOperation::SheetsStructure`] —
//! presentational, destroys no data, exactly like `format-cells`/
//! `set-data-validation`. `list-conditional-formats` is a plain read,
//! ungated like `list-protections` — its own module lives entirely in
//! `crate::cli::drive::sheets::conditional_format`, this module has no
//! "list" verb of its own. Shape mirrors `format.rs`/`validation.rs`/
//! `protection.rs`: compose a target, resolve it against a freshly-fetched
//! workbook, gate, dry-run, mutate, log.
//!
//! **Index-addressed, not id-addressed.** Unlike a [`crate::drive::sheets::types::ProtectedRange`],
//! a conditional format rule has no stable id — it is one entry of a
//! [`crate::drive::sheets::types::Sheet`]'s ordered `conditional_formats`
//! list, and `update-conditional-format`/`delete-conditional-format`'s
//! `--index` names an ordinal position that shifts when an earlier rule is
//! deleted. `list-conditional-formats` exists to make that position
//! discoverable immediately before acting on it (the same reason
//! `list-protections` exists), and `--dry-run` on `update`/`delete` echoes
//! the rule *currently* at the given index — both states, "currently" and
//! "to" — so a stale index is visible before it's acted on.
//!
//! **A curated condition/rule surface, not full `BooleanRule`/`GradientRule`
//! coverage**, mirroring `validation.rs`'s own stance. [`FormatCondition`]
//! is a separate, smaller enum from `validation::Condition` — duplicated
//! rather than shared, since the two surfaces support an overlapping but
//! different set of condition types: this one drops `ONE_OF_LIST`/
//! `ONE_OF_RANGE`/`CHECKBOX` (dropdown-only, meaningless for a format
//! trigger) and adds `CELL_EMPTY`/`CELL_NOT_EMPTY` (meaningful only as a
//! format trigger). [`GradientRule`](crate::drive::sheets::types::GradientRule)'s
//! two endpoints are always anchored `MIN`/`MAX`; Sheets also allows an
//! endpoint anchored at an explicit `NUMBER`/`PERCENT`/`PERCENTILE` value,
//! which this crate doesn't build — `docs/drive.md` names both gaps.
//! `update-conditional-format` only replaces the rule at an index; Sheets
//! also supports moving a rule from one index to another without changing
//! its content, which this crate doesn't build either.

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
use crate::drive::sheets::date_value::{
    reject_blank, reject_blank_date, reject_empty, reject_invalid_date_between, reject_nan,
    reject_reversed_range, DateValue,
};
use crate::drive::sheets::format::parse_hex_color;
use crate::drive::sheets::grid_range;
use crate::drive::sheets::target_gate;
use crate::drive::sheets::types::{
    AddConditionalFormatRuleRequest, BatchUpdateRequestItem, BooleanCondition, BooleanRule,
    CellFormat, ColorStyle, ConditionValue, ConditionalFormatRule,
    DeleteConditionalFormatRuleRequest, GradientRule, GridRange, InterpolationPoint, Sheet,
    Spreadsheet, TextFormat, UpdateConditionalFormatRuleRequest,
};
use crate::drive::types::SheetTargetRefusal;
use crate::drive::write_gate::{self, DecidingRule, DriveOperation, FolderPermissionRule};
use crate::request_log::{self, DriveMutationOutcome};

/// The condition shapes a `BooleanRule` can trigger on.
///
/// See the module doc's "curated condition/rule surface" paragraph for how
/// this differs from `validation::Condition`.
#[derive(Debug, Clone, PartialEq)]
pub enum FormatCondition {
    /// A number in `[min, max]` inclusive.
    NumberBetween(f64, f64),
    /// A number outside `[min, max]`.
    NumberNotBetween(f64, f64),
    /// A number strictly greater than this.
    NumberGreater(f64),
    /// A number greater than or equal to this.
    NumberGreaterEq(f64),
    /// A number strictly less than this.
    NumberLess(f64),
    /// A number less than or equal to this.
    NumberLessEq(f64),
    /// A number equal to this.
    NumberEq(f64),
    /// A number not equal to this.
    NumberNotEq(f64),
    /// Text containing this substring.
    TextContains(String),
    /// Text not containing this substring.
    TextNotContains(String),
    /// Text starting with this substring.
    TextStartsWith(String),
    /// Text ending with this substring.
    TextEndsWith(String),
    /// Text equal to this.
    TextEq(String),
    /// A date after this one.
    DateAfter(DateValue),
    /// A date before this one.
    DateBefore(DateValue),
    /// A date equal to this one.
    DateOn(DateValue),
    /// A date in `[start, end]` inclusive (absolute dates only).
    DateBetween(String, String),
    /// The cell is empty.
    CellEmpty,
    /// The cell is not empty.
    CellNotEmpty,
    /// A custom formula that must evaluate truthy.
    CustomFormula(String),
}

impl FormatCondition {
    /// The Sheets API `condition_type` string.
    const fn condition_type_str(&self) -> &'static str {
        match self {
            Self::NumberBetween(..) => "NUMBER_BETWEEN",
            Self::NumberNotBetween(..) => "NUMBER_NOT_BETWEEN",
            Self::NumberGreater(_) => "NUMBER_GREATER",
            Self::NumberGreaterEq(_) => "NUMBER_GREATER_THAN_EQ",
            Self::NumberLess(_) => "NUMBER_LESS",
            Self::NumberLessEq(_) => "NUMBER_LESS_THAN_EQ",
            Self::NumberEq(_) => "NUMBER_EQ",
            Self::NumberNotEq(_) => "NUMBER_NOT_EQ",
            Self::TextContains(_) => "TEXT_CONTAINS",
            Self::TextNotContains(_) => "TEXT_NOT_CONTAINS",
            Self::TextStartsWith(_) => "TEXT_STARTS_WITH",
            Self::TextEndsWith(_) => "TEXT_ENDS_WITH",
            Self::TextEq(_) => "TEXT_EQ",
            Self::DateAfter(_) => "DATE_AFTER",
            Self::DateBefore(_) => "DATE_BEFORE",
            Self::DateOn(_) => "DATE_EQ",
            Self::DateBetween(..) => "DATE_BETWEEN",
            Self::CellEmpty => "CELL_EMPTY",
            Self::CellNotEmpty => "CELL_NOT_EMPTY",
            Self::CustomFormula(_) => "CUSTOM_FORMULA",
        }
    }

    fn into_boolean_condition(self) -> BooleanCondition {
        let value = |s: String| ConditionValue {
            user_entered_value: Some(s),
            relative_date: None,
        };
        let condition_type = self.condition_type_str().to_string();
        let values = match self {
            Self::NumberBetween(min, max) | Self::NumberNotBetween(min, max) => {
                vec![value(min.to_string()), value(max.to_string())]
            }
            Self::NumberGreater(n)
            | Self::NumberGreaterEq(n)
            | Self::NumberLess(n)
            | Self::NumberLessEq(n)
            | Self::NumberEq(n)
            | Self::NumberNotEq(n) => vec![value(n.to_string())],
            Self::TextContains(text)
            | Self::TextNotContains(text)
            | Self::TextStartsWith(text)
            | Self::TextEndsWith(text)
            | Self::TextEq(text) => vec![value(text)],
            Self::DateAfter(date) | Self::DateBefore(date) | Self::DateOn(date) => {
                vec![date.into_condition_value()]
            }
            Self::DateBetween(start, end) => vec![value(start), value(end)],
            Self::CellEmpty | Self::CellNotEmpty => Vec::new(),
            Self::CustomFormula(formula) => vec![value(formula)],
        };
        BooleanCondition {
            condition_type,
            values,
        }
    }
}

/// The mutable subset of a cell's format a `BooleanRule` can apply.
///
/// Deliberately smaller than `format-cells`' full `CellFormatFlags`: only
/// the three properties most conditional-format rules actually set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FormatEffect {
    /// Cell background color, `#RRGGBB`.
    pub background: Option<String>,
    /// Text color, `#RRGGBB`.
    pub text_color: Option<String>,
    /// Bold text.
    pub bold: Option<bool>,
}

impl FormatEffect {
    fn into_cell_format(self) -> Result<CellFormat, String> {
        let mut format = CellFormat::default();
        if let Some(hex) = self.background {
            format.background_color_style = Some(ColorStyle {
                rgb_color: parse_hex_color(&hex).map_err(|e| format!("--background: {e}"))?,
            });
        }
        if self.text_color.is_some() || self.bold.is_some() {
            let mut text_format = TextFormat::default();
            if let Some(hex) = self.text_color {
                text_format.foreground_color_style = Some(ColorStyle {
                    rgb_color: parse_hex_color(&hex).map_err(|e| format!("--text-color: {e}"))?,
                });
            }
            text_format.bold = self.bold;
            format.text_format = Some(text_format);
        }
        Ok(format)
    }
}

/// One of Sheets' `GradientRule` midpoint anchor types — `MIN`/`MAX` are
/// reserved for the two fixed endpoints and never appear here (see the
/// module doc's gradient cut).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GradientPointType {
    /// Anchored at an absolute cell value.
    Number,
    /// Anchored at a percentage of the range's value spread.
    Percent,
    /// Anchored at a percentile of the range's values.
    Percentile,
}

impl GradientPointType {
    const fn as_sheets_str(self) -> &'static str {
        match self {
            Self::Number => "NUMBER",
            Self::Percent => "PERCENT",
            Self::Percentile => "PERCENTILE",
        }
    }
}

/// A `GradientRule`'s optional midpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GradientMidpoint {
    /// The midpoint's color, `#RRGGBB`.
    pub color: String,
    /// What kind of value the midpoint is anchored to.
    pub point_type: GradientPointType,
    /// The threshold value, untouched — Sheets parses it at evaluation
    /// time.
    pub value: String,
}

/// A `GradientRule` this crate can build — see the module doc's gradient
/// cut for what's deliberately excluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GradientSpec {
    /// The color at the low end of the scale (anchored `MIN`).
    pub min_color: String,
    /// The color at the high end of the scale (anchored `MAX`).
    pub max_color: String,
    /// An optional midpoint.
    pub mid: Option<GradientMidpoint>,
}

impl GradientSpec {
    fn into_gradient_rule(self) -> Result<GradientRule, String> {
        let min =
            parse_hex_color(&self.min_color).map_err(|e| format!("--gradient-min-color: {e}"))?;
        let max =
            parse_hex_color(&self.max_color).map_err(|e| format!("--gradient-max-color: {e}"))?;
        let midpoint = match self.mid {
            Some(mid) => Some(InterpolationPoint {
                color_style: ColorStyle {
                    rgb_color: parse_hex_color(&mid.color)
                        .map_err(|e| format!("--gradient-mid-color: {e}"))?,
                },
                point_type: mid.point_type.as_sheets_str().to_string(),
                value: mid.value,
            }),
            None => None,
        };
        Ok(GradientRule {
            min_color_style: ColorStyle { rgb_color: min },
            midpoint,
            max_color_style: ColorStyle { rgb_color: max },
        })
    }
}

/// The rule content `add-conditional-format`/`update-conditional-format`
/// build — exactly one of a condition-triggered format or a color-scale
/// gradient, matching `ConditionalFormatRule`'s own union.
#[derive(Debug, Clone, PartialEq)]
pub enum FormatRule {
    /// A `BooleanRule`: `format` applies to every cell for which
    /// `condition` evaluates true.
    Boolean {
        /// What triggers the format.
        condition: FormatCondition,
        /// The format to apply.
        format: FormatEffect,
    },
    /// A `GradientRule`: a color scale across the range's values.
    Gradient(GradientSpec),
}

impl FormatRule {
    fn into_conditional_format_rule(
        self,
        ranges: Vec<GridRange>,
    ) -> Result<ConditionalFormatRule, String> {
        match self {
            Self::Boolean { condition, format } => Ok(ConditionalFormatRule {
                ranges,
                boolean_rule: Some(BooleanRule {
                    condition: condition.into_boolean_condition(),
                    format: format.into_cell_format()?,
                }),
                gradient_rule: None,
            }),
            Self::Gradient(spec) => Ok(ConditionalFormatRule {
                ranges,
                boolean_rule: None,
                gradient_rule: Some(spec.into_gradient_rule()?),
            }),
        }
    }
}

/// Which mutation to perform.
#[derive(Debug, Clone, PartialEq)]
pub enum ConditionalFormatVerb {
    /// Add a new rule.
    AddConditionalFormat {
        /// The sheet every range belongs to.
        sheet: String,
        /// The range(s) the rule applies to, at least one.
        ranges: Vec<String>,
        /// Where to insert the rule. `None` appends (the sheet's current
        /// `conditional_formats.len()`).
        index: Option<i64>,
        /// The rule to add.
        rule: FormatRule,
    },
    /// Replace the rule at an index.
    UpdateConditionalFormat {
        /// The sheet every range belongs to.
        sheet: String,
        /// The rule's new range(s), at least one.
        ranges: Vec<String>,
        /// Which rule, by ordinal position, to replace.
        index: usize,
        /// The rule's new content.
        rule: FormatRule,
    },
    /// Remove the rule at an index.
    DeleteConditionalFormat {
        /// The sheet the rule belongs to.
        sheet: String,
        /// Which rule, by ordinal position, to remove.
        index: usize,
    },
}

impl ConditionalFormatVerb {
    const fn log_operation(&self) -> &'static str {
        match self {
            Self::AddConditionalFormat { .. } => "sheets-add-conditional-format",
            Self::UpdateConditionalFormat { .. } => "sheets-update-conditional-format",
            Self::DeleteConditionalFormat { .. } => "sheets-delete-conditional-format",
        }
    }

    const fn label(&self) -> &'static str {
        match self {
            Self::AddConditionalFormat { .. } => "add-conditional-format",
            Self::UpdateConditionalFormat { .. } => "update-conditional-format",
            Self::DeleteConditionalFormat { .. } => "delete-conditional-format",
        }
    }

    fn sheet(&self) -> &str {
        match self {
            Self::AddConditionalFormat { sheet, .. }
            | Self::UpdateConditionalFormat { sheet, .. }
            | Self::DeleteConditionalFormat { sheet, .. } => sheet,
        }
    }

    fn ranges(&self) -> &[String] {
        match self {
            Self::AddConditionalFormat { ranges, .. }
            | Self::UpdateConditionalFormat { ranges, .. } => ranges,
            Self::DeleteConditionalFormat { .. } => &[],
        }
    }

    fn rule(&self) -> Option<&FormatRule> {
        match self {
            Self::AddConditionalFormat { rule, .. }
            | Self::UpdateConditionalFormat { rule, .. } => Some(rule),
            Self::DeleteConditionalFormat { .. } => None,
        }
    }
}

/// Per-call options.
#[derive(Debug, Clone)]
pub struct ConditionalFormatOptions {
    /// Spreadsheet id.
    pub spreadsheet_id: String,
    /// Which mutation to perform.
    pub verb: ConditionalFormatVerb,
    /// Classify and describe only; never call `batchUpdate`.
    pub dry_run: bool,
    /// The lease token presented via `--lease`. Checked only when the
    /// deciding rule requires one.
    pub lease_token: Option<String>,
    /// Path to the lease ledger the token is checked against.
    pub ledger_path: PathBuf,
}

/// What happened (or, under `--dry-run`, would happen).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum ConditionalFormatResult {
    /// `--dry-run`, and the gate would allow it.
    WouldChange {
        /// A human-readable summary of the effect.
        summary: String,
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
    /// The `--sheet`/`--range` pair, or the rule's own arguments, was
    /// invalid.
    RefusedInvalidRange {
        /// What was wrong and why.
        detail: String,
    },
    /// `--index` does not name a valid position on this sheet:
    /// `update-conditional-format`/`delete-conditional-format`'s `--index`
    /// doesn't name an existing rule (valid range `0..count`), or
    /// `add-conditional-format`'s explicit `--index` is negative or greater
    /// than the sheet's current rule count (valid range `0..=count`).
    RefusedIndexOutOfBounds {
        /// The sheet the index was checked against.
        sheet: String,
        /// The index that was out of bounds. `i64`, not `usize`, since
        /// `add-conditional-format`'s `--index` accepts (and must be able to
        /// report) a negative value.
        index: i64,
        /// How many rules the sheet actually has.
        count: usize,
    },
    /// The folder write-permission gate refused it.
    Blocked {
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
    Changed {
        /// Same summary as [`Self::WouldChange`].
        summary: String,
    },
    /// An API or validation error.
    Failed {
        /// A human-readable summary of what failed.
        detail: String,
    },
}

impl FromLeaseRefusal for ConditionalFormatResult {
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

impl ConditionalFormatResult {
    fn log_status(&self) -> &'static str {
        match self {
            Self::WouldChange { .. } => "would-change",
            Self::RefusedNotASpreadsheet { .. } => "refused-not-a-spreadsheet",
            Self::RefusedShortcut => "refused-shortcut",
            Self::RefusedNoVisibleParents => "refused-no-visible-parents",
            Self::RefusedSheetNotFound { .. } => "refused-sheet-not-found",
            Self::RefusedInvalidRange { .. } => "refused-invalid-range",
            Self::RefusedIndexOutOfBounds { .. } => "refused-index-out-of-bounds",
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
pub struct ConditionalFormatOutcome {
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
    pub verb: ConditionalFormatVerb,
    /// What happened.
    pub result: ConditionalFormatResult,
}

impl JsonlSerialize for ConditionalFormatOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> anyhow::Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Runs one conditional-format mutation, logging every attempt that isn't a
/// dry run.
pub async fn conditional_format(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &ConditionalFormatOptions,
    rules: &[FolderPermissionRule],
) -> ConditionalFormatOutcome {
    let started = Instant::now();
    let outcome = conditional_format_inner(drive, sheets, opts, rules).await;
    if !opts.dry_run {
        record_attempt(&outcome, opts, started.elapsed());
    }
    outcome
}

async fn conditional_format_inner(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &ConditionalFormatOptions,
    rules: &[FolderPermissionRule],
) -> ConditionalFormatOutcome {
    let bare = |result| ConditionalFormatOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: None,
        resolved_folder_id: None,
        verb: opts.verb.clone(),
        result,
    };

    if opts.verb.ranges().is_empty() && opts.verb.rule().is_some() {
        return bare(ConditionalFormatResult::RefusedInvalidRange {
            detail: format!("{} needs at least one --range", opts.verb.label()),
        });
    }

    let composed_ranges: Vec<String> = match opts
        .verb
        .ranges()
        .iter()
        .map(|range| a1::compose(Some(opts.verb.sheet()), Some(range)))
        .collect::<anyhow::Result<Vec<String>>>()
    {
        Ok(composed) => composed,
        Err(err) => {
            return bare(ConditionalFormatResult::RefusedInvalidRange {
                detail: err.to_string(),
            })
        }
    };

    if let Some(rule) = opts.verb.rule() {
        if let Err(detail) = validate_format_rule(rule) {
            return bare(ConditionalFormatResult::RefusedInvalidRange { detail });
        }
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
            return bare(ConditionalFormatResult::Failed { detail })
        }
        target_gate::TargetGateOutcome::Refused { target, refusal } => {
            let result = match refusal {
                SheetTargetRefusal::Shortcut => ConditionalFormatResult::RefusedShortcut,
                SheetTargetRefusal::NotASpreadsheet { mime_type } => {
                    ConditionalFormatResult::RefusedNotASpreadsheet { mime_type }
                }
                SheetTargetRefusal::NoVisibleParents => {
                    ConditionalFormatResult::RefusedNoVisibleParents
                }
            };
            return ConditionalFormatOutcome {
                spreadsheet_id: opts.spreadsheet_id.clone(),
                file_name: Some(target.name),
                resolved_folder_id: None,
                verb: opts.verb.clone(),
                result,
            };
        }
        target_gate::TargetGateOutcome::GateFetchFailed { target, detail } => {
            return ConditionalFormatOutcome {
                spreadsheet_id: opts.spreadsheet_id.clone(),
                file_name: Some(target.name),
                resolved_folder_id: None,
                verb: opts.verb.clone(),
                result: ConditionalFormatResult::Failed { detail },
            };
        }
        target_gate::TargetGateOutcome::Gated {
            target,
            decision,
            resolved_folder_id,
            requires_lease,
        } => (target, decision, resolved_folder_id, requires_lease),
    };

    let gated = |result| ConditionalFormatOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: Some(target.name.clone()),
        resolved_folder_id: resolved_folder_id.clone(),
        verb: opts.verb.clone(),
        result,
    };

    if decision.verdict == write_gate::Verdict::Deny {
        return gated(ConditionalFormatResult::Blocked {
            decided_by: decision.decided_by,
        });
    }

    let api = SheetsApi::new(sheets);
    let workbook = match api
        .get_spreadsheet_with_conditional_formats(&opts.spreadsheet_id)
        .await
    {
        Ok(workbook) => workbook,
        Err(err) => {
            return gated(ConditionalFormatResult::Failed {
                detail: format!("{err:#}"),
            })
        }
    };

    let sheet_id =
        match grid_range::find_sheet_id(&workbook, opts.verb.sheet(), |title, available| {
            ConditionalFormatResult::RefusedSheetNotFound { title, available }
        }) {
            Ok(id) => id,
            Err(result) => return gated(result),
        };

    let mut grids = Vec::new();
    for composed in &composed_ranges {
        match grid_range::resolve_grid_range(
            &workbook,
            composed,
            |detail| ConditionalFormatResult::RefusedInvalidRange { detail },
            |title, available| ConditionalFormatResult::RefusedSheetNotFound { title, available },
        ) {
            Ok((_, grid)) => grids.push(grid),
            Err(result) => return gated(result),
        }
    }

    let sheet = find_sheet_by_id(&workbook, sheet_id);
    let existing_count = sheet.map_or(0, |s| s.conditional_formats.len());

    let (resolved_index, existing_summary) = match &opts.verb {
        ConditionalFormatVerb::AddConditionalFormat { index, .. } => match index {
            Some(explicit) => {
                // Valid positions are `0..=existing_count` inclusive — unlike
                // update/delete, `existing_count` itself is valid here (it
                // appends). A negative or too-large explicit `--index`
                // reaches the same friendly refusal update/delete give for
                // an out-of-bounds index, instead of an opaque API error.
                let in_bounds = usize::try_from(*explicit).is_ok_and(|i| i <= existing_count);
                if !in_bounds {
                    return gated(ConditionalFormatResult::RefusedIndexOutOfBounds {
                        sheet: opts.verb.sheet().to_string(),
                        index: *explicit,
                        count: existing_count,
                    });
                }
                (*explicit, None)
            }
            None => (i64::try_from(existing_count).unwrap_or(i64::MAX), None),
        },
        ConditionalFormatVerb::UpdateConditionalFormat { index, .. }
        | ConditionalFormatVerb::DeleteConditionalFormat { index, .. } => {
            let idx = *index;
            match sheet.and_then(|s| s.conditional_formats.get(idx)) {
                Some(existing) => (
                    i64::try_from(idx).unwrap_or(i64::MAX),
                    Some(describe_existing_rule(existing)),
                ),
                None => {
                    return gated(ConditionalFormatResult::RefusedIndexOutOfBounds {
                        sheet: opts.verb.sheet().to_string(),
                        index: i64::try_from(idx).unwrap_or(i64::MAX),
                        count: existing_count,
                    })
                }
            }
        }
    };

    // Built before the lease gate, not after — the same "gate must be the
    // last fallible step before the mutating call" discipline
    // `validation.rs::build_request`'s doc comment explains (#1688): unlike
    // that one, converting a `FormatRule` to the wire shape IS fallible
    // (hex-color parsing), so it must happen here, before
    // `gate_optional_leased_write`, not after.
    //
    // The description and the wire request are built together, per verb, in
    // one match rather than via a separate `Option<ConditionalFormatRule>`
    // matched a second time below — that shape needed a `(verb, new_rule)`
    // tuple match with an `unreachable!()` arm for the impossible
    // (Add/Update, None) case, relying on a runtime invariant instead of the
    // type system.
    let (summary, request) = match &opts.verb {
        ConditionalFormatVerb::AddConditionalFormat { rule, .. } => {
            let built = match rule.clone().into_conditional_format_rule(grids) {
                Ok(built) => built,
                Err(detail) => {
                    return gated(ConditionalFormatResult::RefusedInvalidRange { detail })
                }
            };
            let summary = format!(
                "add conditional format ({})",
                describe_existing_rule(&built)
            );
            let request =
                BatchUpdateRequestItem::AddConditionalFormatRule(AddConditionalFormatRuleRequest {
                    rule: built,
                    index: resolved_index,
                });
            (summary, request)
        }
        ConditionalFormatVerb::UpdateConditionalFormat { rule, .. } => {
            let built = match rule.clone().into_conditional_format_rule(grids) {
                Ok(built) => built,
                Err(detail) => {
                    return gated(ConditionalFormatResult::RefusedInvalidRange { detail })
                }
            };
            let current = existing_summary.as_deref().unwrap_or("unknown");
            let summary = format!(
                "update conditional format rule at index {resolved_index} (currently: \
                 {current}) to ({})",
                describe_existing_rule(&built)
            );
            let request = BatchUpdateRequestItem::UpdateConditionalFormatRule(
                UpdateConditionalFormatRuleRequest {
                    rule: built,
                    index: resolved_index,
                },
            );
            (summary, request)
        }
        ConditionalFormatVerb::DeleteConditionalFormat { .. } => {
            let current = existing_summary.as_deref().unwrap_or("unknown");
            let summary = format!(
                "delete conditional format rule at index {resolved_index} (currently: \
                 {current})"
            );
            let request = BatchUpdateRequestItem::DeleteConditionalFormatRule(
                DeleteConditionalFormatRuleRequest {
                    sheet_id,
                    index: resolved_index,
                },
            );
            (summary, request)
        }
    };

    if opts.dry_run {
        return gated(ConditionalFormatResult::WouldChange { summary });
    }

    // The lease check sits here: after the permission gate and the
    // `--dry-run` branch, before the mutating call — see
    // `content_edit.rs::edit_inner`'s doc comment for the full reasoning,
    // shared verbatim by every leased engine.
    let files_api = FilesApi::new(drive);
    let leased = LeasedWrite {
        log_prefix: "drive sheets conditional_format",
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
        Ok(_response) => ConditionalFormatResult::Changed { summary },
        Err(err) => ConditionalFormatResult::Failed {
            detail: format!("{err:#}"),
        },
    };
    drop(lease_grant);
    gated(result)
}

fn find_sheet_by_id(workbook: &Spreadsheet, sheet_id: i64) -> Option<&Sheet> {
    workbook
        .sheets
        .iter()
        .find(|s| s.sheet_id() == Some(sheet_id))
}

fn validate_format_rule(rule: &FormatRule) -> Result<(), String> {
    match rule {
        FormatRule::Boolean { condition, format } => {
            validate_condition(condition)?;
            validate_format_effect(format)
        }
        FormatRule::Gradient(spec) => validate_gradient(spec),
    }
}

fn validate_condition(condition: &FormatCondition) -> Result<(), String> {
    // Exhaustive over `FormatCondition`: a new variant forces a new arm
    // here at compile time.
    match condition {
        FormatCondition::NumberBetween(min, max) => {
            reject_reversed_range(*min, *max, "--number-between")
        }
        FormatCondition::NumberNotBetween(min, max) => {
            reject_reversed_range(*min, *max, "--number-not-between")
        }
        FormatCondition::NumberGreater(n) => reject_nan(*n, "--number-greater"),
        FormatCondition::NumberGreaterEq(n) => reject_nan(*n, "--number-greater-eq"),
        FormatCondition::NumberLess(n) => reject_nan(*n, "--number-less"),
        FormatCondition::NumberLessEq(n) => reject_nan(*n, "--number-less-eq"),
        FormatCondition::NumberEq(n) => reject_nan(*n, "--number-eq"),
        FormatCondition::NumberNotEq(n) => reject_nan(*n, "--number-not-eq"),
        FormatCondition::TextContains(text) => reject_empty(text, "--text-contains"),
        FormatCondition::TextNotContains(text) => reject_empty(text, "--text-not-contains"),
        FormatCondition::TextStartsWith(text) => reject_empty(text, "--text-starts-with"),
        FormatCondition::TextEndsWith(text) => reject_empty(text, "--text-ends-with"),
        FormatCondition::TextEq(text) => reject_empty(text, "--text-eq"),
        FormatCondition::DateAfter(date) => reject_blank_date(date, "--date-after"),
        FormatCondition::DateBefore(date) => reject_blank_date(date, "--date-before"),
        FormatCondition::DateOn(date) => reject_blank_date(date, "--date-on"),
        FormatCondition::DateBetween(start, end) => reject_invalid_date_between(start, end),
        FormatCondition::CellEmpty | FormatCondition::CellNotEmpty => Ok(()),
        FormatCondition::CustomFormula(formula) => reject_blank(formula, "--custom-formula"),
    }
}

fn validate_format_effect(effect: &FormatEffect) -> Result<(), String> {
    if let Some(hex) = &effect.background {
        parse_hex_color(hex).map_err(|e| format!("--background: {e}"))?;
    }
    if let Some(hex) = &effect.text_color {
        parse_hex_color(hex).map_err(|e| format!("--text-color: {e}"))?;
    }
    if effect.background.is_none() && effect.text_color.is_none() && effect.bold.is_none() {
        return Err(
            "a boolean conditional format rule needs at least one of --background/\
             --text-color/--bold"
                .to_string(),
        );
    }
    Ok(())
}

fn validate_gradient(spec: &GradientSpec) -> Result<(), String> {
    parse_hex_color(&spec.min_color).map_err(|e| format!("--gradient-min-color: {e}"))?;
    parse_hex_color(&spec.max_color).map_err(|e| format!("--gradient-max-color: {e}"))?;
    if let Some(mid) = &spec.mid {
        parse_hex_color(&mid.color).map_err(|e| format!("--gradient-mid-color: {e}"))?;
        if mid.value.trim().is_empty() {
            return Err("--gradient-mid-value must not be empty".to_string());
        }
    }
    Ok(())
}

/// Describes a [`ConditionalFormatRule`] in its wire shape — the "currently"
/// side of `update`/`delete`'s dry-run echo, the "to"/new-rule side of
/// `add`/`update`'s own summary (built from the same wire shape right after
/// [`FormatRule::into_conditional_format_rule`] converts it), and reused
/// verbatim by `list-conditional-formats`' CLI rendering (issue #1793) — one
/// function for every surface that describes a rule, so none of them can
/// describe the same rule differently.
pub(crate) fn describe_existing_rule(rule: &ConditionalFormatRule) -> String {
    if let Some(boolean_rule) = &rule.boolean_rule {
        let cond = boolean_rule
            .condition
            .condition_type
            .to_ascii_lowercase()
            .replace('_', " ");
        let mut parts = Vec::new();
        if boolean_rule.format.background_color_style.is_some() {
            parts.push("background");
        }
        if let Some(text_format) = &boolean_rule.format.text_format {
            if text_format.foreground_color_style.is_some() {
                parts.push("text color");
            }
            if text_format.bold == Some(true) {
                parts.push("bold");
            }
        }
        let effect = if parts.is_empty() {
            "format".to_string()
        } else {
            parts.join("+")
        };
        format!("boolean rule ({cond} -> {effect})")
    } else if let Some(gradient_rule) = &rule.gradient_rule {
        let mid = gradient_rule
            .midpoint
            .as_ref()
            .map_or_else(String::new, |m| {
                format!(", mid {} at {}", m.point_type.to_ascii_lowercase(), m.value)
            });
        format!("gradient rule (min/max colors{mid})")
    } else {
        "rule (unrecognized type)".to_string()
    }
}

fn record_attempt(
    outcome: &ConditionalFormatOutcome,
    opts: &ConditionalFormatOptions,
    duration: Duration,
) {
    let error = match &outcome.result {
        ConditionalFormatResult::Failed { detail } => Some(detail.clone()),
        _ => None,
    };
    let decided_by = match &outcome.result {
        ConditionalFormatResult::Blocked { decided_by } => decided_by.as_ref(),
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

/// Renders an outcome as human-readable text.
#[must_use]
pub fn describe(outcome: &ConditionalFormatOutcome) -> String {
    describe_lines(outcome).join("\n")
}

/// Renders an outcome as its individual lines, none of which contains a
/// newline.
#[must_use]
pub fn describe_lines(outcome: &ConditionalFormatOutcome) -> Vec<String> {
    let verb = &outcome.verb;
    let book = outcome.file_name.as_deref().map_or_else(
        || format!("'{}'", outcome.spreadsheet_id),
        |n| format!("'{n}'"),
    );
    match &outcome.result {
        ConditionalFormatResult::WouldChange { summary } => {
            vec![format!("Would {summary} in {book}")]
        }
        ConditionalFormatResult::RefusedNotASpreadsheet { mime_type } => vec![format!(
            "Refused: {book} is not a Google Sheet (mimeType: {mime_type}); \
             `drive sheets {}` only works on spreadsheets",
            verb.label()
        )],
        ConditionalFormatResult::RefusedShortcut => vec![format!(
            "Refused: {book} is a shortcut; `drive sheets {}` doesn't follow shortcuts",
            verb.label()
        )],
        ConditionalFormatResult::RefusedNoVisibleParents => vec![format!(
            "Refused: {book} has no parent folder visible to this account, so no folder \
             rule can apply to it. Grant it by id instead: add {{\"file_id\": \"<spreadsheet \
             id>\", \"allow\": [\"sheets-structure\"]}} to write_permissions.rules."
        )],
        ConditionalFormatResult::RefusedSheetNotFound { title, available } => {
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
        ConditionalFormatResult::RefusedInvalidRange { detail } => {
            vec![format!("Refused: {detail}")]
        }
        ConditionalFormatResult::RefusedIndexOutOfBounds {
            sheet,
            index,
            count,
        } => {
            // `add-conditional-format` may append at `count` (valid range
            // `0..=count`); `update`/`delete` may only replace/remove an
            // existing rule (valid range `0..count`).
            let max_valid = match verb {
                ConditionalFormatVerb::AddConditionalFormat { .. } => *count,
                ConditionalFormatVerb::UpdateConditionalFormat { .. }
                | ConditionalFormatVerb::DeleteConditionalFormat { .. } => count.saturating_sub(1),
            };
            vec![format!(
                "Refused: sheet '{sheet}' has {count} conditional format rule(s) (valid \
                 indices 0-{max_valid}); index {index} is out of bounds. Run `drive sheets \
                 list-conditional-formats` to see the current indices."
            )]
        }
        ConditionalFormatResult::Blocked { decided_by } => vec![match decided_by {
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
        ConditionalFormatResult::RefusedNoLease => LeaseGateRefusal::NoLease
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        ConditionalFormatResult::RefusedLeaseExpired => LeaseGateRefusal::Expired
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        ConditionalFormatResult::RefusedLeaseWrongFile => LeaseGateRefusal::WrongFile
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        ConditionalFormatResult::RefusedLeaseStale => LeaseGateRefusal::Stale
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        ConditionalFormatResult::Changed { summary } => {
            vec![format!("Applied: {summary} in {book}")]
        }
        ConditionalFormatResult::Failed { detail } => vec![format!("Failed: {detail}")],
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

    // ── FormatCondition ──────────────────────────────────────────────

    #[test]
    fn condition_type_str_covers_every_condition() {
        let types = [
            FormatCondition::NumberBetween(1.0, 2.0).condition_type_str(),
            FormatCondition::NumberNotBetween(1.0, 2.0).condition_type_str(),
            FormatCondition::NumberGreater(1.0).condition_type_str(),
            FormatCondition::NumberGreaterEq(1.0).condition_type_str(),
            FormatCondition::NumberLess(1.0).condition_type_str(),
            FormatCondition::NumberLessEq(1.0).condition_type_str(),
            FormatCondition::NumberEq(1.0).condition_type_str(),
            FormatCondition::NumberNotEq(1.0).condition_type_str(),
            FormatCondition::TextContains("x".to_string()).condition_type_str(),
            FormatCondition::TextNotContains("x".to_string()).condition_type_str(),
            FormatCondition::TextStartsWith("x".to_string()).condition_type_str(),
            FormatCondition::TextEndsWith("x".to_string()).condition_type_str(),
            FormatCondition::TextEq("x".to_string()).condition_type_str(),
            FormatCondition::DateAfter(DateValue::Absolute("d".to_string())).condition_type_str(),
            FormatCondition::DateBefore(DateValue::Absolute("d".to_string())).condition_type_str(),
            FormatCondition::DateOn(DateValue::Absolute("d".to_string())).condition_type_str(),
            FormatCondition::DateBetween("a".to_string(), "b".to_string()).condition_type_str(),
            FormatCondition::CellEmpty.condition_type_str(),
            FormatCondition::CellNotEmpty.condition_type_str(),
            FormatCondition::CustomFormula("=TRUE".to_string()).condition_type_str(),
        ];
        let unique: HashSet<&&str> = types.iter().collect();
        assert_eq!(unique.len(), types.len());
    }

    #[test]
    fn cell_empty_and_cell_not_empty_have_no_values() {
        let built = FormatCondition::CellEmpty.into_boolean_condition();
        assert_eq!(built.condition_type, "CELL_EMPTY");
        assert!(built.values.is_empty());
        let built = FormatCondition::CellNotEmpty.into_boolean_condition();
        assert_eq!(built.condition_type, "CELL_NOT_EMPTY");
        assert!(built.values.is_empty());
    }

    #[test]
    fn number_greater_builds_a_single_value() {
        let built = FormatCondition::NumberGreater(10.0).into_boolean_condition();
        assert_eq!(built.condition_type, "NUMBER_GREATER");
        assert_eq!(built.values[0].user_entered_value, Some("10".to_string()));
    }

    #[test]
    fn date_condition_builds_a_relative_value() {
        let condition = FormatCondition::DateAfter(DateValue::parse("today".to_string()));
        let built = condition.into_boolean_condition();
        assert_eq!(built.condition_type, "DATE_AFTER");
        assert_eq!(built.values[0].user_entered_value, None);
        assert_eq!(built.values[0].relative_date, Some("TODAY".to_string()));
    }

    // ── validate_condition / validate_format_effect / validate_gradient ─

    #[test]
    fn validate_condition_rejects_a_reversed_number_between() {
        let err = validate_condition(&FormatCondition::NumberBetween(10.0, 1.0)).unwrap_err();
        assert!(err.contains("must not exceed"), "{err}");
    }

    #[test]
    fn validate_condition_accepts_cell_empty() {
        validate_condition(&FormatCondition::CellEmpty).unwrap();
        validate_condition(&FormatCondition::CellNotEmpty).unwrap();
    }

    #[test]
    fn validate_format_effect_rejects_no_fields_set() {
        let err = validate_format_effect(&FormatEffect::default()).unwrap_err();
        assert!(err.contains("at least one of"), "{err}");
    }

    #[test]
    fn validate_format_effect_accepts_background_only() {
        validate_format_effect(&FormatEffect {
            background: Some("#FF0000".to_string()),
            text_color: None,
            bold: None,
        })
        .unwrap();
    }

    #[test]
    fn validate_format_effect_rejects_a_malformed_color() {
        let err = validate_format_effect(&FormatEffect {
            background: Some("not-a-color".to_string()),
            text_color: None,
            bold: None,
        })
        .unwrap_err();
        assert!(err.contains("--background"), "{err}");
    }

    #[test]
    fn validate_gradient_accepts_min_max_only() {
        validate_gradient(&GradientSpec {
            min_color: "#FFFFFF".to_string(),
            max_color: "#00FF00".to_string(),
            mid: None,
        })
        .unwrap();
    }

    #[test]
    fn validate_gradient_rejects_a_malformed_midpoint_color() {
        let err = validate_gradient(&GradientSpec {
            min_color: "#FFFFFF".to_string(),
            max_color: "#00FF00".to_string(),
            mid: Some(GradientMidpoint {
                color: "bad".to_string(),
                point_type: GradientPointType::Percent,
                value: "50".to_string(),
            }),
        })
        .unwrap_err();
        assert!(err.contains("--gradient-mid-color"), "{err}");
    }

    #[test]
    fn validate_gradient_rejects_a_blank_midpoint_value() {
        let err = validate_gradient(&GradientSpec {
            min_color: "#FFFFFF".to_string(),
            max_color: "#00FF00".to_string(),
            mid: Some(GradientMidpoint {
                color: "#000000".to_string(),
                point_type: GradientPointType::Number,
                value: "  ".to_string(),
            }),
        })
        .unwrap_err();
        assert!(err.contains("--gradient-mid-value"), "{err}");
    }

    // ── describe ──────────────────────────────────────────────────────

    #[test]
    fn describe_existing_rule_covers_boolean_and_gradient() {
        let boolean = ConditionalFormatRule {
            ranges: vec![GridRange::default()],
            boolean_rule: Some(BooleanRule {
                condition: BooleanCondition {
                    condition_type: "NUMBER_GREATER".to_string(),
                    values: Vec::new(),
                },
                format: CellFormat::default(),
            }),
            gradient_rule: None,
        };
        assert!(describe_existing_rule(&boolean).contains("number greater"));

        let gradient = ConditionalFormatRule {
            ranges: vec![GridRange::default()],
            boolean_rule: None,
            gradient_rule: Some(GradientRule::default()),
        };
        assert!(describe_existing_rule(&gradient).contains("gradient rule"));
    }

    // ── log_operation / label ────────────────────────────────────────

    #[test]
    fn log_operation_and_label_cover_every_verb() {
        let add = ConditionalFormatVerb::AddConditionalFormat {
            sheet: "Q1".to_string(),
            ranges: vec!["A1".to_string()],
            index: None,
            rule: FormatRule::Boolean {
                condition: FormatCondition::CellEmpty,
                format: FormatEffect::default(),
            },
        };
        let update = ConditionalFormatVerb::UpdateConditionalFormat {
            sheet: "Q1".to_string(),
            ranges: vec!["A1".to_string()],
            index: 0,
            rule: FormatRule::Boolean {
                condition: FormatCondition::CellEmpty,
                format: FormatEffect::default(),
            },
        };
        let delete = ConditionalFormatVerb::DeleteConditionalFormat {
            sheet: "Q1".to_string(),
            index: 0,
        };
        assert_eq!(add.log_operation(), "sheets-add-conditional-format");
        assert_eq!(update.log_operation(), "sheets-update-conditional-format");
        assert_eq!(delete.log_operation(), "sheets-delete-conditional-format");
        assert_eq!(add.label(), "add-conditional-format");
        assert_eq!(update.label(), "update-conditional-format");
        assert_eq!(delete.label(), "delete-conditional-format");
    }

    // ── integration (wiremock) ───────────────────────────────────────

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

    fn leased_opts_for(spreadsheet_id: &str) -> (Option<String>, std::path::PathBuf) {
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        let token = seed_lease(&ledger_path, spreadsheet_id, "1");
        (Some(token), ledger_path)
    }

    /// A workbook with one sheet ("Q1") holding two existing conditional
    /// format rules at indices 0 and 1.
    fn mount_workbook() -> wiremock::Mock {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/sheet-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "spreadsheetId": "sheet-1",
                    "properties": {"title": "Budget"},
                    "sheets": [
                        {
                            "properties": {"sheetId": 0, "title": "Q1", "index": 0},
                            "conditionalFormats": [
                                {
                                    "ranges": [{"sheetId": 0, "startRowIndex": 0, "endRowIndex": 1}],
                                    "booleanRule": {
                                        "condition": {"type": "CELL_EMPTY", "values": []},
                                        "format": {"backgroundColorStyle": {"rgbColor": {"red": 1.0, "green": 0.0, "blue": 0.0}}},
                                    },
                                },
                                {
                                    "ranges": [{"sheetId": 0, "startRowIndex": 1, "endRowIndex": 2}],
                                    "gradientRule": {
                                        "minColorStyle": {"rgbColor": {"red": 1.0, "green": 1.0, "blue": 1.0}},
                                        "maxColorStyle": {"rgbColor": {"red": 0.0, "green": 1.0, "blue": 0.0}},
                                    },
                                },
                            ],
                        },
                    ],
                })),
            )
    }

    fn add_verb() -> ConditionalFormatVerb {
        ConditionalFormatVerb::AddConditionalFormat {
            sheet: "Q1".to_string(),
            ranges: vec!["A1:A10".to_string()],
            index: None,
            rule: FormatRule::Boolean {
                condition: FormatCondition::NumberGreater(100.0),
                format: FormatEffect {
                    background: Some("#00FF00".to_string()),
                    text_color: None,
                    bold: None,
                },
            },
        }
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
        let rules: Vec<FolderPermissionRule> = Vec::new();
        let opts = ConditionalFormatOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: add_verb(),
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = conditional_format(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            ConditionalFormatResult::Blocked { .. }
        ));
    }

    #[tokio::test]
    async fn add_sends_a_boolean_rule_and_appends_at_the_current_length() {
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
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"replies": [{}]})),
            )
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
        let opts = ConditionalFormatOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: add_verb(),
            dry_run: false,
            lease_token,
            ledger_path,
        };
        let outcome = conditional_format(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(outcome.result, ConditionalFormatResult::Changed { .. }),
            "{:?}",
            outcome.result
        );

        let requests = server.received_requests().await.unwrap();
        let batch = requests
            .iter()
            .find(|r| r.url.path().ends_with(":batchUpdate"))
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&batch.body).unwrap();
        let added = &body["requests"][0]["addConditionalFormatRule"];
        // The mocked workbook already has 2 rules, so a caller-less --index
        // appends at position 2.
        assert_eq!(added["index"], 2);
        assert_eq!(
            added["rule"]["booleanRule"]["condition"]["type"],
            "NUMBER_GREATER"
        );
    }

    #[tokio::test]
    async fn add_refuses_an_explicit_out_of_bounds_or_negative_index() {
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

        for bad_index in [-1, 3] {
            let (lease_token, ledger_path) = leased_opts_for("sheet-1");
            let verb = match add_verb() {
                ConditionalFormatVerb::AddConditionalFormat {
                    sheet,
                    ranges,
                    rule,
                    ..
                } => ConditionalFormatVerb::AddConditionalFormat {
                    sheet,
                    ranges,
                    index: Some(bad_index),
                    rule,
                },
                other => other,
            };
            let opts = ConditionalFormatOptions {
                spreadsheet_id: "sheet-1".to_string(),
                verb,
                dry_run: false,
                lease_token,
                ledger_path,
            };
            let outcome = conditional_format(&drive, &sheets, &opts, &rules).await;
            match outcome.result {
                ConditionalFormatResult::RefusedIndexOutOfBounds {
                    sheet,
                    index,
                    count,
                } => {
                    assert_eq!(sheet, "Q1");
                    assert_eq!(index, bad_index);
                    // The mocked workbook has 2 existing rules; valid
                    // explicit indices for add are 0..=2.
                    assert_eq!(count, 2);
                }
                other => panic!("expected RefusedIndexOutOfBounds, got {other:?}"),
            }
        }

        // Neither attempt reached the API.
        let requests = server.received_requests().await.unwrap();
        assert!(!requests
            .iter()
            .any(|r| r.url.path().ends_with(":batchUpdate")));
    }

    #[tokio::test]
    async fn delete_sends_the_sheet_id_and_index() {
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
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"replies": [{}]})),
            )
            .mount(&server)
            .await;
        let rules = vec![allow_rule("folder-1")];
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
        let opts = ConditionalFormatOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: ConditionalFormatVerb::DeleteConditionalFormat {
                sheet: "Q1".to_string(),
                index: 1,
            },
            dry_run: false,
            lease_token,
            ledger_path,
        };
        let outcome = conditional_format(&drive, &sheets, &opts, &rules).await;
        assert!(
            matches!(outcome.result, ConditionalFormatResult::Changed { .. }),
            "{:?}",
            outcome.result
        );

        let requests = server.received_requests().await.unwrap();
        let batch = requests
            .iter()
            .find(|r| r.url.path().ends_with(":batchUpdate"))
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&batch.body).unwrap();
        let deleted = &body["requests"][0]["deleteConditionalFormatRule"];
        assert_eq!(deleted["sheetId"], 0);
        assert_eq!(deleted["index"], 1);
    }

    #[tokio::test]
    async fn delete_refuses_an_out_of_bounds_index() {
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
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
        let opts = ConditionalFormatOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: ConditionalFormatVerb::DeleteConditionalFormat {
                sheet: "Q1".to_string(),
                index: 5,
            },
            dry_run: false,
            lease_token,
            ledger_path,
        };
        let outcome = conditional_format(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            ConditionalFormatResult::RefusedIndexOutOfBounds {
                sheet,
                index,
                count,
            } => {
                assert_eq!(sheet, "Q1");
                assert_eq!(index, 5);
                assert_eq!(count, 2);
            }
            other => panic!("expected RefusedIndexOutOfBounds, got {other:?}"),
        }

        // No batchUpdate call was made — the workbook GET is the only
        // request beyond the metadata/folder fetches.
        let requests = server.received_requests().await.unwrap();
        assert!(!requests
            .iter()
            .any(|r| r.url.path().ends_with(":batchUpdate")));
    }

    #[tokio::test]
    async fn update_dry_run_echoes_the_rule_currently_at_the_index() {
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
        let opts = ConditionalFormatOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: ConditionalFormatVerb::UpdateConditionalFormat {
                sheet: "Q1".to_string(),
                ranges: vec!["A1:A10".to_string()],
                index: 0,
                rule: FormatRule::Boolean {
                    condition: FormatCondition::NumberGreater(100.0),
                    format: FormatEffect {
                        background: Some("#00FF00".to_string()),
                        text_color: None,
                        bold: None,
                    },
                },
            },
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = conditional_format(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            ConditionalFormatResult::WouldChange { summary } => {
                assert!(
                    summary.contains("currently: boolean rule (cell empty"),
                    "{summary}"
                );
                assert!(summary.contains("number greater"), "{summary}");
            }
            other => panic!("expected WouldChange, got {other:?}"),
        }

        // A dry run never calls batchUpdate.
        let requests = server.received_requests().await.unwrap();
        assert!(!requests
            .iter()
            .any(|r| r.url.path().ends_with(":batchUpdate")));
    }

    #[tokio::test]
    async fn add_rejects_a_malformed_background_color_before_the_lease_gate() {
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
        let rules = vec![allow_rule("folder-1")];
        let opts = ConditionalFormatOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: ConditionalFormatVerb::AddConditionalFormat {
                sheet: "Q1".to_string(),
                ranges: vec!["A1:A10".to_string()],
                index: None,
                rule: FormatRule::Boolean {
                    condition: FormatCondition::CellEmpty,
                    format: FormatEffect {
                        background: Some("not-a-color".to_string()),
                        text_color: None,
                        bold: None,
                    },
                },
            },
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = conditional_format(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            ConditionalFormatResult::RefusedInvalidRange { detail } => {
                assert!(detail.contains("--background"), "{detail}");
            }
            other => panic!("expected RefusedInvalidRange, got {other:?}"),
        }
    }
}
