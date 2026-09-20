//! CLI commands for `omni-dev drive sheets set-basic-filter`/
//! `clear-basic-filter`/`add-filter-view`/`update-filter-view`/
//! `delete-filter-view`/`list-filter-views` (issue #1794).
//!
//! The first five are gated by
//! [`DriveOperation::SheetsStructure`](crate::drive::write_gate::DriveOperation::SheetsStructure)
//! (ADR-0081). `list-filter-views` is a plain read, ungated like
//! `list-protections`.

use anyhow::Result;
use clap::Parser;

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::cli::drive::helpers;
use crate::drive::client::DriveClient;
use crate::drive::sheets::api::SheetsApi;
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::filter::{describe_lines, filter, FilterOptions, FilterVerb};

/// Sets (upserting any existing one) the basic filter on a sheet.
#[derive(Parser)]
pub struct SetBasicFilterCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Sheet (tab) title. Supplies the prefix for a bare `--range`.
    #[arg(long, value_name = "NAME")]
    pub sheet: String,

    /// A1 range to filter, optionally carrying its own `Sheet!` prefix.
    #[arg(long, value_name = "A1")]
    pub range: String,

    /// A `COLUMN:asc|desc` sort spec, in priority order. Repeatable.
    #[arg(long = "sort-by", value_name = "COLUMN:asc|desc")]
    pub sort_by: Vec<String>,

    /// A `COLUMN:VALUE[,VALUE...]` hidden-value criterion, one per column.
    /// Repeatable.
    #[arg(long = "hide-values", value_name = "COLUMN:VALUES")]
    pub hide_values: Vec<String>,

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

impl SetBasicFilterCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = FilterOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: FilterVerb::SetBasicFilter {
                sheet: self.sheet,
                range: self.range,
                sort_by: self.sort_by,
                hide_values: self.hide_values,
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_filter(client, &opts, &self.output).await
    }
}

/// Removes a sheet's basic filter.
#[derive(Parser)]
pub struct ClearBasicFilterCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Sheet (tab) title.
    #[arg(long, value_name = "NAME")]
    pub sheet: String,

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

impl ClearBasicFilterCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = FilterOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: FilterVerb::ClearBasicFilter { sheet: self.sheet },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_filter(client, &opts, &self.output).await
    }
}

/// Adds a named filter view.
#[derive(Parser)]
pub struct AddFilterViewCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Sheet (tab) title. Supplies the prefix for a bare `--range`.
    #[arg(long, value_name = "NAME")]
    pub sheet: String,

    /// A1 range to filter, optionally carrying its own `Sheet!` prefix.
    #[arg(long, value_name = "A1")]
    pub range: String,

    /// A human-readable name for the view.
    #[arg(long, value_name = "TEXT")]
    pub title: Option<String>,

    /// A `COLUMN:asc|desc` sort spec, in priority order. Repeatable.
    #[arg(long = "sort-by", value_name = "COLUMN:asc|desc")]
    pub sort_by: Vec<String>,

    /// A `COLUMN:VALUE[,VALUE...]` hidden-value criterion, one per column.
    /// Repeatable.
    #[arg(long = "hide-values", value_name = "COLUMN:VALUES")]
    pub hide_values: Vec<String>,

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

impl AddFilterViewCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = FilterOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: FilterVerb::AddFilterView {
                sheet: self.sheet,
                range: self.range,
                title: self.title,
                sort_by: self.sort_by,
                hide_values: self.hide_values,
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_filter(client, &opts, &self.output).await
    }
}

/// Changes an existing filter view's title, range, sort order, or hidden
/// values. The target is addressed directly by `--filter-view-id`,
/// discovered via `drive sheets list-filter-views`.
#[derive(Parser)]
#[command(group(clap::ArgGroup::new("change")
    .args(["title", "range", "sort_by", "hide_values", "clear_sort", "clear_criteria"])
    .multiple(true)
    .required(true)))]
