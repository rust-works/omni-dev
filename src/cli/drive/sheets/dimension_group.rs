//! CLI commands for `omni-dev drive sheets add-dimension-group`/
//! `update-dimension-group`/`delete-dimension-group`/`list-dimension-groups`
//! (issue #1833).
//!
//! The first three are gated by
//! [`DriveOperation::SheetsStructure`](crate::drive::write_gate::DriveOperation::SheetsStructure)
//! (ADR-0084, following ADR-0082's own reasoning). `list-dimension-groups`
//! is a plain read, ungated like `list-bandings`.

use anyhow::Result;
use clap::Parser;

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::cli::drive::helpers;
use crate::cli::drive::sheets::format::DimensionArg;
use crate::drive::client::DriveClient;
use crate::drive::sheets::api::SheetsApi;
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::dimension_group::{
    describe_lines, dimension_group, DimensionGroupOptions, DimensionGroupVerb,
};

/// Adds a new outline group — the collapsible +/- grouping bar — over a
/// span of rows or columns.
///
/// No client-side depth cap: the server derives the new group's depth from
/// how the span overlaps existing groups on the same axis, and no maximum
/// is documented; only the span itself is validated against the sheet's
/// current extent, matching `auto-resize-dimension`'s own flag shape.
#[derive(Parser)]
pub struct AddDimensionGroupCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Title of the sheet to modify.
    #[arg(long, value_name = "NAME")]
    pub sheet: String,

    /// Rows or columns.
    #[arg(long, value_enum)]
    pub dimension: DimensionArg,

    /// 1-based first row/column, inclusive.
    #[arg(long, value_name = "N")]
    pub start: i64,

    /// 1-based last row/column, inclusive.
    #[arg(long, value_name = "N")]
    pub end: i64,

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

impl AddDimensionGroupCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = DimensionGroupOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: DimensionGroupVerb::AddDimensionGroup {
                sheet: self.sheet,
                dimension: self.dimension.engine(),
                start: self.start,
                end: self.end,
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_dimension_group(client, &opts, &self.output).await
    }
}

/// Changes an existing group's `collapsed` state — the only field this
/// crate ever updates, since the span is the group's identity and the
/// depth is server-derived. The target span may match more than one group
/// (the API creates this when a group is added over a span equal to an
/// existing one); `--depth` disambiguates, discovered via
/// `drive sheets list-dimension-groups`.
#[derive(Parser)]
pub struct UpdateDimensionGroupCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Title of the sheet the group is on.
    #[arg(long, value_name = "NAME")]
    pub sheet: String,

    /// Rows or columns.
    #[arg(long, value_enum)]
    pub dimension: DimensionArg,

    /// 1-based first row/column, inclusive — identifies the group together
    /// with `--end`.
    #[arg(long, value_name = "N")]
    pub start: i64,

    /// 1-based last row/column, inclusive.
    #[arg(long, value_name = "N")]
    pub end: i64,

    /// Disambiguates which group at that exact span to update, when more
    /// than one exists at different depths. Omit when only one exists.
    #[arg(long, value_name = "N")]
    pub depth: Option<i64>,

    /// The new collapsed state. Required — unlike `format-cells`'
    /// optional flags, `update-dimension-group` changes nothing else, so
    /// there is no "leave it unset" case. `bool`, not `Option<bool>`, so
    /// clap requires it as a value-taking flag rather than treating it as
    /// a `--collapsed`-with-no-value switch.
    #[arg(long, value_name = "BOOL", action = clap::ArgAction::Set)]
    pub collapsed: bool,

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

impl UpdateDimensionGroupCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = DimensionGroupOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: DimensionGroupVerb::UpdateDimensionGroup {
                sheet: self.sheet,
                dimension: self.dimension.engine(),
                start: self.start,
                end: self.end,
                depth: self.depth,
                collapsed: self.collapsed,
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_dimension_group(client, &opts, &self.output).await
    }
}

/// Removes an outline group. Requires an **exact** span match among the
/// groups `list-dimension-groups` would show — the API's own partial-span
/// delete (which decrements an overlapping group's depth rather than
/// removing anything) is not exposed.
#[derive(Parser)]
pub struct DeleteDimensionGroupCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Title of the sheet the group is on.
    #[arg(long, value_name = "NAME")]
    pub sheet: String,

    /// Rows or columns.
    #[arg(long, value_enum)]
    pub dimension: DimensionArg,

    /// 1-based first row/column, inclusive — identifies the group together
    /// with `--end`.
    #[arg(long, value_name = "N")]
    pub start: i64,

    /// 1-based last row/column, inclusive.
    #[arg(long, value_name = "N")]
    pub end: i64,

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

impl DeleteDimensionGroupCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = DimensionGroupOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: DimensionGroupVerb::DeleteDimensionGroup {
                sheet: self.sheet,
                dimension: self.dimension.engine(),
                start: self.start,
                end: self.end,
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_dimension_group(client, &opts, &self.output).await
    }
}

