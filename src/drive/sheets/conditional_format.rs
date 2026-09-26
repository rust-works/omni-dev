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
//! trigger) and adds `BLANK`/`NOT_BLANK` (`--cell-empty`/`--cell-not-empty`,
//! meaningful only as a format trigger). [`GradientRule`](crate::drive::sheets::types::GradientRule)'s
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
            Self::CellEmpty => "BLANK",
            Self::CellNotEmpty => "NOT_BLANK",
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
                value: Some(mid.value),
            }),
            None => None,
        };
        let endpoint = |rgb_color, point_type: &str| InterpolationPoint {
            color_style: ColorStyle { rgb_color },
            point_type: point_type.to_string(),
            value: None,
        };
        Ok(GradientRule {
            minpoint: Some(endpoint(min, "MIN")),
            midpoint,
            maxpoint: Some(endpoint(max, "MAX")),
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
        // The midpoint always has an anchor worth naming; an endpoint only
        // when it isn't this crate's own `MIN`/`MAX` (a Sheets UI rule can
        // anchor it at a value).
        let mut anchors = String::new();
        for (name, point, builtin) in [
            ("min", &gradient_rule.minpoint, Some("MIN")),
            ("mid", &gradient_rule.midpoint, None),
            ("max", &gradient_rule.maxpoint, Some("MAX")),
        ] {
            let Some(point) = point else { continue };
            if builtin.is_some_and(|t| point.point_type == t && point.value.is_none()) {
                continue;
            }
            let point_type = point.point_type.to_ascii_lowercase();
            anchors.push_str(&match &point.value {
                Some(value) => format!(", {name} {point_type} at {value}"),
                None => format!(", {name} {point_type}"),
            });
        }
        format!("gradient rule (min/max colors{anchors})")
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
        assert_eq!(built.condition_type, "BLANK");
        assert!(built.values.is_empty());
        let built = FormatCondition::CellNotEmpty.into_boolean_condition();
        assert_eq!(built.condition_type, "NOT_BLANK");
        assert!(built.values.is_empty());
    }

    /// Serialises `rule` as the `addConditionalFormatRule` request the
    /// engine sends, so a test can pin the exact wire body.
    fn add_request_json(rule: FormatRule) -> serde_json::Value {
        let rule = rule
            .into_conditional_format_rule(vec![GridRange::default()])
            .unwrap();
        serde_json::to_value(BatchUpdateRequestItem::AddConditionalFormatRule(
            AddConditionalFormatRuleRequest { rule, index: 0 },
        ))
        .unwrap()
    }

    #[test]
    fn cell_empty_and_cell_not_empty_send_the_sheets_blank_condition_types() {
        // Issue #1943: `CELL_EMPTY`/`CELL_NOT_EMPTY` aren't `ConditionType`
        // values, and Sheets rejected them with a 400.
        for (condition, expected) in [
            (FormatCondition::CellEmpty, "BLANK"),
            (FormatCondition::CellNotEmpty, "NOT_BLANK"),
        ] {
            let body = add_request_json(FormatRule::Boolean {
                condition,
                format: FormatEffect {
                    background: Some("#CCCCCC".to_string()),
                    ..FormatEffect::default()
                },
            });
            assert_eq!(
                body["addConditionalFormatRule"]["rule"]["booleanRule"]["condition"],
                serde_json::json!({"type": expected}),
            );
        }
    }

    #[test]
    fn a_two_point_gradient_serialises_as_min_and_max_interpolation_points() {
        // Issue #1944: the body used to carry invented
        // `minColorStyle`/`maxColorStyle` fields, which Sheets rejected.
        let body = add_request_json(FormatRule::Gradient(GradientSpec {
            min_color: "#FFFFFF".to_string(),
            max_color: "#00FF00".to_string(),
            mid: None,
        }));
        assert_eq!(
            body["addConditionalFormatRule"]["rule"]["gradientRule"],
            serde_json::json!({
                "minpoint": {
                    "colorStyle": {"rgbColor": {"red": 1.0, "green": 1.0, "blue": 1.0}},
                    "type": "MIN",
                },
                "maxpoint": {
                    "colorStyle": {"rgbColor": {"red": 0.0, "green": 1.0, "blue": 0.0}},
                    "type": "MAX",
                },
            }),
        );
    }

    #[test]
    fn a_three_point_gradient_serialises_its_midpoint_with_a_value() {
        let body = add_request_json(FormatRule::Gradient(GradientSpec {
            min_color: "#FFFFFF".to_string(),
            max_color: "#00FF00".to_string(),
            mid: Some(GradientMidpoint {
                color: "#FF0000".to_string(),
                point_type: GradientPointType::Percent,
                value: "50".to_string(),
            }),
        }));
        let gradient = &body["addConditionalFormatRule"]["rule"]["gradientRule"];
        assert_eq!(
            gradient["midpoint"],
            serde_json::json!({
                "colorStyle": {"rgbColor": {"red": 1.0, "green": 0.0, "blue": 0.0}},
                "type": "PERCENT",
                "value": "50",
            }),
        );
        assert_eq!(gradient["minpoint"]["type"], "MIN");
        assert_eq!(gradient["maxpoint"]["type"], "MAX");
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

    #[test]
    fn into_boolean_condition_covers_every_remaining_variant() {
        // NumberGreater, DateAfter, CellEmpty and CellNotEmpty are already
        // covered by the tests above; this exercises the rest of the
        // `values` match in `into_boolean_condition`.
        let cases: Vec<(FormatCondition, &str, usize)> = vec![
            (
                FormatCondition::NumberBetween(1.0, 2.0),
                "NUMBER_BETWEEN",
                2,
            ),
            (
                FormatCondition::NumberNotBetween(1.0, 2.0),
                "NUMBER_NOT_BETWEEN",
                2,
            ),
            (
                FormatCondition::NumberGreaterEq(1.0),
                "NUMBER_GREATER_THAN_EQ",
                1,
            ),
            (FormatCondition::NumberLess(1.0), "NUMBER_LESS", 1),
            (FormatCondition::NumberLessEq(1.0), "NUMBER_LESS_THAN_EQ", 1),
            (FormatCondition::NumberEq(1.0), "NUMBER_EQ", 1),
            (FormatCondition::NumberNotEq(1.0), "NUMBER_NOT_EQ", 1),
            (
                FormatCondition::TextContains("x".to_string()),
                "TEXT_CONTAINS",
                1,
            ),
            (
                FormatCondition::TextNotContains("x".to_string()),
                "TEXT_NOT_CONTAINS",
                1,
            ),
            (
                FormatCondition::TextStartsWith("x".to_string()),
                "TEXT_STARTS_WITH",
                1,
            ),
            (
                FormatCondition::TextEndsWith("x".to_string()),
                "TEXT_ENDS_WITH",
                1,
            ),
            (FormatCondition::TextEq("x".to_string()), "TEXT_EQ", 1),
            (
                FormatCondition::DateBefore(DateValue::Absolute("2024-01-01".to_string())),
                "DATE_BEFORE",
                1,
            ),
            (
                FormatCondition::DateOn(DateValue::Absolute("2024-01-01".to_string())),
                "DATE_EQ",
                1,
            ),
            (
                FormatCondition::DateBetween("2024-01-01".to_string(), "2024-01-02".to_string()),
                "DATE_BETWEEN",
                2,
            ),
            (
                FormatCondition::CustomFormula("=A1>0".to_string()),
                "CUSTOM_FORMULA",
                1,
            ),
        ];
        for (condition, expected_type, expected_len) in cases {
            let built = condition.into_boolean_condition();
            assert_eq!(built.condition_type, expected_type, "{expected_type}");
            assert_eq!(built.values.len(), expected_len, "{expected_type}");
        }
    }

    // ── GradientSpec ──────────────────────────────────────────────────

    #[test]
    fn into_gradient_rule_builds_each_midpoint_type() {
        for (point_type, expected) in [
            (GradientPointType::Number, "NUMBER"),
            (GradientPointType::Percent, "PERCENT"),
            (GradientPointType::Percentile, "PERCENTILE"),
        ] {
            let spec = GradientSpec {
                min_color: "#FFFFFF".to_string(),
                max_color: "#000000".to_string(),
                mid: Some(GradientMidpoint {
                    color: "#FF00FF".to_string(),
                    point_type,
                    value: "50".to_string(),
                }),
            };
            let built = spec.into_gradient_rule().unwrap();
            let midpoint = built.midpoint.expect("midpoint should be set");
            assert_eq!(midpoint.point_type, expected, "{expected}");
            assert_eq!(midpoint.value.as_deref(), Some("50"));
            assert_eq!(
                midpoint.color_style.rgb_color,
                parse_hex_color("#FF00FF").unwrap()
            );
            let min = built.minpoint.expect("minpoint should be set");
            assert_eq!(min.point_type, "MIN");
            assert_eq!(min.value, None);
            assert_eq!(
                min.color_style.rgb_color,
                parse_hex_color("#FFFFFF").unwrap()
            );
            let max = built.maxpoint.expect("maxpoint should be set");
            assert_eq!(max.point_type, "MAX");
            assert_eq!(max.value, None);
            assert_eq!(
                max.color_style.rgb_color,
                parse_hex_color("#000000").unwrap()
            );
        }
    }

    #[test]
    fn into_gradient_rule_propagates_a_malformed_midpoint_color() {
        // `validate_gradient` catches this before `into_gradient_rule` is
        // ever reached in the real flow, so this exercises the `?`
        // error-propagation arm directly, the same way
        // `into_cell_format_propagates_a_malformed_color` below does for
        // `FormatEffect`.
        let spec = GradientSpec {
            min_color: "#FFFFFF".to_string(),
            max_color: "#000000".to_string(),
            mid: Some(GradientMidpoint {
                color: "not-a-color".to_string(),
                point_type: GradientPointType::Number,
                value: "50".to_string(),
            }),
        };
        let err = spec.into_gradient_rule().unwrap_err();
        assert!(err.contains("--gradient-mid-color"), "{err}");
    }

    #[test]
    fn into_cell_format_propagates_a_malformed_color() {
        // Like the gradient case above: `validate_format_effect` catches
        // this in the real flow, so this exercises `into_cell_format`'s two
        // `?` error-propagation arms (background, then text color)
        // directly.
        let err = FormatEffect {
            background: Some("not-a-color".to_string()),
            text_color: None,
            bold: None,
        }
        .into_cell_format()
        .unwrap_err();
        assert!(err.contains("--background"), "{err}");

        let err = FormatEffect {
            background: None,
            text_color: Some("not-a-color".to_string()),
            bold: None,
        }
        .into_cell_format()
        .unwrap_err();
        assert!(err.contains("--text-color"), "{err}");
    }

    #[test]
    fn into_gradient_rule_propagates_a_malformed_min_or_max_color() {
        let err = GradientSpec {
            min_color: "not-a-color".to_string(),
            max_color: "#000000".to_string(),
            mid: None,
        }
        .into_gradient_rule()
        .unwrap_err();
        assert!(err.contains("--gradient-min-color"), "{err}");

        let err = GradientSpec {
            min_color: "#FFFFFF".to_string(),
            max_color: "not-a-color".to_string(),
            mid: None,
        }
        .into_gradient_rule()
        .unwrap_err();
        assert!(err.contains("--gradient-max-color"), "{err}");
    }

    #[test]
    fn into_gradient_rule_with_no_midpoint_leaves_it_none() {
        // `mid: None` is the shape `mount_workbook`'s fixture and every
        // add/update test use, but none of them go through
        // `into_gradient_rule` itself (they all build a boolean rule) —
        // this hits the `None => None` arm directly.
        let built = GradientSpec {
            min_color: "#FFFFFF".to_string(),
            max_color: "#000000".to_string(),
            mid: None,
        }
        .into_gradient_rule()
        .unwrap();
        assert!(built.midpoint.is_none());
    }

    #[test]
    fn into_conditional_format_rule_covers_both_variants_and_their_errors() {
        // No test anywhere in this file builds a `FormatRule::Gradient`
        // through the verb/engine flow (`add_verb` and friends only use
        // `FormatRule::Boolean`), so `into_conditional_format_rule`'s
        // `Gradient` arm — and its own `?` forwarding for both arms — is
        // otherwise never reached.
        let rule = FormatRule::Boolean {
            condition: FormatCondition::CellEmpty,
            format: FormatEffect {
                background: Some("#FF0000".to_string()),
                text_color: None,
                bold: None,
            },
        }
        .into_conditional_format_rule(vec![GridRange::default()])
        .unwrap();
        assert!(rule.boolean_rule.is_some());
        assert!(rule.gradient_rule.is_none());

        let err = FormatRule::Boolean {
            condition: FormatCondition::CellEmpty,
            format: FormatEffect {
                background: Some("not-a-color".to_string()),
                text_color: None,
                bold: None,
            },
        }
        .into_conditional_format_rule(vec![GridRange::default()])
        .unwrap_err();
        assert!(err.contains("--background"), "{err}");

        let rule = FormatRule::Gradient(GradientSpec {
            min_color: "#FFFFFF".to_string(),
            max_color: "#000000".to_string(),
            mid: None,
        })
        .into_conditional_format_rule(vec![GridRange::default()])
        .unwrap();
        assert!(rule.gradient_rule.is_some());
        assert!(rule.boolean_rule.is_none());

        let err = FormatRule::Gradient(GradientSpec {
            min_color: "not-a-color".to_string(),
            max_color: "#000000".to_string(),
            mid: None,
        })
        .into_conditional_format_rule(vec![GridRange::default()])
        .unwrap_err();
        assert!(err.contains("--gradient-min-color"), "{err}");
    }

    // ── validate_format_rule ─────────────────────────────────────────

    #[test]
    fn validate_format_rule_short_circuits_on_a_condition_error() {
        // The one existing wiremock test that reaches `validate_format_rule`
        // (`add_rejects_a_malformed_background_color_before_the_lease_gate`)
        // always uses `FormatCondition::CellEmpty`, which never fails, so
        // the condition-side `?`'s error arm is otherwise never taken.
        let err = validate_format_rule(&FormatRule::Boolean {
            condition: FormatCondition::NumberBetween(10.0, 1.0),
            format: FormatEffect {
                background: Some("#FF0000".to_string()),
                text_color: None,
                bold: None,
            },
        })
        .unwrap_err();
        assert!(err.contains("must not exceed"), "{err}");

        validate_format_rule(&FormatRule::Boolean {
            condition: FormatCondition::CellEmpty,
            format: FormatEffect {
                background: Some("#FF0000".to_string()),
                text_color: None,
                bold: None,
            },
        })
        .unwrap();
    }

    #[test]
    fn validate_format_rule_dispatches_gradient_specs_to_validate_gradient() {
        // No test anywhere in this file calls `validate_format_rule` with a
        // `FormatRule::Gradient` — every wiremock verb builder uses
        // `FormatRule::Boolean` — so this arm is otherwise dead.
        validate_format_rule(&FormatRule::Gradient(GradientSpec {
            min_color: "#FFFFFF".to_string(),
            max_color: "#000000".to_string(),
            mid: None,
        }))
        .unwrap();

        let err = validate_format_rule(&FormatRule::Gradient(GradientSpec {
            min_color: "not-a-color".to_string(),
            max_color: "#000000".to_string(),
            mid: None,
        }))
        .unwrap_err();
        assert!(err.contains("--gradient-min-color"), "{err}");
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
    fn validate_condition_rejects_a_reversed_number_not_between() {
        let err = validate_condition(&FormatCondition::NumberNotBetween(10.0, 1.0)).unwrap_err();
        assert!(err.contains("--number-not-between"), "{err}");
        assert!(err.contains("must not exceed"), "{err}");
    }

    #[test]
    fn validate_condition_rejects_nan_across_every_single_number_flag() {
        let cases = [
            (FormatCondition::NumberGreater(f64::NAN), "--number-greater"),
            (
                FormatCondition::NumberGreaterEq(f64::NAN),
                "--number-greater-eq",
            ),
            (FormatCondition::NumberLess(f64::NAN), "--number-less"),
            (FormatCondition::NumberLessEq(f64::NAN), "--number-less-eq"),
            (FormatCondition::NumberEq(f64::NAN), "--number-eq"),
            (FormatCondition::NumberNotEq(f64::NAN), "--number-not-eq"),
        ];
        for (condition, flag) in cases {
            let err = validate_condition(&condition).unwrap_err();
            assert!(err.contains(flag), "{err}");
            assert!(err.contains("NaN"), "{err}");
        }
        // A non-NaN value on each of those flags is accepted.
        validate_condition(&FormatCondition::NumberGreater(1.0)).unwrap();
        validate_condition(&FormatCondition::NumberGreaterEq(1.0)).unwrap();
        validate_condition(&FormatCondition::NumberLess(1.0)).unwrap();
        validate_condition(&FormatCondition::NumberLessEq(1.0)).unwrap();
        validate_condition(&FormatCondition::NumberEq(1.0)).unwrap();
        validate_condition(&FormatCondition::NumberNotEq(1.0)).unwrap();
    }

    #[test]
    fn validate_condition_rejects_an_empty_string_across_every_text_flag() {
        let cases = [
            (
                FormatCondition::TextContains(String::new()),
                "--text-contains",
            ),
            (
                FormatCondition::TextNotContains(String::new()),
                "--text-not-contains",
            ),
            (
                FormatCondition::TextStartsWith(String::new()),
                "--text-starts-with",
            ),
            (
                FormatCondition::TextEndsWith(String::new()),
                "--text-ends-with",
            ),
            (FormatCondition::TextEq(String::new()), "--text-eq"),
        ];
        for (condition, flag) in cases {
            let err = validate_condition(&condition).unwrap_err();
            assert!(err.contains(flag), "{err}");
        }
        // A non-empty value on each of those flags is accepted.
        validate_condition(&FormatCondition::TextContains("x".to_string())).unwrap();
        validate_condition(&FormatCondition::TextNotContains("x".to_string())).unwrap();
        validate_condition(&FormatCondition::TextStartsWith("x".to_string())).unwrap();
        validate_condition(&FormatCondition::TextEndsWith("x".to_string())).unwrap();
        validate_condition(&FormatCondition::TextEq("x".to_string())).unwrap();
    }

    #[test]
    fn validate_condition_rejects_a_blank_date_across_every_single_date_flag() {
        let blank = DateValue::Absolute(String::new());
        let cases = [
            (FormatCondition::DateAfter(blank.clone()), "--date-after"),
            (FormatCondition::DateBefore(blank.clone()), "--date-before"),
            (FormatCondition::DateOn(blank), "--date-on"),
        ];
        for (condition, flag) in cases {
            let err = validate_condition(&condition).unwrap_err();
            assert!(err.contains(flag), "{err}");
        }
        // A non-blank date is accepted.
        let today = DateValue::parse("today".to_string());
        validate_condition(&FormatCondition::DateAfter(today.clone())).unwrap();
        validate_condition(&FormatCondition::DateBefore(today.clone())).unwrap();
        validate_condition(&FormatCondition::DateOn(today)).unwrap();
    }

    #[test]
    fn validate_condition_rejects_an_invalid_or_reversed_date_between() {
        let err = validate_condition(&FormatCondition::DateBetween(
            "2024-01-01".to_string(),
            String::new(),
        ))
        .unwrap_err();
        assert!(err.contains("--date-between"), "{err}");

        let err = validate_condition(&FormatCondition::DateBetween(
            "2024-06-01".to_string(),
            "2024-01-01".to_string(),
        ))
        .unwrap_err();
        assert!(err.contains("--date-between"), "{err}");

        validate_condition(&FormatCondition::DateBetween(
            "2024-01-01".to_string(),
            "2024-06-01".to_string(),
        ))
        .unwrap();
    }

    #[test]
    fn validate_condition_rejects_a_blank_custom_formula() {
        let err =
            validate_condition(&FormatCondition::CustomFormula("   ".to_string())).unwrap_err();
        assert!(err.contains("--custom-formula"), "{err}");

        validate_condition(&FormatCondition::CustomFormula("=A1>0".to_string())).unwrap();
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

    #[test]
    fn describe_existing_rule_joins_multiple_boolean_format_parts() {
        let rule = ConditionalFormatRule {
            ranges: vec![GridRange::default()],
            boolean_rule: Some(BooleanRule {
                condition: BooleanCondition {
                    condition_type: "BLANK".to_string(),
                    values: Vec::new(),
                },
                format: CellFormat {
                    background_color_style: Some(ColorStyle {
                        rgb_color: parse_hex_color("#FF0000").unwrap(),
                    }),
                    text_format: Some(TextFormat {
                        foreground_color_style: Some(ColorStyle {
                            rgb_color: parse_hex_color("#00FF00").unwrap(),
                        }),
                        bold: Some(true),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            }),
            gradient_rule: None,
        };
        let described = describe_existing_rule(&rule);
        assert!(
            described.contains("background+text color+bold"),
            "{described}"
        );
    }

    #[test]
    fn describe_existing_rule_includes_the_gradient_midpoint() {
        let rule = ConditionalFormatRule {
            ranges: vec![GridRange::default()],
            boolean_rule: None,
            gradient_rule: Some(GradientRule {
                minpoint: Some(point("MIN", None)),
                midpoint: Some(point("PERCENT", Some("50"))),
                maxpoint: Some(point("MAX", None)),
            }),
        };
        assert_eq!(
            describe_existing_rule(&rule),
            "gradient rule (min/max colors, mid percent at 50)"
        );
    }

    fn point(point_type: &str, value: Option<&str>) -> InterpolationPoint {
        InterpolationPoint {
            color_style: ColorStyle::default(),
            point_type: point_type.to_string(),
            value: value.map(str::to_string),
        }
    }

    #[test]
    fn describe_existing_rule_names_a_value_anchored_endpoint() {
        // A Sheets UI rule can anchor an endpoint at a value, which this
        // crate never builds; don't describe it as a plain min/max scale.
        let rule = ConditionalFormatRule {
            ranges: vec![GridRange::default()],
            boolean_rule: None,
            gradient_rule: Some(GradientRule {
                minpoint: Some(point("NUMBER", Some("10"))),
                midpoint: None,
                maxpoint: Some(point("PERCENTILE", Some("90"))),
            }),
        };
        assert_eq!(
            describe_existing_rule(&rule),
            "gradient rule (min/max colors, min number at 10, max percentile at 90)"
        );
    }

    #[test]
    fn describe_existing_rule_falls_back_for_an_unrecognized_rule_type() {
        // Not a shape the real API would send (a `ConditionalFormatRule`
        // with neither variant of its union set), but the struct allows it,
        // so the defensive fallback is worth pinning directly.
        let rule = ConditionalFormatRule {
            ranges: vec![GridRange::default()],
            boolean_rule: None,
            gradient_rule: None,
        };
        assert_eq!(describe_existing_rule(&rule), "rule (unrecognized type)");
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

    // ── ConditionalFormatResult: FromLeaseRefusal / log_status ─────────

    #[test]
    fn from_lease_refusal_maps_every_variant() {
        assert_eq!(
            <ConditionalFormatResult as FromLeaseRefusal>::from_no_lease(),
            ConditionalFormatResult::RefusedNoLease
        );
        assert_eq!(
            <ConditionalFormatResult as FromLeaseRefusal>::from_lease_expired(),
            ConditionalFormatResult::RefusedLeaseExpired
        );
        assert_eq!(
            <ConditionalFormatResult as FromLeaseRefusal>::from_lease_wrong_file(),
            ConditionalFormatResult::RefusedLeaseWrongFile
        );
        assert_eq!(
            <ConditionalFormatResult as FromLeaseRefusal>::from_lease_stale(),
            ConditionalFormatResult::RefusedLeaseStale
        );
        assert_eq!(
            <ConditionalFormatResult as FromLeaseRefusal>::from_lease_failed("boom".to_string()),
            ConditionalFormatResult::Failed {
                detail: "boom".to_string()
            }
        );
    }

    #[test]
    fn log_status_covers_every_result_variant() {
        let cases = [
            (
                ConditionalFormatResult::WouldChange {
                    summary: String::new(),
                },
                "would-change",
            ),
            (
                ConditionalFormatResult::RefusedNotASpreadsheet {
                    mime_type: String::new(),
                },
                "refused-not-a-spreadsheet",
            ),
            (ConditionalFormatResult::RefusedShortcut, "refused-shortcut"),
            (
                ConditionalFormatResult::RefusedNoVisibleParents,
                "refused-no-visible-parents",
            ),
            (
                ConditionalFormatResult::RefusedSheetNotFound {
                    title: String::new(),
                    available: Vec::new(),
                },
                "refused-sheet-not-found",
            ),
            (
                ConditionalFormatResult::RefusedInvalidRange {
                    detail: String::new(),
                },
                "refused-invalid-range",
            ),
            (
                ConditionalFormatResult::RefusedIndexOutOfBounds {
                    sheet: String::new(),
                    index: 0,
                    count: 0,
                },
                "refused-index-out-of-bounds",
            ),
            (
                ConditionalFormatResult::Blocked { decided_by: None },
                "blocked",
            ),
            (ConditionalFormatResult::RefusedNoLease, "refused-no-lease"),
            (
                ConditionalFormatResult::RefusedLeaseExpired,
                "refused-lease-expired",
            ),
            (
                ConditionalFormatResult::RefusedLeaseWrongFile,
                "refused-lease-wrong-file",
            ),
            (
                ConditionalFormatResult::RefusedLeaseStale,
                "refused-lease-stale",
            ),
            (
                ConditionalFormatResult::Changed {
                    summary: String::new(),
                },
                "changed",
            ),
            (
                ConditionalFormatResult::Failed {
                    detail: String::new(),
                },
                "failed",
            ),
        ];
        for (result, expected) in cases {
            assert_eq!(result.log_status(), expected, "{result:?}");
        }
    }

    #[test]
    fn write_jsonl_delegates_to_write_scalar_jsonl() {
        let outcome = ConditionalFormatOutcome {
            spreadsheet_id: "sheet-1".to_string(),
            file_name: Some("Budget".to_string()),
            resolved_folder_id: None,
            verb: add_verb(),
            result: ConditionalFormatResult::Changed {
                summary: "x".to_string(),
            },
        };
        let mut buf = Vec::new();
        outcome.write_jsonl(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("\"status\":\"changed\""), "{text}");
        assert!(text.contains("\"spreadsheet_id\":\"sheet-1\""), "{text}");
    }

    // ── describe_lines ───────────────────────────────────────────────

    fn describe_outcome(
        verb: ConditionalFormatVerb,
        result: ConditionalFormatResult,
    ) -> ConditionalFormatOutcome {
        ConditionalFormatOutcome {
            spreadsheet_id: "sheet-1".to_string(),
            file_name: Some("Budget".to_string()),
            resolved_folder_id: None,
            verb,
            result,
        }
    }

    #[test]
    fn describe_lines_renders_would_change_and_changed() {
        let outcome = describe_outcome(
            add_verb(),
            ConditionalFormatResult::WouldChange {
                summary: "add conditional format (boolean rule)".to_string(),
            },
        );
        let lines = describe_lines(&outcome);
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains("Would add conditional format"),
            "{lines:?}"
        );
        assert!(lines[0].contains("'Budget'"), "{lines:?}");

        let outcome = describe_outcome(
            add_verb(),
            ConditionalFormatResult::Changed {
                summary: "add conditional format (boolean rule)".to_string(),
            },
        );
        let lines = describe_lines(&outcome);
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains("Applied: add conditional format"),
            "{lines:?}"
        );
    }

    #[test]
    fn describe_lines_falls_back_to_the_spreadsheet_id_with_no_file_name() {
        let outcome = ConditionalFormatOutcome {
            spreadsheet_id: "sheet-1".to_string(),
            file_name: None,
            resolved_folder_id: None,
            verb: add_verb(),
            result: ConditionalFormatResult::Changed {
                summary: "add conditional format".to_string(),
            },
        };
        let lines = describe_lines(&outcome);
        assert!(lines[0].contains("'sheet-1'"), "{lines:?}");
    }

    #[test]
    fn describe_lines_renders_refused_not_a_spreadsheet() {
        let outcome = describe_outcome(
            add_verb(),
            ConditionalFormatResult::RefusedNotASpreadsheet {
                mime_type: "text/plain".to_string(),
            },
        );
        let lines = describe_lines(&outcome);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("not a Google Sheet"), "{lines:?}");
        assert!(lines[0].contains("text/plain"), "{lines:?}");
        assert!(lines[0].contains("add-conditional-format"), "{lines:?}");
    }

    #[test]
    fn describe_lines_renders_refused_shortcut() {
        let outcome = describe_outcome(add_verb(), ConditionalFormatResult::RefusedShortcut);
        let lines = describe_lines(&outcome);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("is a shortcut"), "{lines:?}");
    }

    #[test]
    fn describe_lines_renders_refused_no_visible_parents() {
        let outcome =
            describe_outcome(add_verb(), ConditionalFormatResult::RefusedNoVisibleParents);
        let lines = describe_lines(&outcome);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("no parent folder visible"), "{lines:?}");
        assert!(lines[0].contains("sheets-structure"), "{lines:?}");
    }

    #[test]
    fn describe_lines_renders_refused_sheet_not_found() {
        let outcome = describe_outcome(
            add_verb(),
            ConditionalFormatResult::RefusedSheetNotFound {
                title: "Missing".to_string(),
                available: vec!["Q1".to_string(), "Q2".to_string()],
            },
        );
        let lines = describe_lines(&outcome);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("no sheet titled 'Missing'"), "{lines:?}");
        assert!(lines[0].contains("'Q1', 'Q2'"), "{lines:?}");

        let outcome = describe_outcome(
            add_verb(),
            ConditionalFormatResult::RefusedSheetNotFound {
                title: "Missing".to_string(),
                available: Vec::new(),
            },
        );
        let lines = describe_lines(&outcome);
        assert!(lines[0].contains("Available: none"), "{lines:?}");
    }

    #[test]
    fn describe_lines_renders_refused_invalid_range() {
        let outcome = describe_outcome(
            add_verb(),
            ConditionalFormatResult::RefusedInvalidRange {
                detail: "bad range".to_string(),
            },
        );
        let lines = describe_lines(&outcome);
        assert_eq!(lines, vec!["Refused: bad range".to_string()]);
    }

    #[test]
    fn describe_lines_renders_index_out_of_bounds_for_add_with_full_range() {
        // `add-conditional-format` may append at `count`, so its valid
        // range is `0..=count` — max_valid == count.
        let outcome = describe_outcome(
            add_verb(),
            ConditionalFormatResult::RefusedIndexOutOfBounds {
                sheet: "Q1".to_string(),
                index: 5,
                count: 3,
            },
        );
        let lines = describe_lines(&outcome);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("valid indices 0-3"), "{lines:?}");
        assert!(lines[0].contains("index 5 is out of bounds"), "{lines:?}");
    }

    #[test]
    fn describe_lines_renders_index_out_of_bounds_for_update_and_delete_with_shrunk_range() {
        // `update`/`delete` may only act on an existing rule, so their
        // valid range is `0..count` — max_valid == count - 1.
        let update_verb = ConditionalFormatVerb::UpdateConditionalFormat {
            sheet: "Q1".to_string(),
            ranges: vec!["A1".to_string()],
            index: 5,
            rule: FormatRule::Boolean {
                condition: FormatCondition::CellEmpty,
                format: FormatEffect::default(),
            },
        };
        let outcome = describe_outcome(
            update_verb,
            ConditionalFormatResult::RefusedIndexOutOfBounds {
                sheet: "Q1".to_string(),
                index: 5,
                count: 3,
            },
        );
        let lines = describe_lines(&outcome);
        assert!(lines[0].contains("valid indices 0-2"), "{lines:?}");

        let delete_verb = ConditionalFormatVerb::DeleteConditionalFormat {
            sheet: "Q1".to_string(),
            index: 5,
        };
        let outcome = describe_outcome(
            delete_verb,
            ConditionalFormatResult::RefusedIndexOutOfBounds {
                sheet: "Q1".to_string(),
                index: 5,
                count: 3,
            },
        );
        let lines = describe_lines(&outcome);
        assert!(lines[0].contains("valid indices 0-2"), "{lines:?}");

        // The `count == 0` edge case: `saturating_sub` keeps max_valid at 0
        // rather than underflowing.
        let delete_verb = ConditionalFormatVerb::DeleteConditionalFormat {
            sheet: "Q1".to_string(),
            index: 0,
        };
        let outcome = describe_outcome(
            delete_verb,
            ConditionalFormatResult::RefusedIndexOutOfBounds {
                sheet: "Q1".to_string(),
                index: 0,
                count: 0,
            },
        );
        let lines = describe_lines(&outcome);
        assert!(lines[0].contains("valid indices 0-0"), "{lines:?}");
    }

    #[test]
    fn describe_lines_renders_blocked_with_and_without_a_deciding_rule() {
        let outcome = describe_outcome(
            add_verb(),
            ConditionalFormatResult::Blocked {
                decided_by: Some(DecidingRule::Folder {
                    folder_id: "folder-1".to_string(),
                    depth: 2,
                }),
            },
        );
        let lines = describe_lines(&outcome);
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains("Blocked: add-conditional-format"),
            "{lines:?}"
        );
        assert!(lines[0].contains("rule on folder folder-1"), "{lines:?}");
        assert!(lines[0].contains("(depth 2)"), "{lines:?}");

        let outcome = describe_outcome(
            add_verb(),
            ConditionalFormatResult::Blocked { decided_by: None },
        );
        let lines = describe_lines(&outcome);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("refused by default policy"), "{lines:?}");
        assert!(lines[0].contains("sheets-structure"), "{lines:?}");
    }

    #[test]
    fn describe_lines_renders_every_lease_refusal() {
        for result in [
            ConditionalFormatResult::RefusedNoLease,
            ConditionalFormatResult::RefusedLeaseExpired,
            ConditionalFormatResult::RefusedLeaseWrongFile,
            ConditionalFormatResult::RefusedLeaseStale,
        ] {
            let outcome = describe_outcome(add_verb(), result.clone());
            let lines = describe_lines(&outcome);
            assert_eq!(lines.len(), 1, "{result:?} -> {lines:?}");
            assert!(lines[0].starts_with("Refused:"), "{result:?} -> {lines:?}");
        }
    }

    #[test]
    fn describe_lines_renders_failed() {
        let outcome = describe_outcome(
            add_verb(),
            ConditionalFormatResult::Failed {
                detail: "boom".to_string(),
            },
        );
        let lines = describe_lines(&outcome);
        assert_eq!(lines, vec!["Failed: boom".to_string()]);
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
                                        "condition": {"type": "BLANK", "values": []},
                                        "format": {"backgroundColorStyle": {"rgbColor": {"red": 1.0, "green": 0.0, "blue": 0.0}}},
                                    },
                                },
                                {
                                    "ranges": [{"sheetId": 0, "startRowIndex": 1, "endRowIndex": 2}],
                                    "gradientRule": {
                                        "minpoint": {"colorStyle": {"rgbColor": {"red": 1.0, "green": 1.0, "blue": 1.0}}, "type": "MIN"},
                                        "maxpoint": {"colorStyle": {"rgbColor": {"red": 0.0, "green": 1.0, "blue": 0.0}}, "type": "MAX"},
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
                    summary.contains("currently: boolean rule (blank"),
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

    #[tokio::test]
    async fn add_refuses_an_empty_ranges_list_before_any_network_call() {
        // No mounts at all: an empty `--range` list is refused before the
        // metadata fetch, so nothing should reach the mock server.
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        let rules: Vec<FolderPermissionRule> = Vec::new();
        let opts = ConditionalFormatOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: ConditionalFormatVerb::AddConditionalFormat {
                sheet: "Q1".to_string(),
                ranges: Vec::new(),
                index: None,
                rule: FormatRule::Boolean {
                    condition: FormatCondition::CellEmpty,
                    format: FormatEffect::default(),
                },
            },
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = conditional_format(&drive, &sheets, &opts, &rules).await;
        match outcome.result {
            ConditionalFormatResult::RefusedInvalidRange { detail } => {
                assert!(detail.contains("needs at least one --range"), "{detail}");
            }
            other => panic!("expected RefusedInvalidRange, got {other:?}"),
        }
        let requests = server.received_requests().await.unwrap();
        assert!(requests.is_empty(), "{requests:?}");
    }
}
