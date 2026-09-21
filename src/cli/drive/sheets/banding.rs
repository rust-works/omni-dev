//! CLI commands for `omni-dev drive sheets add-banding`/`update-banding`/
//! `delete-banding`/`list-bandings` (issue #1832).
//!
//! The first three are gated by
//! [`DriveOperation::SheetsStructure`](crate::drive::write_gate::DriveOperation::SheetsStructure)
//! (ADR-0082, following ADR-0078's original reasoning). `list-bandings` is
//! a plain read, ungated like `list-protections`.

use anyhow::Result;
use clap::Parser;

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::cli::drive::helpers;
use crate::drive::client::DriveClient;
use crate::drive::sheets::api::SheetsApi;
use crate::drive::sheets::banding::{
    banding, describe_lines, BandingAxis, BandingOptions, BandingVerb,
};
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::render_grid_range;

/// The `--axis` values for `add-banding`/`update-banding` — mirrors
/// [`BandingAxis`] with a `clap::ValueEnum` derive, since the engine type
/// deliberately carries no `clap` dependency (the same split
/// `conditional_format.rs`'s `GradientMidTypeArg`/`GradientPointType` uses).
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum BandingAxisArg {
    /// Alternate by row (the common case — the Sheets UI's default).
    Rows,
    /// Alternate by column.
    Columns,
}

impl From<BandingAxisArg> for BandingAxis {
    fn from(value: BandingAxisArg) -> Self {
        match value {
            BandingAxisArg::Rows => Self::Rows,
            BandingAxisArg::Columns => Self::Columns,
        }
    }
}

/// Adds a new banded range — alternating row or column colors.
///
/// v1 sets at most one of row/column banding per call, selected by `--axis`
/// (default `rows`); the Sheets API allows both simultaneously on one
/// range, which this crate does not expose. Colors are `#RRGGBB`, written
/// via the modern `*ColorStyle` fields — never the deprecated plain
/// `Color` fields, never a theme color.
#[derive(Parser)]
pub struct AddBandingCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Sheet (tab) title. Supplies the prefix for a bare `--range`.
    #[arg(long, value_name = "NAME")]
    pub sheet: String,

    /// A1 range to band, optionally carrying its own `Sheet!` prefix.
    #[arg(long, value_name = "A1")]
    pub range: String,

    /// Which axis to alternate along.
    #[arg(long, value_enum, default_value_t = BandingAxisArg::Rows)]
    pub axis: BandingAxisArg,

    /// The header row/column's color, `#RRGGBB`, if distinct from the
    /// alternating bands.
    #[arg(long, value_name = "HEX")]
    pub header_color: Option<String>,

    /// The first band's color, `#RRGGBB`.
    #[arg(long, value_name = "HEX")]
    pub first_band_color: String,

    /// The second (alternating) band's color, `#RRGGBB`.
    #[arg(long, value_name = "HEX")]
    pub second_band_color: String,

    /// The footer row/column's color, `#RRGGBB`, if distinct from the
    /// alternating bands.
    #[arg(long, value_name = "HEX")]
    pub footer_color: Option<String>,

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

impl AddBandingCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = BandingOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: BandingVerb::AddBanding {
                sheet: self.sheet,
                range: self.range,
                axis: self.axis.into(),
                header_color: self.header_color,
                first_band_color: self.first_band_color,
                second_band_color: self.second_band_color,
                footer_color: self.footer_color,
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_banding(client, &opts, &self.output).await
    }
}

/// Changes an existing banded range's range and/or colors. The target is
/// addressed directly by `--banded-range-id`, discovered via
/// `drive sheets list-bandings`.
#[derive(Parser)]
#[command(group(clap::ArgGroup::new("change")
    .args(["sheet", "range", "header_color", "first_band_color", "second_band_color", "footer_color"])
    .multiple(true)
    .required(true)))]
