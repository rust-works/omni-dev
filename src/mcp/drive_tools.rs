//! MCP tool handlers for Drive search/read operations.
//!
//! Each tool builds a fresh [`DriveClient`] via
//! [`crate::cli::drive::helpers::create_client_for`] and then delegates to
//! the same API façade (`FilesApi`) that the CLI uses under
//! `src/cli/drive/`. Tool outputs are YAML serialisations of the typed
//! response structs, matching the CLI's default rendering.
//!
//! `drive auth login` has no MCP equivalent — it's an interactive browser
//! flow with no non-interactive analogue.
//!
//! Every tool below takes an optional `account` parameter (mirrors Gmail's
//! issue #1500, [ADR-0066](../../docs/adrs/adr-0066.md), applied to Drive by
//! [ADR-0069](../../docs/adrs/adr-0069.md)): `Some(name)` forces that named
//! Drive account, `None` falls through to ambient
//! `--account`/`OMNI_DEV_DRIVE_ACCOUNT` resolution (relevant if the MCP
//! server process itself was launched with that env var pinned) — see
//! [`crate::drive::account::resolve_account`].
//!
//! `drive_search` has no `enrich`/`concurrency` params, unlike
//! `gmail_search`: `files.list` returns full metadata per hit in one call
//! (see `src/drive/files_api.rs`'s module doc), so there's no
//! ids-then-hydrate split to expose.
//!
//! `drive_sheets_info`/`drive_sheets_read` mirror the CLI's `drive sheets
//! info`/`read` (#1614) — read-only, so, like the rest of this file, they
//! consult no permission gate and write no request-log record (a known,
//! deliberate gap across the whole Drive read surface, not something
//! specific to Sheets; see [ADR-0071](../../docs/adrs/adr-0071.md) §11 and
//! [ADR-0073](../../docs/adrs/adr-0073.md) §5). The write verbs (`write`/
//! `append`/`clear`) have no MCP equivalent yet, per ADR-0073 §12.

use anyhow::{Context, Result};
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock as Content},
    schemars, tool, tool_router, ErrorData as McpError,
};
use serde::{Deserialize, Serialize};

use crate::cli::drive::dedupe::group_duplicates;
use crate::cli::drive::helpers::create_client_for;
use crate::cli::drive::read::{
    is_texty, resolve_export_mime_type, verify_sha256_checksum, GOOGLE_FOLDER, GOOGLE_SHORTCUT,
};
use crate::drive::account;
use crate::drive::auth;
use crate::drive::client::DriveClient;
use crate::drive::files_api::{FilesApi, DEFAULT_SEARCH_LIMIT};
use crate::drive::sheets::api::{SheetsApi, ValueRenderOption};
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::read::{read as sheets_read, ReadOptions};
use crate::utils::settings::Settings;

use super::error::tool_error;
use super::git_tools::build_truncated_result;
use super::output_file::WriteFileSummary;
use super::server::OmniDevServer;

// ── Parameter structs ───────────────────────────────────────────────

/// Doc comment shared by every `account` parameter below (mirrors Gmail's
/// issue #1500) — kept as one string so the copies can't drift.
macro_rules! account_param_doc {
    () => {
        "Selects a named Drive account instead of the ambient \
         `--account`/`OMNI_DEV_DRIVE_ACCOUNT` resolution — e.g. `work`. Omit to use the \
         resolved default account (or the legacy single-account credentials, if no named \
         accounts are configured). Call `drive_account_list` to discover configured names."
    };
}

/// Parameters for `drive_auth_status`.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct DriveAuthStatusParams {
    #[doc = account_param_doc!()]
    #[serde(default)]
    pub account: Option<String>,
}

/// Parameters for the `drive_search` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DriveSearchParams {
    /// Drive query, same syntax as `drive search`'s query argument (e.g.
    /// `name contains 'report' and mimeType = 'application/pdf'`). Required.
    pub query: String,
    /// Maximum results. Defaults to 50 when omitted; `0` explicitly means
    /// fetch every match up to the hard cap (10000).
    #[serde(default)]
    pub limit: Option<usize>,
    #[doc = account_param_doc!()]
    #[serde(default)]
    pub account: Option<String>,
}

/// Parameters for the `drive_dedupe` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DriveDedupeParams {
    /// Drive query, same syntax as `drive search`'s query argument (e.g.
    /// `'<folder-id>' in parents` to dedupe within one folder). Required.
    pub query: String,
    /// Maximum results to scan. Defaults to 50 when omitted; `0` explicitly
    /// means scan every match up to the hard cap (10000).
    #[serde(default)]
    pub limit: Option<usize>,
    #[doc = account_param_doc!()]
    #[serde(default)]
    pub account: Option<String>,
}

