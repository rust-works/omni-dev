//! CLI commands for `omni-dev drive sheets` — reading and writing the
//! *cells* of a Google Sheet via the Sheets v4 API (issue #1589), editing
//! its *structure* via `spreadsheets.batchUpdate` (issue #1613),
//! *destructively* editing it the same way (issue #1623), and applying
//! formatting, data validation and protected ranges (issue #1643), the
//! basic filter and filter views (issue #1794), conditional formatting
//! rules (issue #1793), and charts and slicers (issue #1797).
//!
//! Nested under `drive` rather than given its own top-level tree so it
//! inherits `--account` resolution, the `auth` commands and the write-
//! permission diagnostics: a Sheet is a Drive file, and the permission gate
//! is a Drive concept.

pub(crate) mod banding;
pub(crate) mod conditional_format;
pub(crate) mod create;
pub(crate) mod developer_metadata;
pub(crate) mod dimension_group;
pub(crate) mod embedded_object;
pub(crate) mod filter;
pub(crate) mod format;
pub(crate) mod info;
pub(crate) mod named_range;
pub(crate) mod pivot;
pub(crate) mod protection;
pub(crate) mod read;
pub(crate) mod structure;
pub(crate) mod validation;
pub(crate) mod values;
pub(crate) mod write;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::drive::client::DriveClient;

/// Reads and writes the cells and structure of a Google Sheet.
#[derive(Parser)]
pub struct SheetsCommand {
    /// The sheets subcommand to execute.
    #[command(subcommand)]
    pub command: SheetsSubcommands,
}

