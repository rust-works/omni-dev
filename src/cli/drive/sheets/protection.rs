//! CLI commands for `omni-dev drive sheets protect-range`/
//! `update-protection`/`unprotect-range`/`list-protections` (issue #1643).
//!
//! The first three are gated by
//! [`DriveOperation::SheetsProtection`](crate::drive::write_gate::DriveOperation::SheetsProtection),
//! not `sheets-structure` — see that operation's doc comment.
//! `list-protections` is a plain read, ungated like `sheets info`.

use anyhow::Result;
use clap::Parser;

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::cli::drive::helpers;
use crate::drive::client::DriveClient;
use crate::drive::sheets::api::SheetsApi;
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::protection::{
    describe_lines, protection, ProtectionOptions, ProtectionVerb,
};

/// Protects a range (or, with `--whole-sheet`, an entire sheet).
#[derive(Parser)]
pub struct ProtectRangeCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// A1 range to protect, optionally carrying its own `Sheet!` prefix.
    /// Mutually exclusive with `--whole-sheet`.
    #[arg(long, value_name = "A1")]
    pub range: Option<String>,

    /// Sheet (tab) title. Supplies the prefix for a bare `--range`, or the
    /// target sheet directly with `--whole-sheet`.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// Protect the entire sheet named by `--sheet`, rather than a range
    /// within it.
    #[arg(long)]
    pub whole_sheet: bool,

    /// A human-readable note about why the range is protected.
    #[arg(long, value_name = "TEXT")]
    pub description: Option<String>,

    /// Only warn on an edit rather than blocking it outright.
    #[arg(long)]
    pub warning_only: bool,

    /// An editor exempted from the protection. Repeatable.
    #[arg(long = "editor", value_name = "EMAIL")]
    pub editors: Vec<String>,

    /// Reports the gate verdict and the change that would be made, without
    /// calling `spreadsheets.batchUpdate`.
    #[arg(long)]
    pub dry_run: bool,

    /// The lease token from `drive lease acquire`, required unless the
    /// deciding write-permission rule sets `require_lease: false`
    /// ([ADR-0080](../../../../docs/adrs/adr-0080.md) §1/§9/§13). Never
    /// needed with `--dry-run`.
    #[arg(long, value_name = "TOKEN")]
    pub lease: Option<String>,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ProtectRangeCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = ProtectionOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: ProtectionVerb::ProtectRange {
                sheet: self.sheet,
                range: self.range,
                whole_sheet: self.whole_sheet,
                description: self.description,
                warning_only: self.warning_only,
                editors: self.editors,
            },
            dry_run: self.dry_run,
            lease_token: self.lease,
            ledger_path: resolve_ledger_path(self.dry_run)?,
        };
        run_protection(client, &opts, &self.output).await
    }
}

/// Changes an existing protected range's description, warning-only flag,
/// or editor list. The target protection is resolved by exact range match
/// — see `drive sheets list-protections` to find it.
#[derive(Parser)]
pub struct UpdateProtectionCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// A1 range identifying the existing protection, optionally carrying
    /// its own `Sheet!` prefix. Must match exactly — see
    /// `drive sheets list-protections`. Mutually exclusive with
    /// `--whole-sheet`.
    #[arg(long, value_name = "A1")]
    pub range: Option<String>,

    /// Sheet (tab) title. Supplies the prefix for a bare `--range`, or the
    /// target sheet directly with `--whole-sheet`.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// Target the whole-sheet protection on `--sheet`, rather than one
    /// covering a range within it — the only way to reach a protection
    /// created with `protect-range --whole-sheet`, which has no range of
    /// its own to match against.
    #[arg(long)]
    pub whole_sheet: bool,

    /// The new description.
    #[arg(long, value_name = "TEXT")]
    pub description: Option<String>,

    /// The new warning-only flag.
    #[arg(long, value_name = "BOOL")]
    pub warning_only: Option<bool>,

    /// An editor to add. Repeatable.
    #[arg(long = "add-editor", value_name = "EMAIL")]
    pub add_editors: Vec<String>,

    /// An editor to remove. Repeatable.
    #[arg(long = "remove-editor", value_name = "EMAIL")]
    pub remove_editors: Vec<String>,

    /// Reports the gate verdict and the change that would be made, without
    /// calling `spreadsheets.batchUpdate`.
    #[arg(long)]
    pub dry_run: bool,

    /// The lease token from `drive lease acquire`, required unless the
    /// deciding write-permission rule sets `require_lease: false`
    /// ([ADR-0080](../../../../docs/adrs/adr-0080.md) §1/§9/§13). Never
    /// needed with `--dry-run`.
    #[arg(long, value_name = "TOKEN")]
    pub lease: Option<String>,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl UpdateProtectionCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = ProtectionOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: ProtectionVerb::UpdateProtection {
                sheet: self.sheet,
                range: self.range,
                whole_sheet: self.whole_sheet,
                description: self.description,
                warning_only: self.warning_only,
                add_editors: self.add_editors,
                remove_editors: self.remove_editors,
            },
            dry_run: self.dry_run,
            lease_token: self.lease,
            ledger_path: resolve_ledger_path(self.dry_run)?,
        };
        run_protection(client, &opts, &self.output).await
    }
}

