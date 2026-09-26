//! CLI commands for `omni-dev drive sheets set-data-validation`/
//! `clear-data-validation` (issue #1643).

use anyhow::Result;
use clap::Parser;

use crate::cli::drive::format::{output_as, OutputFormat};
use crate::cli::drive::helpers;
use crate::cli::format::sanitize_for_terminal;
use crate::drive::client::DriveClient;
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::date_value::DateValue;
use crate::drive::sheets::validation::{
    describe_lines, validation, Condition, ValidationOptions, ValidationVerb,
};

/// Sets a data validation rule on a range.
///
/// Exactly one condition flag is required, from five families: `--one-of-list`/
/// `--one-of-range`; the numeric comparators (`--number-between`,
/// `--number-not-between`, `--number-greater(-eq)`, `--number-less(-eq)`,
/// `--number-eq`, `--number-not-eq`); the text conditions
/// (`--text-contains`, `--text-not-contains`, `--text-starts-with`,
/// `--text-ends-with`, `--text-eq`); the date conditions (`--date-after`,
/// `--date-before`, `--date-on`, `--date-between` — absolute dates only;
/// Sheets rejects a relative keyword like `today` in data validation, even
/// though `add-conditional-format`/`update-conditional-format` accept one
/// for the identically-shaped condition); `--blank`/`--not-blank`;
/// `--checkbox`; or `--custom-formula`.
#[derive(Parser)]
#[command(group(clap::ArgGroup::new("condition")
    .args([
        "one_of_list", "one_of_range",
        "number_between", "number_not_between",
        "number_greater", "number_greater_eq", "number_less", "number_less_eq",
        "number_eq", "number_not_eq",
        "text_contains", "text_not_contains", "text_starts_with", "text_ends_with", "text_eq",
        "date_after", "date_before", "date_on", "date_between",
        "blank", "not_blank",
        "checkbox", "custom_formula",
    ])
    .required(true)))]