/// Lists the row and column outline groups in a spreadsheet.
///
/// Read-only and ungated, like `list-bandings` — needed so
/// `update-dimension-group`/`delete-dimension-group` are usable at all,
/// since a group's `(range, depth)` is otherwise invisible from the CLI.
#[derive(Parser)]
pub struct ListDimensionGroupsCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ListDimensionGroupsCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let sheets = SheetsClient::from_drive_client(client)?;
        let api = SheetsApi::new(&sheets);
        let workbook = api
            .get_spreadsheet_with_dimension_groups(&self.spreadsheet_id)
            .await?;
        if output_as(&workbook, &self.output)? {
            return Ok(());
        }
        for sheet in &workbook.sheets {
            for (axis, groups) in [
                ("rows", &sheet.row_groups),
                ("columns", &sheet.column_groups),
            ] {
                for group in groups {
                    let span = format!("{}:{}", group.range.start_index + 1, group.range.end_index);
                    println!(
                        "{}",
                        sanitize_for_terminal(&format!(
                            "{axis} {span}  depth={}  collapsed={}  sheet={}",
                            group.depth,
                            group.collapsed,
                            sheet.title()
                        ))
                    );
                }
            }
        }
        Ok(())
    }
}

async fn run_dimension_group(
    client: &DriveClient,
    opts: &DimensionGroupOptions,
    output: &OutputFormat,
) -> Result<()> {
    let sheets = SheetsClient::from_drive_client(client)?;
    let rules = helpers::active_account_rules()?;
    let outcome = dimension_group(client, &sheets, opts, &rules).await;
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

    #[test]
    fn dimension_arg_converts_to_the_matching_dimension() {
        assert_eq!(
            DimensionArg::Rows.engine(),
            crate::drive::sheets::types::Dimension::Rows
        );
        assert_eq!(
            DimensionArg::Columns.engine(),
            crate::drive::sheets::types::Dimension::Columns
        );
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
    async fn list_dimension_groups_prints_every_group() {
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
                    "sheets": [{
                        "properties": {"sheetId": 0, "title": "Sheet1"},
                        "rowGroups": [
                            {"range": {"sheetId": 0, "dimension": "ROWS", "startIndex": 4, "endIndex": 9}, "depth": 0},
                        ],
                        "columnGroups": [
                            {"range": {"sheetId": 0, "dimension": "COLUMNS", "startIndex": 0, "endIndex": 3}, "depth": 0, "collapsed": true},
                        ],
                    }],
                })),
            )
            .mount(&server)
            .await;

        let cmd = ListDimensionGroupsCommand {
            spreadsheet_id: "sheet-1".to_string(),
            output: crate::cli::drive::format::OutputFormat::Table,
        };
        assert!(cmd.execute(&client).await.is_ok());
    }

    #[tokio::test]
    async fn list_dimension_groups_yaml_output_short_circuits_before_printing_lines() {
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
                })),
            )
            .mount(&server)
            .await;

        let cmd = ListDimensionGroupsCommand {
            spreadsheet_id: "sheet-1".to_string(),
            output: crate::cli::drive::format::OutputFormat::Yaml,
        };
        assert!(cmd.execute(&client).await.is_ok());
    }

    /// Mounts a Drive-file/parent-folder pair with no write-permission
    /// rules configured, so the gate refuses by default policy. Enough to
    /// drive `Add`/`Update`/`DeleteDimensionGroupCommand::execute` (and
    /// thus `run_dimension_group`) through their full CLI-level path —
    /// building `DimensionGroupOptions`, calling `dimension_group`, and
    /// rendering the `describe_lines` output — without needing a lease or
    /// a workbook fetch, which a `Blocked` verdict never reaches.
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

    fn no_lease() -> crate::cli::drive::helpers::LeaseTokenArg {
        crate::cli::drive::helpers::LeaseTokenArg { lease: None }
    }

    #[tokio::test]
    async fn add_dimension_group_command_reaches_the_engine_under_a_blocked_gate() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_ungated_target(&server).await;

        let cmd = AddDimensionGroupCommand {
            spreadsheet_id: "sheet-1".to_string(),
            sheet: "Sheet1".to_string(),
            dimension: DimensionArg::Rows,
            start: 1,
            end: 5,
            dry_run: false,
            lease: no_lease(),
            output: crate::cli::drive::format::OutputFormat::Table,
        };
        assert!(cmd.execute(&client).await.is_ok());
    }

    #[tokio::test]
    async fn update_dimension_group_command_reaches_the_engine_under_a_blocked_gate() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_ungated_target(&server).await;

        let cmd = UpdateDimensionGroupCommand {
            spreadsheet_id: "sheet-1".to_string(),
            sheet: "Sheet1".to_string(),
            dimension: DimensionArg::Rows,
            start: 1,
            end: 5,
            depth: None,
            collapsed: true,
            dry_run: false,
            lease: no_lease(),
            output: crate::cli::drive::format::OutputFormat::Table,
        };
        assert!(cmd.execute(&client).await.is_ok());
    }

    #[tokio::test]
    async fn delete_dimension_group_command_reaches_the_engine_under_a_blocked_gate() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_ungated_target(&server).await;

        let cmd = DeleteDimensionGroupCommand {
            spreadsheet_id: "sheet-1".to_string(),
            sheet: "Sheet1".to_string(),
            dimension: DimensionArg::Rows,
            start: 1,
            end: 5,
            dry_run: false,
            lease: no_lease(),
            output: crate::cli::drive::format::OutputFormat::Table,
        };
        assert!(cmd.execute(&client).await.is_ok());
    }
}
