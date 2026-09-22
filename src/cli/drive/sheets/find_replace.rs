//! CLI surface for `omni-dev drive sheets find-replace` (issue #1841).

use anyhow::Result;
use clap::Parser;

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::cli::drive::helpers;
use crate::drive::client::DriveClient;
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::find_replace::{describe_lines, find_replace, FindReplaceOptions};

/// Finds text and replaces it in a range, a sheet, or every sheet.
#[derive(Parser)]
pub struct FindReplaceCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Text to find. Cannot be empty.
    #[arg(long, value_name = "TEXT")]
    pub find: String,

    /// Replacement text. Pass an empty string to remove matches.
    #[arg(long, value_name = "TEXT", allow_hyphen_values = true)]
    pub replacement: String,

    /// A1 range to search, optionally carrying its own `Sheet!` prefix.
    #[arg(long, value_name = "A1")]
    pub range: Option<String>,

    /// Sheet (tab) title. Supplies the prefix for a bare `--range`, and is
    /// required with `--whole-sheet`.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// Search the whole sheet named by `--sheet`.
    #[arg(long)]
    pub whole_sheet: bool,

    /// Search every sheet in the workbook.
    #[arg(long)]
    pub all_sheets: bool,

    /// Match case exactly.
    #[arg(long)]
    pub match_case: bool,

    /// Require the search text to match the whole cell.
    #[arg(long)]
    pub match_entire_cell: bool,

    /// Interpret the search and replacement as Java regular-expression text.
    #[arg(long)]
    pub search_by_regex: bool,

    /// Include cells containing formulas. Without this flag, formula cells
    /// are skipped; Sheets has no formulas-only mode.
    #[arg(long)]
    pub include_formulas: bool,

    /// Reports the gate verdict and request scope without calling
    /// `spreadsheets.batchUpdate`. Sheets computes matching counts only when
    /// the request executes.
    #[arg(long)]
    pub dry_run: bool,

    #[command(flatten)]
    pub lease: crate::cli::drive::helpers::LeaseTokenArg,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl FindReplaceCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = FindReplaceOptions {
            spreadsheet_id: self.spreadsheet_id,
            find: self.find,
            replacement: self.replacement,
            sheet: self.sheet,
            range: self.range,
            whole_sheet: self.whole_sheet,
            all_sheets: self.all_sheets,
            match_case: self.match_case,
            match_entire_cell: self.match_entire_cell,
            search_by_regex: self.search_by_regex,
            include_formulas: self.include_formulas,
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_find_replace(client, &opts, &self.output).await
    }
}

async fn run_find_replace(
    client: &DriveClient,
    opts: &FindReplaceOptions,
    output: &OutputFormat,
) -> Result<()> {
    let sheets = SheetsClient::from_drive_client(client)?;
    let rules = helpers::active_account_rules()?;
    let outcome = find_replace(client, &sheets, opts, &rules).await;
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
    use std::path::PathBuf;

    use super::*;
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

    fn allow_find_replace_settings() -> serde_json::Value {
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
            &[("write_permissions", allow_find_replace_settings())],
        )
        .unwrap();

        let server = wiremock::MockServer::start().await;
        let client = client(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_dry_run_prerequisites(&server).await;

        FindReplaceCommand {
            spreadsheet_id: "sheet-1".to_string(),
            find: "draft".to_string(),
            replacement: "final".to_string(),
            range: Some("Q1!A1:B2".to_string()),
            sheet: None,
            whole_sheet: false,
            all_sheets: false,
            match_case: true,
            match_entire_cell: true,
            search_by_regex: true,
            include_formulas: true,
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
            &[("write_permissions", allow_find_replace_settings())],
        )
        .unwrap();

        let server = wiremock::MockServer::start().await;
        let client = client(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_dry_run_prerequisites(&server).await;
        let opts = FindReplaceOptions {
            spreadsheet_id: "sheet-1".to_string(),
            find: "draft".to_string(),
            replacement: "final".to_string(),
            sheet: None,
            range: Some("Q1!A1:B2".to_string()),
            whole_sheet: false,
            all_sheets: false,
            match_case: false,
            match_entire_cell: false,
            search_by_regex: false,
            include_formulas: false,
            dry_run: true,
            lease_token: None,
            ledger_path: PathBuf::default(),
        };

        run_find_replace(&client, &opts, &OutputFormat::Json)
            .await
            .unwrap();
    }
}
