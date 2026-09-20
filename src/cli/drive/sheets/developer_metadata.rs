//! CLI commands for `omni-dev drive sheets set-developer-metadata`/
//! `delete-developer-metadata`/`search-developer-metadata` (issue #1795).
//!
//! `--sheet`/`--dimension`/`--start`/`--end` are shared, optional flags on
//! all three commands, following the same combination `resolve_location`
//! interprets: none given means spreadsheet-scoped, `--sheet` alone means
//! sheet-scoped, all four together mean a row/column span. There is no
//! `--visibility` flag anywhere — see [`crate::drive::sheets::types::DOCUMENT_VISIBILITY`]'s
//! doc comment for why.

use anyhow::Result;
use clap::Parser;

use crate::cli::drive::format::{output_as, OutputFormat};
use crate::cli::drive::helpers;
use crate::cli::drive::sheets::format::DimensionArg;
use crate::cli::format::sanitize_for_terminal;
use crate::drive::client::DriveClient;
use crate::drive::sheets::api::SheetsApi;
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::developer_metadata::{
    describe_lines, developer_metadata, search, DeveloperMetadataOptions, DeveloperMetadataVerb,
    SearchLocationFilter,
};

/// Creates a new developer-metadata key/value pair, or updates every
/// existing entry matching the same key and location.
#[derive(Parser)]
pub struct SetDeveloperMetadataCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// The key to set.
    #[arg(long, value_name = "KEY")]
    pub key: String,

    /// The value to write.
    #[arg(long, value_name = "VALUE")]
    pub value: String,

    /// Title of the sheet, when the location is sheet- or dimension-scoped.
    /// Omit entirely, along with `--dimension`/`--start`/`--end`, for a
    /// spreadsheet-scoped entry.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// Rows or columns, when the location is a row/column span. Requires
    /// `--sheet`, `--start` and `--end` together.
    #[arg(long, value_enum)]
    pub dimension: Option<DimensionArg>,

    /// 1-based first row/column, inclusive.
    #[arg(long, value_name = "N")]
    pub start: Option<i64>,

    /// 1-based last row/column, inclusive.
    #[arg(long, value_name = "N")]
    pub end: Option<i64>,

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

impl SetDeveloperMetadataCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = DeveloperMetadataOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: DeveloperMetadataVerb::Set {
                key: self.key,
                value: self.value,
                sheet: self.sheet,
                dimension: self.dimension.map(DimensionArg::engine),
                start: self.start,
                end: self.end,
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_developer_metadata(client, &opts, &self.output).await
    }
}

/// Removes every developer-metadata entry matching a key and location,
/// after reporting what would be removed.
#[derive(Parser)]
pub struct DeleteDeveloperMetadataCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// The key to remove.
    #[arg(long, value_name = "KEY")]
    pub key: String,

    /// Title of the sheet, when the location is sheet- or dimension-scoped.
    /// Omit entirely, along with `--dimension`/`--start`/`--end`, for a
    /// spreadsheet-scoped entry.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// Rows or columns, when the location is a row/column span. Requires
    /// `--sheet`, `--start` and `--end` together.
    #[arg(long, value_enum)]
    pub dimension: Option<DimensionArg>,

    /// 1-based first row/column, inclusive.
    #[arg(long, value_name = "N")]
    pub start: Option<i64>,

    /// 1-based last row/column, inclusive.
    #[arg(long, value_name = "N")]
    pub end: Option<i64>,

    /// Reports the gate verdict and every entry that would be removed,
    /// without calling `spreadsheets.batchUpdate`.
    #[arg(long)]
    pub dry_run: bool,

    #[command(flatten)]
    pub lease: crate::cli::drive::helpers::LeaseTokenArg,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl DeleteDeveloperMetadataCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = DeveloperMetadataOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: DeveloperMetadataVerb::Delete {
                key: self.key,
                sheet: self.sheet,
                dimension: self.dimension.map(DimensionArg::engine),
                start: self.start,
                end: self.end,
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_developer_metadata(client, &opts, &self.output).await
    }
}

