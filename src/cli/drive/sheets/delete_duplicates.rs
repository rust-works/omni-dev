//! CLI surface for `omni-dev drive sheets delete-duplicates` (issue #1844).

use anyhow::Result;
use clap::Parser;

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::cli::drive::helpers;
use crate::drive::client::DriveClient;
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::delete_duplicates::{
    delete_duplicates, describe_lines, DeleteDuplicatesOptions,
};

/// Removes duplicate row cells within a bounded range.
///
/// The API decides which rows go: it keeps the first instance of each
/// duplicate, treats rows differing only in letter case, formatting or
/// formulas as duplicates, and removes filter-hidden rows too. So
/// `--dry-run` names the range and the compared columns and states that
/// rule — it cannot list the rows, and deliberately does not guess.
///
/// The range must be fully bounded. Blank rows duplicate one another, so a
/// range extending past the data can remove every blank row but the first.
/// Content outside the range stays in place; selecting fewer than all sheet
/// columns can misalign records.
#[derive(Parser)]
pub struct DeleteDuplicatesCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Sheet (tab) title. Supplies the prefix for a bare `--range`.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// Fully bounded A1 range to dedupe, optionally carrying its own
    /// `Sheet!` prefix (for example `A2:D100`).
    #[arg(long, value_name = "A1")]
    pub range: String,

    /// An absolute, zero-based sheet column index to compare. Repeat to
    /// compare several. Omit to compare every column in the range.
    #[arg(long, value_name = "COLUMN")]
    pub comparison_column: Vec<i64>,

    /// Reports the gate verdict and the request scope without calling
    /// `spreadsheets.batchUpdate`. Which rows are duplicates is computed
    /// by Sheets only when the request executes.
    #[arg(long)]
    pub dry_run: bool,

    #[command(flatten)]
    pub lease: crate::cli::drive::helpers::LeaseTokenArg,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl DeleteDuplicatesCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = DeleteDuplicatesOptions {
            spreadsheet_id: self.spreadsheet_id,
            sheet: self.sheet,
            range: Some(self.range),
            comparison_columns: self.comparison_column,
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        let sheets = SheetsClient::from_drive_client(client)?;
        let rules = helpers::active_account_rules()?;
        let outcome = delete_duplicates(client, &sheets, &opts, &rules).await;
        if output_as(&outcome, &self.output)? {
            return Ok(());
        }
        let lines: Vec<String> = describe_lines(&outcome)
            .into_iter()
            .map(|line| sanitize_for_terminal(&line))
            .collect();
        println!("{}", lines.join("\n"));
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::sheets::client::SHEETS_API_URL;
    use crate::utils::secret::Secret;
    use clap::CommandFactory;

    #[test]
    fn requires_a_range() {
        let err = DeleteDuplicatesCommand::command()
            .try_get_matches_from(["delete-duplicates", "sheet-1"])
            .unwrap_err();
        assert!(err.to_string().contains("--range"), "{err}");
    }

    #[test]
    fn accepts_repeated_comparison_columns() {
        let matches = DeleteDuplicatesCommand::command()
            .try_get_matches_from([
                "delete-duplicates",
                "sheet-1",
                "--range",
                "Q1!A1:C9",
                "--comparison-column",
                "0",
                "--comparison-column",
                "2",
            ])
            .unwrap();
        let columns: Vec<i64> = matches
            .get_many::<i64>("comparison_column")
            .unwrap()
            .copied()
            .collect();
        assert_eq!(columns, vec![0, 2]);
    }

    #[test]
    fn a_non_numeric_comparison_column_is_rejected_by_clap() {
        let err = DeleteDuplicatesCommand::command()
            .try_get_matches_from([
                "delete-duplicates",
                "sheet-1",
                "--range",
                "Q1!A1:C9",
                "--comparison-column",
                "B",
            ])
            .unwrap_err();
        assert!(err.to_string().contains('B'), "{err}");
    }

    fn test_credentials() -> DriveCredentials {
        DriveCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: DriveGrantedScopes::READONLY,
        }
    }

    async fn client(server: &wiremock::MockServer) -> DriveClient {
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/token"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "test-token", "expires_in": 3600,
                })),
            )
            .mount(server)
            .await;
        let mut client = DriveClient::new(&server.uri(), &test_credentials()).unwrap();
        crate::drive::client::test_support::replace_session(
            &mut client,
            &test_credentials(),
            &format!("{}/token", server.uri()),
        );
        client
    }

    fn allow_delete_duplicates_settings() -> serde_json::Value {
        serde_json::json!({
            "rules": [{
                "folder_id": "parent-1",
                "recursive": true,
                "allow": ["sheets-delete"],
                "require_lease": false,
            }],
        })
    }

    async fn mount_dry_run_prerequisites(server: &wiremock::MockServer) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/sheet-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "sheet-1", "name": "Budget",
                    "mimeType": "application/vnd.google-apps.spreadsheet",
                    "parents": ["parent-1"], "version": "1",
                })),
            )
            .mount(server)
            .await;
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
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/sheet-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "spreadsheetId": "sheet-1",
                    "sheets": [{"properties": {
                        "sheetId": 0, "title": "Q1",
                        "gridProperties": {"rowCount": 10, "columnCount": 3},
                    }}],
                })),
            )
            .mount(server)
            .await;
    }

    fn command(output: OutputFormat) -> DeleteDuplicatesCommand {
        DeleteDuplicatesCommand {
            spreadsheet_id: "sheet-1".to_string(),
            sheet: None,
            range: "Q1!A1:B2".to_string(),
            comparison_column: vec![0],
            dry_run: true,
            lease: helpers::LeaseTokenArg { lease: None },
            output,
        }
    }

    #[tokio::test]
    async fn command_execute_builds_options_and_reaches_the_dry_run_engine() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        crate::utils::settings::Settings::upsert_drive_account(
            &settings_path,
            "work",
            &[("write_permissions", allow_delete_duplicates_settings())],
        )
        .unwrap();

        let server = wiremock::MockServer::start().await;
        let client = client(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_dry_run_prerequisites(&server).await;

        command(OutputFormat::Table).execute(&client).await.unwrap();
    }

    #[tokio::test]
    async fn json_output_short_circuits_the_table_renderer() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        crate::utils::settings::Settings::upsert_drive_account(
            &settings_path,
            "work",
            &[("write_permissions", allow_delete_duplicates_settings())],
        )
        .unwrap();

        let server = wiremock::MockServer::start().await;
        let client = client(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_dry_run_prerequisites(&server).await;

        command(OutputFormat::Json).execute(&client).await.unwrap();
    }
}
