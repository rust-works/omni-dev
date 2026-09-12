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
//! Sheets models a couple dozen condition types (`NUMBER_GREATER`,
//! `TEXT_IS_EMAIL`, per-type date conditions, …); this ships the four with
//! the broadest use: `--one-of-list` (dropdown), `--number-between`,
//! `--checkbox`, `--custom-formula`. Everything else is a documented cut —
//! `docs/drive.md` names it — rather than a silent gap, matching this
//! issue's general stance on `CellFormat`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::cli::drive::format::{write_scalar_jsonl, JsonlSerialize};
use crate::drive::client::DriveClient;
use crate::drive::files_api::FilesApi;
use crate::drive::lease::check::{
    finish_leased_native_write, gate_leased_write, record_failed_leased_write, LeaseGateRefusal,
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

/// One of the four condition shapes `set-data-validation` builds.
///
/// Each variant carries exactly what its condition needs; there is no "raw
/// condition type + values" escape hatch, following the same
/// no-passthrough discipline as `format.rs::CellFormatFlags`.
#[derive(Debug, Clone, PartialEq)]
pub enum Condition {
    /// A dropdown restricted to these exact values.
    OneOfList(Vec<String>),
    /// A number in `[min, max]` inclusive.
    NumberBetween(f64, f64),
    /// `TRUE`/`FALSE` only.
    Checkbox,
    /// A custom formula that must evaluate truthy.
    CustomFormula(String),
}

impl Condition {
    fn into_boolean_condition(self) -> BooleanCondition {
        let value = |s: String| ConditionValue {
            user_entered_value: s,
        };
        match self {
            Self::OneOfList(items) => BooleanCondition {
                condition_type: "ONE_OF_LIST".to_string(),
                values: items.into_iter().map(value).collect(),
            },
            Self::NumberBetween(min, max) => BooleanCondition {
                condition_type: "NUMBER_BETWEEN".to_string(),
                values: vec![value(min.to_string()), value(max.to_string())],
            },
            Self::Checkbox => BooleanCondition {
                condition_type: "BOOLEAN".to_string(),
                values: Vec::new(),
            },
            Self::CustomFormula(formula) => BooleanCondition {
                condition_type: "CUSTOM_FORMULA".to_string(),
                values: vec![value(formula)],
            },
        }
    }

    /// The `validation_type` the request log records.
    const fn log_type(&self) -> &'static str {
        match self {
            Self::OneOfList(_) => "ONE_OF_LIST",
            Self::NumberBetween(..) => "NUMBER_BETWEEN",
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
    let lease_lock = if requires_lease {
        match gate_leased_write(leased, &files_api, opts.lease_token.as_deref()).await {
            Ok(lock) => Some(lock),
            Err(LeaseGateRefusal::NoLease) => return gated(ValidationResult::RefusedNoLease),
            Err(LeaseGateRefusal::Expired) => return gated(ValidationResult::RefusedLeaseExpired),
            Err(LeaseGateRefusal::WrongFile) => {
                return gated(ValidationResult::RefusedLeaseWrongFile)
            }
            Err(LeaseGateRefusal::Stale) => return gated(ValidationResult::RefusedLeaseStale),
            Err(LeaseGateRefusal::Failed(detail)) => {
                return gated(ValidationResult::Failed { detail })
            }
        }
    } else {
        None
    };

    let request = build_request(&opts.verb, grid);
    let result = match api.batch_update(&opts.spreadsheet_id, vec![request]).await {
        Ok(_response) => {
            if let (Some(token), Some(lock)) = (&opts.lease_token, &lease_lock) {
                finish_leased_native_write(leased, lock, token, &files_api).await;
            }
            ValidationResult::Changed { summary }
        }
        Err(err) => {
            let detail = format!("{err:#}");
            if let (Some(token), Some(_lock)) = (&opts.lease_token, &lease_lock) {
                record_failed_leased_write(leased, token, &detail);
            }
            ValidationResult::Failed { detail }
        }
    };
    drop(lease_lock);
    gated(result)
}

fn validate_condition(condition: &Condition) -> Result<(), String> {
    // Exhaustive over `Condition`: a new variant forces a new arm here at
    // compile time, rather than silently falling through unvalidated.
    match condition {
        Condition::OneOfList(items) => {
            if items.is_empty() {
                Err("--one-of-list needs at least one value".to_string())
            } else {
                Ok(())
            }
        }
        Condition::NumberBetween(min, max) => {
            // `NaN > x` and `x > NaN` are both `false`, so a NaN bound must
            // be checked explicitly or it silently reaches the API.
            if min.is_nan() || max.is_nan() || min > max {
                Err(format!(
                    "--number-between's first value ({min}) must not exceed the second ({max})"
                ))
            } else {
                Ok(())
            }
        }
        Condition::Checkbox => Ok(()),
        Condition::CustomFormula(formula) => {
            if formula.trim().is_empty() {
                Err("--custom-formula must not be empty".to_string())
            } else {
                Ok(())
            }
        }
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
        ValidationResult::RefusedNoLease => vec![format!(
            "Refused: {book} requires a Drive write lease — run `omni-dev drive lease acquire \
             {}` and pass the printed token via `--lease`.",
            outcome.spreadsheet_id
        )],
        ValidationResult::RefusedLeaseExpired => vec![format!(
            "Refused: the presented lease is expired, released, or unknown to this ledger — \
             run `omni-dev drive lease acquire {}` again.",
            outcome.spreadsheet_id
        )],
        ValidationResult::RefusedLeaseWrongFile => vec![format!(
            "Refused: the presented lease was acquired for a different file — run `omni-dev \
             drive lease acquire {}` for this one.",
            outcome.spreadsheet_id
        )],
        ValidationResult::RefusedLeaseStale => vec![format!(
            "Refused: {book} changed since the lease was acquired (or last written under) — \
             re-run `omni-dev drive lease acquire {}` to lease the current version.",
            outcome.spreadsheet_id
        )],
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
    use crate::test_support::env::MapEnv;
    use crate::utils::secret::Secret;
    use std::collections::HashSet;

    #[test]
    fn one_of_list_builds_a_value_per_item() {
        let condition = Condition::OneOfList(vec!["a".to_string(), "b".to_string()]);
        let built = condition.into_boolean_condition();
        assert_eq!(built.condition_type, "ONE_OF_LIST");
        assert_eq!(built.values.len(), 2);
        assert_eq!(built.values[0].user_entered_value, "a");
    }

    #[test]
    fn number_between_builds_two_values() {
        let condition = Condition::NumberBetween(1.0, 10.0);
        let built = condition.into_boolean_condition();
        assert_eq!(built.condition_type, "NUMBER_BETWEEN");
        assert_eq!(built.values[0].user_entered_value, "1");
        assert_eq!(built.values[1].user_entered_value, "10");
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
        assert_eq!(built.values[0].user_entered_value, "=A1>0");
    }

    #[test]
    fn log_type_covers_every_condition() {
        let types = [
            Condition::OneOfList(vec!["a".to_string()]).log_type(),
            Condition::NumberBetween(1.0, 2.0).log_type(),
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
        let mut lock_path = ledger_path.clone().into_os_string();
        lock_path.push(".lock");
        std::fs::write(std::path::PathBuf::from(lock_path), b"").unwrap();

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
