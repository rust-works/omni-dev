//! CLI command for `omni-dev drive sheets read-cell-format` (issue #1878).
//!
//! Read-only and ungated, like `sheets read` and every `list-*` verb — see
//! [ADR-0071](../../../../docs/adrs/adr-0071.md) §11.

use anyhow::Result;
use clap::Parser;

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::drive::client::DriveClient;
use crate::drive::sheets::api::SheetsApi;
use crate::drive::sheets::cell_format::{read_cell_formats, render_line, ReadCellFormatOptions};
use crate::drive::sheets::client::SheetsClient;

/// Reads a range's cell-level formatting back — background, text format,
/// number format, horizontal alignment, notes and data validation.
///
/// Reports only `userEnteredFormat`, never `effectiveFormat`: what a sort,
/// fill or paste physically moves is the user-entered format, while
/// `effectiveFormat` folds in conditional formatting, which follows the
/// *range* rather than the cell and would give false positives. This is the
/// tool [ADR-0083](../../../../docs/adrs/adr-0083.md) §5 relies on to answer
/// whether a verb moves formatting — run it before and after a verb and
/// diff the two outputs.
#[derive(Parser)]
pub struct ReadCellFormatCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Sheet (tab) title. Supplies the prefix for a bare `--range`.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// A1 range to read, optionally carrying its own `Sheet!` prefix (e.g.
    /// `A1:D10`, `'My Sheet'!A1:D10`). Required — a whole-workbook read is
    /// out of scope, since the response grows with the range requested.
    #[arg(long, value_name = "A1")]
    pub range: String,

    /// Output format. The default `table` emits one compact, diff-friendly
    /// line per non-default cell (e.g. `B3  bg=#FF0000 bold note`); a cell
    /// carrying none of the reported properties is never listed. Use
    /// `json`/`yaml` for the full structured format, including note text.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ReadCellFormatCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let sheets = SheetsClient::from_drive_client(client)?;
        let opts = ReadCellFormatOptions {
            spreadsheet_id: self.spreadsheet_id,
            sheet: self.sheet,
            range: self.range,
        };
        run_read_cell_format(&sheets, &opts, &self.output).await
    }
}

/// Reads and renders. Split from [`ReadCellFormatCommand::execute`] so tests
/// can inject a wiremock client without going through the credential-loading
/// path — the same split `read.rs::run_read` uses.
async fn run_read_cell_format(
    client: &SheetsClient,
    opts: &ReadCellFormatOptions,
    output: &OutputFormat,
) -> Result<()> {
    let outcome = read_cell_formats(&SheetsApi::new(client), opts).await?;
    if output_as(&outcome, output)? {
        return Ok(());
    }
    for sheet in &outcome.sheets {
        for entry in &sheet.cells {
            println!("{}", sanitize_for_terminal(&render_line(entry)));
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
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

    async fn client_with_bootstrapped_token(server: &wiremock::MockServer) -> DriveClient {
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/token"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "test-token",
                    "expires_in": 3600,
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

    fn mount_cell_format_response() -> wiremock::Mock {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/sheet-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "spreadsheetId": "sheet-1",
                    "properties": {"title": "Budget"},
                    "sheets": [{
                        "properties": {"sheetId": 0, "title": "Q1"},
                        "data": [{
                            "startRow": 0,
                            "startColumn": 0,
                            "rowData": [
                                {"values": [{}, {}]},
                                {"values": [{}, {
                                    "userEnteredFormat": {
                                        "backgroundColorStyle": {"rgbColor": {"red": 1}},
                                        "textFormat": {"bold": true},
                                    },
                                    "note": "check this",
                                }]},
                            ],
                        }],
                    }],
                })),
            )
    }

    #[tokio::test]
    async fn read_cell_format_table_output_prints_only_non_default_cells() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_cell_format_response().mount(&server).await;

        let opts = ReadCellFormatOptions {
            spreadsheet_id: "sheet-1".to_string(),
            sheet: Some("Q1".to_string()),
            range: "A1:B2".to_string(),
        };
        let sheets = SheetsClient::from_drive_client(&client).unwrap();
        let outcome = read_cell_formats(&SheetsApi::new(&sheets), &opts)
            .await
            .unwrap();
        assert_eq!(outcome.sheets.len(), 1);
        assert_eq!(outcome.sheets[0].cells.len(), 1);
        assert_eq!(outcome.sheets[0].cells[0].cell, "B2");
        // Exercise the CLI-level table path too.
        assert!(run_read_cell_format(&sheets, &opts, &OutputFormat::Table)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn read_cell_format_yaml_output_short_circuits_before_printing_lines() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_cell_format_response().mount(&server).await;

        let cmd = ReadCellFormatCommand {
            spreadsheet_id: "sheet-1".to_string(),
            sheet: Some("Q1".to_string()),
            range: "A1:B2".to_string(),
            output: OutputFormat::Yaml,
        };
        assert!(cmd.execute(&client).await.is_ok());
    }

    #[tokio::test]
    async fn read_cell_format_refuses_an_over_budget_range_before_any_http_call() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        // Deliberately mount no `spreadsheets.get` response: a passing test
        // proves the refusal never reaches the network.

        let cmd = ReadCellFormatCommand {
            spreadsheet_id: "sheet-1".to_string(),
            sheet: None,
            range: "A1:ALL1000".to_string(),
            output: OutputFormat::Table,
        };
        let err = cmd.execute(&client).await.unwrap_err();
        assert!(err.to_string().contains("cell budget"), "{err}");
    }

    #[tokio::test]
    async fn read_cell_format_refuses_an_empty_range() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());

        let cmd = ReadCellFormatCommand {
            spreadsheet_id: "sheet-1".to_string(),
            sheet: Some("Q1".to_string()),
            range: "   ".to_string(),
            output: OutputFormat::Table,
        };
        let err = cmd.execute(&client).await.unwrap_err();
        assert!(err.to_string().contains("must not be empty"), "{err}");
    }
}