/// Parameters for the `drive_file_read` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DriveFileReadParams {
    /// Drive file id (from `drive_search`, or the `id` segment of a Drive
    /// URL). Required.
    pub file_id: String,
    /// `metadata` (default) returns only the file's metadata; `content`
    /// additionally fetches its actual content — exported for Google-native
    /// files (Docs/Sheets/Slides/...), downloaded as-is otherwise.
    #[serde(default)]
    pub format: Option<String>,
    /// Export MIME type for a Google-native file's content — only relevant
    /// with `format: "content"`, ignored otherwise. Defaults: Google Docs ->
    /// text/markdown, Sheets -> text/csv (first sheet only), Slides ->
    /// text/plain. Required for every other Google-native type (Forms,
    /// Drawings, Apps Script, Sites, ...); the error names the file's
    /// actually supported export MIME types.
    #[serde(default)]
    pub export_mime_type: Option<String>,
    /// Only valid with `format: "content"`. When set, writes the fetched
    /// content to this path and returns a short YAML summary instead of the
    /// inline body — required for binary content (this tool refuses to
    /// return it inline), and recommended for large files that would exceed
    /// the response size limit.
    #[serde(default)]
    pub output_file: Option<String>,
    /// Only valid with `format: "content"`. When true, locally recomputes
    /// the SHA-256 checksum of the fetched bytes and checks it against
    /// Drive's reported `sha256Checksum`. Only supported for non-Google-
    /// native files — Drive never returns a checksum for exported content,
    /// so this errors immediately on a Google-native file. Fails clearly on
    /// a mismatch or a missing checksum.
    #[serde(default)]
    pub verify: Option<bool>,
    #[doc = account_param_doc!()]
    #[serde(default)]
    pub account: Option<String>,
}

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
    /// A1 range to read, optionally carrying its own `Sheet!` prefix (e.g.
    /// `A1:C10`, `'My Sheet'!A:A`). Combined with `sheet` when bare. Omit
    /// both `range` and `sheet` to read every sheet in the workbook (capped
    /// at 200 sheets).
    #[serde(default)]
    pub range: Option<String>,
    /// Sheet (tab) title to read. Supplies the prefix for a bare `range`,
    /// or selects the whole tab on its own. Conflicts with a `range` that
    /// already names a sheet.
    #[serde(default)]
    pub sheet: Option<String>,
    /// How cell values are rendered: `formatted` (default) matches the
    /// spreadsheet as displayed; `unformatted` yields raw typed numbers
    /// rather than locale-formatted strings; `formula` yields formula text.
    #[serde(default)]
    pub render: Option<String>,
    #[doc = account_param_doc!()]
    #[serde(default)]
    pub account: Option<String>,
}

/// Parameters for `drive_account_list` (none, mirrors Gmail's issue #1500).
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct DriveAccountListParams {}

// ── Tool handlers ────────────────────────────────────────────────────

