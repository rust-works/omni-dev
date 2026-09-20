//! Data validation via `spreadsheets.batchUpdate` (issue #1643,
//! [ADR-0077](../../../docs/adrs/adr-0077.md)).
//!
//! Two verbs, gated by [`DriveOperation::SheetsStructure`] — neither
//! destroys data. `clear-data-validation` removes a constraint, not a
//! value, the same way `format.rs::UnmergeCells` removes a merge without
//! touching a cell's content.
//!
//! Shape mirrors `format.rs` exactly, down to reusing its target
//! resolution: a range, composed from `--sheet`/`--range` and converted to
//! a numeric [`GridRange`] via `grid_range::parse_grid_range`.
//!
//! **A curated condition surface, not full `BooleanCondition` coverage.**
//! Sheets models a couple dozen condition types. Tranche 1 (#1643) shipped
//! the four with the broadest use: `--one-of-list` (dropdown),
//! `--number-between`, `--checkbox`, `--custom-formula`. Tranche 2 (#1792)
//! adds every remaining type addressable with a flat flag: `ONE_OF_RANGE`,
//! the numeric comparators and `NUMBER_NOT_BETWEEN`, the `TEXT_*`
//! contains/starts/ends/eq family, the `DATE_*` after/before/on/between
//! family (with relative-date support for the three single-value forms),
//! and `BLANK`/`NOT_BLANK`. Still excluded, a documented cut rather than a
//! silent gap: `TEXT_IS_EMAIL`, `TEXT_IS_URL`, `DATE_ON_OR_BEFORE`,
//! `DATE_ON_OR_AFTER`, `DATE_NOT_BETWEEN`, `DATE_IS_VALID`, and every
//! condition type meaningful only inside a conditional-format rule —
//! `docs/drive.md` names the boundary, matching this issue's general stance
//! on `CellFormat`.

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
    BatchUpdateRequestItem, BooleanCondition, ConditionValue, DataValidationRule, GridRange,
    SetDataValidationRequest,
};
use crate::drive::types::SheetTargetRefusal;
use crate::drive::write_gate::{self, DecidingRule, DriveOperation, FolderPermissionRule};
use crate::request_log::{self, DriveMutationOutcome};

/// One of Sheets' six `RelativeDate` values.
///
/// Usable wherever a date condition takes a single value
/// (`DATE_AFTER`/`DATE_BEFORE`/`DATE_EQ`); `DATE_BETWEEN` requires two
/// absolute dates and never accepts one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelativeDate {
    /// The 365 days up to and including today.
    PastYear,
    /// The 30 days up to and including today.
    PastMonth,
    /// The 7 days up to and including today.
    PastWeek,
    /// The day before today.
    Yesterday,
    /// Today.
    Today,
    /// The day after today.
    Tomorrow,
}

impl RelativeDate {
    const fn as_sheets_str(self) -> &'static str {
        match self {
            Self::PastYear => "PAST_YEAR",
            Self::PastMonth => "PAST_MONTH",
            Self::PastWeek => "PAST_WEEK",
            Self::Yesterday => "YESTERDAY",
            Self::Today => "TODAY",
            Self::Tomorrow => "TOMORROW",
        }
    }

    /// Matches case- and separator-insensitively (`past-week`, `past_week`,
    /// `Past Week` all match) so the CLI value doesn't force one style.
    fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().replace(['-', '_', ' '], "").as_str() {
            "pastyear" => Some(Self::PastYear),
            "pastmonth" => Some(Self::PastMonth),
            "pastweek" => Some(Self::PastWeek),
            "yesterday" => Some(Self::Yesterday),
            "today" => Some(Self::Today),
            "tomorrow" => Some(Self::Tomorrow),
            _ => None,
        }
    }
}

/// A date condition's operand.
///
/// Either a literal date string (passed through untouched, the same
/// trust-the-caller stance as every other numeric/text value in this file —
/// Sheets parses it at evaluation time) or one of the six [`RelativeDate`]
/// keywords.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DateValue {
    /// A literal date string, untouched.
    Absolute(String),
    /// One of Sheets' relative-date keywords.
    Relative(RelativeDate),
}

impl DateValue {
    /// Never fails: anything that isn't a recognized relative keyword is
    /// treated as a literal date string.
    pub fn parse(raw: String) -> Self {
        match RelativeDate::parse(&raw) {
            Some(rd) => Self::Relative(rd),
            None => Self::Absolute(raw),
        }
    }

    fn is_blank(&self) -> bool {
        matches!(self, Self::Absolute(s) if s.trim().is_empty())
    }

    fn into_condition_value(self) -> ConditionValue {
        match self {
            Self::Absolute(s) => ConditionValue {
                user_entered_value: Some(s),
                relative_date: None,
            },
            Self::Relative(rd) => ConditionValue {
                user_entered_value: None,
                relative_date: Some(rd.as_sheets_str().to_string()),
            },
        }
    }
}

/// The condition shapes `set-data-validation` builds.
///
/// Each variant carries exactly what its condition needs; there is no "raw
/// condition type + values" escape hatch, following the same
/// no-passthrough discipline as `format.rs::CellFormatFlags`.
#[derive(Debug, Clone, PartialEq)]
pub enum Condition {
    /// A dropdown restricted to these exact values.
    OneOfList(Vec<String>),
    /// A dropdown sourced from a range of cells.
    OneOfRange(String),
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
    /// The cell must be empty.
    Blank,
    /// The cell must not be empty.
    NotBlank,
    /// `TRUE`/`FALSE` only.
    Checkbox,
    /// A custom formula that must evaluate truthy.
    CustomFormula(String),
}

impl Condition {
    fn into_boolean_condition(self) -> BooleanCondition {
        let value = |s: String| ConditionValue {
            user_entered_value: Some(s),
            relative_date: None,
        };
        let condition = |condition_type: &str, values: Vec<ConditionValue>| BooleanCondition {
            condition_type: condition_type.to_string(),
            values,
        };
        match self {
            Self::OneOfList(items) => {
                condition("ONE_OF_LIST", items.into_iter().map(value).collect())
            }
            Self::OneOfRange(range) => condition("ONE_OF_RANGE", vec![value(range)]),
            Self::NumberBetween(min, max) => condition(
                "NUMBER_BETWEEN",
                vec![value(min.to_string()), value(max.to_string())],
            ),
            Self::NumberNotBetween(min, max) => condition(
                "NUMBER_NOT_BETWEEN",
                vec![value(min.to_string()), value(max.to_string())],
            ),
            Self::NumberGreater(n) => condition("NUMBER_GREATER", vec![value(n.to_string())]),
            Self::NumberGreaterEq(n) => {
                condition("NUMBER_GREATER_THAN_EQ", vec![value(n.to_string())])
            }
            Self::NumberLess(n) => condition("NUMBER_LESS", vec![value(n.to_string())]),
            Self::NumberLessEq(n) => condition("NUMBER_LESS_THAN_EQ", vec![value(n.to_string())]),
            Self::NumberEq(n) => condition("NUMBER_EQ", vec![value(n.to_string())]),
            Self::NumberNotEq(n) => condition("NUMBER_NOT_EQ", vec![value(n.to_string())]),
            Self::TextContains(text) => condition("TEXT_CONTAINS", vec![value(text)]),
            Self::TextNotContains(text) => condition("TEXT_NOT_CONTAINS", vec![value(text)]),
            Self::TextStartsWith(text) => condition("TEXT_STARTS_WITH", vec![value(text)]),
            Self::TextEndsWith(text) => condition("TEXT_ENDS_WITH", vec![value(text)]),
            Self::TextEq(text) => condition("TEXT_EQ", vec![value(text)]),
            Self::DateAfter(date) => condition("DATE_AFTER", vec![date.into_condition_value()]),
            Self::DateBefore(date) => condition("DATE_BEFORE", vec![date.into_condition_value()]),
            Self::DateOn(date) => condition("DATE_EQ", vec![date.into_condition_value()]),
            Self::DateBetween(start, end) => {
                condition("DATE_BETWEEN", vec![value(start), value(end)])
            }
            Self::Blank => condition("BLANK", Vec::new()),
            Self::NotBlank => condition("NOT_BLANK", Vec::new()),
            Self::Checkbox => condition("BOOLEAN", Vec::new()),
            Self::CustomFormula(formula) => condition("CUSTOM_FORMULA", vec![value(formula)]),
        }
    }