pub struct SetDataValidationCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// A1 range to validate, optionally carrying its own `Sheet!` prefix.
    #[arg(long, value_name = "A1")]
    pub range: Option<String>,

    /// Sheet (tab) title. Supplies the prefix for a bare `--range`.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// Restrict entries to this comma-separated list (renders as a
    /// dropdown).
    #[arg(long, value_name = "A,B,C", value_delimiter = ',')]
    pub one_of_list: Option<Vec<String>>,

    /// Restrict entries to a dropdown sourced from this range.
    #[arg(long, value_name = "A1_RANGE")]
    pub one_of_range: Option<String>,

    /// Restrict entries to a number in this inclusive range.
    // `allow_hyphen_values` lets a negative bound (e.g. `-5`) pass instead of
    // being misread as an unrecognized flag.
    #[arg(long, num_args = 2, value_names = ["MIN", "MAX"], allow_hyphen_values = true)]
    pub number_between: Option<Vec<f64>>,

    /// Restrict entries to a number outside this inclusive range.
    #[arg(long, num_args = 2, value_names = ["MIN", "MAX"], allow_hyphen_values = true)]
    pub number_not_between: Option<Vec<f64>>,

    /// Restrict entries to a number strictly greater than this.
    #[arg(long, value_name = "N", allow_hyphen_values = true)]
    pub number_greater: Option<f64>,

    /// Restrict entries to a number greater than or equal to this.
    #[arg(long, value_name = "N", allow_hyphen_values = true)]
    pub number_greater_eq: Option<f64>,

    /// Restrict entries to a number strictly less than this.
    #[arg(long, value_name = "N", allow_hyphen_values = true)]
    pub number_less: Option<f64>,

    /// Restrict entries to a number less than or equal to this.
    #[arg(long, value_name = "N", allow_hyphen_values = true)]
    pub number_less_eq: Option<f64>,

    /// Restrict entries to a number equal to this.
    #[arg(long, value_name = "N", allow_hyphen_values = true)]
    pub number_eq: Option<f64>,

    /// Restrict entries to a number not equal to this.
    #[arg(long, value_name = "N", allow_hyphen_values = true)]
    pub number_not_eq: Option<f64>,

    /// Restrict entries to text containing this substring.
    #[arg(long, value_name = "TEXT")]
    pub text_contains: Option<String>,

    /// Restrict entries to text not containing this substring.
    #[arg(long, value_name = "TEXT")]
    pub text_not_contains: Option<String>,

    /// Restrict entries to text starting with this substring.
    #[arg(long, value_name = "TEXT")]
    pub text_starts_with: Option<String>,

    /// Restrict entries to text ending with this substring.
    #[arg(long, value_name = "TEXT")]
    pub text_ends_with: Option<String>,

    /// Restrict entries to text equal to this.
    #[arg(long, value_name = "TEXT")]
    pub text_eq: Option<String>,

    /// Restrict entries to a date after this one. Absolute dates only —
    /// Sheets rejects a relative keyword like `today` in data validation
    /// (accepted in `add-conditional-format`/`update-conditional-format`,
    /// not here).
    #[arg(long, value_name = "DATE")]
    pub date_after: Option<String>,

    /// Restrict entries to a date before this one. Absolute dates only
    /// (see `--date-after`).
    #[arg(long, value_name = "DATE")]
    pub date_before: Option<String>,

    /// Restrict entries to a date equal to this one. Absolute dates only
    /// (see `--date-after`).
    #[arg(long, value_name = "DATE")]
    pub date_on: Option<String>,

    /// Restrict entries to a date in this inclusive range (absolute dates
    /// only; relative keywords are not accepted here).
    #[arg(long, num_args = 2, value_names = ["START", "END"])]
    pub date_between: Option<Vec<String>>,

    /// Restrict entries to an empty cell.
    #[arg(long)]
    pub blank: bool,

    /// Restrict entries to a non-empty cell.
    #[arg(long)]
    pub not_blank: bool,

    /// Restrict entries to `TRUE`/`FALSE`, rendered as a checkbox.
    #[arg(long)]
    pub checkbox: bool,

    /// Restrict entries to ones for which this formula evaluates truthy.
    #[arg(long, value_name = "FORMULA")]
    pub custom_formula: Option<String>,

    /// Tooltip text shown on the cell.
    #[arg(long, value_name = "TEXT")]
    pub input_message: Option<String>,

    /// Allow an invalid entry through with a warning, instead of rejecting
    /// it outright.
    #[arg(long)]
    pub show_warning: bool,

    /// Reports the gate verdict and the change that would be made, without
    /// calling `spreadsheets.batchUpdate`.
    #[arg(long)]
    pub dry_run: bool,

    #[command(flatten)]
    pub lease: crate::cli::drive::helpers::LeaseTokenArg,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

/// Selects the one condition flag the `ArgGroup` on
/// [`SetDataValidationCommand`] guarantees is set. `--custom-formula` is the
/// unconditional final fallback: if nothing else in this chain matched, it
/// must be the one that's set.
fn select_condition(cmd: &SetDataValidationCommand) -> Condition {
    if let Some(items) = &cmd.one_of_list {
        Condition::OneOfList(items.clone())
    } else if let Some(range) = &cmd.one_of_range {
        Condition::OneOfRange(range.clone())
    } else if let Some(v) = &cmd.number_between {
        Condition::NumberBetween(v[0], v[1])
    } else if let Some(v) = &cmd.number_not_between {
        Condition::NumberNotBetween(v[0], v[1])
    } else if let Some(n) = cmd.number_greater {
        Condition::NumberGreater(n)
    } else if let Some(n) = cmd.number_greater_eq {
        Condition::NumberGreaterEq(n)
    } else if let Some(n) = cmd.number_less {
        Condition::NumberLess(n)
    } else if let Some(n) = cmd.number_less_eq {
        Condition::NumberLessEq(n)
    } else if let Some(n) = cmd.number_eq {
        Condition::NumberEq(n)
    } else if let Some(n) = cmd.number_not_eq {
        Condition::NumberNotEq(n)
    } else if let Some(text) = &cmd.text_contains {
        Condition::TextContains(text.clone())
    } else if let Some(text) = &cmd.text_not_contains {
        Condition::TextNotContains(text.clone())
    } else if let Some(text) = &cmd.text_starts_with {
        Condition::TextStartsWith(text.clone())
    } else if let Some(text) = &cmd.text_ends_with {
        Condition::TextEndsWith(text.clone())
    } else if let Some(text) = &cmd.text_eq {
        Condition::TextEq(text.clone())
    } else if let Some(raw) = &cmd.date_after {
        Condition::DateAfter(DateValue::parse(raw.clone()))
    } else if let Some(raw) = &cmd.date_before {
        Condition::DateBefore(DateValue::parse(raw.clone()))
    } else if let Some(raw) = &cmd.date_on {
        Condition::DateOn(DateValue::parse(raw.clone()))
    } else if let Some(v) = &cmd.date_between {
        Condition::DateBetween(v[0].clone(), v[1].clone())
    } else if cmd.blank {
        Condition::Blank
    } else if cmd.not_blank {
        Condition::NotBlank
    } else if cmd.checkbox {
        Condition::Checkbox
    } else {
        Condition::CustomFormula(cmd.custom_formula.clone().unwrap_or_default())
    }
}