#[allow(missing_docs)] // #[tool_router] generates a pub `drive_tool_router` fn.
#[tool_router(router = drive_tool_router, vis = "pub")]
impl OmniDevServer {
    /// Reports whether Drive OAuth2 credentials are configured.
    ///
    /// Presence flags only — never calls the Drive API and never returns
    /// secret values.
    #[tool(
        description = "Report whether Drive OAuth2 credentials are configured \
                       (DRIVE_CLIENT_ID/DRIVE_CLIENT_SECRET/refresh token present) and which \
                       scope was granted at login. Returns presence flags and the granted \
                       scope only — NEVER the client secret, refresh token, or access token. \
                       Unlike the CLI `omni-dev drive auth status`, this tool does not call the \
                       Drive API and cannot confirm the refresh token is still accepted — use \
                       the CLI status command to actually verify. \
                       Read-only. Mirrors `omni-dev drive auth status`."
    )]
    pub async fn drive_auth_status(
        &self,
        Parameters(params): Parameters<DriveAuthStatusParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = run_auth_status(params.account.as_deref()).map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: search Drive files.
    #[tool(
        description = "Search Drive files with a Drive query (same syntax as `drive search`'s \
                       query argument, e.g. `name contains 'report' and mimeType = \
                       'application/pdf'`). Returns id/name/mimeType/size/md5Checksum/\
                       sha1Checksum/sha256Checksum/modifiedTime/parents/webViewLink/owners per \
                       hit — `files.list` returns full metadata in one call, so there's no \
                       separate hydration step. Checksum fields are present only for \
                       binary-content files (absent for folders and Google-native docs). \
                       Always searches shared drives too. `limit` defaults to 50 when omitted; \
                       pass `0` explicitly to auto-paginate up to a hard cap (10000). \
                       Read-only. Mirrors `omni-dev drive search`. Output is YAML."
    )]
    pub async fn drive_search(
        &self,
        Parameters(params): Parameters<DriveSearchParams>,
    ) -> Result<CallToolResult, McpError> {
        let client = create_client_for(params.account.as_deref()).map_err(tool_error)?;
        let yaml = run_search(&client, &params).await.map_err(tool_error)?;
        Ok(build_truncated_result(yaml))
    }

    /// Tool: find Drive files sharing the same content hash.
    #[tool(
        description = "Find Drive files sharing the same content hash, within the results of a \
                       Drive query (same syntax as `drive_search`'s `query`, e.g. `'<folder-id>' \
                       in parents` to dedupe within one folder). Reuses the same bulk-search \
                       path as `drive_search` — no per-file follow-up call. Groups by \
                       md5Checksum (the broadest-coverage checksum field); files with no \
                       checksum (folders, Google-native documents) are skipped, and groups of \
                       one are omitted. `limit` defaults to 50 when omitted; pass `0` explicitly \
                       to scan up to a hard cap (10000). \
                       Read-only. Mirrors `omni-dev drive dedupe`. Output is YAML."
    )]
    pub async fn drive_dedupe(
        &self,
        Parameters(params): Parameters<DriveDedupeParams>,
    ) -> Result<CallToolResult, McpError> {
        let client = create_client_for(params.account.as_deref()).map_err(tool_error)?;
        let yaml = run_dedupe(&client, &params).await.map_err(tool_error)?;
        Ok(build_truncated_result(yaml))
    }

    /// Tool: read a single Drive file's metadata or content.
    #[tool(
        description = "Read a single Drive file by id. `format: \"metadata\"` (default) \
                       returns only metadata (including md5Checksum/sha1Checksum/\
                       sha256Checksum when available); `format: \"content\"` additionally \
                       fetches the file's actual content — exported for Google-native files \
                       (Docs/Sheets/Slides/...; see `export_mime_type`), downloaded as-is \
                       otherwise. Folders and shortcuts are rejected with an actionable error \
                       in content mode. Text content is returned inline; binary content is \
                       refused inline and requires `output_file`. When `output_file` is set, \
                       writes the content to that path and returns a short YAML summary \
                       instead of the inline body. Set `verify: true` (only with `format: \
                       \"content\"`, only for non-Google-native files) to locally recompute the \
                       SHA-256 checksum of the fetched bytes and check it against Drive's \
                       reported sha256Checksum, failing clearly on a mismatch. \
                       Read-only. Mirrors `omni-dev drive read`. Output is YAML."
    )]
    pub async fn drive_file_read(
        &self,
        Parameters(params): Parameters<DriveFileReadParams>,
    ) -> Result<CallToolResult, McpError> {
        let client = create_client_for(params.account.as_deref()).map_err(tool_error)?;
        let wrote_to_file = params.output_file.is_some();
        let text = run_file_read(&client, &params).await.map_err(tool_error)?;
        if wrote_to_file {
            Ok(CallToolResult::success(vec![Content::text(text)]))
        } else {
            Ok(build_truncated_result(text))
        }
    }

    /// Tool: a spreadsheet's title and the sheets (tabs) it contains.
    #[tool(
        description = "Show a Sheets spreadsheet's title and the sheets (tabs) it contains — \
                       id, title, index, hidden flag, and grid dimensions per tab. Use this to \
                       discover sheet titles before calling `drive_sheets_read` with a `sheet` \
                       parameter. \
                       Read-only. Mirrors `omni-dev drive sheets info`. Output is YAML."
    )]
    pub async fn drive_sheets_info(
        &self,
        Parameters(params): Parameters<DriveSheetsInfoParams>,
    ) -> Result<CallToolResult, McpError> {
        let client = create_client_for(params.account.as_deref()).map_err(tool_error)?;
        let yaml = run_sheets_info(&client, &params)
            .await
            .map_err(tool_error)?;
        Ok(build_truncated_result(yaml))
    }

    /// Tool: read cell values from a Sheets spreadsheet.
    #[tool(
        description = "Read cell values from a Sheets spreadsheet. Set `range` (e.g. `A1:C10`, \
                       `'My Sheet'!A:A`) and/or `sheet` (a tab title) to read one range or one \
                       whole tab; omit both to read every sheet in the workbook (capped at 200 \
                       sheets — call `drive_sheets_info` first on a larger workbook). `render` \
                       controls how values come back: `formatted` (default) matches the \
                       spreadsheet as displayed, `unformatted` yields raw typed numbers, \
                       `formula` yields formula text. Rows are ragged, exactly as the API \
                       returns them (trailing empty cells are not padded). \
                       Read-only. Mirrors `omni-dev drive sheets read`. Output is YAML."
    )]
    pub async fn drive_sheets_read(
        &self,
        Parameters(params): Parameters<DriveSheetsReadParams>,
    ) -> Result<CallToolResult, McpError> {
        let client = create_client_for(params.account.as_deref()).map_err(tool_error)?;
        let yaml = run_sheets_read(&client, &params)
            .await
            .map_err(tool_error)?;
        Ok(build_truncated_result(yaml))
    }

    /// Tool: list configured named Drive accounts.
    #[tool(
        description = "List Drive accounts configured in ~/.omni-dev/settings.json — name, \
                       cached email address (if known), granted scope, and which one is the \
                       default. Call this first to discover valid `account` values before \
                       passing one to `drive_search`/`drive_file_read`/`drive_auth_status`. \
                       Never returns a secret. \
                       Read-only, no parameters. Mirrors `omni-dev drive account list`."
    )]
    pub async fn drive_account_list(
        &self,
        Parameters(_params): Parameters<DriveAccountListParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = run_account_list().map_err(tool_error)?;
        Ok(build_truncated_result(yaml))
    }
}

// ── Internal run_* implementations ──────────────────────────────────
//
// Split out from the tool handlers, taking an already-resolved
// `&DriveClient`, so they can be tested against a wiremock-backed client
// without needing real credentials (mirrors `run_search`/`run_message_read`
// in `gmail_tools.rs`).

/// Renders the credential-presence summary as YAML.
///
/// Pure: never touches the network and never reads any secret values.
/// `account`, when `Some`, forces that named account instead of falling
/// through to ambient `--account`/`OMNI_DEV_DRIVE_ACCOUNT` resolution.
fn run_auth_status(account: Option<&str>) -> Result<String> {
    let status = auth::status_for(account)?;
    serde_yaml::to_string(&status).context("Failed to serialize Drive auth status")
}

/// Renders the configured Drive accounts (name/email/scope/default) as
/// YAML — never a secret, never a network call.
fn run_account_list() -> Result<String> {
    let settings = Settings::load().unwrap_or_default();
    let accounts = account::list_accounts(&settings.drive);
    yaml_result(&accounts)
}

async fn run_search(client: &DriveClient, params: &DriveSearchParams) -> Result<String> {
    let limit = params.limit.unwrap_or(DEFAULT_SEARCH_LIMIT);
    let list = FilesApi::new(client)
        .search_all(Some(&params.query), limit)
        .await?;
    yaml_result(&list.files)
}

async fn run_dedupe(client: &DriveClient, params: &DriveDedupeParams) -> Result<String> {
    let limit = params.limit.unwrap_or(DEFAULT_SEARCH_LIMIT);
    let list = FilesApi::new(client)
        .search_all(Some(&params.query), limit)
        .await?;
    yaml_result(&group_duplicates(&list.files))
}

