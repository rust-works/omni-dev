//! CLI surface for `omni-dev drive sheets sort-range` (issue #1842).

use anyhow::Result;
use clap::Parser;

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::cli::drive::helpers;
use crate::drive::client::DriveClient;
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::sort_range::{describe_lines, sort_range, SortRangeOptions};

/// Reorders the rows in a bounded range by one or more column keys.
///
/// `--sort-by` values use `COLUMN:asc` or `COLUMN:desc` and are applied in
/// the order given. This v1 command sorts by column values only; color-based
/// sort criteria are not yet supported. `--dry-run` does not read cells or
/// imitate Sheets' comparison rules: it reports the request and its record
/// integrity caveats instead.
#[derive(Parser)]
pub struct SortRangeCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Sheet (tab) title. Supplies the prefix for a bare `--range`.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// Fully bounded A1 range to sort, optionally carrying its own `Sheet!`
    /// prefix (for example `A2:D100`).
    #[arg(long, value_name = "A1")]
    pub range: String,

    /// A column sort key, as `COLUMN:asc` or `COLUMN:desc`. Repeat to add
    /// lower-precedence keys.
    #[arg(long, value_name = "COLUMN:ORDER", required = true)]
    pub sort_by: Vec<String>,

    /// Reports the gate verdict and request shape without calling
    /// `spreadsheets.batchUpdate` or reading cell values.
    #[arg(long)]
    pub dry_run: bool,

    #[command(flatten)]
    pub lease: crate::cli::drive::helpers::LeaseTokenArg,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl SortRangeCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = SortRangeOptions {
            spreadsheet_id: self.spreadsheet_id,
            sheet: self.sheet,
            range: Some(self.range),
            sort_by: self.sort_by,
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        let sheets = SheetsClient::from_drive_client(client)?;
        let rules = helpers::active_account_rules()?;
        let outcome = sort_range(client, &sheets, &opts, &rules).await;
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
    use clap::CommandFactory;

    #[test]
    fn requires_a_sort_key() {
        let err = SortRangeCommand::command()
            .try_get_matches_from(["sort-range", "sheet-1", "--range", "A1:B2"])
            .unwrap_err();
        assert!(err.to_string().contains("--sort-by"));
    }

    #[test]
    fn accepts_multiple_sort_keys() {
        SortRangeCommand::command()
            .try_get_matches_from([
                "sort-range",
                "sheet-1",
                "--range",
                "A1:B2",
                "--sort-by",
                "0:asc",
                "--sort-by",
                "1:desc",
            ])
            .unwrap();
    }

    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::sheets::client::SHEETS_API_URL;
    use crate::utils::secret::Secret;

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

    fn allow_sort_range_settings() -> serde_json::Value {
        serde_json::json!({
            "rules": [{
                "folder_id": "parent-1",
                "recursive": true,
                "allow": ["sheets-write"],
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
                    "sheets": [{"properties": {"sheetId": 0, "title": "Q1"}}],
                })),
            )
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn command_execute_builds_options_and_reaches_the_dry_run_engine() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        crate::utils::settings::Settings::upsert_drive_account(
            &settings_path,
            "work",
            &[("write_permissions", allow_sort_range_settings())],
        )
        .unwrap();

        let server = wiremock::MockServer::start().await;
        let client = client(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_dry_run_prerequisites(&server).await;

        SortRangeCommand {
            spreadsheet_id: "sheet-1".to_string(),
            sheet: None,
            range: "Q1!A1:B2".to_string(),
            sort_by: vec!["0:asc".to_string()],
            dry_run: true,
            lease: helpers::LeaseTokenArg { lease: None },
            output: OutputFormat::Table,
        }
        .execute(&client)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn json_output_short_circuits_the_table_renderer() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        crate::utils::settings::Settings::upsert_drive_account(
            &settings_path,
            "work",
            &[("write_permissions", allow_sort_range_settings())],
        )
        .unwrap();

        let server = wiremock::MockServer::start().await;
        let client = client(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_dry_run_prerequisites(&server).await;

        SortRangeCommand {
            spreadsheet_id: "sheet-1".to_string(),
            sheet: None,
            range: "Q1!A1:B2".to_string(),
            sort_by: vec!["0:asc".to_string()],
            dry_run: true,
            lease: helpers::LeaseTokenArg { lease: None },
            output: OutputFormat::Json,
        }
        .execute(&client)
        .await
        .unwrap();
    }
}
