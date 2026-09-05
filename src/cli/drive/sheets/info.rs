//! CLI command for `omni-dev drive sheets info`.

use anyhow::{Context, Result};
use clap::Parser;

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::drive::client::DriveClient;
use crate::drive::sheets::api::SheetsApi;
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::types::Spreadsheet;

/// Shows a spreadsheet's title and the sheets (tabs) it contains.
#[derive(Parser)]
pub struct InfoCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl InfoCommand {
    /// Runs the command, deriving a Sheets client from the shared Drive one.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let sheets = SheetsClient::from_drive_client(client)?;
        run_info(&sheets, &self.spreadsheet_id, &self.output).await
    }
}

/// Fetches the spreadsheet's metadata and renders it.
///
/// Split from [`InfoCommand::execute`] so tests can inject a wiremock client
/// without going through the credential-loading path.
async fn run_info(
    client: &SheetsClient,
    spreadsheet_id: &str,
    output: &OutputFormat,
) -> Result<()> {
    let spreadsheet = SheetsApi::new(client)
        .get_spreadsheet(spreadsheet_id)
        .await?;
    if output_as(&spreadsheet, output)? {
        return Ok(());
    }
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    render_info_table(&spreadsheet, &mut handle)
}

/// Renders the workbook as a bespoke header block plus one line per sheet —
/// a "table" in the sense of "one command, one rendering", matching
/// `crate::cli::drive::read::render_metadata_table`'s precedent.
fn render_info_table(spreadsheet: &Spreadsheet, out: &mut dyn std::io::Write) -> Result<()> {
    let ctx = "Failed to write sheets info";
    if let Some(id) = &spreadsheet.spreadsheet_id {
        writeln!(out, "Id: {}", sanitize_for_terminal(id)).context(ctx)?;
    }
    writeln!(out, "Title: {}", sanitize_for_terminal(spreadsheet.title())).context(ctx)?;
    writeln!(out, "Sheets: {}", spreadsheet.sheets.len()).context(ctx)?;

    for sheet in &spreadsheet.sheets {
        let Some(props) = &sheet.properties else {
            continue;
        };
        let mut line = format!("  {}", sanitize_for_terminal(&props.title));
        if let Some(grid) = &props.grid_properties {
            if let (Some(rows), Some(cols)) = (grid.row_count, grid.column_count) {
                line.push_str(&format!(" ({rows}x{cols})"));
            }
        }
        if sheet.hidden() {
            line.push_str(" [hidden]");
        }
        writeln!(out, "{line}").context(ctx)?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::sheets::client::SHEETS_API_URL;
    use crate::drive::sheets::types::{GridProperties, Sheet, SheetProperties};
    use crate::test_support::env::MapEnv;
    use crate::utils::secret::Secret;

    fn test_credentials() -> DriveCredentials {
        DriveCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: DriveGrantedScopes::READONLY,
        }
    }

    async fn sheets_client(server: &wiremock::MockServer) -> SheetsClient {
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
        let mut drive = DriveClient::new(&server.uri(), &test_credentials()).unwrap();
        crate::drive::client::test_support::replace_session(
            &mut drive,
            &test_credentials(),
            &format!("{}/token", server.uri()),
        );
        let env = MapEnv::new().with(SHEETS_API_URL, &server.uri());
        SheetsClient::from_drive_client_with(&env, &drive).unwrap()
    }

    fn sheet(title: &str, hidden: bool, grid: Option<(i64, i64)>) -> Sheet {
        Sheet {
            properties: Some(SheetProperties {
                title: title.to_string(),
                hidden: hidden.then_some(true),
                grid_properties: grid.map(|(rows, cols)| GridProperties {
                    row_count: Some(rows),
                    column_count: Some(cols),
                }),
                ..Default::default()
            }),
        }
    }

    // ── render_info_table ──────────────────────────────────────────────

    #[test]
    fn render_info_table_lists_every_sheet_with_dimensions() {
        let spreadsheet = Spreadsheet {
            spreadsheet_id: Some("s1".to_string()),
            properties: Some(crate::drive::sheets::types::SpreadsheetProperties {
                title: "Budget".to_string(),
            }),
            sheets: vec![
                sheet("Q1", false, Some((1000, 26))),
                sheet("Notes", true, None),
            ],
        };
        let mut buf = Vec::new();
        render_info_table(&spreadsheet, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("Id: s1"), "{text}");
        assert!(text.contains("Title: Budget"), "{text}");
        assert!(text.contains("Sheets: 2"), "{text}");
        assert!(text.contains("  Q1 (1000x26)"), "{text}");
        assert!(text.contains("  Notes [hidden]"), "{text}");
    }

    #[test]
    fn render_info_table_strips_control_bytes_from_server_strings() {
        // Sheet titles are attacker-influenced text rendered as chrome.
        let spreadsheet = Spreadsheet {
            spreadsheet_id: Some("s\x1b[31m1".to_string()),
            properties: Some(crate::drive::sheets::types::SpreadsheetProperties {
                title: "evil\x1b[31mbook".to_string(),
            }),
            sheets: vec![sheet("tab\x1b[0mname", false, None)],
        };
        let mut buf = Vec::new();
        render_info_table(&spreadsheet, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(
            !text.contains(|c: char| c.is_control() && c != '\n'),
            "{text:?}"
        );
        assert!(text.contains("evil[31mbook"), "{text}");
    }

    #[test]
    fn render_info_table_handles_a_workbook_with_no_sheets() {
        let mut buf = Vec::new();
        render_info_table(&Spreadsheet::default(), &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("Sheets: 0"), "{text}");
    }

    // ── run_info ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn run_info_table_path_requests_a_fields_mask() {
        let server = wiremock::MockServer::start().await;
        let client = sheets_client(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/s1"))
            // Without this mask the response carries every cell of every
            // sheet; asserting it here keeps that from silently regressing.
            .and(wiremock::matchers::query_param_contains(
                "fields",
                "sheets.properties",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "spreadsheetId": "s1",
                    "properties": {"title": "Book"},
                    "sheets": [{"properties": {"title": "Q1"}}],
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        run_info(&client, "s1", &OutputFormat::Table).await.unwrap();
    }

    #[tokio::test]
    async fn run_info_json_path_returns_ok() {
        let server = wiremock::MockServer::start().await;
        let client = sheets_client(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/s1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"spreadsheetId": "s1"})),
            )
            .mount(&server)
            .await;

        run_info(&client, "s1", &OutputFormat::Json).await.unwrap();
    }

    #[tokio::test]
    async fn run_info_propagates_a_not_found_error() {
        let server = wiremock::MockServer::start().await;
        let client = sheets_client(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/missing"))
            .respond_with(
                wiremock::ResponseTemplate::new(404).set_body_json(serde_json::json!({
                    "error": {"code": 404, "message": "Requested entity was not found.",
                              "status": "NOT_FOUND"},
                })),
            )
            .mount(&server)
            .await;

        let err = run_info(&client, "missing", &OutputFormat::Table)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }
}