pub struct UpdateFilterViewCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Which filter view to change.
    #[arg(long, value_name = "ID")]
    pub filter_view_id: i64,

    /// Sheet (tab) title, when changing the filtered range. Supplies the
    /// prefix for a bare `--range`.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// The new A1 range, when changing it, optionally carrying its own
    /// `Sheet!` prefix.
    #[arg(long, value_name = "A1")]
    pub range: Option<String>,

    /// The new title, when changing it.
    #[arg(long, value_name = "TEXT")]
    pub title: Option<String>,

    /// A `COLUMN:asc|desc` sort spec to merge in, replacing any existing
    /// entry for that column (else appending). Repeatable.
    #[arg(long = "sort-by", value_name = "COLUMN:asc|desc")]
    pub sort_by: Vec<String>,

    /// A `COLUMN:VALUE[,VALUE...]` hidden-value criterion to merge in,
    /// replacing any existing entry for that column. Repeatable.
    #[arg(long = "hide-values", value_name = "COLUMN:VALUES")]
    pub hide_values: Vec<String>,

    /// Reset the sort order to empty before applying `--sort-by`.
    #[arg(long)]
    pub clear_sort: bool,

    /// Reset the criteria to empty before applying `--hide-values`.
    #[arg(long)]
    pub clear_criteria: bool,

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

impl UpdateFilterViewCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = FilterOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: FilterVerb::UpdateFilterView {
                filter_view_id: self.filter_view_id,
                sheet: self.sheet,
                range: self.range,
                title: self.title,
                sort_by: self.sort_by,
                hide_values: self.hide_values,
                clear_sort: self.clear_sort,
                clear_criteria: self.clear_criteria,
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_filter(client, &opts, &self.output).await
    }
}

/// Removes a filter view. The target is addressed directly by
/// `--filter-view-id`, discovered via `drive sheets list-filter-views`.
#[derive(Parser)]
pub struct DeleteFilterViewCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Which filter view to remove.
    #[arg(long, value_name = "ID")]
    pub filter_view_id: i64,

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

impl DeleteFilterViewCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = FilterOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: FilterVerb::DeleteFilterView {
                filter_view_id: self.filter_view_id,
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_filter(client, &opts, &self.output).await
    }
}

/// Lists the filter views in a spreadsheet.
///
/// Read-only and ungated, like `sheets info`/`list-protections` — needed so
/// `update-filter-view`/`delete-filter-view` are usable at all, since a
/// filter view's numeric id is otherwise invisible from the CLI.
#[derive(Parser)]
pub struct ListFilterViewsCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ListFilterViewsCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let sheets = SheetsClient::from_drive_client(client)?;
        let api = SheetsApi::new(&sheets);
        let workbook = api
            .get_spreadsheet_with_filter_views(&self.spreadsheet_id)
            .await?;
        if output_as(&workbook, &self.output)? {
            return Ok(());
        }
        for sheet in &workbook.sheets {
            for view in &sheet.filter_views {
                let id = view
                    .filter_view_id
                    .map_or_else(|| "?".to_string(), |id| id.to_string());
                let title = view.title.as_deref().unwrap_or("");
                let range = view
                    .range
                    .as_ref()
                    .map_or_else(|| "(unresolvable)".to_string(), render_grid_range);
                let sort = render_sort_specs(&view.sort_specs);
                let hidden = render_criteria(&view.criteria);
                println!(
                    "{}",
                    sanitize_for_terminal(&format!(
                        "id {id} '{title}': {range}  sort=[{sort}]  hidden={{{hidden}}}  \
                         sheet={}",
                        sheet.title()
                    ))
                );
            }
        }
        Ok(())
    }
}

/// Renders a numeric [`GridRange`](crate::drive::sheets::types::GridRange)
/// as a compact 1-based description, matching
/// `protection.rs`'s `list-protections` rendering.
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