/// Sheets subcommands.
#[derive(Subcommand)]
pub enum SheetsSubcommands {
    /// Shows a spreadsheet's title and the sheets (tabs) it contains.
    Info(info::InfoCommand),
    /// Reads cell values from one range, or from every sheet.
    Read(read::ReadCommand),
    /// Overwrites the cells of a range, gated by the
    /// write-permission rules (issues #1589, #1612). Requires the `drive.file` or
    /// `drive` scope (`drive auth login --write-file`/`--write-full`).
    Write(write::WriteCommand),
    /// Appends rows after the last row of a range's table, gated by the
    /// write-permission rules (issues #1589, #1612).
    Append(write::AppendCommand),
    /// Clears a range's values, leaving formatting intact. Gated by the
    /// write-permission rules (issues #1589, #1612).
    Clear(write::ClearCommand),
    /// Creates a new Google Sheet, optionally seeded with values. Gated by
    /// the folder write-permission rules' `create` operation (issue #1589).
    Create(create::CreateCommand),
    /// Adds a new sheet (tab) to a spreadsheet. Gated by the folder
    /// write-permission rules' `sheets-structure` operation (issue #1613).
    AddSheet(structure::AddSheetCommand),
    /// Renames an existing sheet. Gated by the folder write-permission
    /// rules' `sheets-structure` operation (issue #1613).
    RenameSheet(structure::RenameSheetCommand),
    /// Inserts empty rows, shifting existing rows down. Gated by the folder
    /// write-permission rules' `sheets-structure` operation (issue #1613).
    InsertRows(structure::InsertRowsCommand),
    /// Inserts empty columns, shifting existing columns right. Gated by the
    /// folder write-permission rules' `sheets-structure` operation
    /// (issue #1613).
    InsertColumns(structure::InsertColumnsCommand),
    /// Moves a contiguous block of rows to a new position within a sheet,
    /// shifting the rows in between. Gated by the folder write-permission
    /// rules' `sheets-structure` operation (issue #1834).
    MoveRows(structure::MoveRowsCommand),
    /// Moves a contiguous block of columns to a new position within a
    /// sheet, shifting the columns in between. Gated by the folder
    /// write-permission rules' `sheets-structure` operation (issue #1834).
    MoveColumns(structure::MoveColumnsCommand),
    /// Deletes an entire sheet (tab) from a spreadsheet. Gated by the folder
    /// write-permission rules' `sheets-delete` operation (issue #1623).
    /// Cannot be undone through omni-dev.
    DeleteSheet(structure::DeleteSheetCommand),
    /// Deletes whole rows, shifting existing rows up. Gated by the folder
    /// write-permission rules' `sheets-delete` operation (issue #1623).
    /// Cannot be undone through omni-dev.
    DeleteRows(structure::DeleteRowsCommand),
    /// Deletes whole columns, shifting existing columns left. Gated by the
    /// folder write-permission rules' `sheets-delete` operation (issue
    /// #1623). Cannot be undone through omni-dev.
    DeleteColumns(structure::DeleteColumnsCommand),
    /// Deletes a rectangular cell range, shifting the remainder along one
    /// axis. Gated by the folder write-permission rules' `sheets-delete`
    /// operation (issue #1623). Cannot be undone through omni-dev.
    DeleteRange(structure::DeleteRangeCommand),
    /// Copies an existing sheet within the same workbook. Gated by the
    /// folder write-permission rules' `sheets-structure` operation
    /// (issue #1643).
    DuplicateSheet(structure::DuplicateSheetCommand),
    /// Moves an existing sheet to a new position among its siblings. Gated
    /// by the folder write-permission rules' `sheets-structure` operation
    /// (issue #1643).
    ReorderSheet(structure::ReorderSheetCommand),
    /// Hides an existing sheet. Gated by the folder write-permission rules'
    /// `sheets-structure` operation (issue #1643).
    HideSheet(structure::HideSheetCommand),
    /// Shows an existing hidden sheet. Gated by the folder write-permission
    /// rules' `sheets-structure` operation (issue #1643).
    ShowSheet(structure::ShowSheetCommand),
    /// Changes a sheet's view properties — frozen rows/columns, tab color,
    /// right-to-left, hidden gridlines. Gated by the folder
    /// write-permission rules' `sheets-structure` operation (issue #1835).
    UpdateSheetProperties(structure::UpdateSheetPropertiesCommand),
    /// Changes workbook-level properties: locale, time zone, automatic
    /// recalculation, and iterative calculation. Gated by the folder
    /// write-permission rules' `sheets-structure` operation (issue #1836).
    UpdateWorkbookProperties(structure::UpdateWorkbookPropertiesCommand),
    /// Applies a cell format across a range. Gated by the folder
    /// write-permission rules' `sheets-structure` operation (issue #1643).
    FormatCells(format::FormatCellsCommand),
    /// Sets border lines on a range's edges. Gated by the folder
    /// write-permission rules' `sheets-structure` operation (issue #1643).
    UpdateBorders(format::UpdateBordersCommand),
    /// Merges a range into one cell, discarding every value but the
    /// top-left's. Gated by the folder write-permission rules'
    /// `sheets-structure` operation (issue #1643).
    MergeCells(format::MergeCellsCommand),
    /// Splits a previously merged range back apart. Gated by the folder
    /// write-permission rules' `sheets-structure` operation (issue #1643).
    UnmergeCells(format::UnmergeCellsCommand),
    /// Resizes rows or columns to fit their content. Gated by the folder
    /// write-permission rules' `sheets-structure` operation (issue #1643).
    AutoResizeDimension(format::AutoResizeDimensionCommand),
    /// Sets an explicit pixel width (columns) or height (rows). Gated by
    /// the folder write-permission rules' `sheets-structure` operation
    /// (issue #1643).
    UpdateDimensionProperties(format::UpdateDimensionPropertiesCommand),
    /// Sets a data validation rule on a range. Gated by the folder
    /// write-permission rules' `sheets-structure` operation (issue #1643).
    ///
    /// Boxed (#1792): tranche 2's ~19 extra condition flags pushed this
    /// variant far past every sibling's size, which `clippy::large_enum_variant`
    /// flags transitively up through `SheetsSubcommands`/`DriveSubcommands`/
    /// `Commands`.
    SetDataValidation(Box<validation::SetDataValidationCommand>),
    /// Removes a range's data validation rule. Gated by the folder
    /// write-permission rules' `sheets-structure` operation (issue #1643).
    ClearDataValidation(validation::ClearDataValidationCommand),
    /// Creates or updates a developer-metadata key/value pair on a
    /// spreadsheet, sheet, row or column. Restricted to `DOCUMENT`
    /// visibility. Gated by the folder write-permission rules'
    /// `sheets-structure` operation (issue #1795).
    SetDeveloperMetadata(developer_metadata::SetDeveloperMetadataCommand),
    /// Removes developer metadata matching a key and location, after
    /// reporting what would be removed. Restricted to `DOCUMENT`
    /// visibility. Gated by the folder write-permission rules'
    /// `sheets-structure` operation (issue #1795).
    DeleteDeveloperMetadata(developer_metadata::DeleteDeveloperMetadataCommand),
    /// Searches for developer metadata by key and/or location, restricted
    /// to `DOCUMENT` visibility. Read-only and ungated, like `sheets info`
    /// (issue #1795).
    SearchDeveloperMetadata(developer_metadata::SearchDeveloperMetadataCommand),
    /// Protects a range or an entire sheet. Gated by the folder
    /// write-permission rules' `sheets-protection` operation — distinct
    /// from `sheets-structure` (issue #1643).
    ProtectRange(protection::ProtectRangeCommand),
    /// Changes an existing protected range's description, warning-only
    /// flag, or editor list. Gated by the folder write-permission rules'
    /// `sheets-protection` operation (issue #1643).
    UpdateProtection(protection::UpdateProtectionCommand),
    /// Removes a protected range. Gated by the folder write-permission
    /// rules' `sheets-protection` operation (issue #1643).
    UnprotectRange(protection::UnprotectRangeCommand),
    /// Lists the protected ranges in a spreadsheet. Read-only and ungated,
    /// like `sheets info` (issue #1643).
    ListProtections(protection::ListProtectionsCommand),
    /// Sets (upserting any existing one) the basic filter on a sheet. Gated
    /// by the folder write-permission rules' `sheets-structure` operation
    /// (issue #1794).
    SetBasicFilter(filter::SetBasicFilterCommand),
    /// Removes a sheet's basic filter. Gated by the folder write-permission
    /// rules' `sheets-structure` operation (issue #1794).
    ClearBasicFilter(filter::ClearBasicFilterCommand),
    /// Adds a named filter view. Gated by the folder write-permission
    /// rules' `sheets-structure` operation (issue #1794).
    AddFilterView(filter::AddFilterViewCommand),
    /// Changes an existing filter view's title, range, sort order, or
    /// hidden values. Gated by the folder write-permission rules'
    /// `sheets-structure` operation (issue #1794).
    UpdateFilterView(filter::UpdateFilterViewCommand),
    /// Removes a filter view. Gated by the folder write-permission rules'
    /// `sheets-structure` operation (issue #1794).
    DeleteFilterView(filter::DeleteFilterViewCommand),
    /// Lists the filter views in a spreadsheet. Read-only and ungated, like
    /// `list-protections` (issue #1794).
    ListFilterViews(filter::ListFilterViewsCommand),
    /// Adds a conditional format rule to one or more ranges. Gated by the
    /// folder write-permission rules' `sheets-structure` operation
    /// (issue #1793, ADR-0081 §1).
    ///
    /// Boxed for the same `clippy::large_enum_variant` reason as
    /// `SetDataValidation` (#1792): the condition/gradient flag set is wide.
    AddConditionalFormat(Box<conditional_format::AddConditionalFormatCommand>),
    /// Replaces the conditional format rule at an index. Gated by the
    /// folder write-permission rules' `sheets-structure` operation
    /// (issue #1793, ADR-0081 §1).
    UpdateConditionalFormat(Box<conditional_format::UpdateConditionalFormatCommand>),
    /// Removes the conditional format rule at an index. Gated by the
    /// folder write-permission rules' `sheets-structure` operation
    /// (issue #1793, ADR-0081 §1).
    DeleteConditionalFormat(conditional_format::DeleteConditionalFormatCommand),
    /// Lists the conditional format rules in a spreadsheet. Read-only and
    /// ungated, like `list-protections` (issue #1793).
    ListConditionalFormats(conditional_format::ListConditionalFormatsCommand),
    /// Adds a named range. Gated by the folder write-permission rules'
    /// `sheets-structure` operation (issue #1796).
    AddNamedRange(named_range::AddNamedRangeCommand),
    /// Changes an existing named range's name and/or the range it covers.
    /// Gated by the folder write-permission rules' `sheets-structure`
    /// operation (issue #1796).
    UpdateNamedRange(named_range::UpdateNamedRangeCommand),
    /// Removes a named range. Gated by the folder write-permission rules'
    /// `sheets-structure` operation — not `sheets-delete`, since a named
    /// range is a label, not grid data (issue #1796, ADR-0081 §2). Reports
    /// every cell formula that referenced the name before removing it.
    DeleteNamedRange(named_range::DeleteNamedRangeCommand),
    /// Lists the named ranges in a spreadsheet. Read-only and ungated, like
    /// `sheets list-protections` (issue #1796).
    ListNamedRanges(named_range::ListNamedRangesCommand),
    /// Adds a chart. Gated by the folder write-permission rules'
    /// `sheets-structure` operation (issue #1797, ADR-0081 §3).
    ///
    /// Boxed for the same `clippy::large_enum_variant` reason as
    /// `SetDataValidation` (#1792): the chart-spec flag set is wide.
    AddChart(Box<embedded_object::AddChartCommand>),
    /// Replaces an existing chart's spec wholesale — `updateChartSpec`
    /// carries no field mask. Gated by the folder write-permission rules'
    /// `sheets-structure` operation (issue #1797, ADR-0081 §3).
    UpdateChart(Box<embedded_object::UpdateChartCommand>),
    /// Removes a chart, after reporting its spec (type, title, anchor).
    /// Gated by the folder write-permission rules' `sheets-structure`
    /// operation (issue #1797, ADR-0081 §3) — not `sheets-delete`; see that
    /// ADR section for why an unrecoverable embedded-object removal still
    /// sits here. Cannot be undone through omni-dev.
    DeleteChart(embedded_object::DeleteChartCommand),
    /// Lists the charts in a spreadsheet. Read-only and ungated, like
    /// `list-protections` (issue #1797).
    ListCharts(embedded_object::ListChartsCommand),
    /// Adds a slicer. Gated by the folder write-permission rules'
    /// `sheets-structure` operation (issue #1797, ADR-0081 §3).
    AddSlicer(embedded_object::AddSlicerCommand),
    /// Changes an existing slicer's range, filter column/criteria, title,
    /// or pivot-table linkage. Gated by the folder write-permission rules'
    /// `sheets-structure` operation (issue #1797, ADR-0081 §3).
    UpdateSlicer(embedded_object::UpdateSlicerCommand),
    /// Removes a slicer, after reporting its spec. Gated by the folder
    /// write-permission rules' `sheets-structure` operation (issue #1797,
    /// ADR-0081 §3) — not `sheets-delete`. Cannot be undone through
    /// omni-dev.
    DeleteSlicer(embedded_object::DeleteSlicerCommand),
    /// Lists the slicers in a spreadsheet. Read-only and ungated, like
    /// `list-protections` (issue #1797).
    ListSlicers(embedded_object::ListSlicersCommand),
    /// Writes a new pivot table at an anchor cell. Gated by **both** the
    /// folder write-permission rules' `sheets-write` and
    /// `sheets-structure` operations (issue #1798, ADR-0081 §5).
    AddPivotTable(pivot::AddPivotTableCommand),
    /// Clears the pivot table at an anchor cell. Gated by the folder
    /// write-permission rules' `sheets-write` operation alone (issue
    /// #1798, ADR-0081 §5).
    DeletePivotTable(pivot::DeletePivotTableCommand),
    /// Lists the pivot tables in a spreadsheet, by anchor cell. Read-only
    /// and ungated, like `list-conditional-formats` (issue #1798).
    ListPivotTables(pivot::ListPivotTablesCommand),
    /// Adds a banded range — alternating row or column colors. Gated by
    /// the folder write-permission rules' `sheets-structure` operation
    /// (issue #1832, ADR-0082): presentation applied to a range, same
    /// reasoning as `unmerge-cells`/`clear-data-validation`.
    AddBanding(banding::AddBandingCommand),
    /// Changes an existing banded range's range and/or colors. Gated by
    /// the folder write-permission rules' `sheets-structure` operation
    /// (issue #1832, ADR-0082).
    UpdateBanding(banding::UpdateBandingCommand),
    /// Removes a banded range. Gated by the folder write-permission rules'
    /// `sheets-structure` operation (issue #1832, ADR-0082) — it removes
    /// presentation, not grid data.
    DeleteBanding(banding::DeleteBandingCommand),
    /// Lists the banded ranges in a spreadsheet. Read-only and ungated,
    /// like `list-protections` (issue #1832).
    ListBandings(banding::ListBandingsCommand),
    /// Adds a new outline group — the collapsible +/- grouping bar — over
    /// a span of rows or columns. Gated by the folder write-permission
    /// rules' `sheets-structure` operation (issue #1833, ADR-0084): the
    /// same reasoning as `add-banding`.
    AddDimensionGroup(dimension_group::AddDimensionGroupCommand),
    /// Changes an existing group's `collapsed` state. Gated by the folder
    /// write-permission rules' `sheets-structure` operation (issue #1833,
    /// ADR-0084).
    UpdateDimensionGroup(dimension_group::UpdateDimensionGroupCommand),
    /// Removes an outline group. Gated by the folder write-permission
    /// rules' `sheets-structure` operation (issue #1833, ADR-0084) — it
    /// removes presentation, not grid data.
    DeleteDimensionGroup(dimension_group::DeleteDimensionGroupCommand),
    /// Lists the row and column outline groups in a spreadsheet. Read-only
    /// and ungated, like `list-bandings` (issue #1833).
    ListDimensionGroups(dimension_group::ListDimensionGroupsCommand),
}

