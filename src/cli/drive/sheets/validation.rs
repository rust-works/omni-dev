//! CLI commands for `omni-dev drive sheets set-data-validation`/
//! `clear-data-validation` (issue #1643).

use anyhow::Result;
use clap::Parser;

use crate::cli::drive::format::{output_as, OutputFormat};
use crate::cli::drive::helpers;
use crate::cli::format::sanitize_for_terminal;
use crate::drive::client::DriveClient;
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::validation::{
    describe_lines, validation, Condition, ValidationOptions, ValidationVerb,
};

/// Sets a data validation rule on a range.
///
/// Exactly one condition flag is required: `--one-of-list`,
/// `--number-between`, `--checkbox`, or `--custom-formula`.
#[derive(Parser)]
#[command(group(clap::ArgGroup::new("condition")
    .args(["one_of_list", "number_between", "checkbox", "custom_formula"])
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

    /// Restrict entries to a number in this inclusive range.
    #[arg(long, num_args = 2, value_names = ["MIN", "MAX"])]
    pub number_between: Option<Vec<f64>>,

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

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl SetDataValidationCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        // The `ArgGroup` above guarantees exactly one of these is set.
        let condition = if let Some(items) = self.one_of_list {
            Condition::OneOfList(items)
        } else if let Some(bounds) = self.number_between {
            Condition::NumberBetween(bounds[0], bounds[1])
        } else if self.checkbox {
            Condition::Checkbox
        } else {
            Condition::CustomFormula(self.custom_formula.unwrap_or_default())
        };
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
            number_between: None,
            checkbox: false,
            custom_formula: None,
            input_message: None,
            show_warning: false,
            dry_run: false,
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
}
