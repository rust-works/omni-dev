//! MCP tool handlers for `drive sheets info`/`drive sheets read`.
//!
//! Split out from `drive_tools.rs` (rather than growing it further) the same
//! way `src/cli/drive/sheets/` is its own subtree under `src/cli/drive/`.
//!
//! Both tools are read-only. Per [`crate::drive::sheets::read`]'s own module
//! doc and [ADR-0071](../../docs/adrs/adr-0071.md) §11, the Sheets read path
//! consults no write gate — unlike `sheets write`/`append`/`clear`, which
//! stay CLI-only for now ([ADR-0073](../../docs/adrs/adr-0073.md) §12; see
//! issue #1614). That means these tools need no `FolderPermissionRule`s, no
//! `dry_run` param, and can never produce a `RefusedNoVisibleParents`-shaped
//! outcome — that concept belongs to the write gate alone.
//!
//! Like every Drive tool, each handler takes an optional `account` parameter
//! (see `drive_tools.rs`'s module doc); the doc string is shared via
//! [`crate::mcp::drive_tools::account_param_doc`] rather than forked.

use anyhow::{Context, Result};
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock as Content},
    schemars, tool, tool_router, ErrorData as McpError,
};
use serde::{Deserialize, Serialize};

use crate::cli::drive::helpers::create_client_for;
use crate::drive::sheets::api::{SheetsApi, ValueRenderOption};
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::read::{read, ReadOptions};
use crate::mcp::drive_tools::account_param_doc;

use super::error::tool_error;
use super::git_tools::build_truncated_result;
use super::output_file;
use super::server::OmniDevServer;

// ── Parameter structs ───────────────────────────────────────────────

/// Parameters for the `drive_sheets_info` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DriveSheetsInfoParams {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL). Required.
    pub spreadsheet_id: String,
    #[doc = account_param_doc!()]
    #[serde(default)]
    pub account: Option<String>,
}

/// Parameters for the `drive_sheets_read` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DriveSheetsReadParams {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL). Required.
    pub spreadsheet_id: String,
    /// An explicit A1 range, which may carry its own `Sheet!` prefix (e.g.
    /// `A1:C10`, `'My Sheet'!A:A`). Combined with `sheet` when bare. Omit
    /// both `range` and `sheet` to read every sheet in the workbook (capped
    /// at 200 sheets; narrow with `sheet`/`range` above that cap).
    #[serde(default)]
    pub range: Option<String>,
    /// Sheet (tab) title to read. Supplies the prefix for a bare `range`, or
    /// selects the whole tab on its own. Conflicts with a `range` that
    /// already names a sheet.
    #[serde(default)]
    pub sheet: Option<String>,
    /// How cell values are rendered. `formatted` (default) matches the
    /// spreadsheet as displayed; `unformatted` yields raw typed numbers
    /// rather than locale-formatted strings; `formula` yields formula text.
    #[serde(default)]
    pub render: Option<String>,
    /// When set, writes the result (YAML) to this path and returns a short
    /// summary instead of the inline body — recommended for a whole-workbook
    /// read that would exceed the response size limit.
    #[serde(default)]
    pub output_file: Option<String>,
    #[doc = account_param_doc!()]
    #[serde(default)]
    pub account: Option<String>,
}

// ── Tool handlers ────────────────────────────────────────────────────