impl SheetsCommand {
    /// Runs the command against the shared Drive client resolved by the
    /// parent `DriveCommand::execute`.
    ///
    /// Each leaf derives its own `SheetsClient` from that Drive client so the
    /// two hosts share one OAuth session — see
    /// [`crate::drive::sheets::client::SheetsClient::from_drive_client`].
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        match self.command {
            SheetsSubcommands::Info(cmd) => cmd.execute(client).await,
            SheetsSubcommands::Read(cmd) => cmd.execute(client).await,
            SheetsSubcommands::Write(cmd) => cmd.execute(client).await,
            SheetsSubcommands::Append(cmd) => cmd.execute(client).await,
            SheetsSubcommands::Clear(cmd) => cmd.execute(client).await,
            SheetsSubcommands::Create(cmd) => cmd.execute(client).await,
            SheetsSubcommands::AddSheet(cmd) => cmd.execute(client).await,
            SheetsSubcommands::RenameSheet(cmd) => cmd.execute(client).await,
            SheetsSubcommands::InsertRows(cmd) => cmd.execute(client).await,
            SheetsSubcommands::InsertColumns(cmd) => cmd.execute(client).await,
            SheetsSubcommands::MoveRows(cmd) => cmd.execute(client).await,
            SheetsSubcommands::MoveColumns(cmd) => cmd.execute(client).await,
            SheetsSubcommands::DeleteSheet(cmd) => cmd.execute(client).await,
            SheetsSubcommands::DeleteRows(cmd) => cmd.execute(client).await,
            SheetsSubcommands::DeleteColumns(cmd) => cmd.execute(client).await,
            SheetsSubcommands::DeleteRange(cmd) => cmd.execute(client).await,
            SheetsSubcommands::DuplicateSheet(cmd) => cmd.execute(client).await,
            SheetsSubcommands::ReorderSheet(cmd) => cmd.execute(client).await,
            SheetsSubcommands::HideSheet(cmd) => cmd.execute(client).await,
            SheetsSubcommands::ShowSheet(cmd) => cmd.execute(client).await,
            SheetsSubcommands::UpdateSheetProperties(cmd) => cmd.execute(client).await,
            SheetsSubcommands::UpdateWorkbookProperties(cmd) => cmd.execute(client).await,
            SheetsSubcommands::FormatCells(cmd) => cmd.execute(client).await,
            SheetsSubcommands::UpdateBorders(cmd) => cmd.execute(client).await,
            SheetsSubcommands::MergeCells(cmd) => cmd.execute(client).await,
            SheetsSubcommands::UnmergeCells(cmd) => cmd.execute(client).await,
            SheetsSubcommands::AutoResizeDimension(cmd) => cmd.execute(client).await,
            SheetsSubcommands::UpdateDimensionProperties(cmd) => cmd.execute(client).await,
            SheetsSubcommands::SetDataValidation(cmd) => cmd.execute(client).await,
            SheetsSubcommands::ClearDataValidation(cmd) => cmd.execute(client).await,
            SheetsSubcommands::SetDeveloperMetadata(cmd) => cmd.execute(client).await,
            SheetsSubcommands::DeleteDeveloperMetadata(cmd) => cmd.execute(client).await,
            SheetsSubcommands::SearchDeveloperMetadata(cmd) => cmd.execute(client).await,
            SheetsSubcommands::ProtectRange(cmd) => cmd.execute(client).await,
            SheetsSubcommands::UpdateProtection(cmd) => cmd.execute(client).await,
            SheetsSubcommands::UnprotectRange(cmd) => cmd.execute(client).await,
            SheetsSubcommands::ListProtections(cmd) => cmd.execute(client).await,
            SheetsSubcommands::SetBasicFilter(cmd) => cmd.execute(client).await,
            SheetsSubcommands::ClearBasicFilter(cmd) => cmd.execute(client).await,
            SheetsSubcommands::AddFilterView(cmd) => cmd.execute(client).await,
            SheetsSubcommands::UpdateFilterView(cmd) => cmd.execute(client).await,
            SheetsSubcommands::DeleteFilterView(cmd) => cmd.execute(client).await,
            SheetsSubcommands::ListFilterViews(cmd) => cmd.execute(client).await,
            SheetsSubcommands::AddConditionalFormat(cmd) => cmd.execute(client).await,
            SheetsSubcommands::UpdateConditionalFormat(cmd) => cmd.execute(client).await,
            SheetsSubcommands::DeleteConditionalFormat(cmd) => cmd.execute(client).await,
            SheetsSubcommands::ListConditionalFormats(cmd) => cmd.execute(client).await,
            SheetsSubcommands::AddNamedRange(cmd) => cmd.execute(client).await,
            SheetsSubcommands::UpdateNamedRange(cmd) => cmd.execute(client).await,
            SheetsSubcommands::DeleteNamedRange(cmd) => cmd.execute(client).await,
            SheetsSubcommands::ListNamedRanges(cmd) => cmd.execute(client).await,
            SheetsSubcommands::AddChart(cmd) => cmd.execute(client).await,
            SheetsSubcommands::UpdateChart(cmd) => cmd.execute(client).await,
            SheetsSubcommands::DeleteChart(cmd) => cmd.execute(client).await,
            SheetsSubcommands::ListCharts(cmd) => cmd.execute(client).await,
            SheetsSubcommands::AddSlicer(cmd) => cmd.execute(client).await,
            SheetsSubcommands::UpdateSlicer(cmd) => cmd.execute(client).await,
            SheetsSubcommands::DeleteSlicer(cmd) => cmd.execute(client).await,
            SheetsSubcommands::ListSlicers(cmd) => cmd.execute(client).await,
            SheetsSubcommands::AddPivotTable(cmd) => cmd.execute(client).await,
            SheetsSubcommands::DeletePivotTable(cmd) => cmd.execute(client).await,
            SheetsSubcommands::ListPivotTables(cmd) => cmd.execute(client).await,
            SheetsSubcommands::AddBanding(cmd) => cmd.execute(client).await,
            SheetsSubcommands::UpdateBanding(cmd) => cmd.execute(client).await,
            SheetsSubcommands::DeleteBanding(cmd) => cmd.execute(client).await,
            SheetsSubcommands::ListBandings(cmd) => cmd.execute(client).await,
            SheetsSubcommands::AddDimensionGroup(cmd) => cmd.execute(client).await,
            SheetsSubcommands::UpdateDimensionGroup(cmd) => cmd.execute(client).await,
            SheetsSubcommands::DeleteDimensionGroup(cmd) => cmd.execute(client).await,
            SheetsSubcommands::ListDimensionGroups(cmd) => cmd.execute(client).await,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
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

    /// Runs `cmd` through `SheetsCommand::execute`'s own dispatch match,
    /// rather than calling the leaf's `execute` directly — every other test
    /// in this crate's `drive sheets` leaves does the latter, which leaves
    /// this match's arms themselves uncovered (issue #1796's coverage
    /// review, PR #1811).
    async fn dispatch(command: SheetsSubcommands, client: &DriveClient) -> Result<()> {
        SheetsCommand { command }.execute(client).await
    }

    #[tokio::test]
    async fn the_named_range_dispatch_arms_reach_their_leaf_commands() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(crate::drive::sheets::client::SHEETS_API_URL, server.uri());
        // No write-permission rules are configured (an unconfigured
        // account), so every mutating leaf below is `Blocked` by default
        // policy — enough to reach and return from the leaf without a
        // lease or a workbook fetch, which a `Blocked` verdict never gets
        // to. `list-named-ranges` is ungated, so it goes further and
        // actually fetches the (named-range-free) workbook.
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
            .mount(&server)
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
            .mount(&server)
            .await;
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

        fn no_lease() -> crate::cli::drive::helpers::LeaseTokenArg {
            crate::cli::drive::helpers::LeaseTokenArg { lease: None }
        }

        assert!(dispatch(
            SheetsSubcommands::AddNamedRange(named_range::AddNamedRangeCommand {
                spreadsheet_id: "sheet-1".to_string(),
                name: "Foo".to_string(),
                range: Some("A1:A5".to_string()),
                sheet: Some("Q1".to_string()),
                whole_sheet: false,
                dry_run: true,
                lease: no_lease(),
                output: crate::cli::drive::format::OutputFormat::Table,
            }),
            &client,
        )
        .await
        .is_ok());

        assert!(dispatch(
            SheetsSubcommands::UpdateNamedRange(named_range::UpdateNamedRangeCommand {
                spreadsheet_id: "sheet-1".to_string(),
                name: "Foo".to_string(),
                new_name: Some("Bar".to_string()),
                range: None,
                sheet: None,
                whole_sheet: false,
                dry_run: true,
                lease: no_lease(),
                output: crate::cli::drive::format::OutputFormat::Table,
            }),
            &client,
        )
        .await
        .is_ok());

        assert!(dispatch(
            SheetsSubcommands::DeleteNamedRange(named_range::DeleteNamedRangeCommand {
                spreadsheet_id: "sheet-1".to_string(),
                name: "Foo".to_string(),
                dry_run: true,
                lease: no_lease(),
                output: crate::cli::drive::format::OutputFormat::Table,
            }),
            &client,
        )
        .await
        .is_ok());

        assert!(dispatch(
            SheetsSubcommands::ListNamedRanges(named_range::ListNamedRangesCommand {
                spreadsheet_id: "sheet-1".to_string(),
                output: crate::cli::drive::format::OutputFormat::Table,
            }),
            &client,
        )
        .await
        .is_ok());
    }