impl SetDataValidationCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        // The `ArgGroup` above guarantees exactly one condition flag is set.
        let condition = select_condition(&self);
        let opts = ValidationOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: ValidationVerb::SetDataValidation {
                sheet: self.sheet,
                range: self.range,
                condition,
                input_message: self.input_message,
                show_warning: self.show_warning,
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_validation(client, &opts, &self.output).await
    }
}

/// Removes a range's data validation rule.
#[derive(Parser)]
pub struct ClearDataValidationCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// A1 range to clear validation from, optionally carrying its own
    /// `Sheet!` prefix.
    #[arg(long, value_name = "A1")]
    pub range: Option<String>,

    /// Sheet (tab) title. Supplies the prefix for a bare `--range`.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// Reports the gate verdict and the change that would be made, without
    /// calling `spreadsheets.batchUpdate`.
    #[arg(long)]
    pub dry_run: bool,

    #[command(flatten)]
    pub lease: crate::cli::drive::helpers::LeaseTokenArg,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ClearDataValidationCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = ValidationOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: ValidationVerb::ClearDataValidation {
                sheet: self.sheet,
                range: self.range,
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_validation(client, &opts, &self.output).await
    }
}

async fn run_validation(
    client: &DriveClient,
    opts: &ValidationOptions,
    output: &OutputFormat,
) -> Result<()> {
    let sheets = SheetsClient::from_drive_client(client)?;
    let rules = helpers::active_account_rules()?;
    let outcome = validation(client, &sheets, opts, &rules).await;
    if output_as(&outcome, output)? {
        return Ok(());
    }
    let lines: Vec<String> = describe_lines(&outcome)
        .into_iter()
        .map(|line| sanitize_for_terminal(&line))
        .collect();
    println!("{}", lines.join("\n"));
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::utils::secret::Secret;

    fn dead_credentials() -> DriveCredentials {
        DriveCredentials {
            client_id: "client".to_string(),
            client_secret: Secret::new("secret"),
            refresh_token: Secret::new("refresh"),
            scope: DriveGrantedScopes::READONLY,
        }
    }

    fn dead_client() -> DriveClient {
        DriveClient::new("http://127.0.0.1:1", &dead_credentials()).unwrap()
    }

    fn base_cmd() -> SetDataValidationCommand {
        SetDataValidationCommand {
            spreadsheet_id: "sheet-1".to_string(),
            range: Some("A1:A10".to_string()),
            sheet: None,
            one_of_list: None,
            one_of_range: None,
            number_between: None,
            number_not_between: None,
            number_greater: None,
            number_greater_eq: None,
            number_less: None,
            number_less_eq: None,
            number_eq: None,
            number_not_eq: None,
            text_contains: None,
            text_not_contains: None,
            text_starts_with: None,
            text_ends_with: None,
            text_eq: None,
            date_after: None,
            date_before: None,
            date_on: None,
            date_between: None,
            blank: false,
            not_blank: false,
            checkbox: false,
            custom_formula: None,
            input_message: None,
            show_warning: false,
            dry_run: false,
            lease: crate::cli::drive::helpers::LeaseTokenArg { lease: None },
            output: crate::cli::drive::format::OutputFormat::Table,
        }
    }

    #[tokio::test]
    async fn execute_selects_number_between_condition() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let cmd = SetDataValidationCommand {
            number_between: Some(vec![1.0, 10.0]),
            ..base_cmd()
        };
        assert!(cmd.execute(&dead_client()).await.is_ok());
    }

    #[tokio::test]
    async fn execute_selects_checkbox_condition() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let cmd = SetDataValidationCommand {
            checkbox: true,
            ..base_cmd()
        };
        assert!(cmd.execute(&dead_client()).await.is_ok());
    }

    #[tokio::test]
    async fn execute_selects_custom_formula_condition() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let cmd = SetDataValidationCommand {
            custom_formula: Some("=A1>0".to_string()),
            ..base_cmd()
        };
        assert!(cmd.execute(&dead_client()).await.is_ok());
    }

    #[tokio::test]
    async fn execute_selects_one_of_range_condition() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let cmd = SetDataValidationCommand {
            one_of_range: Some("Sheet2!A1:A10".to_string()),
            ..base_cmd()
        };
        assert!(cmd.execute(&dead_client()).await.is_ok());
    }

    #[tokio::test]
    async fn execute_selects_a_numeric_comparator_condition() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let cmd = SetDataValidationCommand {
            number_greater: Some(0.0),
            ..base_cmd()
        };
        assert!(cmd.execute(&dead_client()).await.is_ok());
    }

    /// Regression: `--number-greater -5` and friends start with `-`, which
    /// clap will otherwise mistake for an unrecognized flag rather than a
    /// negative value. `allow_hyphen_values = true` on each numeric field
    /// keeps the parse working.
    #[test]
    fn numeric_flags_accept_a_negative_value() {
        let cmd = SetDataValidationCommand::try_parse_from([
            "set-data-validation",
            "sheet-1",
            "--number-greater",
            "-5",
        ])
        .expect("clap should accept a negative --number-greater value");
        assert_eq!(cmd.number_greater, Some(-5.0));

        let cmd = SetDataValidationCommand::try_parse_from([
            "set-data-validation",
            "sheet-1",
            "--number-between",
            "-5",
            "10",
        ])
        .expect("clap should accept a negative --number-between bound");
        assert_eq!(cmd.number_between, Some(vec![-5.0, 10.0]));
    }

    #[tokio::test]
    async fn execute_selects_a_text_condition() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let cmd = SetDataValidationCommand {
            text_contains: Some("x".to_string()),
            ..base_cmd()
        };
        assert!(cmd.execute(&dead_client()).await.is_ok());
    }

    #[tokio::test]
    async fn execute_selects_an_absolute_date_condition() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let cmd = SetDataValidationCommand {
            date_after: Some("2024-01-01".to_string()),
            ..base_cmd()
        };
        assert!(cmd.execute(&dead_client()).await.is_ok());
    }

    #[tokio::test]
    async fn execute_selects_a_relative_date_condition() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let cmd = SetDataValidationCommand {
            date_before: Some("tomorrow".to_string()),
            ..base_cmd()
        };
        assert!(cmd.execute(&dead_client()).await.is_ok());
    }

    #[tokio::test]
    async fn execute_selects_a_date_between_condition() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let cmd = SetDataValidationCommand {
            date_between: Some(vec!["2024-01-01".to_string(), "2024-12-31".to_string()]),
            ..base_cmd()
        };
        assert!(cmd.execute(&dead_client()).await.is_ok());
    }

    #[tokio::test]
    async fn execute_selects_a_blank_condition() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let cmd = SetDataValidationCommand {
            blank: true,
            ..base_cmd()
        };
        assert!(cmd.execute(&dead_client()).await.is_ok());
    }

    #[tokio::test]
    async fn execute_selects_a_not_blank_condition() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let cmd = SetDataValidationCommand {
            not_blank: true,
            ..base_cmd()
        };
        assert!(cmd.execute(&dead_client()).await.is_ok());
    }
}
