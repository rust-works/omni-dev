//! CLI commands for `omni-dev drive sheets add-named-range`/
//! `update-named-range`/`delete-named-range`/`list-named-ranges` (issue
//! #1796).
//!
//! The first three are gated by
//! [`DriveOperation::SheetsStructure`](crate::drive::write_gate::DriveOperation::SheetsStructure)
//! — see [ADR-0081](../../../../../docs/adrs/adr-0081.md) §2 for why
//! `delete-named-range` stays here rather than joining `sheets-delete`.
//! `list-named-ranges` is a plain read, ungated like `sheets
//! list-protections`.

use anyhow::Result;
use clap::Parser;

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::cli::drive::helpers;
use crate::drive::client::DriveClient;
use crate::drive::sheets::api::SheetsApi;
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::named_range::{
    describe_lines, named_range, NamedRangeOptions, NamedRangeVerb,
};

/// Adds a named range (or, with `--whole-sheet`, one covering an entire
/// sheet).
#[derive(Parser)]
pub struct AddNamedRangeCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// The name to create. Must be unique workbook-wide.
    #[arg(long, value_name = "NAME")]
    pub name: String,

    /// A1 range the name refers to, optionally carrying its own `Sheet!`
    /// prefix. Mutually exclusive with `--whole-sheet`.
    #[arg(long, value_name = "A1")]
    pub range: Option<String>,

    /// Sheet (tab) title. Supplies the prefix for a bare `--range`, or the
    /// target sheet directly with `--whole-sheet`.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// Cover the entire sheet named by `--sheet`, rather than a range
    /// within it.
    #[arg(long)]
    pub whole_sheet: bool,

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

impl AddNamedRangeCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = NamedRangeOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: NamedRangeVerb::AddNamedRange {
                name: self.name,
                sheet: self.sheet,
                range: self.range,
                whole_sheet: self.whole_sheet,
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_named_range(client, &opts, &self.output).await
    }
}

/// Changes an existing named range's name and/or the range it covers. The
/// target is resolved by exact name match — see
/// `drive sheets list-named-ranges` to find it.
#[derive(Parser)]
pub struct UpdateNamedRangeCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// The existing name to change, by exact match.
    #[arg(long, value_name = "NAME")]
    pub name: String,

    /// The new name, when renaming.
    #[arg(long, value_name = "NAME")]
    pub new_name: Option<String>,

    /// A1 range for the new target, optionally carrying its own `Sheet!`
    /// prefix. Mutually exclusive with `--whole-sheet`. Omitting this,
    /// `--sheet` and `--whole-sheet` together keeps the existing range
    /// unchanged — this command may be a rename only.
    #[arg(long, value_name = "A1")]
    pub range: Option<String>,

    /// Sheet (tab) title. Supplies the prefix for a bare `--range`, or the
    /// target sheet directly with `--whole-sheet`.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// Re-point at the entire sheet named by `--sheet`, rather than a range
    /// within it.
    #[arg(long)]
    pub whole_sheet: bool,

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

impl UpdateNamedRangeCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = NamedRangeOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: NamedRangeVerb::UpdateNamedRange {
                name: self.name,
                new_name: self.new_name,
                sheet: self.sheet,
                range: self.range,
                whole_sheet: self.whole_sheet,
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_named_range(client, &opts, &self.output).await
    }
}

/// Removes a named range. The target is resolved by exact name match — see
/// `drive sheets list-named-ranges` to find it.
///
/// Every formula referencing the removed name starts evaluating to
/// `#NAME?`. `--dry-run` (and the real run, before mutating) reports the
/// count and A1 locations of every such formula — read it before running
/// for real.
#[derive(Parser)]
pub struct DeleteNamedRangeCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// The existing name to remove, by exact match.
    #[arg(long, value_name = "NAME")]
    pub name: String,

    /// Reports the gate verdict and the referencing-formula preview,
    /// without calling `spreadsheets.batchUpdate`.
    #[arg(long)]
    pub dry_run: bool,

    #[command(flatten)]
    pub lease: crate::cli::drive::helpers::LeaseTokenArg,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl DeleteNamedRangeCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = NamedRangeOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: NamedRangeVerb::DeleteNamedRange { name: self.name },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_named_range(client, &opts, &self.output).await
    }
}

/// Lists the named ranges in a spreadsheet.
///
/// Read-only and ungated, like `sheets list-protections` — needed so
/// `update-named-range`/`delete-named-range` are usable at all, since a
/// named range's current extent is otherwise invisible from the CLI.
#[derive(Parser)]
pub struct ListNamedRangesCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ListNamedRangesCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let sheets = SheetsClient::from_drive_client(client)?;
        let api = SheetsApi::new(&sheets);
        let workbook = api
            .get_spreadsheet_with_named_ranges(&self.spreadsheet_id)
            .await?;
        if output_as(&workbook, &self.output)? {
            return Ok(());
        }
        for named in &workbook.named_ranges {
            let id = named.named_range_id.as_deref().unwrap_or("?");
            let sheet = sheet_title(&workbook, named.range.sheet_id);
            println!(
                "{}",
                sanitize_for_terminal(&format!(
                    "id {id}: {}  {}  sheet={sheet:?}",
                    named.name,
                    render_grid_range(&named.range)
                ))
            );
        }
        Ok(())
    }
}