pub struct UpdateBandingCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Which banded range to change.
    #[arg(long, value_name = "ID")]
    pub banded_range_id: i64,

    /// Sheet (tab) title, when changing the banded range. Supplies the
    /// prefix for a bare `--range`.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// The new A1 range, when changing it, optionally carrying its own
    /// `Sheet!` prefix.
    #[arg(long, value_name = "A1")]
    pub range: Option<String>,

    /// Which axis's colors `--header-color`/`--first-band-color`/`--second-band-color`/`--footer-color`
    /// change.
    #[arg(long, value_enum, default_value_t = BandingAxisArg::Rows)]
    pub axis: BandingAxisArg,

    /// The new header color, `#RRGGBB`, when changing it.
    #[arg(long, value_name = "HEX")]
    pub header_color: Option<String>,

    /// The new first-band color, `#RRGGBB`, when changing it.
    #[arg(long, value_name = "HEX")]
    pub first_band_color: Option<String>,

    /// The new second-band color, `#RRGGBB`, when changing it.
    #[arg(long, value_name = "HEX")]
    pub second_band_color: Option<String>,

    /// The new footer color, `#RRGGBB`, when changing it.
    #[arg(long, value_name = "HEX")]
    pub footer_color: Option<String>,

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

impl UpdateBandingCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = BandingOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: BandingVerb::UpdateBanding {
                banded_range_id: self.banded_range_id,
                sheet: self.sheet,
                range: self.range,
                axis: self.axis.into(),
                header_color: self.header_color,
                first_band_color: self.first_band_color,
                second_band_color: self.second_band_color,
                footer_color: self.footer_color,
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_banding(client, &opts, &self.output).await
    }
}

/// Removes a banded range. The target is addressed directly by
/// `--banded-range-id`, discovered via `drive sheets list-bandings`.
#[derive(Parser)]
pub struct DeleteBandingCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Which banded range to remove.
    #[arg(long, value_name = "ID")]
    pub banded_range_id: i64,

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

impl DeleteBandingCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = BandingOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: BandingVerb::DeleteBanding {
                banded_range_id: self.banded_range_id,
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_banding(client, &opts, &self.output).await
    }
}

/// Lists the banded ranges in a spreadsheet.
///
/// Read-only and ungated, like `sheets info`/`list-protections` — needed so
/// `update-banding`/`delete-banding` are usable at all, since a banded
/// range's numeric id is otherwise invisible from the CLI.
#[derive(Parser)]
pub struct ListBandingsCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ListBandingsCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let sheets = SheetsClient::from_drive_client(client)?;
        let api = SheetsApi::new(&sheets);
        let workbook = api
            .get_spreadsheet_with_banding(&self.spreadsheet_id)
            .await?;
        if output_as(&workbook, &self.output)? {
            return Ok(());
        }
        for sheet in &workbook.sheets {
            for banded in &sheet.banded_ranges {
                let id = banded
                    .banded_range_id
                    .map_or_else(|| "?".to_string(), |id| id.to_string());
                let range = banded
                    .range
                    .as_ref()
                    .map_or_else(|| "(unresolvable)".to_string(), render_grid_range);
                let axes = match (
                    banded.row_properties.is_some(),
                    banded.column_properties.is_some(),
                ) {
                    (true, true) => "rows+columns",
                    (true, false) => "rows",
                    (false, true) => "columns",
                    (false, false) => "none",
                };
                println!(
                    "{}",
                    sanitize_for_terminal(&format!(
                        "id {id}: {range}  axis={axes}  sheet={}",
                        sheet.title()
                    ))
                );
            }
        }
        Ok(())
    }
}