    /// The `validation_type` the request log records.
    const fn log_type(&self) -> &'static str {
        match self {
            Self::OneOfList(_) => "ONE_OF_LIST",
            Self::OneOfRange(_) => "ONE_OF_RANGE",
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
            Self::Blank => "BLANK",
            Self::NotBlank => "NOT_BLANK",
            Self::Checkbox => "BOOLEAN",
            Self::CustomFormula(_) => "CUSTOM_FORMULA",
        }
    }
}

/// Which mutation to perform.
#[derive(Debug, Clone, PartialEq)]
pub enum ValidationVerb {
    /// Set a validation rule on a range.
    SetDataValidation {
        /// A sheet title, supplying a prefix for a bare `range`.
        sheet: Option<String>,
        /// An explicit A1 range, which may carry its own `Sheet!` prefix.
        range: Option<String>,
        /// The condition to enforce.
        condition: Condition,
        /// Tooltip shown on the cell.
        input_message: Option<String>,
        /// `false` (the default) rejects an invalid entry outright; `true`
        /// only warns.
        show_warning: bool,
    },
    /// Remove a range's validation rule.
    ClearDataValidation {
        /// A sheet title, supplying a prefix for a bare `range`.
        sheet: Option<String>,
        /// An explicit A1 range, which may carry its own `Sheet!` prefix.
        range: Option<String>,
    },
}

impl ValidationVerb {
    const fn log_operation(&self) -> &'static str {
        match self {
            Self::SetDataValidation { .. } => "sheets-set-data-validation",
            Self::ClearDataValidation { .. } => "sheets-clear-data-validation",
        }
    }

    const fn label(&self) -> &'static str {
        match self {
            Self::SetDataValidation { .. } => "set-data-validation",
            Self::ClearDataValidation { .. } => "clear-data-validation",
        }
    }

    fn sheet_and_range(&self) -> (Option<&str>, Option<&str>) {
        match self {
            Self::SetDataValidation { sheet, range, .. }
            | Self::ClearDataValidation { sheet, range } => (sheet.as_deref(), range.as_deref()),
        }
    }
}

/// Per-call options.
#[derive(Debug, Clone)]
pub struct ValidationOptions {
    /// Spreadsheet id.
    pub spreadsheet_id: String,
    /// Which mutation to perform.
    pub verb: ValidationVerb,
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
pub enum ValidationResult {
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
    /// The `--sheet`/`--range` pair, or the condition's own arguments, was
    /// invalid.
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
        /// Same summary as [`Self::WouldChange`].
        summary: String,
    },
    /// An API or validation error.
    Failed {
        /// A human-readable summary of what failed.
        detail: String,
    },
}

impl FromLeaseRefusal for ValidationResult {
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

impl ValidationResult {
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
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ValidationOutcome {
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
    pub verb: ValidationVerb,
    /// What happened.
    pub result: ValidationResult,
}

impl JsonlSerialize for ValidationOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> anyhow::Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Runs one data-validation mutation, logging every attempt that isn't a
/// dry run.
pub async fn validation(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &ValidationOptions,
    rules: &[FolderPermissionRule],
) -> ValidationOutcome {
    let started = Instant::now();
    let outcome = validation_inner(drive, sheets, opts, rules).await;
    if !opts.dry_run {
        record_attempt(&outcome, opts, started.elapsed());
    }
    outcome
}

async fn validation_inner(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &ValidationOptions,
    rules: &[FolderPermissionRule],
) -> ValidationOutcome {
    let bare = |result| ValidationOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: None,
        resolved_folder_id: None,
        verb: opts.verb.clone(),
        result,
    };

    let (sheet, range) = opts.verb.sheet_and_range();
    let composed_range = match a1::compose(sheet, range) {
        Ok(composed) => composed,
        Err(err) => {
            return bare(ValidationResult::RefusedInvalidRange {
                detail: err.to_string(),
            })
        }
    };

    if let ValidationVerb::SetDataValidation {
        input_message: _,
        condition,
        ..
    } = &opts.verb
    {
        if let Err(detail) = validate_condition(condition) {
            return bare(ValidationResult::RefusedInvalidRange { detail });
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
            return bare(ValidationResult::Failed { detail })
        }
        target_gate::TargetGateOutcome::Refused { target, refusal } => {
            let result = match refusal {
                SheetTargetRefusal::Shortcut => ValidationResult::RefusedShortcut,
                SheetTargetRefusal::NotASpreadsheet { mime_type } => {
                    ValidationResult::RefusedNotASpreadsheet { mime_type }
                }
                SheetTargetRefusal::NoVisibleParents => ValidationResult::RefusedNoVisibleParents,
            };
            return ValidationOutcome {
                spreadsheet_id: opts.spreadsheet_id.clone(),
                file_name: Some(target.name),
                resolved_folder_id: None,
                verb: opts.verb.clone(),
                result,
            };
        }
        target_gate::TargetGateOutcome::GateFetchFailed { target, detail } => {
            return ValidationOutcome {
                spreadsheet_id: opts.spreadsheet_id.clone(),
                file_name: Some(target.name),
                resolved_folder_id: None,
                verb: opts.verb.clone(),
                result: ValidationResult::Failed { detail },
            };
        }
        target_gate::TargetGateOutcome::Gated {
            target,
            decision,
            resolved_folder_id,
            requires_lease,
        } => (target, decision, resolved_folder_id, requires_lease),
    };

    let gated = |result| ValidationOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: Some(target.name.clone()),
        resolved_folder_id: resolved_folder_id.clone(),
        verb: opts.verb.clone(),
        result,
    };

    if decision.verdict == write_gate::Verdict::Deny {
        return gated(ValidationResult::Blocked {
            decided_by: decision.decided_by,
        });
    }

    let api = SheetsApi::new(sheets);
    let workbook = match api.get_spreadsheet(&opts.spreadsheet_id).await {
        Ok(workbook) => workbook,
        Err(err) => {
            return gated(ValidationResult::Failed {
                detail: format!("{err:#}"),
            })
        }
    };