/// The title of the sheet a [`GridRange`](crate::drive::sheets::types::GridRange)'s
/// `sheet_id` names, or `"?"` if the workbook carries no sheet with that id
/// (a `spreadsheets.get`/`fields`-mask mismatch this command never expects
/// in practice, but must still render something for).
fn sheet_title(workbook: &crate::drive::sheets::types::Spreadsheet, sheet_id: i64) -> String {
    workbook
        .sheets
        .iter()
        .find(|sheet| sheet.sheet_id() == Some(sheet_id))
        .map_or_else(|| "?".to_string(), |sheet| sheet.title().to_string())
}

/// Renders a numeric [`GridRange`](crate::drive::sheets::types::GridRange)
/// as a compact 1-based description for `list-named-ranges`' human-readable
/// output — e.g. `"sheetId 0, rows 1-5, cols 1-2"`, or `"sheetId 0 (whole
/// sheet)"` when every bound is `None`. Same shape as
/// `protection.rs::render_grid_range` (numeric, not A1-lettered: the
/// column-letter conversion is `drive::sheets::grid_range`-private, and
/// every other Sheets list verb already renders this way).
fn render_grid_range(range: &crate::drive::sheets::types::GridRange) -> String {
    let rows = match (range.start_row_index, range.end_row_index) {
        (Some(start), Some(end)) => format!(", rows {}-{end}", start + 1),
        _ => String::new(),
    };
    let cols = match (range.start_column_index, range.end_column_index) {
        (Some(start), Some(end)) => format!(", cols {}-{}", start + 1, end),
        _ => String::new(),
    };
    if rows.is_empty() && cols.is_empty() {
        format!("sheetId {} (whole sheet)", range.sheet_id)
    } else {
        format!("sheetId {}{rows}{cols}", range.sheet_id)
    }
}

async fn run_named_range(
    client: &DriveClient,
    opts: &NamedRangeOptions,
    output: &OutputFormat,
) -> Result<()> {
    let sheets = SheetsClient::from_drive_client(client)?;
    let rules = helpers::active_account_rules()?;
    let outcome = named_range(client, &sheets, opts, &rules).await;
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
    use crate::drive::sheets::client::SHEETS_API_URL;
    use crate::drive::sheets::types::{GridRange, Sheet, SheetProperties, Spreadsheet};
    use crate::utils::secret::Secret;

    fn workbook_with_one_sheet(sheet_id: i64, title: &str) -> Spreadsheet {
        Spreadsheet {
            sheets: vec![Sheet {
                properties: Some(SheetProperties {
                    sheet_id: Some(sheet_id),
                    title: title.to_string(),
                    ..Default::default()
                }),
                protected_ranges: Vec::new(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn sheet_title_finds_the_matching_sheet() {
        let workbook = workbook_with_one_sheet(0, "Q1");
        assert_eq!(sheet_title(&workbook, 0), "Q1");
        assert_eq!(sheet_title(&workbook, 99), "?");
    }

    #[test]
    fn render_grid_range_whole_sheet_when_all_bounds_none() {
        let range = GridRange {
            sheet_id: 0,
            ..Default::default()
        };
        assert_eq!(render_grid_range(&range), "sheetId 0 (whole sheet)");
    }

    #[test]
    fn render_grid_range_rows_and_cols() {
        let range = GridRange {
            sheet_id: 0,
            start_row_index: Some(0),
            end_row_index: Some(5),
            start_column_index: Some(0),
            end_column_index: Some(2),
        };
        assert_eq!(render_grid_range(&range), "sheetId 0, rows 1-5, cols 1-2");
    }

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

    #[tokio::test]
    async fn list_named_ranges_prints_every_named_range() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/sheet-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "spreadsheetId": "sheet-1",
                    "properties": {"title": "Budget"},
                    "sheets": [{"properties": {"sheetId": 0, "title": "Sheet1"}}],
                    "namedRanges": [
                        {
                            "namedRangeId": "id-1",
                            "name": "Foo",
                            "range": {
                                "sheetId": 0,
                                "startRowIndex": 0,
                                "endRowIndex": 5,
                                "startColumnIndex": 0,
                                "endColumnIndex": 2,
                            },
                        },
                        {"namedRangeId": "id-2", "name": "Bar", "range": {"sheetId": 0}},
                    ],
                })),
            )
            .mount(&server)
            .await;

        let cmd = ListNamedRangesCommand {
            spreadsheet_id: "sheet-1".to_string(),
            output: crate::cli::drive::format::OutputFormat::Table,
        };
        assert!(cmd.execute(&client).await.is_ok());
    }
}