async fn run_banding(
    client: &DriveClient,
    opts: &BandingOptions,
    output: &OutputFormat,
) -> Result<()> {
    let sheets = SheetsClient::from_drive_client(client)?;
    let rules = helpers::active_account_rules()?;
    let outcome = banding(client, &sheets, opts, &rules).await;
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
    fn banding_axis_arg_converts_to_the_matching_banding_axis() {
        assert_eq!(BandingAxis::from(BandingAxisArg::Rows), BandingAxis::Rows);
        assert_eq!(
            BandingAxis::from(BandingAxisArg::Columns),
            BandingAxis::Columns
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
    async fn list_bandings_prints_every_banded_range() {
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
                        "bandedRanges": [
                            {
                                "bandedRangeId": 1,
                                "range": {
                                    "sheetId": 0,
                                    "startRowIndex": 0,
                                    "endRowIndex": 5,
                                    "startColumnIndex": 0,
                                    "endColumnIndex": 2,
                                },
                                "rowProperties": {
                                    "firstBandColorStyle": {"rgbColor": {"red": 1, "green": 1, "blue": 1}},
                                    "secondBandColorStyle": {"rgbColor": {"red": 0, "green": 0, "blue": 0}},
                                },
                            },
                            {
                                "bandedRangeId": 2,
                                "columnProperties": {
                                    "firstBandColorStyle": {"rgbColor": {"red": 1, "green": 1, "blue": 1}},
                                    "secondBandColorStyle": {"rgbColor": {"red": 0, "green": 0, "blue": 0}},
                                },
                            },
                        ],
                    }],
                })),
            )
            .mount(&server)
            .await;

        let cmd = ListBandingsCommand {
            spreadsheet_id: "sheet-1".to_string(),
            output: crate::cli::drive::format::OutputFormat::Table,
        };
        assert!(cmd.execute(&client).await.is_ok());
    }

    #[tokio::test]
    async fn list_bandings_yaml_output_short_circuits_before_printing_lines() {
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

        let cmd = ListBandingsCommand {
            spreadsheet_id: "sheet-1".to_string(),
            output: crate::cli::drive::format::OutputFormat::Yaml,
        };
        assert!(cmd.execute(&client).await.is_ok());
    }

    /// Mounts a Drive-file/parent-folder pair with no write-permission rules
    /// configured (`active_account_rules()` reads an unconfigured account —
    /// see `client_with_bootstrapped_token`'s `EnvGuard::clear_credentials`
    /// caller), so the gate refuses by default policy. That's enough to
    /// drive `Add`/`Update`/`DeleteBandingCommand::execute` (and thus
    /// `run_banding`) through their full CLI-level path — building
    /// `BandingOptions`, calling `banding`, and rendering the
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
    async fn add_banding_command_runs_end_to_end() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_ungated_target(&server).await;

        let cmd = AddBandingCommand {
            spreadsheet_id: "sheet-1".to_string(),
            sheet: "Q1".to_string(),
            range: "A1:D10".to_string(),
            axis: BandingAxisArg::Rows,
            header_color: None,
            first_band_color: "#FFFFFF".to_string(),
            second_band_color: "#EEEEEE".to_string(),
            footer_color: None,
            dry_run: true,
            lease: crate::cli::drive::helpers::LeaseTokenArg { lease: None },
            output: crate::cli::drive::format::OutputFormat::Table,
        };
        assert!(cmd.execute(&client).await.is_ok());
    }

    #[tokio::test]
    async fn update_banding_command_runs_end_to_end() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_ungated_target(&server).await;

        let cmd = UpdateBandingCommand {
            spreadsheet_id: "sheet-1".to_string(),
            banded_range_id: 7,
            sheet: None,
            range: None,
            axis: BandingAxisArg::Rows,
            header_color: Some("#000000".to_string()),
            first_band_color: None,
            second_band_color: None,
            footer_color: None,
            dry_run: true,
            lease: crate::cli::drive::helpers::LeaseTokenArg { lease: None },
            output: crate::cli::drive::format::OutputFormat::Yaml,
        };
        assert!(cmd.execute(&client).await.is_ok());
    }

    #[tokio::test]
    async fn delete_banding_command_runs_end_to_end() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_ungated_target(&server).await;

        let cmd = DeleteBandingCommand {
            spreadsheet_id: "sheet-1".to_string(),
            banded_range_id: 7,
            dry_run: true,
            lease: crate::cli::drive::helpers::LeaseTokenArg { lease: None },
            output: crate::cli::drive::format::OutputFormat::Table,
        };
        assert!(cmd.execute(&client).await.is_ok());
    }
}