    #[tokio::test]
    async fn the_chart_and_slicer_dispatch_arms_reach_their_leaf_commands() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(crate::drive::sheets::client::SHEETS_API_URL, server.uri());
        // Same trick as `the_named_range_dispatch_arms_reach_their_leaf_commands`
        // above: with no write-permission rules configured, every mutating
        // chart/slicer leaf is `Blocked` by default policy, which returns
        // `Ok(())` without a lease or a `batchUpdate`. `list-charts` and
        // `list-slicers` are ungated, so they go on to fetch the workbook.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/sheet-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "sheet-1",
                    "name": "Budget",
                    "mimeType": crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
                    "parents": [],
                })),
            )
            .mount(&server)
            .await;
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

        fn no_lease() -> crate::cli::drive::helpers::LeaseTokenArg {
            crate::cli::drive::helpers::LeaseTokenArg { lease: None }
        }

        assert!(dispatch(
            SheetsSubcommands::AddChart(Box::new(embedded_object::AddChartCommand {
                spreadsheet_id: "sheet-1".to_string(),
                chart_type: "column".to_string(),
                domain: "A1:A10".to_string(),
                series: vec!["B1:B10".to_string()],
                sheet: Some("Sheet1".to_string()),
                title: None,
                subtitle: None,
                legend: None,
                stacked: None,
                header_count: None,
                horizontal_axis_title: None,
                vertical_axis_title: None,
                pie_hole: None,
                anchor: Some("E2".to_string()),
                offset_x: None,
                offset_y: None,
                width: None,
                height: None,
                new_sheet: false,
                dry_run: true,
                lease: no_lease(),
                output: crate::cli::drive::format::OutputFormat::Table,
            })),
            &client,
        )
        .await
        .is_ok());

        assert!(dispatch(
            SheetsSubcommands::UpdateChart(Box::new(embedded_object::UpdateChartCommand {
                spreadsheet_id: "sheet-1".to_string(),
                chart_id: 1,
                chart_type: None,
                domain: None,
                series: Vec::new(),
                sheet: None,
                title: Some("New title".to_string()),
                subtitle: None,
                legend: None,
                stacked: None,
                header_count: None,
                horizontal_axis_title: None,
                vertical_axis_title: None,
                pie_hole: None,
                dry_run: true,
                lease: no_lease(),
                output: crate::cli::drive::format::OutputFormat::Table,
            })),
            &client,
        )
        .await
        .is_ok());

        assert!(dispatch(
            SheetsSubcommands::DeleteChart(embedded_object::DeleteChartCommand {
                spreadsheet_id: "sheet-1".to_string(),
                chart_id: 1,
                dry_run: true,
                lease: no_lease(),
                output: crate::cli::drive::format::OutputFormat::Table,
            }),
            &client,
        )
        .await
        .is_ok());

        assert!(dispatch(
            SheetsSubcommands::ListCharts(embedded_object::ListChartsCommand {
                spreadsheet_id: "sheet-1".to_string(),
                output: crate::cli::drive::format::OutputFormat::Table,
            }),
            &client,
        )
        .await
        .is_ok());

        assert!(dispatch(
            SheetsSubcommands::AddSlicer(embedded_object::AddSlicerCommand {
                spreadsheet_id: "sheet-1".to_string(),
                sheet: Some("Sheet1".to_string()),
                range: "A1:D10".to_string(),
                column: 1,
                hide_values: vec!["Closed".to_string()],
                title: None,
                apply_to_pivot_tables: None,
                anchor: "F2".to_string(),
                offset_x: None,
                offset_y: None,
                width: None,
                height: None,
                dry_run: true,
                lease: no_lease(),
                output: crate::cli::drive::format::OutputFormat::Table,
            }),
            &client,
        )
        .await
        .is_ok());

        assert!(dispatch(
            SheetsSubcommands::UpdateSlicer(embedded_object::UpdateSlicerCommand {
                spreadsheet_id: "sheet-1".to_string(),
                slicer_id: 1,
                sheet: None,
                range: None,
                column: None,
                hide_values: Vec::new(),
                clear_criteria: true,
                title: None,
                apply_to_pivot_tables: None,
                dry_run: true,
                lease: no_lease(),
                output: crate::cli::drive::format::OutputFormat::Table,
            }),
            &client,
        )
        .await
        .is_ok());

        assert!(dispatch(
            SheetsSubcommands::DeleteSlicer(embedded_object::DeleteSlicerCommand {
                spreadsheet_id: "sheet-1".to_string(),
                slicer_id: 1,
                dry_run: true,
                lease: no_lease(),
                output: crate::cli::drive::format::OutputFormat::Table,
            }),
            &client,
        )
        .await
        .is_ok());

        assert!(dispatch(
            SheetsSubcommands::ListSlicers(embedded_object::ListSlicersCommand {
                spreadsheet_id: "sheet-1".to_string(),
                output: crate::cli::drive::format::OutputFormat::Table,
            }),
            &client,
        )
        .await
        .is_ok());
    }

    #[tokio::test]
    async fn the_pivot_table_dispatch_arms_reach_their_leaf_commands() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(crate::drive::sheets::client::SHEETS_API_URL, server.uri());
        // No write-permission rules are configured (an unconfigured
        // account), so both mutating leaves below are `Blocked` by default
        // policy — enough to reach and return from the leaf without a
        // lease or a workbook fetch. `list-pivot-tables` is ungated, so it
        // goes further and actually fetches the (pivot-table-free)
        // workbook.
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
            .mount(&server)
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
            .mount(&server)
            .await;
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

        fn no_lease() -> crate::cli::drive::helpers::LeaseTokenArg {
            crate::cli::drive::helpers::LeaseTokenArg { lease: None }
        }

        assert!(dispatch(
            SheetsSubcommands::AddPivotTable(pivot::AddPivotTableCommand {
                spreadsheet_id: "sheet-1".to_string(),
                sheet: "Q1".to_string(),
                anchor: "A1".to_string(),
                source: "A1:B10".to_string(),
                rows: vec!["0".to_string()],
                columns: Vec::new(),
                values: vec!["1:sum".to_string()],
                filters: Vec::new(),
                value_layout: None,
                no_totals: false,
                dry_run: true,
                lease: no_lease(),
                output: crate::cli::drive::format::OutputFormat::Table,
            }),
            &client,
        )
        .await
        .is_ok());

        assert!(dispatch(
            SheetsSubcommands::DeletePivotTable(pivot::DeletePivotTableCommand {
                spreadsheet_id: "sheet-1".to_string(),
                sheet: "Q1".to_string(),
                anchor: "A1".to_string(),
                dry_run: true,
                lease: no_lease(),
                output: crate::cli::drive::format::OutputFormat::Table,
            }),
            &client,
        )
        .await
        .is_ok());

        assert!(dispatch(
            SheetsSubcommands::ListPivotTables(pivot::ListPivotTablesCommand {
                spreadsheet_id: "sheet-1".to_string(),
                output: crate::cli::drive::format::OutputFormat::Table,
            }),
            &client,
        )
        .await
        .is_ok());
    }

    #[tokio::test]
    async fn the_banding_dispatch_arms_reach_their_leaf_commands() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(crate::drive::sheets::client::SHEETS_API_URL, server.uri());
        // No write-permission rules are configured (an unconfigured
        // account), so every mutating leaf below is `Blocked` by default
        // policy — enough to reach and return from the leaf without a
        // lease or a workbook fetch. `list-bandings` is ungated, so it
        // goes further and actually fetches the (banding-free) workbook.
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
            .mount(&server)
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
            .mount(&server)
            .await;
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

        fn no_lease() -> crate::cli::drive::helpers::LeaseTokenArg {
            crate::cli::drive::helpers::LeaseTokenArg { lease: None }
        }

        assert!(dispatch(
            SheetsSubcommands::AddBanding(banding::AddBandingCommand {
                spreadsheet_id: "sheet-1".to_string(),
                sheet: "Sheet1".to_string(),
                range: "A1:D10".to_string(),
                axis: banding::BandingAxisArg::Rows,
                header_color: None,
                first_band_color: "#FFFFFF".to_string(),
                second_band_color: "#EEEEEE".to_string(),
                footer_color: None,
                dry_run: true,
                lease: no_lease(),
                output: crate::cli::drive::format::OutputFormat::Table,
            }),
            &client,
        )
        .await
        .is_ok());

        assert!(dispatch(
            SheetsSubcommands::UpdateBanding(banding::UpdateBandingCommand {
                spreadsheet_id: "sheet-1".to_string(),
                banded_range_id: 1,
                sheet: None,
                range: None,
                axis: banding::BandingAxisArg::Rows,
                header_color: Some("#000000".to_string()),
                first_band_color: None,
                second_band_color: None,
                footer_color: None,
                dry_run: true,
                lease: no_lease(),
                output: crate::cli::drive::format::OutputFormat::Table,
            }),
            &client,
        )
        .await
        .is_ok());

        assert!(dispatch(
            SheetsSubcommands::DeleteBanding(banding::DeleteBandingCommand {
                spreadsheet_id: "sheet-1".to_string(),
                banded_range_id: 1,
                dry_run: true,
                lease: no_lease(),
                output: crate::cli::drive::format::OutputFormat::Table,
            }),
            &client,
        )
        .await
        .is_ok());

        assert!(dispatch(
            SheetsSubcommands::ListBandings(banding::ListBandingsCommand {
                spreadsheet_id: "sheet-1".to_string(),
                output: crate::cli::drive::format::OutputFormat::Table,
            }),
            &client,
        )
        .await
        .is_ok());
    }

    #[tokio::test]
    async fn the_dimension_group_dispatch_arms_reach_their_leaf_commands() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(crate::drive::sheets::client::SHEETS_API_URL, server.uri());
        // No write-permission rules are configured (an unconfigured
        // account), so every mutating leaf below is `Blocked` by default
        // policy — enough to reach and return from the leaf without a
        // lease or a workbook fetch. `list-dimension-groups` is ungated,
        // so it goes further and actually fetches the (group-free)
        // workbook.
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
            .mount(&server)
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
            .mount(&server)
            .await;
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

        fn no_lease() -> crate::cli::drive::helpers::LeaseTokenArg {
            crate::cli::drive::helpers::LeaseTokenArg { lease: None }
        }

        assert!(dispatch(
            SheetsSubcommands::AddDimensionGroup(dimension_group::AddDimensionGroupCommand {
                spreadsheet_id: "sheet-1".to_string(),
                sheet: "Sheet1".to_string(),
                dimension: crate::cli::drive::sheets::format::DimensionArg::Rows,
                start: 1,
                end: 5,
                dry_run: true,
                lease: no_lease(),
                output: crate::cli::drive::format::OutputFormat::Table,
            }),
            &client,
        )
        .await
        .is_ok());

        assert!(dispatch(
            SheetsSubcommands::UpdateDimensionGroup(dimension_group::UpdateDimensionGroupCommand {
                spreadsheet_id: "sheet-1".to_string(),
                sheet: "Sheet1".to_string(),
                dimension: crate::cli::drive::sheets::format::DimensionArg::Rows,
                start: 1,
                end: 5,
                depth: None,
                collapsed: true,
                dry_run: true,
                lease: no_lease(),
                output: crate::cli::drive::format::OutputFormat::Table,
            }),
            &client,
        )
        .await
        .is_ok());

        assert!(dispatch(
            SheetsSubcommands::DeleteDimensionGroup(dimension_group::DeleteDimensionGroupCommand {
                spreadsheet_id: "sheet-1".to_string(),
                sheet: "Sheet1".to_string(),
                dimension: crate::cli::drive::sheets::format::DimensionArg::Rows,
                start: 1,
                end: 5,
                dry_run: true,
                lease: no_lease(),
                output: crate::cli::drive::format::OutputFormat::Table,
            }),
            &client,
        )
        .await
        .is_ok());

        assert!(dispatch(
            SheetsSubcommands::ListDimensionGroups(dimension_group::ListDimensionGroupsCommand {
                spreadsheet_id: "sheet-1".to_string(),
                output: crate::cli::drive::format::OutputFormat::Table,
            }),
            &client,
        )
        .await
        .is_ok());
    }

    #[tokio::test]
    async fn the_update_sheet_properties_dispatch_arm_reaches_its_leaf_command() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(crate::drive::sheets::client::SHEETS_API_URL, server.uri());
        // No write-permission rules are configured (an unconfigured
        // account), so the mutating leaf below is `Blocked` by default
        // policy — enough to reach and return from the leaf without a
        // lease or a workbook fetch, which a `Blocked` verdict never gets
        // to (same trick as `the_banding_dispatch_arms_reach_their_leaf_commands`).
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
            .mount(&server)
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
            .mount(&server)
            .await;

        fn no_lease() -> crate::cli::drive::helpers::LeaseTokenArg {
            crate::cli::drive::helpers::LeaseTokenArg { lease: None }
        }

        assert!(dispatch(
            SheetsSubcommands::UpdateSheetProperties(structure::UpdateSheetPropertiesCommand {
                spreadsheet_id: "sheet-1".to_string(),
                sheet: "Q1".to_string(),
                freeze_rows: Some(1),
                freeze_columns: None,
                tab_color: Some("#FF8800".to_string()),
                clear_tab_color: false,
                right_to_left: None,
                hide_gridlines: None,
                dry_run: true,
                lease: no_lease(),
                output: crate::cli::drive::format::OutputFormat::Table,
            }),
            &client,
        )
        .await
        .is_ok());
    }
}
