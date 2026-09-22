//! CLI surface for `omni-dev drive sheets trim-whitespace` (issue #1844).

use anyhow::Result;
use clap::Parser;

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::cli::drive::helpers;
use crate::drive::client::DriveClient;
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::trim_whitespace::{
    describe_lines, trim_whitespace, TrimScope, TrimWhitespaceOptions,
};

/// Trims whitespace in every cell of a range, or of a whole sheet.
///
/// Sheets owns the trim rule, so `--dry-run` never claims which cells will
/// change: it reports the count and A1 locations of the range's non-blank
/// cells, any of which *may* be trimmed, and never their values. The real
/// run reports the server's own count of cells actually changed.
#[derive(Parser)]
pub struct TrimWhitespaceCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Sheet (tab) title. Supplies the prefix for a bare `--range`, and is
    /// required with `--whole-sheet`.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// A1 range to trim, optionally carrying its own `Sheet!` prefix (for
    /// example `A2:D100`). An open-ended range (`A:A`) is completed from
    /// the sheet's current grid extent.
    #[arg(long, value_name = "A1", conflicts_with = "whole_sheet")]
    pub range: Option<String>,

    /// Trim every cell of the sheet named by `--sheet`.
    #[arg(long, requires = "sheet")]
    pub whole_sheet: bool,

    /// Reports the gate verdict and the cells that may be trimmed, without
    /// calling `spreadsheets.batchUpdate`.
    #[arg(long)]
    pub dry_run: bool,

    #[command(flatten)]
    pub lease: crate::cli::drive::helpers::LeaseTokenArg,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl TrimWhitespaceCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = TrimWhitespaceOptions {
            spreadsheet_id: self.spreadsheet_id,
            sheet: self.sheet,
            range: self.range,
            scope: if self.whole_sheet {
                TrimScope::WholeSheet
            } else {
                TrimScope::Range
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        let sheets = SheetsClient::from_drive_client(client)?;
        let rules = helpers::active_account_rules()?;
        let outcome = trim_whitespace(client, &sheets, &opts, &rules).await;
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
    fn range_and_whole_sheet_are_mutually_exclusive() {
        let err = TrimWhitespaceCommand::command()
            .try_get_matches_from([
                "trim-whitespace",
                "sheet-1",
                "--sheet",
                "Q1",
                "--range",
                "A1:B2",
                "--whole-sheet",
            ])
            .unwrap_err();
        assert!(err.to_string().contains("--range"), "{err}");
    }

    #[test]
    fn whole_sheet_requires_a_sheet_at_the_clap_layer_too() {
        let err = TrimWhitespaceCommand::command()
            .try_get_matches_from(["trim-whitespace", "sheet-1", "--whole-sheet"])
            .unwrap_err();
        assert!(err.to_string().contains("--sheet"), "{err}");
    }

    #[test]
    fn a_bare_range_is_accepted_without_a_sheet() {
        TrimWhitespaceCommand::command()
            .try_get_matches_from(["trim-whitespace", "sheet-1", "--range", "Q1!A1:B2"])
            .unwrap();
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

    fn allow_trim_whitespace_settings() -> serde_json::Value {
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
                    "sheets": [{"properties": {
                        "sheetId": 0, "title": "Q1",
                        "gridProperties": {"rowCount": 10, "columnCount": 3},
                    }}],
                })),
            )
            .mount(server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path_regex(
                r"^/v4/spreadsheets/sheet-1/values/.*$",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"values": [["a"]]})),
            )
            .mount(server)
            .await;
    }

    fn command(whole_sheet: bool, output: OutputFormat) -> TrimWhitespaceCommand {
        TrimWhitespaceCommand {
            spreadsheet_id: "sheet-1".to_string(),
            sheet: whole_sheet.then(|| "Q1".to_string()),
            range: (!whole_sheet).then(|| "Q1!A1:B2".to_string()),
            whole_sheet,
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
            &[("write_permissions", allow_trim_whitespace_settings())],
        )
        .unwrap();

        let server = wiremock::MockServer::start().await;
        let client = client(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_dry_run_prerequisites(&server).await;

        command(false, OutputFormat::Table)
            .execute(&client)
            .await
            .unwrap();
    }

    /// `--whole-sheet` takes the other branch of the scope mapping, so it
    /// needs its own pass through `execute`.
    #[tokio::test]
    async fn whole_sheet_reaches_the_engine_as_the_whole_sheet_scope() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        crate::utils::settings::Settings::upsert_drive_account(
            &settings_path,
            "work",
            &[("write_permissions", allow_trim_whitespace_settings())],
        )
        .unwrap();

        let server = wiremock::MockServer::start().await;
        let client = client(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_dry_run_prerequisites(&server).await;

        command(true, OutputFormat::Table)
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
            &[("write_permissions", allow_trim_whitespace_settings())],
        )
        .unwrap();

        let server = wiremock::MockServer::start().await;
        let client = client(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_dry_run_prerequisites(&server).await;

        command(false, OutputFormat::Json)
            .execute(&client)
            .await
            .unwrap();
    }
}