async fn run_file_read(client: &DriveClient, params: &DriveFileReadParams) -> Result<String> {
    let format = parse_read_format(params.format.as_deref())?;
    let api = FilesApi::new(client);
    match format {
        ReadFormat::Metadata => {
            anyhow::ensure!(
                params.output_file.is_none(),
                "output_file requires format: \"content\"; metadata is always returned inline"
            );
            anyhow::ensure!(
                !params.verify.unwrap_or(false),
                "verify requires format: \"content\"; metadata is always returned inline"
            );
            let meta = api.get_metadata(&params.file_id).await?;
            yaml_result(&meta)
        }
        ReadFormat::Content => run_file_read_content(&api, params).await,
    }
}

/// Content path: mirrors `src/cli/drive/read.rs::run_read_content` —
/// fetches metadata first, rejects folders/shortcuts, then exports
/// (Google-native) or downloads (everything else).
async fn run_file_read_content(api: &FilesApi<'_>, params: &DriveFileReadParams) -> Result<String> {
    let meta = api.get_metadata(&params.file_id).await?;

    if meta.mime_type == GOOGLE_FOLDER {
        anyhow::bail!(
            "'{}' is a folder; folders have no content to read — use drive_search to list what \
             it contains",
            meta.name
        );
    }
    if meta.mime_type == GOOGLE_SHORTCUT {
        anyhow::bail!(
            "'{}' is a shortcut; drive_file_read doesn't follow shortcuts to their target file \
             — resolve the target file's id and read that instead",
            meta.name
        );
    }
    let verify = params.verify.unwrap_or(false);
    if verify && meta.is_google_native() {
        anyhow::bail!(
            "verify is not supported for Google-native files: Drive does not return a checksum \
             for exported content"
        );
    }

    let (bytes, content_mime_type) = if meta.is_google_native() {
        let export_mime = resolve_export_mime_type(&meta, params.export_mime_type.as_deref())?;
        let bytes = api.export(&params.file_id, &export_mime).await?;
        (bytes, export_mime)
    } else {
        let bytes = api.download(&params.file_id).await?;
        (bytes, meta.mime_type.clone())
    };

    if verify {
        verify_sha256_checksum(&bytes, meta.sha256_checksum.as_deref())?;
    }

    match params.output_file.as_deref() {
        Some(path) => write_bytes_to_file_yaml(path, &bytes, &content_mime_type),
        None => inline_content(&bytes, &content_mime_type),
    }
}

/// Writes raw (possibly binary) bytes to `path` and returns a YAML-encoded
/// [`WriteFileSummary`]. Can't reuse
/// [`super::output_file::write_to_file_yaml`], which takes a `&str` — mirrors
/// `confluence_attachment_download`'s `download_attachment_yaml`
/// (`src/mcp/confluence_tools.rs`), the existing precedent for writing
/// arbitrary binary MCP tool output to disk.
fn write_bytes_to_file_yaml(path: &str, bytes: &[u8], format: &str) -> Result<String> {
    std::fs::write(path, bytes).with_context(|| format!("Failed to write to {path}"))?;
    let summary = WriteFileSummary {
        path: path.to_string(),
        bytes: bytes.len(),
        format: format.to_string(),
    };
    serde_yaml::to_string(&summary).context("Failed to serialize write summary as YAML")
}

/// Returns texty content inline as UTF-8; refuses binary or non-UTF-8 texty
/// content, telling the caller to set `output_file` instead — mirrors
/// `src/cli/drive/read.rs::run_read_content`'s terminal-safety refusal.
fn inline_content(bytes: &[u8], content_mime_type: &str) -> Result<String> {
    if is_texty(content_mime_type) {
        return String::from_utf8(bytes.to_vec()).with_context(|| {
            format!(
                "content (mimeType: {content_mime_type}) claims to be text but is not valid \
                 UTF-8; set output_file to save it"
            )
        });
    }
    Err(anyhow::anyhow!(
        "refusing to return binary content (mimeType: {content_mime_type}) inline; set \
         output_file to save it to disk"
    ))
}

/// Which representation `drive_file_read` should fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadFormat {
    Metadata,
    Content,
}

/// Parses an MCP-supplied read-format string. `None` defaults to `metadata`.
fn parse_read_format(raw: Option<&str>) -> Result<ReadFormat> {
    match raw.map(str::to_ascii_lowercase).as_deref() {
        None | Some("metadata") => Ok(ReadFormat::Metadata),
        Some("content") => Ok(ReadFormat::Content),
        Some(other) => {
            anyhow::bail!("unknown format {other:?} (expected 'metadata' or 'content')")
        }
    }
}

async fn run_sheets_info(client: &DriveClient, params: &DriveSheetsInfoParams) -> Result<String> {
    let sheets = SheetsClient::from_drive_client(client)?;
    let spreadsheet = SheetsApi::new(&sheets)
        .get_spreadsheet(&params.spreadsheet_id)
        .await?;
    yaml_result(&spreadsheet)
}

async fn run_sheets_read(client: &DriveClient, params: &DriveSheetsReadParams) -> Result<String> {
    let render = parse_render_option(params.render.as_deref())?;
    let sheets = SheetsClient::from_drive_client(client)?;
    let opts = ReadOptions {
        spreadsheet_id: params.spreadsheet_id.clone(),
        range: params.range.clone(),
        sheet: params.sheet.clone(),
        render,
    };
    let outcome = sheets_read(&SheetsApi::new(&sheets), &opts).await?;
    yaml_result(&outcome)
}