    let grid = match grid_range::resolve_grid_range(
        &workbook,
        &composed_range,
        |detail| ValidationResult::RefusedInvalidRange { detail },
        |title, available| ValidationResult::RefusedSheetNotFound { title, available },
    ) {
        Ok((_, grid)) => grid,
        Err(result) => return gated(result),
    };

    let summary = describe_effect(&opts.verb);

    if opts.dry_run {
        return gated(ValidationResult::WouldChange { summary });
    }

    // Built before the gate, not after — unlike its siblings this is
    // infallible today (`ValidationVerb::into_boolean_condition` is an
    // exhaustive, panic-free match), but the gate must still be the *last*
    // fallible step before the mutating call (see `gate_leased_write`'s doc
    // comment and `structure.rs`'s identical comment) so a future fallible
    // verb here cannot fsync a `pending` audit record for a write that never
    // happens (#1688).
    let request = build_request(&opts.verb, grid);

    // The lease check (ADR-0080 §9) sits here: after the permission gate
    // and the `--dry-run` branch, before the mutating call — see
    // `content_edit.rs::edit_inner`'s doc comment for the full reasoning,
    // shared verbatim by every leased engine. A fresh `files.get` immediately
    // before `batchUpdate`, not a reuse of the metadata `target_gate::resolve`
    // fetched before the (potentially slow) ancestor-chain walk and workbook
    // fetch above.
    let files_api = FilesApi::new(drive);
    let leased = LeasedWrite {
        log_prefix: "drive sheets validation",
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
        Ok(_response) => ValidationResult::Changed { summary },
        Err(err) => ValidationResult::Failed {
            detail: format!("{err:#}"),
        },
    };
    drop(lease_grant);
    gated(result)
}

fn validate_condition(condition: &Condition) -> Result<(), String> {
    // Exhaustive over `Condition`: a new variant forces a new arm here at
    // compile time, rather than silently falling through unvalidated.
    match condition {
        Condition::OneOfList(items) => reject_empty_list(items, "--one-of-list"),
        Condition::OneOfRange(range) => a1::compose(None, Some(range))
            .map(|_| ())
            .map_err(|err| format!("--one-of-range: {err:#}")),
        Condition::NumberBetween(min, max) => reject_reversed_range(*min, *max, "--number-between"),
        Condition::NumberNotBetween(min, max) => {
            reject_reversed_range(*min, *max, "--number-not-between")
        }
        Condition::NumberGreater(n) => reject_nan(*n, "--number-greater"),
        Condition::NumberGreaterEq(n) => reject_nan(*n, "--number-greater-eq"),
        Condition::NumberLess(n) => reject_nan(*n, "--number-less"),
        Condition::NumberLessEq(n) => reject_nan(*n, "--number-less-eq"),
        Condition::NumberEq(n) => reject_nan(*n, "--number-eq"),
        Condition::NumberNotEq(n) => reject_nan(*n, "--number-not-eq"),
        Condition::TextContains(text) => reject_blank(text, "--text-contains"),
        Condition::TextNotContains(text) => reject_blank(text, "--text-not-contains"),
        Condition::TextStartsWith(text) => reject_blank(text, "--text-starts-with"),
        Condition::TextEndsWith(text) => reject_blank(text, "--text-ends-with"),
        Condition::TextEq(text) => reject_blank(text, "--text-eq"),
        Condition::DateAfter(date) => reject_blank_date(date, "--date-after"),
        Condition::DateBefore(date) => reject_blank_date(date, "--date-before"),
        Condition::DateOn(date) => reject_blank_date(date, "--date-on"),
        Condition::DateBetween(start, end) => {
            if start.trim().is_empty() || end.trim().is_empty() {
                Err("--date-between's values must not be empty".to_string())
            } else {
                Ok(())
            }
        }
        Condition::Blank | Condition::NotBlank | Condition::Checkbox => Ok(()),
        Condition::CustomFormula(formula) => reject_blank(formula, "--custom-formula"),
    }
}

fn reject_empty_list(items: &[String], flag: &str) -> Result<(), String> {
    if items.is_empty() {
        Err(format!("{flag} needs at least one value"))
    } else {
        Ok(())
    }
}

/// `NaN > x` and `x > NaN` are both `false`, so a NaN bound must be checked
/// explicitly or it silently reaches the API.
fn reject_reversed_range(min: f64, max: f64, flag: &str) -> Result<(), String> {
    if min.is_nan() || max.is_nan() || min > max {
        Err(format!(
            "{flag}'s first value ({min}) must not exceed the second ({max})"
        ))
    } else {
        Ok(())
    }
}

fn reject_nan(n: f64, flag: &str) -> Result<(), String> {
    if n.is_nan() {
        Err(format!("{flag} must not be NaN"))
    } else {
        Ok(())
    }
}

fn reject_blank(text: &str, flag: &str) -> Result<(), String> {
    if text.trim().is_empty() {
        Err(format!("{flag} must not be empty"))
    } else {
        Ok(())
    }
}

fn reject_blank_date(date: &DateValue, flag: &str) -> Result<(), String> {
    if date.is_blank() {
        Err(format!("{flag} must not be empty"))
    } else {
        Ok(())
    }
}

fn describe_effect(verb: &ValidationVerb) -> String {
    match verb {
        ValidationVerb::SetDataValidation {
            condition,
            show_warning,
            ..
        } => {
            let strictness = if *show_warning {
                "warn only"
            } else {
                "reject invalid entries"
            };
            format!(
                "set data validation ({}, {strictness})",
                condition.log_type().to_ascii_lowercase().replace('_', " ")
            )
        }
        ValidationVerb::ClearDataValidation { .. } => "clear data validation".to_string(),
    }
}

fn build_request(verb: &ValidationVerb, grid: GridRange) -> BatchUpdateRequestItem {
    let rule = match verb {
        ValidationVerb::SetDataValidation {
            condition,
            input_message,
            show_warning,
            ..
        } => Some(DataValidationRule {
            condition: condition.clone().into_boolean_condition(),
            input_message: input_message.clone(),
            strict: Some(!show_warning),
        }),
        ValidationVerb::ClearDataValidation { .. } => None,
    };
    BatchUpdateRequestItem::SetDataValidation(SetDataValidationRequest { range: grid, rule })
}

fn record_attempt(outcome: &ValidationOutcome, opts: &ValidationOptions, duration: Duration) {
    let error = match &outcome.result {
        ValidationResult::Failed { detail } => Some(detail.clone()),
        _ => None,
    };
    let decided_by = match &outcome.result {
        ValidationResult::Blocked { decided_by } => decided_by.as_ref(),
        _ => None,
    };
    let decided_by = write_gate::decided_by_log_fields(decided_by);
    let validation_type = match &opts.verb {
        ValidationVerb::SetDataValidation { condition, .. } => {
            Some(condition.log_type().to_string())
        }
        ValidationVerb::ClearDataValidation { .. } => Some("cleared".to_string()),
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
        validation_type,
        error,
        duration,
        ..Default::default()
    });
}

/// Renders an outcome as human-readable text.
#[must_use]
pub fn describe(outcome: &ValidationOutcome) -> String {
    describe_lines(outcome).join("\n")
}