/// Shared tail for `set-developer-metadata`/`delete-developer-metadata`,
/// mirroring `format.rs`'s CLI leaf's `run_format`.
async fn run_developer_metadata(
    client: &DriveClient,
    opts: &DeveloperMetadataOptions,
    output: &OutputFormat,
) -> Result<()> {
    let sheets = SheetsClient::from_drive_client(client)?;
    let rules = helpers::active_account_rules()?;
    let outcome = developer_metadata(client, &sheets, opts, &rules).await;
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

/// Searches for developer-metadata entries by key and/or location.
///
/// Restricted to `DOCUMENT` visibility. Read-only and ungated, like
/// `sheets info` — needed so a caller can discover an entry's exact key
/// and location before `set-developer-metadata`/`delete-developer-metadata`.
#[derive(Parser)]
pub struct SearchDeveloperMetadataCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Restrict to this key. Omit to match every key.
    #[arg(long, value_name = "KEY")]
    pub key: Option<String>,

    /// Title of the sheet, when restricting the search to a sheet or
    /// dimension span. Omit entirely, along with `--dimension`/`--start`/`--end`,
    /// to search the whole workbook.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// Rows or columns, when restricting the search to a row/column span.
    #[arg(long, value_enum)]
    pub dimension: Option<DimensionArg>,

    /// 1-based first row/column, inclusive.
    #[arg(long, value_name = "N")]
    pub start: Option<i64>,

    /// 1-based last row/column, inclusive.
    #[arg(long, value_name = "N")]
    pub end: Option<i64>,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl SearchDeveloperMetadataCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let sheets = SheetsClient::from_drive_client(client)?;
        let api = SheetsApi::new(&sheets);
        let workbook = api.get_spreadsheet(&self.spreadsheet_id).await?;
        let entries = search(
            &api,
            &self.spreadsheet_id,
            &workbook,
            self.key.as_deref(),
            SearchLocationFilter {
                sheet: self.sheet.as_deref(),
                dimension: self.dimension.map(DimensionArg::engine),
                start: self.start,
                end: self.end,
            },
        )
        .await
        .map_err(|detail| anyhow::anyhow!("{detail}"))?;
        if output_as(&entries, &self.output)? {
            return Ok(());
        }
        if entries.is_empty() {
            println!("No matching developer metadata.");
            return Ok(());
        }
        for entry in &entries {
            println!(
                "{}",
                sanitize_for_terminal(&format!(
                    "id {}: {:?}={:?} at {}",
                    entry.metadata_id, entry.key, entry.value, entry.location
                ))
            );
        }
        Ok(())
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::utils::secret::Secret;

    fn test_credentials() -> DriveCredentials {
        DriveCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: DriveGrantedScopes::READONLY,
        }
    }

    fn dead_client() -> DriveClient {
        DriveClient::new("http://127.0.0.1:1", &test_credentials()).unwrap()
    }

    /// A `DriveClient` pointed at `server`, whose token endpoint is already
    /// mocked. Callers must additionally point `SHEETS_API_URL` at the same
    /// server, since every command here derives its `SheetsClient`
    /// internally from the real process environment (mirrors
    /// `create.rs::client_with_bootstrapped_token`).
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

    fn mount_workbook() -> wiremock::Mock {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/sheet-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "spreadsheetId": "sheet-1",
                    "properties": {"title": "Budget"},
                    "sheets": [
                        {"properties": {"sheetId": 0, "title": "Q1", "index": 0}},
                    ],
                })),
            )
    }

    fn mount_search(body: serde_json::Value) -> wiremock::Mock {
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/developerMetadata:search",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
    }

    fn search_cmd(
        key: Option<&str>,
        sheet: Option<&str>,
        output: OutputFormat,
    ) -> SearchDeveloperMetadataCommand {
        SearchDeveloperMetadataCommand {
            spreadsheet_id: "sheet-1".to_string(),
            key: key.map(str::to_string),
            sheet: sheet.map(str::to_string),
            dimension: None,
            start: None,
            end: None,
            output,
        }
    }

    #[tokio::test]
    async fn set_command_execute_with_no_configured_account_reports_blocked() {
        // No `SheetsClient` HTTP call ever succeeds here: with every
        // credential cleared, `active_account_rules` returns an empty rule
        // set and the target's own metadata fetch against a dead port
        // fails fast, both of which `developer_metadata` reports as a
        // printed outcome rather than an `Err` — see
        // `run_developer_metadata`'s doc comment.
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let cmd = SetDeveloperMetadataCommand {
            spreadsheet_id: "sheet-1".to_string(),
            key: "owner".to_string(),
            value: "team-a".to_string(),
            sheet: None,
            dimension: None,
            start: None,
            end: None,
            dry_run: false,
            lease: crate::cli::drive::helpers::LeaseTokenArg { lease: None },
            output: OutputFormat::Table,
        };
        assert!(cmd.execute(&dead_client()).await.is_ok());
    }

    #[tokio::test]
    async fn set_command_execute_json_output_short_circuits_before_the_text_lines() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let cmd = SetDeveloperMetadataCommand {
            spreadsheet_id: "sheet-1".to_string(),
            key: "owner".to_string(),
            value: "team-a".to_string(),
            sheet: None,
            dimension: None,
            start: None,
            end: None,
            dry_run: false,
            lease: crate::cli::drive::helpers::LeaseTokenArg { lease: None },
            output: OutputFormat::Json,
        };
        assert!(cmd.execute(&dead_client()).await.is_ok());
    }

    #[tokio::test]
    async fn delete_command_execute_with_no_configured_account_reports_blocked() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let cmd = DeleteDeveloperMetadataCommand {
            spreadsheet_id: "sheet-1".to_string(),
            key: "owner".to_string(),
            sheet: None,
            dimension: None,
            start: None,
            end: None,
            dry_run: false,
            lease: crate::cli::drive::helpers::LeaseTokenArg { lease: None },
            output: OutputFormat::Table,
        };
        assert!(cmd.execute(&dead_client()).await.is_ok());
    }

    #[tokio::test]
    async fn search_command_execute_reports_no_matching_entries() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(crate::drive::sheets::client::SHEETS_API_URL, server.uri());
        mount_workbook().mount(&server).await;
        mount_search(serde_json::json!({"matchedDeveloperMetadata": []}))
            .mount(&server)
            .await;

        let cmd = search_cmd(None, None, OutputFormat::Table);
        cmd.execute(&client).await.unwrap();
    }

    #[tokio::test]
    async fn search_command_execute_prints_every_matching_entry() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(crate::drive::sheets::client::SHEETS_API_URL, server.uri());
        mount_workbook().mount(&server).await;
        mount_search(serde_json::json!({"matchedDeveloperMetadata": [
            {"developerMetadata": {
                "metadataId": 7, "metadataKey": "owner", "metadataValue": "team-a",
                "location": {"spreadsheet": true}, "visibility": "DOCUMENT",
            }},
        ]}))
        .mount(&server)
        .await;

        let cmd = search_cmd(Some("owner"), None, OutputFormat::Table);
        cmd.execute(&client).await.unwrap();
    }

    #[tokio::test]
    async fn search_command_execute_json_output_short_circuits_before_the_text_lines() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(crate::drive::sheets::client::SHEETS_API_URL, server.uri());
        mount_workbook().mount(&server).await;
        mount_search(serde_json::json!({"matchedDeveloperMetadata": []}))
            .mount(&server)
            .await;

        let cmd = search_cmd(None, None, OutputFormat::Json);
        cmd.execute(&client).await.unwrap();
    }

    #[tokio::test]
    async fn search_command_execute_maps_an_invalid_location_to_an_error() {
        // `--sheet` naming a title absent from the workbook drives
        // `search`'s `resolve_location` failure through
        // `location_error_to_string`, which `execute` maps into a plain
        // `anyhow::Error` since `search` has no `DeveloperMetadataOutcome`
        // to attach a structured result to.
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(crate::drive::sheets::client::SHEETS_API_URL, server.uri());
        mount_workbook().mount(&server).await;

        let cmd = search_cmd(None, Some("Nope"), OutputFormat::Table);
        let err = cmd.execute(&client).await.unwrap_err();
        assert!(err.to_string().contains("Nope"), "{err}");
    }
}