/// Removes a protected range. The target is resolved by exact range match
/// — see `drive sheets list-protections` to find it.
#[derive(Parser)]
pub struct UnprotectRangeCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// A1 range identifying the protection to remove, optionally carrying
    /// its own `Sheet!` prefix. Must match exactly. Mutually exclusive with
    /// `--whole-sheet`.
    #[arg(long, value_name = "A1")]
    pub range: Option<String>,

    /// Sheet (tab) title. Supplies the prefix for a bare `--range`, or the
    /// target sheet directly with `--whole-sheet`.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// Target the whole-sheet protection on `--sheet`, rather than one
    /// covering a range within it — the only way to reach a protection
    /// created with `protect-range --whole-sheet`, which has no range of
    /// its own to match against.
    #[arg(long)]
    pub whole_sheet: bool,

    /// Reports the gate verdict and the change that would be made, without
    /// calling `spreadsheets.batchUpdate`.
    #[arg(long)]
    pub dry_run: bool,

    /// The lease token from `drive lease acquire`, required unless the
    /// deciding write-permission rule sets `require_lease: false`
    /// ([ADR-0080](../../../../docs/adrs/adr-0080.md) §1/§9/§13). Never
    /// needed with `--dry-run`.
    #[arg(long, value_name = "TOKEN")]
    pub lease: Option<String>,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl UnprotectRangeCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = ProtectionOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: ProtectionVerb::UnprotectRange {
                sheet: self.sheet,
                range: self.range,
                whole_sheet: self.whole_sheet,
            },
            dry_run: self.dry_run,
            lease_token: self.lease,
            ledger_path: resolve_ledger_path(self.dry_run)?,
        };
        run_protection(client, &opts, &self.output).await
    }
}

/// Lists the protected ranges in a spreadsheet.
///
/// Read-only and ungated, like `sheets info` — needed so `update-protection`/
/// `unprotect-range` are usable at all, since a protected range's numeric
/// id and current editors are otherwise invisible from the CLI.
#[derive(Parser)]
pub struct ListProtectionsCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ListProtectionsCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let sheets = SheetsClient::from_drive_client(client)?;
        let api = SheetsApi::new(&sheets);
        let workbook = api
            .get_spreadsheet_with_protections(&self.spreadsheet_id)
            .await?;
        if output_as(&workbook, &self.output)? {
            return Ok(());
        }
        for sheet in &workbook.sheets {
            for protected in &sheet.protected_ranges {
                let id = protected
                    .protected_range_id
                    .map_or_else(|| "?".to_string(), |id| id.to_string());
                let range = protected
                    .range
                    .as_ref()
                    .map_or_else(|| "(unresolvable)".to_string(), render_grid_range);
                let description = protected.description.as_deref().unwrap_or("");
                let warning_only = protected.warning_only.unwrap_or(false);
                let editors = protected
                    .editors
                    .as_ref()
                    .map(|e| e.users.join(", "))
                    .unwrap_or_default();
                println!(
                    "{}",
                    sanitize_for_terminal(&format!(
                        "id {id}: {range}  description={description:?}  \
                         warning_only={warning_only}  editors=[{editors}]  \
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
/// as a compact 1-based description for `list-protections`' human-readable
/// output — e.g. `"sheetId 0, rows 1-5, cols 1-2"`, or `"sheetId 0 (whole
/// sheet)"` when every bound is `None`. Row/column *count* is deliberately
/// not shown: unlike `structure.rs`'s inserts, a protection's bounds are
/// static, so there is no "before -> after" to state.
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

/// Resolves the lease ledger path for one of this module's commands.
///
/// A dry run never checks a lease (`protection_inner` returns `WouldChange`
/// before the ledger is ever touched, mirroring `drive edit`'s own
/// `--dry-run` reasoning) — resolving a real path here would make a purely
/// read-only preview depend on the state directory existing at all.
fn resolve_ledger_path(dry_run: bool) -> Result<std::path::PathBuf> {
    if dry_run {
        Ok(std::path::PathBuf::new())
    } else {
        crate::drive::lease::ledger::ledger_path()
    }
}

async fn run_protection(
    client: &DriveClient,
    opts: &ProtectionOptions,
    output: &OutputFormat,
) -> Result<()> {
    let sheets = SheetsClient::from_drive_client(client)?;
    let rules = helpers::active_account_rules()?;
    let outcome = protection(client, &sheets, opts, &rules).await;
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
    use crate::drive::sheets::types::GridRange;
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
    fn render_grid_range_rows_only() {
        let range = GridRange {
            sheet_id: 0,
            start_row_index: Some(0),
            end_row_index: Some(5),
            ..Default::default()
        };
        assert_eq!(render_grid_range(&range), "sheetId 0, rows 1-5");
    }

    #[test]
    fn render_grid_range_cols_only() {
        let range = GridRange {
            sheet_id: 0,
            start_column_index: Some(0),
            end_column_index: Some(2),
            ..Default::default()
        };
        assert_eq!(render_grid_range(&range), "sheetId 0, cols 1-2");
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
    async fn list_protections_prints_every_protected_range() {
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
                        "protectedRanges": [
                            {
                                "protectedRangeId": 1,
                                "range": {
                                    "sheetId": 0,
                                    "startRowIndex": 0,
                                    "endRowIndex": 5,
                                    "startColumnIndex": 0,
                                    "endColumnIndex": 2,
                                },
                                "description": "locked",
                                "warningOnly": true,
                                "editors": {"users": ["a@example.com", "b@example.com"]},
                            },
                            {"protectedRangeId": 2},
                        ],
                    }],
                })),
            )
            .mount(&server)
            .await;

        let cmd = ListProtectionsCommand {
            spreadsheet_id: "sheet-1".to_string(),
            output: crate::cli::drive::format::OutputFormat::Table,
        };
        assert!(cmd.execute(&client).await.is_ok());
    }
}
