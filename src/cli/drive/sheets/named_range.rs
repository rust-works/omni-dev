//! CLI commands for `omni-dev drive sheets add-named-range`/
//! `update-named-range`/`delete-named-range`/`list-named-ranges` (issue
//! #1796).
//!
//! The first three are gated by
//! [`DriveOperation::SheetsStructure`](crate::drive::write_gate::DriveOperation::SheetsStructure)
//! — see [ADR-0081](../../../../docs/adrs/adr-0081.md) §2 for why
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
use crate::drive::sheets::{render_grid_range, sheet_title_by_id};

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
/// target is resolved by a case-insensitive exact name match — see
/// `drive sheets list-named-ranges` to find it.
#[derive(Parser)]
pub struct UpdateNamedRangeCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// The existing name to change, by case-insensitive exact match.
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

/// Removes a named range. The target is resolved by a case-insensitive
/// exact name match — see `drive sheets list-named-ranges` to find it.
///
/// Every cell formula referencing the removed name starts evaluating to
/// `#NAME?` (conditional formatting, data validation and chart references
/// are not scanned). `--dry-run` (and the real run, before mutating)
/// reports the count and A1 locations of every such cell formula — read it
/// before running for real.
#[derive(Parser)]
pub struct DeleteNamedRangeCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// The existing name to remove, by case-insensitive exact match.
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
            let sheet = sheet_title_by_id(&workbook, named.range.sheet_id)
                .unwrap_or_else(|| "?".to_string());
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
    use crate::utils::secret::Secret;

    // `render_grid_range`/`sheet_title_by_id` are tested in `grid_range.rs`,
    // the module they're shared from.

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

    #[tokio::test]
    async fn list_named_ranges_yaml_output_short_circuits_before_printing_lines() {
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
                    "namedRanges": [],
                })),
            )
            .mount(&server)
            .await;

        let cmd = ListNamedRangesCommand {
            spreadsheet_id: "sheet-1".to_string(),
            output: crate::cli::drive::format::OutputFormat::Yaml,
        };
        assert!(cmd.execute(&client).await.is_ok());
    }

    /// Mounts a Drive-file/parent-folder pair with no write-permission rules
    /// configured (`active_account_rules()` reads an unconfigured account —
    /// see `client_with_bootstrapped_token`'s `EnvGuard::clear_credentials`
    /// caller), so the gate refuses by default policy. That's enough to
    /// drive `Add`/`Update`/`DeleteNamedRangeCommand::execute` (and thus
    /// `run_named_range`) through their full CLI-level path — building
    /// `NamedRangeOptions`, calling `named_range`, and rendering the
    /// `describe_lines` output — without needing a lease or a workbook
    /// fetch, which a `Blocked` verdict never reaches.
    async fn mount_ungated_target(server: &wiremock::MockServer) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/sheet-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "sheet-1",
                    "name": "Budget",
                    "mimeType": crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
                    "parents": ["folder-1"],
                })),
            )
            .mount(server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/folder-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "folder-1",
                    "name": "folder-1",
                    "mimeType": "application/vnd.google-apps.folder",
                    "parents": [],
                })),
            )
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn add_named_range_command_runs_end_to_end() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_ungated_target(&server).await;

        let cmd = AddNamedRangeCommand {
            spreadsheet_id: "sheet-1".to_string(),
            name: "Foo".to_string(),
            range: Some("A1:A5".to_string()),
            sheet: Some("Q1".to_string()),
            whole_sheet: false,
            dry_run: true,
            lease: crate::cli::drive::helpers::LeaseTokenArg { lease: None },
            output: crate::cli::drive::format::OutputFormat::Table,
        };
        assert!(cmd.execute(&client).await.is_ok());
    }

    #[tokio::test]
    async fn update_named_range_command_runs_end_to_end() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_ungated_target(&server).await;

        let cmd = UpdateNamedRangeCommand {
            spreadsheet_id: "sheet-1".to_string(),
            name: "Foo".to_string(),
            new_name: Some("Bar".to_string()),
            range: None,
            sheet: None,
            whole_sheet: false,
            dry_run: true,
            lease: crate::cli::drive::helpers::LeaseTokenArg { lease: None },
            output: crate::cli::drive::format::OutputFormat::Table,
        };
        assert!(cmd.execute(&client).await.is_ok());
    }

    #[tokio::test]
    async fn delete_named_range_command_runs_end_to_end() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_ungated_target(&server).await;

        let cmd = DeleteNamedRangeCommand {
            spreadsheet_id: "sheet-1".to_string(),
            name: "Foo".to_string(),
            dry_run: true,
            lease: crate::cli::drive::helpers::LeaseTokenArg { lease: None },
            output: crate::cli::drive::format::OutputFormat::Yaml,
        };
        assert!(cmd.execute(&client).await.is_ok());
    }
}