/// Parses an MCP-supplied render-option string. `None` defaults to
/// `formatted`, mirroring `RenderArg::default()` in
/// `src/cli/drive/sheets/read.rs`.
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
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes, SCOPE_READONLY};
    use crate::drive::sheets::client::SHEETS_API_URL;
    use crate::drive::test_support::EnvGuard;
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

    /// Pulls the text body out of a successful `CallToolResult`, panicking
    /// with a clear message if the result was an error or non-text payload.
    fn handler_text(result: &CallToolResult) -> String {
        assert!(!result.is_error.unwrap_or(false), "tool returned error");
        result.content[0]
            .as_text()
            .expect("expected text content")
            .text
            .clone()
    }

    // ── parse_read_format ───────────────────────────────────────────

    #[test]
    fn parse_read_format_defaults_to_metadata() {
        assert_eq!(parse_read_format(None).unwrap(), ReadFormat::Metadata);
    }

    #[test]
    fn parse_read_format_accepts_known_strings() {
        assert_eq!(
            parse_read_format(Some("metadata")).unwrap(),
            ReadFormat::Metadata
        );
        assert_eq!(
            parse_read_format(Some("content")).unwrap(),
            ReadFormat::Content
        );
    }

    #[test]
    fn parse_read_format_is_case_insensitive() {
        assert_eq!(
            parse_read_format(Some("CONTENT")).unwrap(),
            ReadFormat::Content
        );
    }

    #[test]
    fn parse_read_format_rejects_unknown_value() {
        let err = parse_read_format(Some("bogus")).unwrap_err();
        assert!(err.to_string().contains("format"));
    }

    // ── run_auth_status ────────────────────────────────────────────────

    #[test]
    fn run_auth_status_reports_unconfigured_state() {
        let guard = EnvGuard::take();
        let _dir = guard.clear_credentials();

        let yaml = run_auth_status(None).unwrap();
        assert!(yaml.contains("has_client_id: false"));
        assert!(yaml.contains("has_refresh_token: false"));
    }

    #[test]
    fn run_auth_status_never_emits_secret_values() {
        let guard = EnvGuard::take();
        let dir = guard.clear_credentials();
        let omni_dir = dir.path().join(".omni-dev");
        std::fs::create_dir_all(&omni_dir).unwrap();
        std::fs::write(
            omni_dir.join("settings.json"),
            r#"{"env":{
                "DRIVE_CLIENT_ID":"client-visible",
                "DRIVE_CLIENT_SECRET":"sekret-do-not-leak",
                "DRIVE_REFRESH_TOKEN":"sekret-refresh-do-not-leak",
                "DRIVE_SCOPE":"https://www.googleapis.com/auth/drive.readonly"
            }}"#,
        )
        .unwrap();
        std::env::remove_var(auth::DRIVE_CLIENT_ID);
        std::env::remove_var(auth::DRIVE_CLIENT_SECRET);
        std::env::remove_var(auth::DRIVE_REFRESH_TOKEN);
        std::env::remove_var(auth::DRIVE_SCOPE);

        let yaml = run_auth_status(None).unwrap();
        assert!(yaml.contains("has_client_id: true"));
        assert!(!yaml.contains("sekret-do-not-leak"));
        assert!(!yaml.contains("sekret-refresh-do-not-leak"));
    }

    // ── run_search ───────────────────────────────────────────────────

    #[tokio::test]
    async fn run_search_returns_hits_as_yaml() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "files": [{"id": "f1", "name": "report.pdf", "mimeType": "application/pdf"}],
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let yaml = run_search(
            &client,
            &DriveSearchParams {
                query: "name contains 'report'".to_string(),
                limit: Some(10),
                account: None,
            },
        )
        .await
        .unwrap();
        assert!(yaml.contains("id: f1"));
        assert!(yaml.contains("mimeType: application/pdf"));
    }

    #[tokio::test]
    async fn run_search_omitted_limit_defaults_to_50_not_hard_cap() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files"))
            .and(wiremock::matchers::query_param("pageSize", "50"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "files": [{"id": "f1", "name": "a"}],
                })),
            )
            .expect(1)
            .mount(&server)
            .await;
        // No mock matching any other `pageSize` value — if an omitted
        // `limit` fell back to 0 (fetch-to-HARD_CAP) instead of 50, this
        // request wouldn't match and wiremock would 404.

        let yaml = run_search(
            &client,
            &DriveSearchParams {
                query: "name contains 'a'".to_string(),
                limit: None,
                account: None,
            },
        )
        .await
        .unwrap();
        assert!(yaml.contains("id: f1"));
    }

    #[tokio::test]
    async fn run_search_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("forbidden"))
            .mount(&server)
            .await;

        let err = run_search(
            &client,
            &DriveSearchParams {
                query: "*".to_string(),
                limit: None,
                account: None,
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    // ── run_dedupe ──────────────────────────────────────────────────

    #[tokio::test]
    async fn run_dedupe_groups_files_sharing_a_checksum() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "files": [
                        {"id": "f1", "name": "a", "md5Checksum": "hash-a"},
                        {"id": "f2", "name": "b", "md5Checksum": "hash-a"},
                        {"id": "f3", "name": "c", "md5Checksum": "hash-b"},
                    ],
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let yaml = run_dedupe(
            &client,
            &DriveDedupeParams {
                query: "name contains 'x'".to_string(),
                limit: Some(10),
                account: None,
            },
        )
        .await
        .unwrap();
        assert!(yaml.contains("checksum: hash-a"));
        assert!(yaml.contains("id: f1"));
        assert!(yaml.contains("id: f2"));
        assert!(!yaml.contains("hash-b"));
    }

    #[tokio::test]
    async fn run_dedupe_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let err = run_dedupe(
            &client,
            &DriveDedupeParams {
                query: "*".to_string(),
                limit: None,
                account: None,
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    // ── run_file_read (metadata) ────────────────────────────────────

    fn read_params(
        file_id: &str,
        format: Option<&str>,
        export_mime_type: Option<&str>,
        output_file: Option<&str>,
    ) -> DriveFileReadParams {
        DriveFileReadParams {
            file_id: file_id.to_string(),
            format: format.map(str::to_string),
            export_mime_type: export_mime_type.map(str::to_string),
            output_file: output_file.map(str::to_string),
            verify: None,
            account: None,
        }
    }

    #[tokio::test]
    async fn run_file_read_metadata_returns_yaml_object() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "n",
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let yaml = run_file_read(&client, &read_params("f1", None, None, None))
            .await
            .unwrap();
        assert!(yaml.contains("id: f1"));
    }

    #[tokio::test]
    async fn run_file_read_metadata_rejects_output_file() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;

        let err = run_file_read(
            &client,
            &read_params("f1", None, None, Some("/tmp/out.txt")),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("requires format: \"content\""));
    }

    #[tokio::test]
    async fn run_file_read_rejects_invalid_format() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;

        let err = run_file_read(&client, &read_params("f1", Some("bogus"), None, None))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("format"));
    }

    // ── run_file_read (content) ─────────────────────────────────────

    #[tokio::test]
    async fn run_file_read_content_non_google_native_downloads_via_alt_media() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "n", "mimeType": "text/plain",
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param("alt", "media"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()))
            .expect(1)
            .mount(&server)
            .await;

        let text = run_file_read(&client, &read_params("f1", Some("content"), None, None))
            .await
            .unwrap();
        assert_eq!(text, "hello");
    }

    #[tokio::test]
    async fn run_file_read_content_rejects_folder_with_actionable_error() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "My Folder", "mimeType": GOOGLE_FOLDER,
                })),
            )
            .mount(&server)
            .await;

        let err = run_file_read(&client, &read_params("f1", Some("content"), None, None))
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("'My Folder' is a folder"), "{message}");
        assert!(message.contains("drive_search"), "{message}");
    }

    #[tokio::test]
    async fn run_file_read_content_rejects_shortcut_with_actionable_error() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "My Shortcut", "mimeType": GOOGLE_SHORTCUT,
                })),
            )
            .mount(&server)
            .await;

        let err = run_file_read(&client, &read_params("f1", Some("content"), None, None))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("'My Shortcut' is a shortcut"));
    }

    #[tokio::test]
    async fn run_file_read_content_google_doc_default_exports_text_markdown() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "n", "mimeType": "application/vnd.google-apps.document",
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1/export"))
            .and(wiremock::matchers::query_param("mimeType", "text/markdown"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"# Title".to_vec()))
            .expect(1)
            .mount(&server)
            .await;

        let text = run_file_read(&client, &read_params("f1", Some("content"), None, None))
            .await
            .unwrap();
        assert_eq!(text, "# Title");
    }

    #[tokio::test]
    async fn run_file_read_content_explicit_export_mime_type_overrides_default() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "n", "mimeType": "application/vnd.google-apps.document",
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1/export"))
            .and(wiremock::matchers::query_param(
                "mimeType",
                "application/pdf",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"%PDF".to_vec()))
            .expect(1)
            .mount(&server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("doc.pdf");
        let summary = run_file_read(
            &client,
            &read_params(
                "f1",
                Some("content"),
                Some("application/pdf"),
                Some(path.to_str().unwrap()),
            ),
        )
        .await
        .unwrap();
        assert!(summary.contains("bytes:"));
        assert_eq!(std::fs::read(&path).unwrap(), b"%PDF");
    }

    #[tokio::test]
    async fn run_file_read_content_writes_binary_to_output_file() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "n", "mimeType": "application/pdf",
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param("alt", "media"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"%PDF-1.4".to_vec()))
            .mount(&server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("out.pdf");
        let summary = run_file_read(
            &client,
            &read_params("f1", Some("content"), None, Some(path.to_str().unwrap())),
        )
        .await
        .unwrap();
        assert!(summary.contains("bytes: 8"));
        assert!(summary.contains("format: application/pdf"));
        assert_eq!(std::fs::read(&path).unwrap(), b"%PDF-1.4");
    }

    #[tokio::test]
    async fn run_file_read_content_refuses_binary_content_without_output_file() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "n", "mimeType": "application/pdf",
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param("alt", "media"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"%PDF".to_vec()))
            .mount(&server)
            .await;

        let err = run_file_read(&client, &read_params("f1", Some("content"), None, None))
            .await
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("refusing to return binary content"));
    }

    #[tokio::test]
    async fn run_file_read_content_other_google_native_without_export_mime_type_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1",
                    "name": "myform",
                    "mimeType": "application/vnd.google-apps.form",
                    "exportLinks": {"application/zip": "url"},
                })),
            )
            .mount(&server)
            .await;

        let err = run_file_read(&client, &read_params("f1", Some("content"), None, None))
            .await
            .unwrap_err();
        // Names both surfaces' syntax — an MCP caller can't type a CLI flag.
        assert!(err.to_string().contains("--export-mime-type"));
        assert!(err.to_string().contains("export_mime_type"));
        assert!(err.to_string().contains("application/zip"));
    }

    #[tokio::test]
    async fn run_file_read_content_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/missing"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;

        let err = run_file_read(
            &client,
            &read_params("missing", Some("content"), None, None),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    // ── run_file_read (content, verify) ─────────────────────────────

    #[tokio::test]
    async fn run_file_read_content_verify_succeeds_on_matching_checksum() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "n", "mimeType": "text/plain",
                    "sha256Checksum": "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param("alt", "media"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()))
            .mount(&server)
            .await;

        let mut params = read_params("f1", Some("content"), None, None);
        params.verify = Some(true);
        run_file_read(&client, &params).await.unwrap();
    }

    #[tokio::test]
    async fn run_file_read_content_verify_fails_clearly_on_mismatch() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "n", "mimeType": "text/plain",
                    "sha256Checksum": "not-the-real-hash",
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param("alt", "media"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()))
            .mount(&server)
            .await;

        let mut params = read_params("f1", Some("content"), None, None);
        params.verify = Some(true);
        let err = run_file_read(&client, &params).await.unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"), "{err}");
    }

    #[tokio::test]
    async fn run_file_read_content_verify_rejects_google_native_file_before_exporting() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "n", "mimeType": "application/vnd.google-apps.document",
                })),
            )
            .mount(&server)
            .await;
        // No mock for /drive/v3/files/f1/export — if the Google-native
        // rejection didn't run before the export call, this test would 404
        // rather than surfacing the expected error.

        let mut params = read_params("f1", Some("content"), None, None);
        params.verify = Some(true);
        let err = run_file_read(&client, &params).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("verify is not supported for Google-native files"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn run_file_read_metadata_rejects_verify() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;

        let mut params = read_params("f1", None, None, None);
        params.verify = Some(true);
        let err = run_file_read(&client, &params).await.unwrap_err();
        assert!(err.to_string().contains("verify requires format"), "{err}");
    }

    // ── parse_render_option ─────────────────────────────────────────

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
            parse_render_option(Some("formatted")).unwrap(),
            ValueRenderOption::Formatted
        );
        assert_eq!(
            parse_render_option(Some("unformatted")).unwrap(),
            ValueRenderOption::Unformatted
        );
        assert_eq!(
            parse_render_option(Some("formula")).unwrap(),
            ValueRenderOption::Formula
        );
    }

    #[test]
    fn parse_render_option_is_case_insensitive() {
        assert_eq!(
            parse_render_option(Some("FORMULA")).unwrap(),
            ValueRenderOption::Formula
        );
    }

    #[test]
    fn parse_render_option_rejects_unknown_value() {
        let err = parse_render_option(Some("bogus")).unwrap_err();
        assert!(err.to_string().contains("render"), "{err}");
    }

    // ── run_sheets_info / run_sheets_read ───────────────────────────

    /// Points `SHEETS_API_URL` at `server` for the guard's lifetime, so
    /// `SheetsClient::from_drive_client`'s real `SystemEnv` lookup (inside
    /// `run_sheets_info`/`run_sheets_read`) resolves to the mock server
    /// instead of the real `sheets.googleapis.com`. Mirrors
    /// `EnvGuard::redirect_api_hosts_to_a_dead_port`'s pattern of setting
    /// the var directly once the guard already holds the process-wide lock.
    fn point_sheets_api_at(_guard: &EnvGuard, server: &wiremock::MockServer) {
        std::env::set_var(SHEETS_API_URL, server.uri());
    }

    #[tokio::test]
    async fn run_sheets_info_returns_spreadsheet_as_yaml() {
        let guard = EnvGuard::take();
        let server = wiremock::MockServer::start().await;
        point_sheets_api_at(&guard, &server);
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/s1"))
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

        let yaml = run_sheets_info(
            &client,
            &DriveSheetsInfoParams {
                spreadsheet_id: "s1".to_string(),
                account: None,
            },
        )
        .await
        .unwrap();
        assert!(yaml.contains("spreadsheetId: s1"), "{yaml}");
        assert!(yaml.contains("title: Q1"), "{yaml}");
    }

    #[tokio::test]
    async fn run_sheets_info_propagates_a_not_found_error() {
        let guard = EnvGuard::take();
        let server = wiremock::MockServer::start().await;
        point_sheets_api_at(&guard, &server);
        let client = client_with_bootstrapped_token(&server).await;
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

        let err = run_sheets_info(
            &client,
            &DriveSheetsInfoParams {
                spreadsheet_id: "missing".to_string(),
                account: None,
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }

    #[tokio::test]
    async fn run_sheets_read_single_range_returns_values_as_yaml() {
        let guard = EnvGuard::take();
        let server = wiremock::MockServer::start().await;
        point_sheets_api_at(&guard, &server);
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/s1/values/A1:B2"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "range": "Sheet1!A1:B2",
                    "values": [["a", "b"], ["1", "2"]],
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let yaml = run_sheets_read(
            &client,
            &DriveSheetsReadParams {
                spreadsheet_id: "s1".to_string(),
                range: Some("A1:B2".to_string()),
                sheet: None,
                render: None,
                account: None,
            },
        )
        .await
        .unwrap();
        assert!(yaml.contains("- - a"), "{yaml}");
        assert!(yaml.contains("- '1'"), "{yaml}");
    }

    #[tokio::test]
    async fn run_sheets_read_whole_workbook_fetches_every_sheet() {
        let guard = EnvGuard::take();
        let server = wiremock::MockServer::start().await;
        point_sheets_api_at(&guard, &server);
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/s1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "spreadsheetId": "s1",
                    "properties": {"title": "Book"},
                    "sheets": [
                        {"properties": {"sheetId": 0, "title": "Q1", "index": 0}},
                    ],
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
                    "spreadsheetId": "s1",
                    "valueRanges": [
                        {"range": "Q1!A1:Z1000", "values": [["x"]]},
                    ],
                })),
            )
            .mount(&server)
            .await;

        let yaml = run_sheets_read(
            &client,
            &DriveSheetsReadParams {
                spreadsheet_id: "s1".to_string(),
                range: None,
                sheet: None,
                render: None,
                account: None,
            },
        )
        .await
        .unwrap();
        assert!(yaml.contains("title: Q1"), "{yaml}");
        assert!(yaml.contains("- x"), "{yaml}");
    }

    #[tokio::test]
    async fn run_sheets_read_rejects_unknown_render_value() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;

        let err = run_sheets_read(
            &client,
            &DriveSheetsReadParams {
                spreadsheet_id: "s1".to_string(),
                range: Some("A1".to_string()),
                sheet: None,
                render: Some("bogus".to_string()),
                account: None,
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("render"), "{err}");
    }

    // ── Tool handler bodies (smoke + auth-status full path) ───────────

    #[tokio::test(flavor = "current_thread")]
    async fn drive_auth_status_handler_returns_yaml_no_secrets() {
        let guard = EnvGuard::take();
        let dir = guard.clear_credentials();
        let omni_dir = dir.path().join(".omni-dev");
        std::fs::create_dir_all(&omni_dir).unwrap();
        std::fs::write(
            omni_dir.join("settings.json"),
            r#"{"env":{
                "DRIVE_CLIENT_ID":"client-1",
                "DRIVE_CLIENT_SECRET":"sekret-secret",
                "DRIVE_REFRESH_TOKEN":"sekret-refresh"
            }}"#,
        )
        .unwrap();
        std::env::remove_var(auth::DRIVE_CLIENT_ID);
        std::env::remove_var(auth::DRIVE_CLIENT_SECRET);
        std::env::remove_var(auth::DRIVE_REFRESH_TOKEN);

        let server = OmniDevServer::new();
        let result = server
            .drive_auth_status(Parameters(DriveAuthStatusParams::default()))
            .await
            .unwrap();
        let body = handler_text(&result);
        assert!(body.contains("has_client_id: true"));
        assert!(!body.contains("sekret-secret"));
        assert!(!body.contains("sekret-refresh"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn drive_search_handler_propagates_credentials_error() {
        let guard = EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = OmniDevServer::new();
        let err = server
            .drive_search(Parameters(DriveSearchParams {
                query: "*".to_string(),
                limit: None,
                account: None,
            }))
            .await
            .unwrap_err();
        assert!(err.message.contains("not configured"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn drive_search_handler_honors_named_account_param() {
        // A named `account` reaches `create_client_for` — an unknown name
        // surfaces as the account-resolution error, not the generic "not
        // configured" one, proving the param actually propagates.
        let guard = EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        Settings::upsert_drive_account(
            &settings_path,
            "work",
            &[("client_id", serde_json::Value::String("id".to_string()))],
        )
        .unwrap();

        let server = OmniDevServer::new();
        let err = server
            .drive_search(Parameters(DriveSearchParams {
                query: "*".to_string(),
                limit: None,
                account: Some("bogus".to_string()),
            }))
            .await
            .unwrap_err();
        assert!(err.message.contains("unknown Drive account 'bogus'"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn drive_dedupe_handler_propagates_credentials_error() {
        let guard = EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = OmniDevServer::new();
        let err = server
            .drive_dedupe(Parameters(DriveDedupeParams {
                query: "*".to_string(),
                limit: None,
                account: None,
            }))
            .await
            .unwrap_err();
        assert!(err.message.contains("not configured"));
    }

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
            .drive_sheets_read(Parameters(DriveSheetsReadParams {
                spreadsheet_id: "s1".to_string(),
                range: None,
                sheet: None,
                render: None,
                account: None,
            }))
            .await
            .unwrap_err();
        assert!(err.message.contains("not configured"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn drive_sheets_read_handler_honors_named_account_param() {
        // A named `account` reaches `create_client_for` — an unknown name
        // surfaces as the account-resolution error, not the generic "not
        // configured" one, proving the param actually propagates.
        let guard = EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        Settings::upsert_drive_account(
            &settings_path,
            "work",
            &[("client_id", serde_json::Value::String("id".to_string()))],
        )
        .unwrap();

        let server = OmniDevServer::new();
        let err = server
            .drive_sheets_read(Parameters(DriveSheetsReadParams {
                spreadsheet_id: "s1".to_string(),
                range: None,
                sheet: None,
                render: None,
                account: Some("bogus".to_string()),
            }))
            .await
            .unwrap_err();
        assert!(err.message.contains("unknown Drive account 'bogus'"));
    }

    // ── run_account_list / drive_account_list ──────────────────────────

    #[test]
    fn run_account_list_renders_configured_accounts() {
        let guard = EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        Settings::upsert_drive_account(
            &settings_path,
            "work",
            &[
                ("client_id", serde_json::Value::String("id".to_string())),
                (
                    "email_address",
                    serde_json::Value::String("me@work.com".to_string()),
                ),
                (
                    "scope",
                    serde_json::Value::String(SCOPE_READONLY.to_string()),
                ),
            ],
        )
        .unwrap();
        Settings::set_drive_default_account(&settings_path, Some("work")).unwrap();

        let yaml = run_account_list().unwrap();
        assert!(yaml.contains("work"));
        assert!(yaml.contains("me@work.com"));
        assert!(yaml.contains("is_default: true"));
    }

    #[test]
    fn run_account_list_empty_when_none_configured() {
        let guard = EnvGuard::take();
        let _dir = guard.clear_credentials();

        let yaml = run_account_list().unwrap();
        assert_eq!(yaml.trim(), "[]");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn drive_account_list_handler_returns_yaml_no_client_needed() {
        let guard = EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        Settings::upsert_drive_account(
            &settings_path,
            "work",
            &[("client_id", serde_json::Value::String("id".to_string()))],
        )
        .unwrap();

        let server = OmniDevServer::new();
        // Succeeds with no legacy credentials configured at all — proving
        // this tool never resolves a client.
        let result = server
            .drive_account_list(Parameters(DriveAccountListParams::default()))
            .await
            .unwrap();
        let body = handler_text(&result);
        assert!(body.contains("work"));
    }
}