#[allow(missing_docs)] // #[tool_router] generates a pub `drive_sheets_tool_router` fn.
#[tool_router(router = drive_sheets_tool_router, vis = "pub")]
impl OmniDevServer {
    /// Tool: show a spreadsheet's title and the sheets (tabs) it contains.
    #[tool(
        description = "Show a spreadsheet's title and the sheets (tabs) it contains — dimensions \
                       and a hidden flag per tab, no cell data (the underlying `spreadsheets.get` \
                       call requests a fields mask that excludes cell values, so this stays cheap \
                       regardless of workbook size). Use `drive_sheets_read` for actual cell \
                       values. \
                       Read-only. Mirrors `omni-dev drive sheets info`. Output is YAML."
    )]
    pub async fn drive_sheets_info(
        &self,
        Parameters(params): Parameters<DriveSheetsInfoParams>,
    ) -> Result<CallToolResult, McpError> {
        let client = create_client_for(params.account.as_deref()).map_err(tool_error)?;
        let sheets = SheetsClient::from_drive_client(&client).map_err(tool_error)?;
        let yaml = run_sheets_info(&sheets, &params)
            .await
            .map_err(tool_error)?;
        Ok(build_truncated_result(yaml))
    }

    /// Tool: read cell values from a spreadsheet.
    #[tool(
        description = "Read cell values from a spreadsheet: one A1 `range`/`sheet`, or — when \
                       both are omitted — every sheet in the workbook (capped at 200 sheets; \
                       narrow with `sheet`/`range` above that cap, rather than returning a \
                       partial workbook). `render` controls how values come back: `formatted` \
                       (default) matches the spreadsheet as displayed, `unformatted` gives raw \
                       typed numbers/booleans, `formula` gives formula text. Values are returned \
                       ragged exactly as the API returns them (trailing empty cells/rows \
                       truncated). When `output_file` is set, writes the YAML result to that path \
                       and returns a short summary instead — recommended for a large \
                       whole-workbook read. \
                       Read-only — no write gate or dry-run applies (unlike `sheets \
                       write`/`append`/`clear`, which have no MCP equivalent yet). \
                       Mirrors `omni-dev drive sheets read`. Output is YAML."
    )]
    pub async fn drive_sheets_read(
        &self,
        Parameters(params): Parameters<DriveSheetsReadParams>,
    ) -> Result<CallToolResult, McpError> {
        let client = create_client_for(params.account.as_deref()).map_err(tool_error)?;
        let sheets = SheetsClient::from_drive_client(&client).map_err(tool_error)?;
        let wrote_to_file = params.output_file.is_some();
        let text = run_sheets_read(&sheets, &params)
            .await
            .map_err(tool_error)?;
        if wrote_to_file {
            Ok(CallToolResult::success(vec![Content::text(text)]))
        } else {
            Ok(build_truncated_result(text))
        }
    }
}

// ── Internal run_* implementations ──────────────────────────────────
//
// Split out from the tool handlers, taking an already-resolved
// `&SheetsClient`, so they can be tested against a wiremock-backed client
// without needing real credentials (mirrors `run_search`/`run_file_read` in
// `drive_tools.rs`, and the CLI's `run_info`/`run_read`).

async fn run_sheets_info(client: &SheetsClient, params: &DriveSheetsInfoParams) -> Result<String> {
    let spreadsheet = SheetsApi::new(client)
        .get_spreadsheet(&params.spreadsheet_id)
        .await?;
    yaml_result(&spreadsheet)
}

async fn run_sheets_read(client: &SheetsClient, params: &DriveSheetsReadParams) -> Result<String> {
    let render = parse_render_option(params.render.as_deref())?;
    let opts = ReadOptions {
        spreadsheet_id: params.spreadsheet_id.clone(),
        range: params.range.clone(),
        sheet: params.sheet.clone(),
        render,
    };
    let outcome = read(&SheetsApi::new(client), &opts).await?;
    let yaml = yaml_result(&outcome)?;
    match params.output_file.as_deref() {
        Some(path) => output_file::write_to_file_yaml(path, &yaml, "yaml"),
        None => Ok(yaml),
    }
}

/// Parses an MCP-supplied render string. `None` defaults to `formatted`.
fn parse_render_option(raw: Option<&str>) -> Result<ValueRenderOption> {
    match raw.map(str::to_ascii_lowercase).as_deref() {
        None | Some("formatted") => Ok(ValueRenderOption::Formatted),
        Some("unformatted") => Ok(ValueRenderOption::Unformatted),
        Some("formula") => Ok(ValueRenderOption::Formula),
        Some(other) => {
            anyhow::bail!(
                "unknown render {other:?} (expected 'formatted', 'unformatted', or 'formula')"
            )
        }
    }
}