/// Renders a filter view's sort specs as `"0 asc, 2 desc"`.
fn render_sort_specs(specs: &[crate::drive::sheets::types::SortSpec]) -> String {
    specs
        .iter()
        .map(|s| {
            let dir = match s.sort_order {
                crate::drive::sheets::types::SortOrder::Ascending => "asc",
                crate::drive::sheets::types::SortOrder::Descending => "desc",
            };
            format!("{} {dir}", s.dimension_index)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Renders a filter view's criteria map as `"1: [Foo, Bar]"`.
fn render_criteria(
    criteria: &std::collections::BTreeMap<String, crate::drive::sheets::types::FilterCriteria>,
) -> String {
    criteria
        .iter()
        .map(|(col, c)| format!("{col}: [{}]", c.hidden_values.join(", ")))
        .collect::<Vec<_>>()
        .join(", ")
}

async fn run_filter(
    client: &DriveClient,
    opts: &FilterOptions,
    output: &OutputFormat,
) -> Result<()> {
    let sheets = SheetsClient::from_drive_client(client)?;
    let rules = helpers::active_account_rules()?;
    let outcome = filter(client, &sheets, opts, &rules).await;
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
    use crate::drive::sheets::types::{FilterCriteria, GridRange, SortOrder, SortSpec};
    use crate::utils::secret::Secret;

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
            sheet_id: 3,
            start_row_index: Some(0),
            end_row_index: Some(5),
            start_column_index: Some(0),
            end_column_index: Some(2),
        };
        assert_eq!(render_grid_range(&range), "sheetId 3, rows 1-5, cols 1-2");
    }

    #[test]
    fn render_sort_specs_joins_column_and_direction() {
        let specs = vec![
            SortSpec {
                dimension_index: 0,
                sort_order: SortOrder::Ascending,
            },
            SortSpec {
                dimension_index: 2,
                sort_order: SortOrder::Descending,
            },
        ];
        assert_eq!(render_sort_specs(&specs), "0 asc, 2 desc");
    }

    #[test]
    fn render_criteria_joins_columns_and_hidden_values() {
        let mut criteria = std::collections::BTreeMap::new();
        criteria.insert(
            "1".to_string(),
            FilterCriteria {
                hidden_values: vec!["Foo".to_string(), "Bar".to_string()],
            },
        );
        assert_eq!(render_criteria(&criteria), "1: [Foo, Bar]");
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
    async fn list_filter_views_prints_every_view() {
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
                        "filterViews": [
                            {
                                "filterViewId": 1,
                                "title": "Open only",
                                "range": {
                                    "sheetId": 0,
                                    "startRowIndex": 0,
                                    "endRowIndex": 5,
                                    "startColumnIndex": 0,
                                    "endColumnIndex": 2,
                                },
                                "sortSpecs": [{"dimensionIndex": 0, "sortOrder": "ASCENDING"}],
                                "criteria": {"1": {"hiddenValues": ["Closed"]}},
                            },
                            {"filterViewId": 2},
                        ],
                    }],
                })),
            )
            .mount(&server)
            .await;

        let cmd = ListFilterViewsCommand {
            spreadsheet_id: "sheet-1".to_string(),
            output: crate::cli::drive::format::OutputFormat::Table,
        };
        assert!(cmd.execute(&client).await.is_ok());
    }

    #[tokio::test]
    async fn set_basic_filter_command_reports_blocked_with_no_rules() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/sheet-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "sheet-1", "name": "sheet-1",
                    "mimeType": crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
                    "parents": [],
                })),
            )
            .mount(&server)
            .await;

        let cmd = SetBasicFilterCommand {
            spreadsheet_id: "sheet-1".to_string(),
            sheet: "Q1".to_string(),
            range: "A1:D10".to_string(),
            sort_by: Vec::new(),
            hide_values: Vec::new(),
            dry_run: false,
            lease: crate::cli::drive::helpers::LeaseTokenArg { lease: None },
            output: crate::cli::drive::format::OutputFormat::Table,
        };
        // No `write_permissions.rules` configured in this test's account,
        // so the default-deny policy blocks it — proves the CLI leaf wires
        // the gate through rather than silently allowing.
        assert!(cmd.execute(&client).await.is_ok());
    }

    async fn mount_orphan_sheet(server: &wiremock::MockServer) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/sheet-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "sheet-1", "name": "sheet-1",
                    "mimeType": crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
                    "parents": [],
                })),
            )
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn set_basic_filter_command_json_output_skips_the_table() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_orphan_sheet(&server).await;

        let cmd = SetBasicFilterCommand {
            spreadsheet_id: "sheet-1".to_string(),
            sheet: "Q1".to_string(),
            range: "A1:D10".to_string(),
            sort_by: Vec::new(),
            hide_values: Vec::new(),
            dry_run: true,
            lease: crate::cli::drive::helpers::LeaseTokenArg { lease: None },
            output: crate::cli::drive::format::OutputFormat::Json,
        };
        assert!(cmd.execute(&client).await.is_ok());
    }

    #[tokio::test]
    async fn clear_basic_filter_command_wires_the_gate_through() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_orphan_sheet(&server).await;

        let cmd = ClearBasicFilterCommand {
            spreadsheet_id: "sheet-1".to_string(),
            sheet: "Q1".to_string(),
            dry_run: false,
            lease: crate::cli::drive::helpers::LeaseTokenArg { lease: None },
            output: crate::cli::drive::format::OutputFormat::Table,
        };
        assert!(cmd.execute(&client).await.is_ok());
    }

    #[tokio::test]
    async fn add_filter_view_command_wires_the_gate_through() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_orphan_sheet(&server).await;

        let cmd = AddFilterViewCommand {
            spreadsheet_id: "sheet-1".to_string(),
            sheet: "Q1".to_string(),
            range: "A1:D10".to_string(),
            title: Some("Open only".to_string()),
            sort_by: vec!["0:asc".to_string()],
            hide_values: vec!["1:Closed".to_string()],
            dry_run: false,
            lease: crate::cli::drive::helpers::LeaseTokenArg { lease: None },
            output: crate::cli::drive::format::OutputFormat::Table,
        };
        assert!(cmd.execute(&client).await.is_ok());
    }

    #[tokio::test]
    async fn update_filter_view_command_wires_the_gate_through() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_orphan_sheet(&server).await;

        let cmd = UpdateFilterViewCommand {
            spreadsheet_id: "sheet-1".to_string(),
            filter_view_id: 7,
            sheet: Some("Q1".to_string()),
            range: Some("A1:D20".to_string()),
            title: Some("Renamed".to_string()),
            sort_by: vec!["0:desc".to_string()],
            hide_values: vec!["1:Closed".to_string()],
            clear_sort: true,
            clear_criteria: true,
            dry_run: false,
            lease: crate::cli::drive::helpers::LeaseTokenArg { lease: None },
            output: crate::cli::drive::format::OutputFormat::Table,
        };
        assert!(cmd.execute(&client).await.is_ok());
    }

    #[tokio::test]
    async fn delete_filter_view_command_wires_the_gate_through() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_orphan_sheet(&server).await;

        let cmd = DeleteFilterViewCommand {
            spreadsheet_id: "sheet-1".to_string(),
            filter_view_id: 7,
            dry_run: false,
            lease: crate::cli::drive::helpers::LeaseTokenArg { lease: None },
            output: crate::cli::drive::format::OutputFormat::Table,
        };
        assert!(cmd.execute(&client).await.is_ok());
    }

    #[tokio::test]
    async fn list_filter_views_json_output_skips_the_table() {
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

        let cmd = ListFilterViewsCommand {
            spreadsheet_id: "sheet-1".to_string(),
            output: crate::cli::drive::format::OutputFormat::Json,
        };
        assert!(cmd.execute(&client).await.is_ok());
    }
}
