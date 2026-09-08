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

use std::time::{Duration, Instant};

use serde::Serialize;

use crate::cli::drive::format::{write_scalar_jsonl, JsonlSerialize};
use crate::drive::client::DriveClient;
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

    let (target, decision, resolved_folder_id) = match target_gate::resolve(
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
        } => (target, decision, resolved_folder_id),
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

    let request = build_request(&opts.verb, grid);
    match api.batch_update(&opts.spreadsheet_id, vec![request]).await {
        Ok(_response) => gated(ValidationResult::Changed { summary }),
        Err(err) => gated(ValidationResult::Failed {
            detail: format!("{err:#}"),
        }),
    }
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
}