fn yaml_result<T: Serialize>(data: &T) -> Result<String> {
    serde_yaml::to_string(data).context("Failed to serialize result as YAML")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use rmcp::handler::server::wrapper::Parameters;

    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::client::DriveClient;
    use crate::drive::sheets::client::SHEETS_API_URL;
    use crate::drive::test_support::EnvGuard;
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

    /// Builds a Sheets client pointed at wiremock, via the real derivation
    /// path — mirrors `src/drive/sheets/read.rs`'s and
    /// `src/cli/drive/sheets/info.rs`'s test helper of the same name.
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

    // ── parse_render_option ──────────────────────────────────────────

    #[test]
    fn parse_render_option_defaults_to_formatted() {
        assert_eq!(
            parse_render_option(None).unwrap(),
            ValueRenderOption::Formatted
        );
    }

    #[test]
    fn parse_render_option_accepts_known_strings() {
        assert_eq!(
            parse_render_option(Some("unformatted")).unwrap(),
            ValueRenderOption::Unformatted
        );
        assert_eq!(
            parse_render_option(Some("FORMULA")).unwrap(),
            ValueRenderOption::Formula
        );
    }

    #[test]
    fn parse_render_option_rejects_unknown_strings() {
        let err = parse_render_option(Some("bogus")).unwrap_err();
        assert!(err.to_string().contains("unknown render"));
    }

    // ── run_sheets_info ──────────────────────────────────────────────

    #[tokio::test]
    async fn run_sheets_info_requests_a_fields_mask_and_returns_yaml() {
        let server = wiremock::MockServer::start().await;
        let client = sheets_client(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/s1"))
            .and(wiremock::matchers::query_param_contains(
                "fields",
                "sheets.properties",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "spreadsheetId": "s1",
                    "properties": {"title": "Budget"},
                    "sheets": [{"properties": {"title": "Q1"}}],
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let params = DriveSheetsInfoParams {
            spreadsheet_id: "s1".to_string(),
            account: None,
        };
        let yaml = run_sheets_info(&client, &params).await.unwrap();
        assert!(yaml.contains("Budget"), "{yaml}");
        assert!(yaml.contains("Q1"), "{yaml}");
    }

    #[tokio::test]
    async fn run_sheets_info_propagates_a_not_found_error() {
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

        let params = DriveSheetsInfoParams {
            spreadsheet_id: "missing".to_string(),
            account: None,
        };
        let err = run_sheets_info(&client, &params).await.unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }

    // ── run_sheets_read ──────────────────────────────────────────────

    fn read_params(range: Option<&str>, sheet: Option<&str>) -> DriveSheetsReadParams {
        DriveSheetsReadParams {
            spreadsheet_id: "s1".to_string(),
            range: range.map(str::to_string),
            sheet: sheet.map(str::to_string),
            render: None,
            output_file: None,
            account: None,
        }
    }

    #[tokio::test]
    async fn run_sheets_read_reads_a_single_range() {
        let server = wiremock::MockServer::start().await;
        let client = sheets_client(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/s1/values/A1:B2"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "range": "A1:B2",
                    "values": [["a", "b"]],
                })),
            )
            .mount(&server)
            .await;

        let yaml = run_sheets_read(&client, &read_params(Some("A1:B2"), None))
            .await
            .unwrap();
        assert!(yaml.contains('a'), "{yaml}");
    }

    #[tokio::test]
    async fn run_sheets_read_reads_the_whole_workbook_when_range_and_sheet_are_omitted() {
        let server = wiremock::MockServer::start().await;
        let client = sheets_client(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/s1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "spreadsheetId": "s1",
                    "properties": {"title": "Book"},
                    "sheets": [{"properties": {"title": "Q1"}}],
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/s1/values:batchGet",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "valueRanges": [{"range": "'Q1'!A1", "values": [["x"]]}],
                })),
            )
            .mount(&server)
            .await;

        let yaml = run_sheets_read(&client, &read_params(None, None))
            .await
            .unwrap();
        assert!(yaml.contains("spreadsheet_title: Book"), "{yaml}");
        assert!(yaml.contains('x'), "{yaml}");
    }

    #[tokio::test]
    async fn run_sheets_read_rejects_an_unknown_render_string_before_any_request() {
        let server = wiremock::MockServer::start().await;
        let client = sheets_client(&server).await;
        // No mocks mounted at all: the bad `render` value must be rejected
        // client-side, before any request is issued.
        let mut params = read_params(Some("A1"), None);
        params.render = Some("bogus".to_string());
        let err = run_sheets_read(&client, &params).await.unwrap_err();
        assert!(err.to_string().contains("unknown render"), "{err}");
    }

    #[tokio::test]
    async fn run_sheets_read_writes_to_output_file_instead_of_returning_inline() {
        let server = wiremock::MockServer::start().await;
        let client = sheets_client(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/s1/values/A1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "range": "A1",
                    "values": [["only-in-file"]],
                })),
            )
            .mount(&server)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("out.yaml");
        let mut params = read_params(Some("A1"), None);
        params.output_file = Some(path.to_str().unwrap().to_string());

        let summary = run_sheets_read(&client, &params).await.unwrap();
        assert!(!summary.contains("only-in-file"), "{summary}");
        assert!(summary.contains("format: yaml"), "{summary}");
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("only-in-file"), "{written}");
    }

    // ── handler-level smoke tests ────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    async fn drive_sheets_info_handler_propagates_credentials_error() {
        let guard = EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = OmniDevServer::new();
        let err = server
            .drive_sheets_info(Parameters(DriveSheetsInfoParams {
                spreadsheet_id: "s1".to_string(),
                account: None,
            }))
            .await
            .unwrap_err();
        assert!(err.message.contains("not configured"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn drive_sheets_read_handler_propagates_credentials_error() {
        let guard = EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = OmniDevServer::new();
        let err = server
            .drive_sheets_read(Parameters(read_params(Some("A1"), None)))
            .await
            .unwrap_err();
        assert!(err.message.contains("not configured"));
    }
}