/// Renders an outcome as its individual lines, none of which contains a
/// newline.
#[must_use]
pub fn describe_lines(outcome: &ValidationOutcome) -> Vec<String> {
    let verb = &outcome.verb;
    let book = outcome.file_name.as_deref().map_or_else(
        || format!("'{}'", outcome.spreadsheet_id),
        |n| format!("'{n}'"),
    );
    match &outcome.result {
        ValidationResult::WouldChange { summary } => vec![format!("Would {summary} in {book}")],
        ValidationResult::RefusedNotASpreadsheet { mime_type } => vec![format!(
            "Refused: {book} is not a Google Sheet (mimeType: {mime_type}); \
             `drive sheets {}` only works on spreadsheets",
            verb.label()
        )],
        ValidationResult::RefusedShortcut => vec![format!(
            "Refused: {book} is a shortcut; `drive sheets {}` doesn't follow shortcuts",
            verb.label()
        )],
        ValidationResult::RefusedNoVisibleParents => vec![format!(
            "Refused: {book} has no parent folder visible to this account, so no folder \
             rule can apply to it. Grant it by id instead: add {{\"file_id\": \"<spreadsheet \
             id>\", \"allow\": [\"sheets-structure\"]}} to write_permissions.rules."
        )],
        ValidationResult::RefusedSheetNotFound { title, available } => {
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
        ValidationResult::RefusedInvalidRange { detail } => vec![format!("Refused: {detail}")],
        ValidationResult::Blocked { decided_by } => vec![match decided_by {
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
        ValidationResult::RefusedNoLease => LeaseGateRefusal::NoLease
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        ValidationResult::RefusedLeaseExpired => LeaseGateRefusal::Expired
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        ValidationResult::RefusedLeaseWrongFile => LeaseGateRefusal::WrongFile
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        ValidationResult::RefusedLeaseStale => LeaseGateRefusal::Stale
            .describe_line(&outcome.spreadsheet_id, &book)
            .into_iter()
            .collect(),
        ValidationResult::Changed { summary } => {
            vec![format!("Applied: {summary} in {book}")]
        }
        ValidationResult::Failed { detail } => vec![format!("Failed: {detail}")],
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

    #[test]
    fn one_of_list_builds_a_value_per_item() {
        let condition = Condition::OneOfList(vec!["a".to_string(), "b".to_string()]);
        let built = condition.into_boolean_condition();
        assert_eq!(built.condition_type, "ONE_OF_LIST");
        assert_eq!(built.values.len(), 2);
        assert_eq!(built.values[0].user_entered_value, Some("a".to_string()));
    }

    #[test]
    fn one_of_range_builds_a_single_value() {
        let condition = Condition::OneOfRange("Sheet2!A1:A10".to_string());
        let built = condition.into_boolean_condition();
        assert_eq!(built.condition_type, "ONE_OF_RANGE");
        assert_eq!(
            built.values[0].user_entered_value,
            Some("Sheet2!A1:A10".to_string())
        );
    }

    #[test]
    fn number_between_builds_two_values() {
        let condition = Condition::NumberBetween(1.0, 10.0);
        let built = condition.into_boolean_condition();
        assert_eq!(built.condition_type, "NUMBER_BETWEEN");
        assert_eq!(built.values[0].user_entered_value, Some("1".to_string()));
        assert_eq!(built.values[1].user_entered_value, Some("10".to_string()));
    }

    #[test]
    fn number_not_between_builds_two_values() {
        let condition = Condition::NumberNotBetween(1.0, 10.0);
        let built = condition.into_boolean_condition();
        assert_eq!(built.condition_type, "NUMBER_NOT_BETWEEN");
        assert_eq!(built.values.len(), 2);
    }

    #[test]
    fn numeric_comparators_build_a_single_value_each() {
        let cases = [
            (Condition::NumberGreater(1.0), "NUMBER_GREATER"),
            (Condition::NumberGreaterEq(1.0), "NUMBER_GREATER_THAN_EQ"),
            (Condition::NumberLess(1.0), "NUMBER_LESS"),
            (Condition::NumberLessEq(1.0), "NUMBER_LESS_THAN_EQ"),
            (Condition::NumberEq(1.0), "NUMBER_EQ"),
            (Condition::NumberNotEq(1.0), "NUMBER_NOT_EQ"),
        ];
        for (condition, expected_type) in cases {
            let built = condition.into_boolean_condition();
            assert_eq!(built.condition_type, expected_type);
            assert_eq!(
                built.values,
                vec![ConditionValue {
                    user_entered_value: Some("1".to_string()),
                    relative_date: None,
                }]
            );
        }
    }

    #[test]
    fn text_conditions_build_a_single_value_each() {
        let cases = [
            (Condition::TextContains("x".to_string()), "TEXT_CONTAINS"),
            (
                Condition::TextNotContains("x".to_string()),
                "TEXT_NOT_CONTAINS",
            ),
            (
                Condition::TextStartsWith("x".to_string()),
                "TEXT_STARTS_WITH",
            ),
            (Condition::TextEndsWith("x".to_string()), "TEXT_ENDS_WITH"),
            (Condition::TextEq("x".to_string()), "TEXT_EQ"),
        ];
        for (condition, expected_type) in cases {
            let built = condition.into_boolean_condition();
            assert_eq!(built.condition_type, expected_type);
            assert_eq!(built.values[0].user_entered_value, Some("x".to_string()));
        }
    }

    #[test]
    fn date_conditions_build_an_absolute_value() {
        let cases = [
            (
                Condition::DateAfter(DateValue::Absolute("2024-01-01".to_string())),
                "DATE_AFTER",
            ),
            (
                Condition::DateBefore(DateValue::Absolute("2024-01-01".to_string())),
                "DATE_BEFORE",
            ),
            (
                Condition::DateOn(DateValue::Absolute("2024-01-01".to_string())),
                "DATE_EQ",
            ),
        ];
        for (condition, expected_type) in cases {
            let built = condition.into_boolean_condition();
            assert_eq!(built.condition_type, expected_type);
            assert_eq!(
                built.values[0].user_entered_value,
                Some("2024-01-01".to_string())
            );
            assert_eq!(built.values[0].relative_date, None);
        }
    }

    #[test]
    fn date_condition_builds_a_relative_value() {
        let condition = Condition::DateAfter(DateValue::parse("today".to_string()));
        let built = condition.into_boolean_condition();
        assert_eq!(built.condition_type, "DATE_AFTER");
        assert_eq!(built.values[0].user_entered_value, None);
        assert_eq!(built.values[0].relative_date, Some("TODAY".to_string()));
    }

    #[test]
    fn date_value_parse_is_case_and_separator_insensitive() {
        for input in ["past-week", "PAST_WEEK", "Past Week"] {
            assert!(matches!(
                DateValue::parse(input.to_string()),
                DateValue::Relative(RelativeDate::PastWeek)
            ));
        }
        assert!(matches!(
            DateValue::parse("2024-01-01".to_string()),
            DateValue::Absolute(s) if s == "2024-01-01"
        ));
    }

    #[test]
    fn date_between_builds_two_absolute_values() {
        let condition = Condition::DateBetween("2024-01-01".to_string(), "2024-12-31".to_string());
        let built = condition.into_boolean_condition();
        assert_eq!(built.condition_type, "DATE_BETWEEN");
        assert_eq!(built.values.len(), 2);
    }

    #[test]
    fn blank_and_not_blank_have_no_values() {
        assert_eq!(
            Condition::Blank.into_boolean_condition().condition_type,
            "BLANK"
        );
        assert!(Condition::Blank.into_boolean_condition().values.is_empty());
        assert_eq!(
            Condition::NotBlank.into_boolean_condition().condition_type,
            "NOT_BLANK"
        );
        assert!(Condition::NotBlank
            .into_boolean_condition()
            .values
            .is_empty());
    }

    #[test]
    fn checkbox_has_no_values() {
        let built = Condition::Checkbox.into_boolean_condition();
        assert_eq!(built.condition_type, "BOOLEAN");
        assert!(built.values.is_empty());
    }

    #[test]
    fn custom_formula_builds_a_single_value() {
        let condition = Condition::CustomFormula("=A1>0".to_string());
        let built = condition.into_boolean_condition();
        assert_eq!(built.condition_type, "CUSTOM_FORMULA");
        assert_eq!(built.values.len(), 1);
        assert_eq!(
            built.values[0].user_entered_value,
            Some("=A1>0".to_string())
        );
    }

    #[test]
    fn log_type_covers_every_condition() {
        let types = [
            Condition::OneOfList(vec!["a".to_string()]).log_type(),
            Condition::OneOfRange("A1:A10".to_string()).log_type(),
            Condition::NumberBetween(1.0, 2.0).log_type(),
            Condition::NumberNotBetween(1.0, 2.0).log_type(),
            Condition::NumberGreater(1.0).log_type(),
            Condition::NumberGreaterEq(1.0).log_type(),
            Condition::NumberLess(1.0).log_type(),
            Condition::NumberLessEq(1.0).log_type(),
            Condition::NumberEq(1.0).log_type(),
            Condition::NumberNotEq(1.0).log_type(),
            Condition::TextContains("x".to_string()).log_type(),
            Condition::TextNotContains("x".to_string()).log_type(),
            Condition::TextStartsWith("x".to_string()).log_type(),
            Condition::TextEndsWith("x".to_string()).log_type(),
            Condition::TextEq("x".to_string()).log_type(),
            Condition::DateAfter(DateValue::Absolute("d".to_string())).log_type(),
            Condition::DateBefore(DateValue::Absolute("d".to_string())).log_type(),
            Condition::DateOn(DateValue::Absolute("d".to_string())).log_type(),
            Condition::DateBetween("a".to_string(), "b".to_string()).log_type(),
            Condition::Blank.log_type(),
            Condition::NotBlank.log_type(),
            Condition::Checkbox.log_type(),
            Condition::CustomFormula("=TRUE".to_string()).log_type(),
        ];
        let unique: HashSet<&&str> = types.iter().collect();
        assert_eq!(unique.len(), types.len());
    }

    fn set_verb(condition: Condition, show_warning: bool) -> ValidationVerb {
        ValidationVerb::SetDataValidation {
            sheet: Some("Q1".to_string()),
            range: Some("A1:A10".to_string()),
            condition,
            input_message: None,
            show_warning,
        }
    }

    fn clear_verb() -> ValidationVerb {
        ValidationVerb::ClearDataValidation {
            sheet: Some("Q1".to_string()),
            range: Some("A1:A10".to_string()),
        }
    }

    #[test]
    fn log_operation_and_label_cover_every_verb() {
        let set = set_verb(Condition::Checkbox, false);
        let clear = clear_verb();
        assert_eq!(set.log_operation(), "sheets-set-data-validation");
        assert_eq!(clear.log_operation(), "sheets-clear-data-validation");
        assert_eq!(set.label(), "set-data-validation");
        assert_eq!(clear.label(), "clear-data-validation");
    }

    #[test]
    fn describe_effect_covers_every_condition_and_strictness() {
        let reject = describe_effect(&set_verb(
            Condition::OneOfList(vec!["a".to_string()]),
            false,
        ));
        assert!(reject.contains("reject invalid entries"), "{reject}");
        assert!(reject.contains("one of list"), "{reject}");

        let warn = describe_effect(&set_verb(Condition::NumberBetween(1.0, 10.0), true));
        assert!(warn.contains("warn only"), "{warn}");
        assert!(warn.contains("number between"), "{warn}");

        let checkbox = describe_effect(&set_verb(Condition::Checkbox, false));
        assert!(checkbox.contains("boolean"), "{checkbox}");

        let formula = describe_effect(&set_verb(
            Condition::CustomFormula("=A1>0".to_string()),
            false,
        ));
        assert!(formula.contains("custom formula"), "{formula}");

        assert_eq!(describe_effect(&clear_verb()), "clear data validation");
    }

    #[test]
    fn validate_condition_accepts_a_valid_number_between() {
        validate_condition(&Condition::NumberBetween(1.0, 10.0)).unwrap();
    }

    #[test]
    fn validate_condition_accepts_a_non_empty_custom_formula() {
        validate_condition(&Condition::CustomFormula("=A1>0".to_string())).unwrap();
    }

    #[test]
    fn validate_condition_rejects_an_empty_one_of_list() {
        let err = validate_condition(&Condition::OneOfList(Vec::new())).unwrap_err();
        assert!(err.contains("at least one value"), "{err}");
    }

    #[test]
    fn validate_condition_rejects_a_reversed_number_between() {
        let err = validate_condition(&Condition::NumberBetween(10.0, 1.0)).unwrap_err();
        assert!(err.contains("must not exceed"), "{err}");
    }

    #[test]
    fn validate_condition_rejects_a_nan_bound() {
        // `NaN > x` and `x > NaN` are both `false`, so a plain `min > max`
        // guard lets this through uncaught.
        let err = validate_condition(&Condition::NumberBetween(f64::NAN, 5.0)).unwrap_err();
        assert!(err.contains("must not exceed"), "{err}");
        let err = validate_condition(&Condition::NumberBetween(1.0, f64::NAN)).unwrap_err();
        assert!(err.contains("must not exceed"), "{err}");
    }

    #[test]
    fn validate_condition_rejects_a_blank_custom_formula() {
        let err = validate_condition(&Condition::CustomFormula("   ".to_string())).unwrap_err();
        assert!(err.contains("must not be empty"), "{err}");
    }

    #[test]
    fn validate_condition_accepts_a_valid_one_of_range() {
        validate_condition(&Condition::OneOfRange("Sheet2!A1:A10".to_string())).unwrap();
    }

    #[test]
    fn validate_condition_rejects_a_blank_one_of_range() {
        validate_condition(&Condition::OneOfRange(String::new())).unwrap_err();
    }

    #[test]
    fn validate_condition_accepts_a_valid_number_not_between() {
        validate_condition(&Condition::NumberNotBetween(1.0, 10.0)).unwrap();
    }

    #[test]
    fn validate_condition_rejects_a_reversed_number_not_between() {
        let err = validate_condition(&Condition::NumberNotBetween(10.0, 1.0)).unwrap_err();
        assert!(err.contains("must not exceed"), "{err}");
    }

    #[test]
    fn validate_condition_rejects_a_nan_comparator() {
        let err = validate_condition(&Condition::NumberGreater(f64::NAN)).unwrap_err();
        assert!(err.contains("must not be NaN"), "{err}");
    }

    #[test]
    fn validate_condition_accepts_a_numeric_comparator() {
        validate_condition(&Condition::NumberEq(1.0)).unwrap();
    }

    #[test]
    fn validate_condition_rejects_blank_text() {
        let err = validate_condition(&Condition::TextContains("  ".to_string())).unwrap_err();
        assert!(err.contains("must not be empty"), "{err}");
    }

    #[test]
    fn validate_condition_accepts_non_blank_text() {
        validate_condition(&Condition::TextEq("x".to_string())).unwrap();
    }

    #[test]
    fn validate_condition_rejects_a_blank_absolute_date() {
        let err = validate_condition(&Condition::DateAfter(DateValue::Absolute("  ".to_string())))
            .unwrap_err();
        assert!(err.contains("must not be empty"), "{err}");
    }

    #[test]
    fn validate_condition_accepts_a_relative_date() {
        validate_condition(&Condition::DateAfter(DateValue::parse("today".to_string()))).unwrap();
    }

    #[test]
    fn validate_condition_accepts_a_valid_date_between() {
        validate_condition(&Condition::DateBetween(
            "2024-01-01".to_string(),
            "2024-12-31".to_string(),
        ))
        .unwrap();
    }

    #[test]
    fn validate_condition_rejects_a_blank_date_between_bound() {
        let err = validate_condition(&Condition::DateBetween(
            "2024-01-01".to_string(),
            "  ".to_string(),
        ))
        .unwrap_err();
        assert!(err.contains("must not be empty"), "{err}");
    }

    #[test]
    fn validate_condition_accepts_blank_and_not_blank() {
        validate_condition(&Condition::Blank).unwrap();
        validate_condition(&Condition::NotBlank).unwrap();
    }

    #[test]
    fn clear_sets_no_rule() {
        let grid = GridRange {
            sheet_id: 1,
            ..Default::default()
        };
        let request = build_request(
            &ValidationVerb::ClearDataValidation {
                sheet: None,
                range: Some("A1".to_string()),
            },
            grid,
        );
        match request {
            BatchUpdateRequestItem::SetDataValidation(req) => assert!(req.rule.is_none()),
            other => panic!("expected SetDataValidation, got {other:?}"),
        }
    }

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

    /// `version: "1"` throughout — matches [`leased_opts_for`]'s default
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

    /// A fresh, isolated ledger path holding a live lease for `"sheet-1"`
    /// at version `"1"` (matching [`mount_file`]'s default).
    fn leased_opts_for(spreadsheet_id: &str) -> (Option<String>, std::path::PathBuf) {
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        let token = seed_lease(&ledger_path, spreadsheet_id, "1");
        (Some(token), ledger_path)
    }

    fn mount_workbook() -> wiremock::Mock {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/sheet-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "spreadsheetId": "sheet-1",
                    "properties": {"title": "Budget"},
                    "sheets": [
                        {"properties": {"sheetId": 0, "title": "Q1", "index": 0}},
                    ],
                })),
            )
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
        let opts = ValidationOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: ValidationVerb::SetDataValidation {
                sheet: Some("Q1".to_string()),
                range: Some("A1:A10".to_string()),
                condition: Condition::Checkbox,
                input_message: None,
                show_warning: false,
            },
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = validation(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, ValidationResult::Blocked { .. }));
    }

    #[tokio::test]
    async fn set_data_validation_sends_a_boolean_condition() {
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
        let opts = ValidationOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: ValidationVerb::SetDataValidation {
                sheet: Some("Q1".to_string()),
                range: Some("A1:A10".to_string()),
                condition: Condition::OneOfList(vec!["yes".to_string(), "no".to_string()]),
                input_message: Some("pick one".to_string()),
                show_warning: false,
            },
            dry_run: false,
            lease_token,
            ledger_path,
        };
        let outcome = validation(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, ValidationResult::Changed { .. }));

        let requests = server.received_requests().await.unwrap();
        let batch = requests
            .iter()
            .find(|r| r.url.path().ends_with(":batchUpdate"))
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&batch.body).unwrap();
        let rule = &body["requests"][0]["setDataValidation"]["rule"];
        assert_eq!(rule["condition"]["type"], "ONE_OF_LIST");
        assert_eq!(rule["strict"], true);
    }

    #[tokio::test]
    async fn set_data_validation_sends_a_tranche_2_comparator_and_relative_date() {
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
        let opts = ValidationOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: ValidationVerb::SetDataValidation {
                sheet: Some("Q1".to_string()),
                range: Some("A1:A10".to_string()),
                condition: Condition::NumberGreater(0.0),
                input_message: None,
                show_warning: false,
            },
            dry_run: false,
            lease_token,
            ledger_path,
        };
        let outcome = validation(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, ValidationResult::Changed { .. }));

        let requests = server.received_requests().await.unwrap();
        let batch = requests
            .iter()
            .find(|r| r.url.path().ends_with(":batchUpdate"))
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&batch.body).unwrap();
        let rule = &body["requests"][0]["setDataValidation"]["rule"];
        assert_eq!(rule["condition"]["type"], "NUMBER_GREATER");
        assert_eq!(rule["condition"]["values"][0]["userEnteredValue"], "0");
        assert!(rule["condition"]["values"][0]["relativeDate"].is_null());
    }

    #[tokio::test]
    async fn set_data_validation_sends_a_relative_date_condition_without_user_entered_value() {
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
        let opts = ValidationOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: ValidationVerb::SetDataValidation {
                sheet: Some("Q1".to_string()),
                range: Some("A1:A10".to_string()),
                condition: Condition::DateAfter(DateValue::parse("today".to_string())),
                input_message: None,
                show_warning: false,
            },
            dry_run: false,
            lease_token,
            ledger_path,
        };
        let outcome = validation(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, ValidationResult::Changed { .. }));

        let requests = server.received_requests().await.unwrap();
        let batch = requests
            .iter()
            .find(|r| r.url.path().ends_with(":batchUpdate"))
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&batch.body).unwrap();
        let rule = &body["requests"][0]["setDataValidation"]["rule"];
        assert_eq!(rule["condition"]["type"], "DATE_AFTER");
        assert_eq!(rule["condition"]["values"][0]["relativeDate"], "TODAY");
        assert!(rule["condition"]["values"][0]["userEnteredValue"].is_null());
    }

    #[tokio::test]
    async fn clear_data_validation_full_apply_flow() {
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
        let opts = ValidationOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: clear_verb(),
            dry_run: false,
            lease_token,
            ledger_path,
        };
        let outcome = validation(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, ValidationResult::Changed { .. }));

        let requests = server.received_requests().await.unwrap();
        let batch = requests
            .iter()
            .find(|r| r.url.path().ends_with(":batchUpdate"))
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&batch.body).unwrap();
        assert!(body["requests"][0]["setDataValidation"]["rule"].is_null());
    }

    #[tokio::test]
    async fn dry_run_reports_would_change_without_calling_batch_update() {
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
        let opts = ValidationOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: set_verb(Condition::Checkbox, false),
            dry_run: true,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = validation(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            ValidationResult::WouldChange { .. }
        ));
        let requests = server.received_requests().await.unwrap();
        assert!(!requests
            .iter()
            .any(|r| r.url.path().ends_with(":batchUpdate")));
    }

    #[tokio::test]
    async fn compose_error_returns_refused_invalid_range() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        let opts = ValidationOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: ValidationVerb::SetDataValidation {
                sheet: None,
                range: None,
                condition: Condition::Checkbox,
                input_message: None,
                show_warning: false,
            },
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = validation(&drive, &sheets, &opts, &[]).await;
        let ValidationResult::RefusedInvalidRange { detail } = &outcome.result else {
            panic!("expected RefusedInvalidRange, got {:?}", outcome.result);
        };
        assert!(detail.contains("a range is required"), "{detail}");
        assert!(outcome.file_name.is_none());
    }

    #[tokio::test]
    async fn invalid_condition_is_refused_before_any_network_call() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        let opts = ValidationOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: set_verb(Condition::NumberBetween(10.0, 1.0), false),
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = validation(&drive, &sheets, &opts, &[]).await;
        let ValidationResult::RefusedInvalidRange { detail } = &outcome.result else {
            panic!("expected RefusedInvalidRange, got {:?}", outcome.result);
        };
        assert!(detail.contains("must not exceed"), "{detail}");
        let requests = server.received_requests().await.unwrap();
        assert!(!requests
            .iter()
            .any(|r| r.url.path().starts_with("/drive/v3/files")));
    }

    #[tokio::test]
    async fn metadata_fetch_failure_is_reported_as_failed() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/sheet-1"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let opts = ValidationOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: set_verb(Condition::Checkbox, false),
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = validation(&drive, &sheets, &opts, &[]).await;
        assert!(matches!(outcome.result, ValidationResult::Failed { .. }));
        assert!(outcome.file_name.is_none());
    }

    #[tokio::test]
    async fn shortcut_is_refused() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            "application/vnd.google-apps.shortcut",
            &["folder-1"],
        )
        .mount(&server)
        .await;
        let opts = ValidationOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: set_verb(Condition::Checkbox, false),
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = validation(&drive, &sheets, &opts, &[]).await;
        assert!(matches!(outcome.result, ValidationResult::RefusedShortcut));
        assert_eq!(outcome.file_name.as_deref(), Some("sheet-1"));
    }

    #[tokio::test]
    async fn not_a_spreadsheet_is_refused() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", "application/pdf", &["folder-1"])
            .mount(&server)
            .await;
        let opts = ValidationOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: set_verb(Condition::Checkbox, false),
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = validation(&drive, &sheets, &opts, &[]).await;
        assert!(matches!(
            outcome.result,
            ValidationResult::RefusedNotASpreadsheet { ref mime_type } if mime_type == "application/pdf"
        ));
    }

    #[tokio::test]
    async fn no_visible_parents_is_refused() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", crate::drive::types::GOOGLE_SHEET_MIME_TYPE, &[])
            .mount(&server)
            .await;
        let opts = ValidationOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: set_verb(Condition::Checkbox, false),
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = validation(&drive, &sheets, &opts, &[]).await;
        assert!(matches!(
            outcome.result,
            ValidationResult::RefusedNoVisibleParents
        ));
    }

    #[tokio::test]
    async fn gate_fetch_failure_is_reported_as_failed() {
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
        let opts = ValidationOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: set_verb(Condition::Checkbox, false),
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = validation(&drive, &sheets, &opts, &[]).await;
        assert!(matches!(outcome.result, ValidationResult::Failed { .. }));
        assert_eq!(outcome.file_name.as_deref(), Some("sheet-1"));
    }

    #[tokio::test]
    async fn workbook_fetch_failure_is_reported_as_failed() {
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
        let opts = ValidationOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: set_verb(Condition::Checkbox, false),
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = validation(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, ValidationResult::Failed { .. }));
    }

    #[tokio::test]
    async fn sheet_not_found_is_refused() {
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
        let opts = ValidationOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: ValidationVerb::SetDataValidation {
                sheet: Some("Nope".to_string()),
                range: Some("A1:A10".to_string()),
                condition: Condition::Checkbox,
                input_message: None,
                show_warning: false,
            },
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = validation(&drive, &sheets, &opts, &rules).await;
        let ValidationResult::RefusedSheetNotFound { title, available } = &outcome.result else {
            panic!("expected RefusedSheetNotFound, got {:?}", outcome.result);
        };
        assert_eq!(title, "Nope");
        assert_eq!(available, &["Q1".to_string()]);
    }

    #[tokio::test]
    async fn invalid_grid_range_is_refused() {
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
        let opts = ValidationOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: ValidationVerb::SetDataValidation {
                sheet: Some("Q1".to_string()),
                range: Some("??".to_string()),
                condition: Condition::Checkbox,
                input_message: None,
                show_warning: false,
            },
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = validation(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            ValidationResult::RefusedInvalidRange { .. }
        ));
    }

    #[tokio::test]
    async fn batch_update_failure_is_reported_as_failed_not_changed() {
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
        let rules = vec![allow_rule("folder-1")];
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
        let opts = ValidationOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: set_verb(Condition::Checkbox, false),
            dry_run: false,
            lease_token,
            ledger_path,
        };
        let outcome = validation(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, ValidationResult::Failed { .. }));
    }

    fn every_validation_result() -> Vec<ValidationResult> {
        let all = vec![
            ValidationResult::WouldChange {
                summary: "set data validation".to_string(),
            },
            ValidationResult::RefusedNotASpreadsheet {
                mime_type: "application/pdf".to_string(),
            },
            ValidationResult::RefusedShortcut,
            ValidationResult::RefusedNoVisibleParents,
            ValidationResult::RefusedSheetNotFound {
                title: "Nope".to_string(),
                available: vec!["Q1".to_string(), "Q2".to_string()],
            },
            ValidationResult::RefusedSheetNotFound {
                title: "Nope".to_string(),
                available: Vec::new(),
            },
            ValidationResult::RefusedInvalidRange {
                detail: "bad range".to_string(),
            },
            ValidationResult::Blocked { decided_by: None },
            ValidationResult::Blocked {
                decided_by: Some(DecidingRule::Folder {
                    folder_id: "folder-1".to_string(),
                    depth: 2,
                }),
            },
            ValidationResult::RefusedNoLease,
            ValidationResult::RefusedLeaseExpired,
            ValidationResult::RefusedLeaseWrongFile,
            ValidationResult::RefusedLeaseStale,
            ValidationResult::Changed {
                summary: "set data validation".to_string(),
            },
            ValidationResult::Failed {
                detail: "boom".to_string(),
            },
        ];
        // Compile-time exhaustiveness: a new variant fails to match here.
        for result in &all {
            match result {
                ValidationResult::WouldChange { .. }
                | ValidationResult::RefusedNotASpreadsheet { .. }
                | ValidationResult::RefusedShortcut
                | ValidationResult::RefusedNoVisibleParents
                | ValidationResult::RefusedSheetNotFound { .. }
                | ValidationResult::RefusedInvalidRange { .. }
                | ValidationResult::Blocked { .. }
                | ValidationResult::RefusedNoLease
                | ValidationResult::RefusedLeaseExpired
                | ValidationResult::RefusedLeaseWrongFile
                | ValidationResult::RefusedLeaseStale
                | ValidationResult::Changed { .. }
                | ValidationResult::Failed { .. } => {}
            }
        }
        all
    }

    #[test]
    fn describe_lines_covers_every_result_for_every_verb() {
        for verb in [clear_verb(), set_verb(Condition::Checkbox, false)] {
            for result in every_validation_result() {
                let outcome = ValidationOutcome {
                    spreadsheet_id: "sheet-1".to_string(),
                    file_name: Some("Budget".to_string()),
                    resolved_folder_id: None,
                    verb: verb.clone(),
                    result,
                };
                let lines = describe_lines(&outcome);
                assert_eq!(lines.len(), 1, "{:?}", outcome.result);
                assert_eq!(describe(&outcome), lines.join("\n"));
                for rendered in &lines {
                    assert!(!rendered.chars().any(char::is_control), "{rendered:?}");
                }
            }
        }
    }

    #[test]
    fn log_status_covers_every_variant() {
        let statuses: HashSet<&'static str> = every_validation_result()
            .iter()
            .map(ValidationResult::log_status)
            .collect();
        // 13 `ValidationResult` variants; `every_validation_result` lists 15
        // entries so it can also exercise `RefusedSheetNotFound`'s and
        // `Blocked`'s two shapes each, which share a `log_status`.
        assert_eq!(statuses.len(), 13);
        assert!(statuses.iter().all(|s| !s.is_empty()));
    }

    #[test]
    fn write_jsonl_emits_one_line_of_json() {
        let outcome = ValidationOutcome {
            spreadsheet_id: "sheet-1".to_string(),
            file_name: Some("Budget".to_string()),
            resolved_folder_id: None,
            verb: clear_verb(),
            result: ValidationResult::Changed {
                summary: "clear data validation".to_string(),
            },
        };
        let mut buf = Vec::new();
        outcome.write_jsonl(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert_eq!(text.matches('\n').count(), 1);
        let parsed: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(parsed["result"]["status"], "changed");
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

        let opts = ValidationOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: set_verb(Condition::Checkbox, false),
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::new(),
        };
        let outcome = validation(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, ValidationResult::RefusedNoLease));
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
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
        // Under `flock` (issue #1687), a busy lock now waits rather than
        // hard-failing (`check_and_lock_lease` -> `acquire_waiting`), so a
        // held `LedgerLock` no longer reproduces an immediate failure here.
        // A directory at the lock path does: opening it for write fails
        // outright with an I/O error, which is never retried.
        let mut lock_path = ledger_path.clone().into_os_string();
        lock_path.push(".lock");
        std::fs::create_dir(std::path::PathBuf::from(lock_path)).unwrap();

        let opts = ValidationOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: set_verb(Condition::Checkbox, false),
            dry_run: false,
            lease_token,
            ledger_path,
        };
        let outcome = validation(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, ValidationResult::Failed { .. }));
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
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");

        let opts = ValidationOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: set_verb(Condition::Checkbox, false),
            dry_run: false,
            lease_token,
            ledger_path,
        };
        let outcome = validation(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, ValidationResult::Failed { .. }));
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

        let opts = ValidationOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: set_verb(Condition::Checkbox, false),
            dry_run: false,
            lease_token: Some("bogus-token".to_string()),
            ledger_path,
        };
        let outcome = validation(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            ValidationResult::RefusedLeaseExpired
        ));
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

        let opts = ValidationOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: set_verb(Condition::Checkbox, false),
            dry_run: false,
            lease_token: Some(token),
            ledger_path,
        };
        let outcome = validation(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            ValidationResult::RefusedLeaseWrongFile
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

        let opts = ValidationOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: set_verb(Condition::Checkbox, false),
            dry_run: false,
            lease_token: Some(token),
            ledger_path,
        };
        let outcome = validation(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(
            outcome.result,
            ValidationResult::RefusedLeaseStale
        ));
    }

    #[tokio::test]
    async fn require_lease_false_writes_without_a_lease() {
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
        let rule = FolderPermissionRule {
            folder_id: Some("folder-1".to_string()),
            file_id: None,
            recursive: true,
            allow: std::iter::once(DriveOperation::SheetsStructure).collect(),
            deny: HashSet::default(),
            require_lease: false,
        };

        // No lease token presented at all, and no ledger exists.
        let opts = ValidationOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: set_verb(Condition::Checkbox, false),
            dry_run: false,
            lease_token: None,
            ledger_path: std::path::PathBuf::from("/nonexistent/lease-ledger.jsonl"),
        };
        let outcome = validation(&drive, &sheets, &opts, &[rule]).await;
        assert!(matches!(outcome.result, ValidationResult::Changed { .. }));
    }

    #[tokio::test]
    async fn require_lease_false_still_refuses_a_stale_lease_if_one_is_presented() {
        // ADR-0080 §13: `require_lease: false` relaxes the *requirement*,
        // not the *meaning* — a token volunteered anyway is checked exactly
        // like a required one, including staleness.
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
        // No batchUpdate mock mounted — a refusal must make zero mutating
        // calls.
        let rule = FolderPermissionRule {
            folder_id: Some("folder-1".to_string()),
            file_id: None,
            recursive: true,
            allow: std::iter::once(DriveOperation::SheetsStructure).collect(),
            deny: HashSet::default(),
            require_lease: false,
        };
        let ledger_path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("lease-ledger.jsonl");
        let token = seed_lease(&ledger_path, "sheet-1", "0");

        let opts = ValidationOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: set_verb(Condition::Checkbox, false),
            dry_run: false,
            lease_token: Some(token),
            ledger_path,
        };
        let outcome = validation(&drive, &sheets, &opts, &[rule]).await;
        assert!(matches!(
            outcome.result,
            ValidationResult::RefusedLeaseStale
        ));
    }

    // ── the write's own audit trail (ADR-0080 §11) ─────────────────────

    #[tokio::test]
    async fn a_leased_validation_change_concludes_its_audit_pair_with_allowed() {
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
        let dir = tempfile::tempdir().unwrap();
        let audit = crate::test_support::AuditLogGuard::redirect(dir.path());
        let rules = vec![allow_rule("folder-1")];
        let (lease_token, ledger_path) = leased_opts_for("sheet-1");
        let opts = ValidationOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: ValidationVerb::SetDataValidation {
                sheet: Some("Q1".to_string()),
                range: Some("A1:A10".to_string()),
                condition: Condition::OneOfList(vec!["yes".to_string(), "no".to_string()]),
                input_message: None,
                show_warning: false,
            },
            dry_run: false,
            lease_token,
            ledger_path,
        };

        let outcome = validation(&drive, &sheets, &opts, &rules).await;
        assert!(matches!(outcome.result, ValidationResult::Changed { .. }));

        let records = audit.records();
        assert_eq!(audit.verdicts(), ["pending", "allowed"], "{records:?}");
        // The verb, not the engine — the same `["drive", <log_operation>]`
        // this write's `drivemutation` record carries, so an auditor can
        // see which verb ran without joining back to `log.jsonl`.
        assert_eq!(records[0].command, ["drive", "sheets-set-data-validation"]);
    }
}
