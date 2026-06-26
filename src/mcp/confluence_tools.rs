//! MCP tool handlers for Confluence operations.
//!
//! Each handler builds an [`AtlassianClient`] via [`create_client`] and then
//! delegates to the same API methods that the CLI uses under
//! `src/cli/atlassian/confluence/`, so the MCP surface and the CLI share a
//! single implementation.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    schemars, tool, tool_router, ErrorData as McpError,
};
use serde::{Deserialize, Serialize};

use crate::atlassian::adf::AdfDocument;
use crate::atlassian::adf_validated::ValidatedAdfDocument;
use crate::atlassian::api::{AtlassianApi, ContentItem};
use crate::atlassian::client::AtlassianClient;
use crate::atlassian::confluence_api::{
    ChildPage, CommentKind, ConfluenceApi, ConfluenceAttachmentPage, ConfluenceSpacePage,
    MovePosition, PageSummaryPage,
};
use crate::atlassian::convert::markdown_to_adf;
use crate::atlassian::create::{prepend_warnings, resolve_confluence_create};
use crate::atlassian::document::{content_item_to_document, JfmDocument, JfmFrontmatter};
use crate::cli::atlassian::confluence::download::{
    run_download, DownloadParams, ManifestEntry, OnConflict,
};
use crate::cli::atlassian::format::ContentFormat;
use crate::cli::atlassian::helpers::create_client;
use crate::data::yaml::to_yaml;

use super::error::tool_error;
use super::output_file::write_to_file_yaml;
use super::server::OmniDevServer;

// ── Parameter structs ───────────────────────────────────────────────

/// Parameters for the `confluence_read` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfluenceReadParams {
    /// Confluence page ID (e.g., "12345678").
    pub id: String,
    /// Output format: `"jfm"` (default, AI-friendly markdown) or `"adf"`
    /// (raw ADF JSON).
    #[serde(default)]
    pub format: Option<String>,
    /// When set, writes the rendered content to this path and returns a
    /// short YAML summary (path/bytes/format) instead of the inline body.
    /// Useful for large pages that would otherwise blow past the context
    /// window — the assistant can then read the file with offset/limit.
    #[serde(default)]
    pub output_file: Option<String>,
}

/// Parameters for the `confluence_search` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfluenceSearchParams {
    /// Confluence CQL query (e.g., `space = ENG AND title ~ "architecture"`).
    pub cql: String,
    /// Maximum number of results. Defaults to 20.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Parameters for the `confluence_create` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfluenceCreateParams {
    /// Full JFM document (YAML frontmatter + markdown body), e.g. the output
    /// of `confluence_read` with the frontmatter edited. When provided,
    /// `space_key`, `title` and `parent_id` are taken from the frontmatter and
    /// the body becomes the page body — so the read → edit → create round-trip
    /// works. The `space_key`/`title`/`parent_id` parameters below still
    /// override their frontmatter counterparts (a warning is returned when they
    /// do); passing `content` together with `document` is an error. See resource
    /// `omni-dev://specs/jfm`.
    #[serde(default)]
    pub document: Option<String>,
    /// Target Confluence space key (e.g., `"ENG"`). Required unless `document`
    /// carries a `space_key:`. Overrides frontmatter.
    #[serde(default)]
    pub space_key: Option<String>,
    /// Page title. Required unless `document` carries one. Overrides frontmatter.
    #[serde(default)]
    pub title: Option<String>,
    /// Page body. Parsed according to `format`. Required unless `document` is
    /// provided (and rejected when it is — the document body is the page body).
    ///
    /// For `format = "jfm"` (the default), this is GitHub-style markdown,
    /// NOT Confluence wiki markup. Use `##` not `h2.`, triple-backtick fences
    /// not `{code}`, backtick inline code not `{{...}}`. Full reference:
    /// MCP resource `omni-dev://specs/jfm`.
    #[serde(default)]
    pub content: Option<String>,
    /// Optional parent page ID for nesting under an existing page. Overrides
    /// frontmatter `parent_id:`.
    #[serde(default)]
    pub parent_id: Option<String>,
    /// Format of `content`: `"jfm"` (default markdown) or `"adf"` (raw ADF JSON).
    /// Ignored for the `document` path (a document is always JFM).
    #[serde(default)]
    pub format: Option<String>,
}

/// Parameters for the `confluence_write` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfluenceWriteParams {
    /// Confluence page ID.
    pub id: String,
    /// New page body.
    ///
    /// For `format = "jfm"` (the default), this is GitHub-style markdown,
    /// NOT Confluence wiki markup. Use `##` not `h2.`, triple-backtick fences
    /// not `{code}`, backtick inline code not `{{...}}`. Full reference:
    /// MCP resource `omni-dev://specs/jfm`.
    pub content: String,
    /// Format of `content`: `"jfm"` (default markdown) or `"adf"` (raw ADF JSON).
    #[serde(default)]
    pub format: Option<String>,
}

/// Parameters for the `confluence_delete` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfluenceDeleteParams {
    /// Confluence page ID.
    pub id: String,
    /// Must be `true` to confirm this destructive, irreversible operation.
    pub confirm: bool,
    /// Permanently purges the page instead of moving to trash.
    /// Requires space admin permission.
    #[serde(default)]
    pub purge: Option<bool>,
}

/// Parameters for the `confluence_move` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfluenceMoveParams {
    /// ID of the Confluence page to move.
    pub page_id: String,
    /// Target page ID — new parent for `position: "append"`, or sibling
    /// reference for `"before"`/`"after"`.
    pub target_id: String,
    /// Position relative to the target. Defaults to `"append"`.
    /// Accepted values: `"append"` (target becomes new parent), `"before"`,
    /// `"after"`. Same-space only — cross-space moves are not supported.
    #[serde(default)]
    pub position: Option<String>,
}

/// Parameters for the `confluence_download` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfluenceDownloadParams {
    /// Root page ID to download from. Either this or `space` must be set.
    #[serde(default)]
    pub id: Option<String>,
    /// Space key to download from — every top-level page becomes a root.
    #[serde(default)]
    pub space: Option<String>,
    /// Target directory for downloaded files. Defaults to a fresh tempdir
    /// when omitted; the manifest summary reports the path used.
    #[serde(default)]
    pub output_dir: Option<String>,
    /// Only download pages whose title contains this substring (case-insensitive).
    #[serde(default)]
    pub title_filter: Option<String>,
    /// Maximum number of concurrent fetches. Defaults to 8.
    #[serde(default)]
    pub concurrency: Option<usize>,
    /// Maximum tree depth. 0 = unlimited (default).
    #[serde(default)]
    pub max_depth: Option<u32>,
    /// Output format: `"jfm"` (default) or `"adf"`.
    #[serde(default)]
    pub format: Option<String>,
    /// Also download each page's attachment binaries into an `attachments/`
    /// subdirectory beside its content file. Defaults to false.
    #[serde(default)]
    pub include_attachments: Option<bool>,
}

/// Parameters for the `confluence_children` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfluenceChildrenParams {
    /// Page ID whose children should be listed. Omit when using `space`.
    #[serde(default)]
    pub id: Option<String>,
    /// Space key (mutually exclusive with `id`): list top-level pages in
    /// the space.
    #[serde(default)]
    pub space: Option<String>,
    /// Recursively fetch descendants.
    #[serde(default)]
    pub recursive: Option<bool>,
    /// Maximum tree depth when `recursive` is set (0 = unlimited).
    #[serde(default)]
    pub max_depth: Option<u32>,
}

/// Parameters for the `confluence_history` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfluenceHistoryParams {
    /// Confluence page ID.
    pub id: String,
    /// Filter to versions at or after this point. Accepts a numeric version
    /// number (e.g. `"5"`) or an ISO 8601 date (e.g. `"2026-01-01T00:00:00Z"`).
    #[serde(default)]
    pub since: Option<String>,
    /// Maximum number of versions to return. `0` means unlimited. Defaults
    /// to 20.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Parameters for the `confluence_comment_list` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfluenceCommentListParams {
    /// Confluence page ID.
    pub id: String,
    /// Which kind of comments to include: `"footer"`, `"inline"`, or
    /// `"all"` (the default — both, merged and sorted by creation time).
    #[serde(default)]
    pub kind: Option<String>,
    /// Maximum number of comments to return (0 = unlimited).
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Parameters for the `confluence_comment_add` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfluenceCommentAddParams {
    /// Confluence page ID.
    pub id: String,
    /// Markdown content of the comment body. Converted to ADF before posting.
    pub content: String,
}

/// Parameters for the `confluence_comment_add_inline` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfluenceCommentAddInlineParams {
    /// Confluence page ID.
    pub id: String,
    /// Markdown content of the comment body. Converted to ADF before posting.
    pub content: String,
    /// Exact text on the page that the comment should anchor to.
    pub anchor_text: String,
    /// 1-based occurrence to anchor to when `anchor_text` appears more than
    /// once on the page. Required for ambiguous anchors; rejected if out of
    /// range.
    #[serde(default)]
    pub match_index: Option<usize>,
}

/// Parameters for the `confluence_comment_replies` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfluenceCommentRepliesParams {
    /// Parent comment ID.
    pub comment_id: String,
    /// `"footer"` or `"inline"` — Confluence stores reply chains on a
    /// kind-specific endpoint, so the caller must commit to one.
    pub kind: String,
    /// Maximum number of replies to return (0 = unlimited).
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Parameters for the `confluence_label_list` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfluenceLabelListParams {
    /// Confluence page ID.
    pub id: String,
    /// Maximum number of labels to return (0 = unlimited).
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Parameters for the `confluence_label_add` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfluenceLabelAddParams {
    /// Confluence page ID.
    pub id: String,
    /// Labels to add to the page.
    pub labels: Vec<String>,
}

/// Parameters for the `confluence_label_remove` tool.
///
/// `confirm` must be `true` for the removal to proceed. This is the
/// MCP-side guard for a destructive operation; the assistant must
/// explicitly opt in.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfluenceLabelRemoveParams {
    /// Confluence page ID.
    pub id: String,
    /// Labels to remove from the page.
    pub labels: Vec<String>,
    /// Must be set to `true` — destructive guard.
    pub confirm: bool,
}

/// Parameters for the `confluence_user_search` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfluenceUserSearchParams {
    /// Search text; matches display name or email.
    pub query: String,
    /// Maximum number of results (0 = unlimited). Defaults to 25.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Parameters for the `confluence_attachment_upload` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfluenceAttachmentUploadParams {
    /// Confluence page ID to attach the file to.
    pub page_id: String,
    /// Local filesystem path to the file to upload. Streamed from disk
    /// (never fully buffered in memory).
    pub file_path: String,
    /// Override the filename used in Confluence (defaults to the local
    /// basename).
    #[serde(default)]
    pub filename: Option<String>,
    /// Optional version comment recorded with the upload.
    #[serde(default)]
    pub comment: Option<String>,
    /// Marks the upload as a minor edit. Defaults to false.
    #[serde(default)]
    pub minor_edit: Option<bool>,
}

/// Parameters for the `confluence_attachment_list` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfluenceAttachmentListParams {
    /// Confluence page ID.
    pub page_id: String,
    /// Pagination cursor (use `next_cursor` from a previous call).
    #[serde(default)]
    pub cursor: Option<String>,
    /// Maximum number of attachments per page. Defaults to 25.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Parameters for the `confluence_attachment_download` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfluenceAttachmentDownloadParams {
    /// Attachment ID (from `confluence_attachment_list`).
    pub attachment_id: String,
    /// Destination path on the MCP server's filesystem. If omitted, the file
    /// is written to a fresh temp directory whose path is returned in the
    /// result so the assistant can read it via the filesystem tool. If this
    /// names an existing directory, the file is written inside it under the
    /// attachment's filename.
    #[serde(default)]
    pub output_path: Option<String>,
}

/// Parameters for the `confluence_attachment_delete` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfluenceAttachmentDeleteParams {
    /// Attachment ID.
    pub attachment_id: String,
    /// Permanently purge the attachment instead of moving it to trash
    /// (requires space admin). Defaults to false.
    #[serde(default)]
    pub purge: Option<bool>,
}

/// Parameters for the `confluence_space_list` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfluenceSpaceListParams {
    /// Filter to specific space keys. Combined with `type`/`status` as AND.
    #[serde(default)]
    pub keys: Option<Vec<String>>,
    /// Filter by space type. Common values: `global`, `personal`,
    /// `collaboration`, `knowledge_base`, `onboarding`. Passed through to
    /// the Confluence v2 API verbatim.
    #[serde(default)]
    pub r#type: Option<String>,
    /// Filter by space status. Common values: `current`, `archived`.
    /// Passed through to the Confluence v2 API verbatim.
    #[serde(default)]
    pub status: Option<String>,
    /// Pagination cursor (use `next_cursor` from a previous call).
    #[serde(default)]
    pub cursor: Option<String>,
    /// Maximum number of spaces per page. Defaults to 25.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Parameters for the `confluence_space_pages` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfluenceSpacePagesParams {
    /// Space key whose pages to enumerate (e.g. `ENG`).
    pub space: String,
    /// Filter by page status. Common values: `current`, `archived`, `draft`,
    /// `trashed`. Passed through to the Confluence v2 API verbatim.
    #[serde(default)]
    pub status: Option<String>,
    /// Sort order. Common values: `id`, `-id`, `title`, `-title`,
    /// `created-date`, `-created-date`, `modified-date`, `-modified-date`.
    /// Passed through to the Confluence v2 API verbatim.
    #[serde(default)]
    pub sort: Option<String>,
    /// Pagination cursor (use `next_cursor` from a previous call).
    #[serde(default)]
    pub cursor: Option<String>,
    /// Maximum number of pages per response. Defaults to 25.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Parameters for the `confluence_compare` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfluenceCompareParams {
    /// Confluence page ID.
    pub id: String,
    /// `from` version reference. Accepts `"latest"`, `"previous"`,
    /// `"v-N"` (e.g. `"v-2"`), a numeric version, or an ISO 8601 date.
    /// Defaults to `"previous"`.
    #[serde(default)]
    pub from: Option<String>,
    /// `to` version reference. Same accepted forms as `from`. Defaults to
    /// `"latest"`.
    #[serde(default)]
    pub to: Option<String>,
    /// Detail level: `"summary"`, `"outline"` (default), or `"full"`.
    #[serde(default)]
    pub detail: Option<String>,
    /// Top-level fields to include. Comma-separated. Accepted values:
    /// `"body"`, `"title"`, `"labels"`, `"metadata"`. Defaults to
    /// `"body,title,metadata"`.
    #[serde(default)]
    pub include: Option<String>,
    /// Collapse runs of whitespace before diffing. Defaults to `true`.
    #[serde(default)]
    pub ignore_whitespace: Option<bool>,
    /// Drop section deltas with fewer than this many characters of total
    /// changed text. `0` (default) disables the filter.
    #[serde(default)]
    pub min_change_chars: Option<u32>,
    /// Restrict to sections whose path matches one of the given strings.
    #[serde(default)]
    pub filter_sections: Option<Vec<String>>,
    /// Output budget in bytes. Defaults to ~16 KiB (≈4000 tokens).
    #[serde(default)]
    pub budget: Option<usize>,
}

/// Parameters for the `confluence_compare_section` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfluenceCompareSectionParams {
    /// Cursor returned by an outline-mode `confluence_compare` call. The
    /// cursor encodes the page ID and version pair, so this tool is
    /// stateless across calls.
    pub cursor: String,
    /// Output text format: `"unified"` (default), `"side_by_side"`, or
    /// `"markdown_inline"`.
    #[serde(default)]
    pub format: Option<String>,
}

// ── Output summaries ────────────────────────────────────────────────

/// Manifest summary returned by `confluence_download`.
#[derive(Debug, Serialize)]
struct DownloadSummary {
    output_dir: String,
    page_count: usize,
    pages: Vec<DownloadSummaryEntry>,
}

#[derive(Debug, Serialize)]
struct DownloadSummaryEntry {
    id: String,
    title: String,
    path: String,
}

/// A children-tree entry returned by `confluence_children`.
///
/// Mirrors the CLI output shape (see
/// `crate::cli::atlassian::confluence::children::ChildrenEntry`) so that
/// downstream consumers see a stable schema.
#[derive(Debug, Clone, Serialize)]
pub struct ChildrenEntry {
    /// Page ID.
    pub id: String,
    /// Page title.
    pub title: String,
    /// Page status (e.g. "current", "draft"); empty when unknown.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub status: String,
    /// Parent page ID, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Space key, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space_key: Option<String>,
    /// Nested children (populated when `recursive` is set).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Self>,
}

impl From<ChildPage> for ChildrenEntry {
    fn from(p: ChildPage) -> Self {
        Self {
            id: p.id,
            title: p.title,
            status: p.status,
            parent_id: p.parent_id,
            space_key: p.space_key,
            children: Vec::new(),
        }
    }
}

/// Envelope for a mutation's YAML response.
#[derive(Debug, Serialize)]
struct MutationResult<'a> {
    ok: bool,
    message: String,
    /// Page ID the mutation targeted.
    id: &'a str,
    /// Labels touched by the mutation (empty for comment operations).
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    labels: &'a [String],
}

// ── Helpers ─────────────────────────────────────────────────────────

/// Parses a `format` param (`"jfm"`/`"adf"`, case-insensitive).
fn parse_format(raw: Option<&str>) -> Result<ContentFormat> {
    match raw.map(str::to_ascii_lowercase).as_deref() {
        None | Some("jfm") => Ok(ContentFormat::Jfm),
        Some("adf") => Ok(ContentFormat::Adf),
        Some(other) => anyhow::bail!("Invalid format \"{other}\": must be \"jfm\" or \"adf\""),
    }
}

/// Renders a [`ContentItem`] as either JFM markdown or pretty ADF JSON.
fn render_content_item(
    item: &ContentItem,
    format: ContentFormat,
    instance_url: &str,
) -> Result<String> {
    match format {
        ContentFormat::Jfm => {
            let doc = content_item_to_document(item, instance_url)?;
            doc.render()
        }
        ContentFormat::Adf => {
            let body = item.body_adf.clone().unwrap_or(serde_json::Value::Null);
            serde_json::to_string_pretty(&body).context("Failed to serialize ADF JSON")
        }
    }
}

/// Parses `content` into an ADF document, given its format.
///
/// For JFM the frontmatter `title` is returned alongside; for ADF the title
/// is empty (callers provide it separately).
fn parse_write_content(
    content: &str,
    format: ContentFormat,
) -> Result<(ValidatedAdfDocument, String)> {
    let (adf, title): (AdfDocument, String) = match format {
        ContentFormat::Jfm => {
            // JFM inputs with frontmatter are passed as-is; inputs without
            // frontmatter are treated as raw markdown. The CLI requires
            // frontmatter, but the MCP caller already passes `id`/`title`
            // separately, so we don't force it here.
            if content.starts_with("---\n") {
                let doc = JfmDocument::parse(content)?;
                let adf = markdown_to_adf(&doc.body)?;
                let title = match &doc.frontmatter {
                    JfmFrontmatter::Confluence(fm) => fm.title.clone(),
                    JfmFrontmatter::Jira(fm) => fm.summary.clone(),
                };
                (adf, title)
            } else {
                let adf = markdown_to_adf(content)?;
                (adf, String::new())
            }
        }
        ContentFormat::Adf => {
            let adf = AdfDocument::from_json_str(content)?;
            (adf, String::new())
        }
    };
    Ok((ValidatedAdfDocument::try_new(adf)?, title))
}

/// Serializes search results as YAML for the tool response body.
fn serialize_search_results(
    results: &crate::atlassian::client::ConfluenceSearchResults,
) -> Result<String> {
    serde_yaml::to_string(results).context("Failed to serialize search results")
}

/// Build the download summary from the manifest produced by `run_download`.
fn build_download_summary(output_dir: &std::path::Path) -> Result<String> {
    let manifest_path = output_dir.join("manifest.json");
    let pages: Vec<DownloadSummaryEntry> = if manifest_path.exists() {
        let json = std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("Failed to read manifest at {}", manifest_path.display()))?;
        let parsed: BTreeMap<String, ManifestEntry> =
            serde_json::from_str(&json).context("Failed to parse download manifest")?;
        parsed
            .into_iter()
            .map(|(id, entry)| DownloadSummaryEntry {
                id,
                title: entry.title,
                path: entry.path,
            })
            .collect()
    } else {
        Vec::new()
    };

    let summary = DownloadSummary {
        output_dir: output_dir.to_string_lossy().to_string(),
        page_count: pages.len(),
        pages,
    };
    serde_yaml::to_string(&summary).context("Failed to serialize download summary")
}

/// Resolves the download output directory, creating a tempdir when omitted.
///
/// Returns the path plus an optional [`tempfile::TempDir`] guard that must be
/// kept alive for the duration of the download when a tempdir was created.
fn resolve_output_dir(requested: Option<String>) -> Result<(PathBuf, Option<tempfile::TempDir>)> {
    if let Some(raw) = requested {
        let path = PathBuf::from(raw);
        std::fs::create_dir_all(&path)
            .with_context(|| format!("Failed to create output dir {}", path.display()))?;
        Ok((path, None))
    } else {
        let tmp = tempfile::Builder::new()
            .prefix("omni-dev-confluence-download-")
            .tempdir()
            .context("Failed to create download tempdir")?;
        let path = tmp.path().to_path_buf();
        Ok((path, Some(tmp)))
    }
}

// ── Children / comment / label / user-search helpers ───────────────

/// Builds the YAML output for the `confluence_history` tool.
///
/// Delegates to the CLI's `fetch_history` so MCP and CLI share a single
/// schema.
pub async fn fetch_history_yaml(
    api: &ConfluenceApi,
    page_id: &str,
    since: Option<&str>,
    limit: u32,
) -> Result<String> {
    let history =
        crate::cli::atlassian::confluence::history::fetch_history(api, page_id, since, limit)
            .await?;
    to_yaml(&history)
}

/// Builds the YAML output for the `confluence_compare` tool. Delegates to
/// the CLI's `run_compare` so MCP and CLI share a single schema.
pub async fn fetch_compare_yaml(
    api: &ConfluenceApi,
    instance_url: &str,
    params: &ConfluenceCompareParams,
) -> Result<String> {
    use crate::atlassian::diff_format::DEFAULT_OUTPUT_BUDGET;
    use crate::cli::atlassian::confluence::compare::{run_compare, CompareCommand, DetailArg};
    use crate::cli::atlassian::format::OutputFormat;

    let detail = match params.detail.as_deref() {
        None | Some("outline") => DetailArg::Outline,
        Some("summary") => DetailArg::Summary,
        Some("full") => DetailArg::Full,
        Some(other) => anyhow::bail!(
            "Invalid detail \"{other}\"; expected \"summary\", \"outline\", or \"full\""
        ),
    };

    let cmd = CompareCommand {
        id: params.id.clone(),
        from: params
            .from
            .clone()
            .unwrap_or_else(|| "previous".to_string()),
        to: params.to.clone().unwrap_or_else(|| "latest".to_string()),
        detail,
        include: params
            .include
            .clone()
            .unwrap_or_else(|| "body,title,metadata".to_string()),
        ignore_whitespace: params.ignore_whitespace.unwrap_or(true),
        min_change_chars: params.min_change_chars.unwrap_or(0),
        filter_sections: params.filter_sections.clone().unwrap_or_default(),
        budget: params.budget.unwrap_or(DEFAULT_OUTPUT_BUDGET),
        output: OutputFormat::Yaml,
    };
    let out = run_compare(api, instance_url, &cmd).await?;
    to_yaml(&out)
}

/// Builds the text output for the `confluence_compare_section` tool.
pub async fn fetch_compare_section_text(
    api: &ConfluenceApi,
    cursor: &str,
    format: Option<&str>,
) -> Result<String> {
    use crate::atlassian::diff_format::{Cursor, SectionFormat};
    use crate::cli::atlassian::confluence::compare::run_compare_section;

    let cur = Cursor::decode(cursor).context("Invalid cursor")?;
    let format = match format {
        None | Some("unified") => SectionFormat::Unified,
        Some("side_by_side") => SectionFormat::SideBySide,
        Some("markdown_inline") => SectionFormat::MarkdownInline,
        Some(other) => anyhow::bail!(
            "Invalid format \"{other}\"; expected \"unified\", \"side_by_side\", or \"markdown_inline\""
        ),
    };
    run_compare_section(api, &cur, format).await
}

/// Builds the YAML output for the `confluence_children` tool.
///
/// Either `id` or `space` must be set. When `recursive` is true,
/// descendants are fetched up to `max_depth` (0 = unlimited).
pub async fn fetch_children_yaml(
    api: &ConfluenceApi,
    id: Option<&str>,
    space: Option<&str>,
    recursive: bool,
    max_depth: u32,
) -> Result<String> {
    let space_key = space.map(ToString::to_string);
    let top = fetch_top_level(api, id, space).await?;
    let mut entries = to_entries(top, space_key.as_deref());

    if recursive {
        for entry in &mut entries {
            populate_descendants(api, entry, 1, max_depth, space_key.as_deref()).await?;
        }
    }

    to_yaml(&entries)
}

/// Fetches the top-level list for either a page id or a space key.
///
/// `id` takes precedence over `space`. Returns an error if neither is set.
async fn fetch_top_level(
    api: &ConfluenceApi,
    id: Option<&str>,
    space: Option<&str>,
) -> Result<Vec<ChildPage>> {
    if let Some(page_id) = id {
        return api.get_children(page_id).await;
    }
    let space_key = space.context("Provide either `id` or `space`")?;
    let space_id = api.resolve_space_id(space_key).await?;
    api.get_space_root_pages(&space_id).await
}

/// Whether recursion should continue at the given depth.
fn should_recurse(depth: u32, max_depth: u32) -> bool {
    max_depth == 0 || depth < max_depth
}

/// Converts `ChildPage` values into `ChildrenEntry`, filling in a
/// missing `space_key` from the provided key when present.
fn to_entries(pages: Vec<ChildPage>, space_key: Option<&str>) -> Vec<ChildrenEntry> {
    let mut entries = Vec::with_capacity(pages.len());
    for mut page in pages {
        if page.space_key.is_none() {
            page.space_key = space_key.map(str::to_string);
        }
        entries.push(ChildrenEntry::from(page));
    }
    entries
}

/// Recursively fetches descendants and populates the `children` field.
fn populate_descendants<'a>(
    api: &'a ConfluenceApi,
    entry: &'a mut ChildrenEntry,
    depth: u32,
    max_depth: u32,
    space_key: Option<&'a str>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(async move {
        if !should_recurse(depth, max_depth) {
            return Ok(());
        }
        entry.children = to_entries(api.get_children(&entry.id).await?, space_key);
        for child in &mut entry.children {
            populate_descendants(api, child, depth + 1, max_depth, space_key).await?;
        }
        Ok(())
    })
}

/// Which kind(s) of comments a list call should return.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CommentKindSelector {
    /// Footer comments only.
    Footer,
    /// Inline comments only.
    Inline,
    /// Both kinds, merged and sorted by creation time.
    All,
}

impl CommentKindSelector {
    /// Parses the MCP `kind` string. `None` and `"all"` map to [`Self::All`].
    pub fn parse(raw: Option<&str>) -> Result<Self> {
        match raw.map(str::trim) {
            None | Some("" | "all") => Ok(Self::All),
            Some("footer") => Ok(Self::Footer),
            Some("inline") => Ok(Self::Inline),
            Some(other) => {
                anyhow::bail!("`kind` must be \"footer\", \"inline\", or \"all\"; got {other:?}")
            }
        }
    }
}

/// Parses the MCP `kind` string for endpoints that don't accept `"all"`
/// (replies live on a kind-specific endpoint).
pub fn parse_comment_kind(raw: &str) -> Result<CommentKind> {
    match raw.trim() {
        "footer" => Ok(CommentKind::Footer),
        "inline" => Ok(CommentKind::Inline),
        other => anyhow::bail!("`kind` must be \"footer\" or \"inline\"; got {other:?}"),
    }
}

/// Builds the YAML output for the `confluence_comment_list` tool.
///
/// `limit` of 0 returns every comment; otherwise the list is truncated to
/// the requested size (matching the CLI `--limit` semantics).
pub async fn list_comments_yaml(
    api: &ConfluenceApi,
    id: &str,
    kind: CommentKindSelector,
    limit: usize,
) -> Result<String> {
    let mut comments = match kind {
        CommentKindSelector::Footer => api.get_page_comments(id).await?,
        CommentKindSelector::Inline => api.get_page_inline_comments(id).await?,
        CommentKindSelector::All => {
            let mut footer = api.get_page_comments(id).await?;
            let inline = api.get_page_inline_comments(id).await?;
            footer.extend(inline);
            footer.sort_by(|a, b| a.created.cmp(&b.created));
            footer
        }
    };
    if limit > 0 {
        comments.truncate(limit);
    }
    to_yaml(&comments)
}

/// Builds the YAML output for the `confluence_comment_replies` tool.
pub async fn list_comment_replies_yaml(
    api: &ConfluenceApi,
    comment_id: &str,
    kind: CommentKind,
    limit: usize,
) -> Result<String> {
    let mut replies = api.get_comment_replies(comment_id, kind).await?;
    if limit > 0 {
        replies.truncate(limit);
    }
    to_yaml(&replies)
}

/// Posts a footer comment to a Confluence page.
///
/// The markdown `content` is converted to ADF before posting.
pub async fn add_comment_result(api: &ConfluenceApi, id: &str, content: &str) -> Result<String> {
    let adf: AdfDocument = markdown_to_adf(content).context("Failed to convert markdown to ADF")?;
    let adf = ValidatedAdfDocument::try_new(adf)?;
    api.add_page_comment(id, &adf).await?;

    let result = MutationResult {
        ok: true,
        message: format!("Comment added to page {id}."),
        id,
        labels: &[],
    };
    to_yaml(&result)
}

/// Posts an inline (anchored) comment to a Confluence page.
///
/// `anchor_text` must appear at least once in the page body; for ambiguous
/// anchors, `match_index_1based` (1-based) selects which occurrence to bind to.
pub async fn add_inline_comment_result(
    api: &ConfluenceApi,
    id: &str,
    content: &str,
    anchor_text: &str,
    match_index_1based: Option<usize>,
) -> Result<String> {
    let adf: AdfDocument = markdown_to_adf(content).context("Failed to convert markdown to ADF")?;
    let adf = ValidatedAdfDocument::try_new(adf)?;
    let anchor = api
        .resolve_anchor(id, anchor_text, match_index_1based)
        .await?;
    api.add_inline_page_comment(id, &adf, &anchor).await?;

    let result = MutationResult {
        ok: true,
        message: format!(
            "Inline comment added to page {id} anchored to {:?} (occurrence {} of {}).",
            anchor.text,
            anchor.match_index + 1,
            anchor.match_count
        ),
        id,
        labels: &[],
    };
    to_yaml(&result)
}

/// Builds the YAML output for the `confluence_label_list` tool.
pub async fn list_labels_yaml(api: &ConfluenceApi, id: &str, limit: usize) -> Result<String> {
    let mut labels = api.get_labels(id).await?;
    if limit > 0 {
        labels.truncate(limit);
    }
    to_yaml(&labels)
}

/// Adds labels to a Confluence page and returns a YAML confirmation.
pub async fn add_labels_result(api: &ConfluenceApi, id: &str, labels: &[String]) -> Result<String> {
    if labels.is_empty() {
        anyhow::bail!("`labels` must contain at least one label");
    }

    api.add_labels(id, labels).await?;
    let result = MutationResult {
        ok: true,
        message: format!("Added {} label(s) to page {id}.", labels.len()),
        id,
        labels,
    };
    to_yaml(&result)
}

/// Removes labels from a Confluence page and returns a YAML confirmation.
pub async fn remove_labels_result(
    api: &ConfluenceApi,
    id: &str,
    labels: &[String],
    confirm: bool,
) -> Result<String> {
    if labels.is_empty() {
        anyhow::bail!("`labels` must contain at least one label");
    }
    if !confirm {
        anyhow::bail!(
            "Refusing to remove labels from page {id}: pass `confirm: true` to authorise this destructive operation."
        );
    }

    for label in labels {
        api.remove_label(id, label).await?;
    }

    let result = MutationResult {
        ok: true,
        message: format!("Removed {} label(s) from page {id}.", labels.len()),
        id,
        labels,
    };
    to_yaml(&result)
}

/// Builds the YAML output for the `confluence_user_search` tool.
///
/// `limit` of `None` defaults to 25, matching the CLI. A limit of `0`
/// requests every match.
pub async fn search_users_yaml(
    client: &AtlassianClient,
    query: &str,
    limit: u32,
) -> Result<String> {
    let results = client.search_confluence_users(query, limit).await?;
    to_yaml(&results)
}

/// Uploads a file as an attachment and returns YAML for the resulting attachment.
pub async fn upload_attachment_result(
    api: &ConfluenceApi,
    page_id: &str,
    file_path: &str,
    filename: Option<&str>,
    comment: Option<&str>,
    minor_edit: bool,
) -> Result<String> {
    let path = std::path::Path::new(file_path);
    let attachment = api
        .upload_attachment(page_id, path, filename, comment, minor_edit)
        .await?;
    to_yaml(&attachment)
}

/// Builds the YAML output for the `confluence_attachment_list` tool.
pub async fn list_attachments_yaml(
    api: &ConfluenceApi,
    page_id: &str,
    cursor: Option<&str>,
    limit: u32,
) -> Result<String> {
    let page: ConfluenceAttachmentPage = api.list_attachments(page_id, cursor, limit).await?;
    to_yaml(&page)
}

/// Builds the YAML output for the `confluence_space_list` tool.
pub async fn list_spaces_yaml(
    api: &ConfluenceApi,
    keys: &[&str],
    type_: Option<&str>,
    status: Option<&str>,
    cursor: Option<&str>,
    limit: u32,
) -> Result<String> {
    let page: ConfluenceSpacePage = api.list_spaces(keys, type_, status, cursor, limit).await?;
    to_yaml(&page)
}

/// Builds the YAML output for the `confluence_space_pages` tool.
///
/// Resolves the space key to a space ID, then fetches one page of summary
/// records. Pagination is not auto-drained — callers thread `next_cursor`
/// back to fetch subsequent pages.
pub async fn fetch_space_pages_yaml(
    api: &ConfluenceApi,
    space: &str,
    status: Option<&str>,
    sort: Option<&str>,
    cursor: Option<&str>,
    limit: u32,
) -> Result<String> {
    let space_id = api.resolve_space_id(space).await?;
    let page: PageSummaryPage = api
        .list_space_pages(&space_id, status, sort, cursor, limit)
        .await?;
    to_yaml(&page)
}

/// YAML result for the `confluence_attachment_download` tool.
#[derive(Debug, Serialize)]
struct AttachmentDownloadResult {
    /// Attachment ID.
    id: String,
    /// Attachment filename/title.
    title: String,
    /// MIME type, when reported by the API.
    #[serde(skip_serializing_if = "Option::is_none")]
    media_type: Option<String>,
    /// Number of bytes written.
    bytes: usize,
    /// Absolute path on disk where the attachment was written.
    path: String,
}

/// Downloads an attachment to disk and returns YAML metadata describing it
/// (including its on-disk path).
pub async fn download_attachment_yaml(
    api: &ConfluenceApi,
    attachment_id: &str,
    output_path: Option<&str>,
) -> Result<String> {
    let (attachment, bytes) = api.download_attachment(attachment_id).await?;
    let path = resolve_attachment_path(output_path, &attachment.title)?;
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(std::fs::create_dir_all)
        .transpose()
        .with_context(|| format!("Failed to create parent directory for {}", path.display()))?;
    std::fs::write(&path, &bytes).with_context(|| format!("Failed to write {}", path.display()))?;
    let result = AttachmentDownloadResult {
        id: attachment.id,
        title: attachment.title,
        media_type: attachment.media_type,
        bytes: bytes.len(),
        path: path.to_string_lossy().into_owned(),
    };
    to_yaml(&result)
}

/// Resolves the destination path for a single downloaded attachment.
///
/// With no `output_path`, a fresh temp directory is created and the
/// attachment's title is written inside it. An `output_path` that names an
/// existing directory is joined with the title; otherwise it is used verbatim.
fn resolve_attachment_path(output_path: Option<&str>, title: &str) -> Result<PathBuf> {
    if let Some(p) = output_path {
        let path = PathBuf::from(p);
        if path.is_dir() {
            Ok(path.join(title))
        } else {
            Ok(path)
        }
    } else {
        let tmp = tempfile::Builder::new()
            .prefix("omni-dev-confluence-attachment-")
            .tempdir()
            .context("Failed to create temp dir for attachment download")?;
        Ok(tmp.keep().join(title))
    }
}

/// Deletes an attachment and returns a YAML confirmation.
pub async fn delete_attachment_result(
    api: &ConfluenceApi,
    attachment_id: &str,
    purge: bool,
) -> Result<String> {
    api.delete_attachment(attachment_id, purge).await?;
    let result = MutationResult {
        ok: true,
        message: format!(
            "Deleted attachment {attachment_id}{}.",
            if purge { " (purged)" } else { "" }
        ),
        id: attachment_id,
        labels: &[],
    };
    to_yaml(&result)
}

// ── Tool handlers ────────────────────────────────────────────────────

#[allow(missing_docs)] // #[tool_router] generates a pub `confluence_tool_router` fn.
#[tool_router(router = confluence_tool_router, vis = "pub")]
impl OmniDevServer {
    /// Tool: fetch a Confluence page as JFM markdown (default) or ADF JSON.
    #[tool(
        description = "Fetch a Confluence page by ID. Returns JFM markdown by default, or raw ADF JSON when format=\"adf\". \
                       When `output_file` is set, the content is written to that path and the tool returns \
                       a short YAML summary (path/bytes/format) — useful for large pages. \
                       Mirrors `omni-dev atlassian confluence read`."
    )]
    pub async fn confluence_read(
        &self,
        Parameters(params): Parameters<ConfluenceReadParams>,
    ) -> Result<CallToolResult, McpError> {
        let format = parse_format(params.format.as_deref()).map_err(tool_error)?;
        let rendered = run_confluence_read(&params.id, format, params.output_file.as_deref())
            .await
            .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(rendered)]))
    }

    /// Tool: search Confluence pages by CQL.
    #[tool(
        description = "Search Confluence pages using CQL. Returns YAML with matching page IDs, titles, and space keys. \
                       Mirrors `omni-dev atlassian confluence search --cql`."
    )]
    pub async fn confluence_search(
        &self,
        Parameters(params): Parameters<ConfluenceSearchParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = run_confluence_search(&params.cql, params.limit.unwrap_or(20))
            .await
            .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: create a new Confluence page.
    #[tool(
        description = "Create a new Confluence page, from explicit fields or from a full JFM \
                       `document` (frontmatter + body, e.g. the output of `confluence_read`). \
                       With a `document`, `space_key`/`title`/`parent_id` come from the \
                       frontmatter and the body becomes the page body — enabling the \
                       read → edit → create round-trip. Explicit `space_key`/`title`/`parent_id` \
                       override frontmatter and a warning is returned when they do. JFM is \
                       GitHub-style markdown, NOT Confluence wiki markup — see resource \
                       `omni-dev://specs/jfm`. Returns the new page's ID. \
                       Mirrors `omni-dev atlassian confluence create`."
    )]
    pub async fn confluence_create(
        &self,
        Parameters(params): Parameters<ConfluenceCreateParams>,
    ) -> Result<CallToolResult, McpError> {
        let text = run_confluence_create(&params).await.map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    /// Tool: update a Confluence page's body (and optionally title).
    #[tool(
        description = "Overwrite a Confluence page's body from JFM markdown (default) or raw ADF JSON. \
                       JFM is GitHub-style markdown, NOT Confluence wiki markup — see resource \
                       `omni-dev://specs/jfm` for syntax. \
                       Mirrors `omni-dev atlassian confluence write --force`."
    )]
    pub async fn confluence_write(
        &self,
        Parameters(params): Parameters<ConfluenceWriteParams>,
    ) -> Result<CallToolResult, McpError> {
        let format = parse_format(params.format.as_deref()).map_err(tool_error)?;
        run_confluence_write(&params.id, &params.content, format)
            .await
            .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Updated {}",
            params.id
        ))]))
    }

    /// Tool: delete a Confluence page. Destructive — requires `confirm: true`.
    #[tool(
        description = "Delete a Confluence page. IRREVERSIBLE. Requires the caller to pass `confirm: true` \
                       to prevent accidental deletions. Set `purge: true` to permanently purge instead of \
                       moving to trash (requires space admin). Mirrors `omni-dev atlassian confluence delete --force`."
    )]
    pub async fn confluence_delete(
        &self,
        Parameters(params): Parameters<ConfluenceDeleteParams>,
    ) -> Result<CallToolResult, McpError> {
        run_confluence_delete(&params).await.map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Deleted page {}",
            params.id
        ))]))
    }

    /// Tool: move/reparent a Confluence page within its current space.
    #[tool(
        description = "Move or reparent a Confluence page within its current space. \
                       `position` is `\"append\"` (default — target becomes new parent), \
                       `\"before\"`, or `\"after\"` (sibling reorder relative to target). \
                       Same-space only — cross-space moves are not supported. \
                       Returns the moved page's metadata as YAML (id, title, parent_id, ancestors). \
                       Mirrors `omni-dev atlassian confluence move`."
    )]
    pub async fn confluence_move(
        &self,
        Parameters(params): Parameters<ConfluenceMoveParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = run_confluence_move(&params).await.map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: recursively download a Confluence page tree.
    #[tool(
        description = "Recursively download a Confluence page or an entire space into a directory. \
                       Either `id` (root page) or `space` (space key) must be provided. \
                       Set `include_attachments: true` to also fetch each page's attachment \
                       binaries into an `attachments/` subdirectory beside its content file. \
                       Returns a YAML manifest summary of downloaded pages. \
                       Mirrors `omni-dev atlassian confluence download`."
    )]
    pub async fn confluence_download(
        &self,
        Parameters(params): Parameters<ConfluenceDownloadParams>,
    ) -> Result<CallToolResult, McpError> {
        let summary = run_confluence_download(params).await.map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(summary)]))
    }

    /// Lists children of a Confluence page, or top-level pages in a space,
    /// with optional recursion.
    #[tool(
        description = "List children of a Confluence page, or top-level pages in a space. \
                       Supports optional recursion with a max depth. Mirrors \
                       `omni-dev atlassian confluence children`."
    )]
    pub async fn confluence_children(
        &self,
        Parameters(params): Parameters<ConfluenceChildrenParams>,
    ) -> Result<CallToolResult, McpError> {
        let (client, _url) = create_client().map_err(tool_error)?;
        let api = ConfluenceApi::new(client);
        let yaml = fetch_children_yaml(
            &api,
            params.id.as_deref(),
            params.space.as_deref(),
            params.recursive.unwrap_or(false),
            params.max_depth.unwrap_or(0),
        )
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Lists version history (metadata only) for a Confluence page.
    #[tool(
        description = "List version history (metadata only) for a Confluence page. \
                       Returns version number, timestamp, author account ID, edit \
                       message, and minor-edit flag for each version, newest-first. \
                       Does NOT fetch version bodies — use `confluence_read` for \
                       content. `since` filters to versions at or after a numeric \
                       version (\"5\") or ISO 8601 date (\"2026-01-01T00:00:00Z\"). \
                       `limit` defaults to 20; `0` means unlimited. Mirrors \
                       `omni-dev atlassian confluence history`."
    )]
    pub async fn confluence_history(
        &self,
        Parameters(params): Parameters<ConfluenceHistoryParams>,
    ) -> Result<CallToolResult, McpError> {
        let (client, _url) = create_client().map_err(tool_error)?;
        let api = ConfluenceApi::new(client);
        let yaml = fetch_history_yaml(
            &api,
            &params.id,
            params.since.as_deref(),
            params.limit.unwrap_or(20),
        )
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Lists comments on a Confluence page.
    #[tool(description = "List comments on a Confluence page (auto-paginated). \
                       `kind` selects \"footer\", \"inline\", or \"all\" (default — \
                       both kinds merged and sorted by creation time). `limit` of 0 \
                       returns every comment. Mirrors \
                       `omni-dev atlassian confluence comment list`.")]
    pub async fn confluence_comment_list(
        &self,
        Parameters(params): Parameters<ConfluenceCommentListParams>,
    ) -> Result<CallToolResult, McpError> {
        let (client, _url) = create_client().map_err(tool_error)?;
        let api = ConfluenceApi::new(client);
        let kind = CommentKindSelector::parse(params.kind.as_deref()).map_err(tool_error)?;
        let yaml = list_comments_yaml(&api, &params.id, kind, params.limit.unwrap_or(25))
            .await
            .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Posts a footer comment to a Confluence page.
    #[tool(
        description = "Post a markdown comment to a Confluence page as a page-level \
                       footer comment. The content is converted to ADF before posting. \
                       For inline (anchored) comments, use \
                       `confluence_comment_add_inline`. Mirrors \
                       `omni-dev atlassian confluence comment add`."
    )]
    pub async fn confluence_comment_add(
        &self,
        Parameters(params): Parameters<ConfluenceCommentAddParams>,
    ) -> Result<CallToolResult, McpError> {
        let (client, _url) = create_client().map_err(tool_error)?;
        let api = ConfluenceApi::new(client);
        let yaml = add_comment_result(&api, &params.id, &params.content)
            .await
            .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Posts an inline (anchored) comment to a Confluence page.
    #[tool(
        description = "Post a markdown comment anchored to a text selection on a \
                       Confluence page. `anchor_text` must match the on-page text \
                       exactly; if it appears multiple times, pass `match_index` \
                       (1-based) to pick which occurrence. Errors if the anchor \
                       does not match or `match_index` is out of range. Mirrors \
                       `omni-dev atlassian confluence comment add-inline`."
    )]
    pub async fn confluence_comment_add_inline(
        &self,
        Parameters(params): Parameters<ConfluenceCommentAddInlineParams>,
    ) -> Result<CallToolResult, McpError> {
        let (client, _url) = create_client().map_err(tool_error)?;
        let api = ConfluenceApi::new(client);
        let yaml = add_inline_comment_result(
            &api,
            &params.id,
            &params.content,
            &params.anchor_text,
            params.match_index,
        )
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Lists the replies of a Confluence comment.
    #[tool(
        description = "List the replies (child comments) of a Confluence comment. \
                       `kind` must be \"footer\" or \"inline\" — Confluence stores \
                       reply chains on kind-specific endpoints, so the caller must \
                       commit to one. `limit` of 0 returns every reply. Mirrors \
                       `omni-dev atlassian confluence comment replies`."
    )]
    pub async fn confluence_comment_replies(
        &self,
        Parameters(params): Parameters<ConfluenceCommentRepliesParams>,
    ) -> Result<CallToolResult, McpError> {
        let (client, _url) = create_client().map_err(tool_error)?;
        let api = ConfluenceApi::new(client);
        let kind = parse_comment_kind(&params.kind).map_err(tool_error)?;
        let yaml =
            list_comment_replies_yaml(&api, &params.comment_id, kind, params.limit.unwrap_or(25))
                .await
                .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Lists labels on a Confluence page.
    #[tool(description = "List labels on a Confluence page (auto-paginated). \
                       `limit` of 0 returns every label. Mirrors \
                       `omni-dev atlassian confluence label list`.")]
    pub async fn confluence_label_list(
        &self,
        Parameters(params): Parameters<ConfluenceLabelListParams>,
    ) -> Result<CallToolResult, McpError> {
        let (client, _url) = create_client().map_err(tool_error)?;
        let api = ConfluenceApi::new(client);
        let yaml = list_labels_yaml(&api, &params.id, params.limit.unwrap_or(0))
            .await
            .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Adds labels to a Confluence page.
    #[tool(description = "Add one or more labels to a Confluence page. Mirrors \
                       `omni-dev atlassian confluence label add`.")]
    pub async fn confluence_label_add(
        &self,
        Parameters(params): Parameters<ConfluenceLabelAddParams>,
    ) -> Result<CallToolResult, McpError> {
        let (client, _url) = create_client().map_err(tool_error)?;
        let api = ConfluenceApi::new(client);
        let yaml = add_labels_result(&api, &params.id, &params.labels)
            .await
            .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Removes labels from a Confluence page.
    #[tool(
        description = "Remove one or more labels from a Confluence page. Destructive \
                       operation: callers must explicitly pass `confirm: true` for the \
                       removal to proceed; otherwise the tool refuses with an error. \
                       Mirrors `omni-dev atlassian confluence label remove`."
    )]
    pub async fn confluence_label_remove(
        &self,
        Parameters(params): Parameters<ConfluenceLabelRemoveParams>,
    ) -> Result<CallToolResult, McpError> {
        let (client, _url) = create_client().map_err(tool_error)?;
        let api = ConfluenceApi::new(client);
        let yaml = remove_labels_result(&api, &params.id, &params.labels, params.confirm)
            .await
            .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Searches Confluence users by display name or email.
    #[tool(
        description = "Search Confluence users by display name or email. `limit` of 0 \
                       returns every match; defaults to 25. Mirrors \
                       `omni-dev atlassian confluence user search`."
    )]
    pub async fn confluence_user_search(
        &self,
        Parameters(params): Parameters<ConfluenceUserSearchParams>,
    ) -> Result<CallToolResult, McpError> {
        let (client, _url) = create_client().map_err(tool_error)?;
        let yaml = search_users_yaml(&client, &params.query, params.limit.unwrap_or(25))
            .await
            .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Uploads a file as an attachment to a Confluence page.
    #[tool(
        description = "Upload a local file as an attachment to a Confluence page. \
                       `file_path` is a path on the MCP server's filesystem (the file is \
                       streamed from disk, never fully buffered). Optional `filename` \
                       overrides the stored name; `comment` is recorded as a version note; \
                       `minor_edit` (default false) marks the upload as minor. \
                       Returns YAML describing the new attachment. Mirrors \
                       `omni-dev atlassian confluence attachment upload`."
    )]
    pub async fn confluence_attachment_upload(
        &self,
        Parameters(params): Parameters<ConfluenceAttachmentUploadParams>,
    ) -> Result<CallToolResult, McpError> {
        let (client, _url) = create_client().map_err(tool_error)?;
        let api = ConfluenceApi::new(client);
        let yaml = upload_attachment_result(
            &api,
            &params.page_id,
            &params.file_path,
            params.filename.as_deref(),
            params.comment.as_deref(),
            params.minor_edit.unwrap_or(false),
        )
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Lists attachments on a Confluence page (paginated).
    #[tool(
        description = "List attachments on a Confluence page (one page per call). \
                       Pass the returned `next_cursor` back as `cursor` to fetch the next \
                       page. `limit` defaults to 25. Mirrors \
                       `omni-dev atlassian confluence attachment list`."
    )]
    pub async fn confluence_attachment_list(
        &self,
        Parameters(params): Parameters<ConfluenceAttachmentListParams>,
    ) -> Result<CallToolResult, McpError> {
        let (client, _url) = create_client().map_err(tool_error)?;
        let api = ConfluenceApi::new(client);
        let yaml = list_attachments_yaml(
            &api,
            &params.page_id,
            params.cursor.as_deref(),
            params.limit.unwrap_or(25),
        )
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Downloads an attachment binary by ID to disk.
    #[tool(
        description = "Download a Confluence attachment by ID to disk. Returns YAML \
                       metadata (id, title, media_type, bytes, on-disk path). If \
                       `output_path` is omitted, the file is written to a fresh temp \
                       directory whose path is in the result; the assistant can then \
                       read it via the filesystem tool. Mirrors \
                       `omni-dev atlassian confluence attachment download`."
    )]
    pub async fn confluence_attachment_download(
        &self,
        Parameters(params): Parameters<ConfluenceAttachmentDownloadParams>,
    ) -> Result<CallToolResult, McpError> {
        let (client, _url) = create_client().map_err(tool_error)?;
        let api = ConfluenceApi::new(client);
        let yaml =
            download_attachment_yaml(&api, &params.attachment_id, params.output_path.as_deref())
                .await
                .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Lists Confluence spaces (paginated).
    #[tool(
        description = "List Confluence spaces (one page per call). Optional filters: \
                       `keys` (Vec<String>), `type` (common values: `global`, \
                       `personal`, `collaboration`, `knowledge_base`, `onboarding` \
                       — passed through to the API verbatim, so other \
                       template-derived types Atlassian returns are also accepted), \
                       `status` (common values: `current`, `archived`). Filters \
                       combine as AND. Pass the returned `next_cursor` back as \
                       `cursor` to fetch the next page. `limit` defaults to 25. \
                       Mirrors `omni-dev atlassian confluence space list`."
    )]
    pub async fn confluence_space_list(
        &self,
        Parameters(params): Parameters<ConfluenceSpaceListParams>,
    ) -> Result<CallToolResult, McpError> {
        let keys_owned: Vec<String> = params.keys.unwrap_or_default();
        let keys: Vec<&str> = keys_owned.iter().map(String::as_str).collect();

        let (client, _url) = create_client().map_err(tool_error)?;
        let api = ConfluenceApi::new(client);
        let yaml = list_spaces_yaml(
            &api,
            &keys,
            params.r#type.as_deref(),
            params.status.as_deref(),
            params.cursor.as_deref(),
            params.limit.unwrap_or(25),
        )
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Enumerates pages within a Confluence space (paginated).
    #[tool(
        description = "Enumerate pages within a Confluence space (one page per call). \
                       Returns summary records: `id`, `title`, `status`, `parentId`, \
                       `authorId`, `createdAt` — no page bodies. \
                       Optional filters: `status` (common values: `current`, \
                       `archived`, `draft`, `trashed`) and `sort` (common values: \
                       `id`, `-id`, `title`, `-title`, `created-date`, \
                       `-created-date`, `modified-date`, `-modified-date`) — both \
                       passed through to the Confluence v2 API verbatim. Pass the \
                       returned `next_cursor` back as `cursor` to fetch the next \
                       page. `limit` defaults to 25. \
                       Mirrors `omni-dev atlassian confluence space pages`."
    )]
    pub async fn confluence_space_pages(
        &self,
        Parameters(params): Parameters<ConfluenceSpacePagesParams>,
    ) -> Result<CallToolResult, McpError> {
        let (client, _url) = create_client().map_err(tool_error)?;
        let api = ConfluenceApi::new(client);
        let yaml = fetch_space_pages_yaml(
            &api,
            &params.space,
            params.status.as_deref(),
            params.sort.as_deref(),
            params.cursor.as_deref(),
            params.limit.unwrap_or(25),
        )
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Deletes an attachment by ID.
    #[tool(
        description = "Delete a Confluence attachment by ID. Set `purge: true` to \
                       permanently purge instead of moving to trash (requires space admin). \
                       Mirrors `omni-dev atlassian confluence attachment delete --force`."
    )]
    pub async fn confluence_attachment_delete(
        &self,
        Parameters(params): Parameters<ConfluenceAttachmentDeleteParams>,
    ) -> Result<CallToolResult, McpError> {
        let (client, _url) = create_client().map_err(tool_error)?;
        let api = ConfluenceApi::new(client);
        let yaml =
            delete_attachment_result(&api, &params.attachment_id, params.purge.unwrap_or(false))
                .await
                .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Diffs two versions of a Confluence page.
    #[tool(
        description = "Compare two versions of a Confluence page. Returns a structurally-aware \
                       diff: walks the ADF tree, splits the document into heading-delimited \
                       sections, and reports per-block changes rather than character-level \
                       deltas over a serialization. \
                       \n\nVersion refs accept \"latest\", \"previous\", \"v-N\" (e.g. \
                       \"v-2\"), a numeric version, or an ISO 8601 date. `previous` is \
                       relative to `to`. \
                       \n\nDetail levels: `summary` (counts only), `outline` (default — \
                       per-section change kind + drill-in cursors), `full` (embeds per-section \
                       deltas, budget-truncated). \
                       \n\nMirrors `omni-dev atlassian confluence compare run`."
    )]
    pub async fn confluence_compare(
        &self,
        Parameters(params): Parameters<ConfluenceCompareParams>,
    ) -> Result<CallToolResult, McpError> {
        let (client, instance_url) = create_client().map_err(tool_error)?;
        let api = ConfluenceApi::new(client);
        let yaml = fetch_compare_yaml(&api, &instance_url, &params)
            .await
            .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Drills into a single section diff using a cursor from a prior compare call.
    #[tool(description = "Drill into a section diff using a cursor returned by \
                       `confluence_compare` (outline mode). Stateless: the cursor encodes \
                       the page ID and version pair. Output formats: \"unified\" (default), \
                       \"side_by_side\", \"markdown_inline\". Mirrors \
                       `omni-dev atlassian confluence compare section`.")]
    pub async fn confluence_compare_section(
        &self,
        Parameters(params): Parameters<ConfluenceCompareSectionParams>,
    ) -> Result<CallToolResult, McpError> {
        let (client, _url) = create_client().map_err(tool_error)?;
        let api = ConfluenceApi::new(client);
        let text = fetch_compare_section_text(&api, &params.cursor, params.format.as_deref())
            .await
            .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }
}

// ── Internal run_* helpers ──────────────────────────────────────────

async fn run_confluence_read(
    id: &str,
    format: ContentFormat,
    output_file: Option<&str>,
) -> Result<String> {
    let (client, instance_url) = create_client()?;
    let api = ConfluenceApi::new(client);
    let item = api.get_content(id).await?;
    let label = format_label(&format);
    let rendered = render_content_item(&item, format, &instance_url)?;
    match output_file {
        Some(path) => write_to_file_yaml(path, &rendered, label),
        None => Ok(rendered),
    }
}

/// String label for a [`ContentFormat`], used in write summaries.
fn format_label(format: &ContentFormat) -> &'static str {
    match format {
        ContentFormat::Jfm => "jfm",
        ContentFormat::Adf => "adf",
    }
}

async fn run_confluence_search(cql: &str, limit: u32) -> Result<String> {
    let (client, _instance_url) = create_client()?;
    let result = client.search_confluence(cql, limit).await?;
    serialize_search_results(&result)
}

/// Creates a Confluence page and returns the new page ID.
///
/// Two modes: from a full JFM `document` (frontmatter resolved like the CLI),
/// or from explicit `space_key`/`title`/`content` fields. When the document
/// path shadows a frontmatter value with an explicit parameter, a `warning:`
/// line is prepended to the returned text (and logged). Validation runs before
/// any client construction so input errors short-circuit before the wire.
async fn run_confluence_create(params: &ConfluenceCreateParams) -> Result<String> {
    if let Some(document) = params.document.as_deref() {
        if params.content.is_some() {
            anyhow::bail!(
                "Provide either `document` or `content`, not both — the document body becomes \
                 the page body"
            );
        }

        let resolved = resolve_confluence_create(
            document,
            params.space_key.as_deref(),
            params.title.as_deref(),
            params.parent_id.as_deref(),
        )?;
        for shadowed in &resolved.shadowed {
            tracing::warn!("{}", shadowed.warning_line());
        }

        let (client, _instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);
        let id = api
            .create_page(
                &resolved.space_key,
                &resolved.title,
                &resolved.adf,
                resolved.parent_id.as_deref(),
            )
            .await?;
        return Ok(prepend_warnings(&resolved.shadowed, id));
    }

    let space_key = params.space_key.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "`space_key` is required (or provide a `document` whose frontmatter carries \
             `space_key:`)"
        )
    })?;
    let title = params.title.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "`title` is required (or provide a `document` whose frontmatter carries `title:`)"
        )
    })?;
    let content = params
        .content
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("`content` is required (or provide a `document`)"))?;

    let format = parse_format(params.format.as_deref())?;
    let adf = match format {
        ContentFormat::Jfm => markdown_to_adf(content)?,
        ContentFormat::Adf => AdfDocument::from_json_str(content)?,
    };
    let adf = ValidatedAdfDocument::try_new(adf)?;

    let (client, _instance_url) = create_client()?;
    let api = ConfluenceApi::new(client);
    let id = api
        .create_page(space_key, title, &adf, params.parent_id.as_deref())
        .await?;
    Ok(id)
}

async fn run_confluence_write(id: &str, content: &str, format: ContentFormat) -> Result<()> {
    let (adf, title) = parse_write_content(content, format)?;
    let (client, _instance_url) = create_client()?;
    let api = ConfluenceApi::new(client);
    let title_ref = if title.is_empty() {
        None
    } else {
        Some(title.as_str())
    };
    api.update_content(id, &adf, title_ref).await
}

async fn run_confluence_delete(params: &ConfluenceDeleteParams) -> Result<()> {
    if !params.confirm {
        anyhow::bail!("confluence_delete is irreversible — pass `confirm: true` to proceed.");
    }
    let (client, _instance_url) = create_client()?;
    let api = ConfluenceApi::new(client);
    api.delete_page(&params.id, params.purge.unwrap_or(false))
        .await
}

/// Parses a `position` param (`"append"`/`"before"`/`"after"`, case-insensitive).
fn parse_move_position(raw: Option<&str>) -> Result<MovePosition> {
    match raw.map(str::to_ascii_lowercase).as_deref() {
        None | Some("append") => Ok(MovePosition::Append),
        Some("before") => Ok(MovePosition::Before),
        Some("after") => Ok(MovePosition::After),
        Some(other) => anyhow::bail!(
            "Invalid position \"{other}\": must be \"append\", \"before\", or \"after\""
        ),
    }
}

async fn run_confluence_move(params: &ConfluenceMoveParams) -> Result<String> {
    let position = parse_move_position(params.position.as_deref())?;
    let (client, _instance_url) = create_client()?;
    let api = ConfluenceApi::new(client);
    let moved = api
        .move_page(&params.page_id, &params.target_id, position)
        .await?;
    to_yaml(&moved)
}

async fn run_confluence_download(params: ConfluenceDownloadParams) -> Result<String> {
    if params.id.is_none() && params.space.is_none() {
        anyhow::bail!("confluence_download requires either `id` or `space`");
    }

    let (client, instance_url) = create_client()?;
    let api = Arc::new(ConfluenceApi::new(client));

    // Hold the TempDir guard (if any) across the entire download so the
    // directory is not deleted before the manifest is read.
    let (output_dir, _guard) = resolve_output_dir(params.output_dir)?;
    let format = parse_format(params.format.as_deref())?;

    let download_params = DownloadParams {
        id: params.id,
        space: params.space,
        output_dir: output_dir.clone(),
        format,
        concurrency: params.concurrency.unwrap_or(8),
        max_depth: params.max_depth.unwrap_or(0),
        title_filter: params.title_filter,
        resume: false,
        include_attachments: params.include_attachments.unwrap_or(false),
        on_conflict: OnConflict::Overwrite,
        instance_url,
    };

    run_download(&api, &download_params).await?;
    build_download_summary(&output_dir)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::await_holding_lock // env lock intentionally held across await on a single-thread runtime
)]
mod tests {
    use super::*;

    use crate::atlassian::auth::{ATLASSIAN_API_TOKEN, ATLASSIAN_EMAIL, ATLASSIAN_INSTANCE_URL};

    /// Serialize env-backed tests — `create_client()` reads process-wide
    /// environment variables, so concurrent tests would race without a lock.
    /// Routes through the crate-wide `AUTH_ENV_MUTEX` so we don't race
    /// against env-mutating tests in other Atlassian-touching modules.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::atlassian::auth::test_util::AUTH_ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    struct EnvGuard;

    impl EnvGuard {
        fn set(instance_url: &str) -> Self {
            std::env::set_var(ATLASSIAN_INSTANCE_URL, instance_url);
            std::env::set_var(ATLASSIAN_EMAIL, "user@test.com");
            std::env::set_var(ATLASSIAN_API_TOKEN, "fake-token");
            Self
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            std::env::remove_var(ATLASSIAN_INSTANCE_URL);
            std::env::remove_var(ATLASSIAN_EMAIL);
            std::env::remove_var(ATLASSIAN_API_TOKEN);
        }
    }

    // ── parse_format ────────────────────────────────────────────────

    #[test]
    fn parse_format_default_is_jfm() {
        assert!(matches!(parse_format(None).unwrap(), ContentFormat::Jfm));
    }

    #[test]
    fn parse_format_jfm_case_insensitive() {
        assert!(matches!(
            parse_format(Some("JFM")).unwrap(),
            ContentFormat::Jfm
        ));
    }

    #[test]
    fn parse_format_adf() {
        assert!(matches!(
            parse_format(Some("adf")).unwrap(),
            ContentFormat::Adf
        ));
    }

    #[test]
    fn parse_format_invalid_errors() {
        let err = parse_format(Some("xml")).unwrap_err();
        assert!(err.to_string().contains("Invalid format"));
    }

    // ── CommentKindSelector::parse ─────────────────────────────────

    #[test]
    fn comment_kind_selector_parse_default_is_all() {
        assert_eq!(
            CommentKindSelector::parse(None).unwrap(),
            CommentKindSelector::All
        );
        assert_eq!(
            CommentKindSelector::parse(Some("")).unwrap(),
            CommentKindSelector::All
        );
        assert_eq!(
            CommentKindSelector::parse(Some("all")).unwrap(),
            CommentKindSelector::All
        );
    }

    #[test]
    fn comment_kind_selector_parse_footer() {
        assert_eq!(
            CommentKindSelector::parse(Some("footer")).unwrap(),
            CommentKindSelector::Footer
        );
    }

    #[test]
    fn comment_kind_selector_parse_inline() {
        assert_eq!(
            CommentKindSelector::parse(Some("inline")).unwrap(),
            CommentKindSelector::Inline
        );
    }

    #[test]
    fn comment_kind_selector_parse_invalid_errors() {
        let err = CommentKindSelector::parse(Some("bogus")).unwrap_err();
        assert!(err.to_string().contains("\"footer\""));
        assert!(err.to_string().contains("bogus"));
    }

    #[test]
    fn comment_kind_selector_parse_trims_whitespace() {
        assert_eq!(
            CommentKindSelector::parse(Some("  footer  ")).unwrap(),
            CommentKindSelector::Footer
        );
    }

    // ── parse_comment_kind ─────────────────────────────────────────

    #[test]
    fn parse_comment_kind_footer() {
        assert_eq!(parse_comment_kind("footer").unwrap(), CommentKind::Footer);
    }

    #[test]
    fn parse_comment_kind_inline() {
        assert_eq!(parse_comment_kind("inline").unwrap(), CommentKind::Inline);
    }

    #[test]
    fn parse_comment_kind_invalid_errors() {
        let err = parse_comment_kind("all").unwrap_err();
        // `all` is not accepted here — replies endpoints are kind-specific.
        assert!(err.to_string().contains("\"footer\""));
        assert!(err.to_string().contains("all"));
    }

    #[test]
    fn parse_comment_kind_trims_whitespace() {
        assert_eq!(
            parse_comment_kind("  inline  ").unwrap(),
            CommentKind::Inline
        );
    }

    // ── parse_write_content ────────────────────────────────────────

    #[test]
    fn parse_write_content_jfm_without_frontmatter_yields_empty_title() {
        let (adf, title) = parse_write_content("Hello world", ContentFormat::Jfm).unwrap();
        assert!(title.is_empty());
        assert!(!adf.content.is_empty());
    }

    #[test]
    fn parse_write_content_jfm_with_frontmatter_extracts_title() {
        let input = "---\ntype: confluence\ninstance: https://org.atlassian.net\ntitle: My Page\nspace_key: ENG\n---\n\nBody\n";
        let (adf, title) = parse_write_content(input, ContentFormat::Jfm).unwrap();
        assert_eq!(title, "My Page");
        assert!(!adf.content.is_empty());
    }

    #[test]
    fn parse_write_content_jfm_with_jira_frontmatter_uses_summary() {
        let input = "---\ntype: jira\ninstance: https://org.atlassian.net\nkey: PROJ-1\nsummary: Jira Summary\n---\n\nBody\n";
        let (_adf, title) = parse_write_content(input, ContentFormat::Jfm).unwrap();
        assert_eq!(title, "Jira Summary");
    }

    #[test]
    fn parse_write_content_adf_roundtrips() {
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"Hi"}]}]}"#;
        let (adf, title) = parse_write_content(adf_json, ContentFormat::Adf).unwrap();
        assert!(title.is_empty());
        assert_eq!(adf.content.len(), 1);
    }

    #[test]
    fn parse_write_content_adf_invalid_errors() {
        assert!(parse_write_content("not json", ContentFormat::Adf).is_err());
    }

    // ── build_download_summary ─────────────────────────────────────

    #[test]
    fn build_download_summary_missing_manifest_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let yaml = build_download_summary(tmp.path()).unwrap();
        assert!(yaml.contains("page_count: 0"));
    }

    #[test]
    fn build_download_summary_reads_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = serde_json::json!({
            "12345": {"title": "Root Page", "path": "12345-root-page/index.md"},
            "67890": {"title": "Child", "path": "12345-root-page/67890-child/index.md", "parent_id": "12345"}
        });
        std::fs::write(
            tmp.path().join("manifest.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();

        let yaml = build_download_summary(tmp.path()).unwrap();
        assert!(yaml.contains("page_count: 2"));
        assert!(yaml.contains("Root Page"));
        assert!(yaml.contains("Child"));
    }

    #[test]
    fn build_download_summary_corrupt_manifest_errors() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("manifest.json"), "not json").unwrap();
        assert!(build_download_summary(tmp.path()).is_err());
    }

    // ── resolve_output_dir ─────────────────────────────────────────

    #[test]
    fn resolve_output_dir_creates_tempdir_when_absent() {
        let (path, guard) = resolve_output_dir(None).unwrap();
        assert!(path.exists());
        assert!(guard.is_some(), "tempdir guard must be returned");
    }

    #[test]
    fn resolve_output_dir_uses_provided_path() {
        let tmp = tempfile::tempdir().unwrap();
        let requested = tmp.path().join("sub");
        let (path, guard) =
            resolve_output_dir(Some(requested.to_string_lossy().to_string())).unwrap();
        assert_eq!(path, requested);
        assert!(path.exists());
        assert!(guard.is_none());
    }

    // ── serialize_search_results ───────────────────────────────────

    #[test]
    fn serialize_search_results_emits_yaml() {
        use crate::atlassian::client::{ConfluenceSearchResult, ConfluenceSearchResults};
        let results = ConfluenceSearchResults {
            results: vec![ConfluenceSearchResult {
                id: "12345".to_string(),
                title: "Architecture".to_string(),
                space_key: "ENG".to_string(),
            }],
            total: 1,
        };
        let yaml = serialize_search_results(&results).unwrap();
        assert!(yaml.contains("12345"));
        assert!(yaml.contains("ENG"));
        assert!(yaml.contains("total: 1"));
    }

    // ── render_content_item ────────────────────────────────────────

    #[test]
    fn render_content_item_jfm_and_adf() {
        use crate::atlassian::api::{ContentItem, ContentMetadata};

        let item = ContentItem {
            id: "12345".to_string(),
            title: "Page".to_string(),
            body_adf: Some(serde_json::json!({
                "version": 1,
                "type": "doc",
                "content": [{"type": "paragraph", "content": [{"type": "text", "text": "Hi"}]}]
            })),
            metadata: ContentMetadata::Confluence {
                space_key: "ENG".to_string(),
                status: Some("current".to_string()),
                version: Some(1),
                parent_id: None,
            },
        };

        let jfm =
            render_content_item(&item, ContentFormat::Jfm, "https://org.atlassian.net").unwrap();
        assert!(
            jfm.contains("12345"),
            "expected page id in JFM output: {jfm}"
        );
        assert!(jfm.contains("page_id"), "expected page_id field: {jfm}");

        let adf =
            render_content_item(&item, ContentFormat::Adf, "https://org.atlassian.net").unwrap();
        assert!(adf.contains("\"doc\""));
    }

    #[test]
    fn render_content_item_adf_null_body() {
        use crate::atlassian::api::{ContentItem, ContentMetadata};
        let item = ContentItem {
            id: "1".to_string(),
            title: "t".to_string(),
            body_adf: None,
            metadata: ContentMetadata::Confluence {
                space_key: "S".to_string(),
                status: None,
                version: None,
                parent_id: None,
            },
        };
        let adf = render_content_item(&item, ContentFormat::Adf, "https://org").unwrap();
        assert!(adf.contains("null"));
    }

    // ── run_confluence_read ────────────────────────────────────────

    async fn mock_page(server: &wiremock::MockServer, id: &str) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!("/wiki/api/v2/pages/{id}")))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": id,
                "title": "Mock Page",
                "status": "current",
                "spaceId": "98765",
                "version": {"number": 1},
                "body": {
                    "atlas_doc_format": {
                        "value": "{\"version\":1,\"type\":\"doc\",\"content\":[{\"type\":\"paragraph\",\"content\":[{\"type\":\"text\",\"text\":\"Mocked\"}]}]}"
                    }
                }
            })))
            .mount(server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98765"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"key": "ENG"})),
            )
            .mount(server)
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_confluence_read_jfm_success() {
        let _lock = env_lock();
        let server = wiremock::MockServer::start().await;
        mock_page(&server, "12345").await;
        let _env = EnvGuard::set(&server.uri());

        let out = run_confluence_read("12345", ContentFormat::Jfm, None)
            .await
            .unwrap();
        assert!(out.contains("Mocked"));
        assert!(out.contains("page_id"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_confluence_read_adf_success() {
        let _lock = env_lock();
        let server = wiremock::MockServer::start().await;
        mock_page(&server, "12345").await;
        let _env = EnvGuard::set(&server.uri());

        let out = run_confluence_read("12345", ContentFormat::Adf, None)
            .await
            .unwrap();
        assert!(out.contains("\"doc\""));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_confluence_read_404_errors() {
        let _lock = env_lock();
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/99"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;
        let _env = EnvGuard::set(&server.uri());

        let err = run_confluence_read("99", ContentFormat::Jfm, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_confluence_read_jfm_writes_to_output_file() {
        let _lock = env_lock();
        let server = wiremock::MockServer::start().await;
        mock_page(&server, "12345").await;
        let _env = EnvGuard::set(&server.uri());

        let tmp = tempfile::tempdir().unwrap();
        let out_path = tmp.path().join("page.md");
        let path_str = out_path.to_str().unwrap();

        let summary = run_confluence_read("12345", ContentFormat::Jfm, Some(path_str))
            .await
            .unwrap();

        assert!(summary.contains(&format!("path: {path_str}")));
        assert!(summary.contains("format: jfm"));
        assert!(summary.contains("bytes:"));
        // Inline content must NOT leak into the summary.
        assert!(!summary.contains("Mocked"));

        let written = std::fs::read_to_string(&out_path).unwrap();
        assert!(written.contains("Mocked"));
        assert!(written.contains("page_id"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_confluence_read_adf_writes_to_output_file() {
        let _lock = env_lock();
        let server = wiremock::MockServer::start().await;
        mock_page(&server, "12345").await;
        let _env = EnvGuard::set(&server.uri());

        let tmp = tempfile::tempdir().unwrap();
        let out_path = tmp.path().join("page.json");
        let path_str = out_path.to_str().unwrap();

        let summary = run_confluence_read("12345", ContentFormat::Adf, Some(path_str))
            .await
            .unwrap();

        assert!(summary.contains("format: adf"));
        let written = std::fs::read_to_string(&out_path).unwrap();
        assert!(written.contains("\"doc\""));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_confluence_read_output_file_invalid_path_errors() {
        let _lock = env_lock();
        let server = wiremock::MockServer::start().await;
        mock_page(&server, "12345").await;
        let _env = EnvGuard::set(&server.uri());

        let err = run_confluence_read(
            "12345",
            ContentFormat::Jfm,
            Some("/nonexistent_dir_zxq/out.md"),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    #[test]
    fn format_label_matches_expected_strings() {
        assert_eq!(format_label(&ContentFormat::Jfm), "jfm");
        assert_eq!(format_label(&ContentFormat::Adf), "adf");
    }

    /// Mocks a Confluence page whose body ADF is a JSON string instead of an
    /// ADF document — parses past the API layer (the API just stores the
    /// `Value`) but fails inside `content_item_to_document`, exercising the
    /// `?` partial on `render_content_item(...)?` in [`run_confluence_read`].
    async fn mock_page_with_bad_adf(server: &wiremock::MockServer, id: &str) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!("/wiki/api/v2/pages/{id}")))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": id,
                    "title": "Bad ADF",
                    "status": "current",
                    "spaceId": "98765",
                    "version": {"number": 1},
                    "body": {
                        "atlas_doc_format": {
                            "value": "\"this is a JSON string, not an ADF doc\""
                        }
                    }
                })),
            )
            .mount(server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98765"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"key": "ENG"})),
            )
            .mount(server)
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_confluence_read_propagates_render_error() {
        let _lock = env_lock();
        let server = wiremock::MockServer::start().await;
        mock_page_with_bad_adf(&server, "12345").await;
        let _env = EnvGuard::set(&server.uri());

        let err = run_confluence_read("12345", ContentFormat::Jfm, None)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("Failed to parse ADF"),
            "got: {err}"
        );
    }

    // ── run_confluence_search ──────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    async fn run_confluence_search_returns_yaml() {
        let _lock = env_lock();
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/rest/api/content/search"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{
                        "id": "12345",
                        "title": "Arch",
                        "space": {"key": "ENG"}
                    }],
                    "totalSize": 1
                })),
            )
            .mount(&server)
            .await;
        let _env = EnvGuard::set(&server.uri());

        let yaml = run_confluence_search("space = ENG", 20).await.unwrap();
        assert!(yaml.contains("12345"));
        assert!(yaml.contains("Arch"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_confluence_search_400_errors() {
        let _lock = env_lock();
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/rest/api/content/search"))
            .respond_with(wiremock::ResponseTemplate::new(400).set_body_string("bad cql"))
            .mount(&server)
            .await;
        let _env = EnvGuard::set(&server.uri());

        let err = run_confluence_search("bogus", 20).await.unwrap_err();
        assert!(err.to_string().contains("400"));
    }

    // ── run_confluence_create ──────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    async fn run_confluence_create_jfm_success() {
        let _lock = env_lock();
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": [{"id": "98765"}]})),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "54321"})),
            )
            .mount(&server)
            .await;
        let _env = EnvGuard::set(&server.uri());

        let params = ConfluenceCreateParams {
            document: None,
            space_key: Some("ENG".to_string()),
            title: Some("New Page".to_string()),
            content: Some("Body".to_string()),
            parent_id: None,
            format: None,
        };
        let id = run_confluence_create(&params).await.unwrap();
        assert_eq!(id, "54321");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_confluence_create_adf_success() {
        let _lock = env_lock();
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": [{"id": "98765"}]})),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "999"})),
            )
            .mount(&server)
            .await;
        let _env = EnvGuard::set(&server.uri());

        let params = ConfluenceCreateParams {
            document: None,
            space_key: Some("ENG".to_string()),
            title: Some("ADF Page".to_string()),
            content: Some(r#"{"version":1,"type":"doc","content":[]}"#.to_string()),
            parent_id: Some("11111".to_string()),
            format: Some("adf".to_string()),
        };
        let id = run_confluence_create(&params).await.unwrap();
        assert_eq!(id, "999");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_confluence_create_rejects_invalid_adf_nesting() {
        // Issue #714: validation runs before any HTTP call. No EnvGuard
        // needed because the function returns before reaching create_client().
        let params = ConfluenceCreateParams {
            document: None,
            space_key: Some("ENG".to_string()),
            title: Some("Bad".to_string()),
            content: Some(
                ":::panel{type=info}\n:::expand{title=\"x\"}\nbody\n:::\n:::".to_string(),
            ),
            parent_id: None,
            format: Some("jfm".to_string()),
        };
        let err = run_confluence_create(&params).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid ADF nesting"));
        assert!(msg.contains("`expand` cannot be a child of `panel`"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_confluence_write_rejects_invalid_adf_nesting() {
        // Issue #714: validation runs before any HTTP call.
        let bad_jfm = ":::panel{type=info}\n:::expand{title=\"x\"}\nbody\n:::\n:::";
        let err = run_confluence_write("12345", bad_jfm, ContentFormat::Jfm)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid ADF nesting"));
        assert!(msg.contains("`expand` cannot be a child of `panel`"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_confluence_create_invalid_adf_errors() {
        let _lock = env_lock();
        let server = wiremock::MockServer::start().await;
        let _env = EnvGuard::set(&server.uri());

        let params = ConfluenceCreateParams {
            document: None,
            space_key: Some("ENG".to_string()),
            title: Some("Bad".to_string()),
            content: Some("not json".to_string()),
            parent_id: None,
            format: Some("adf".to_string()),
        };
        assert!(run_confluence_create(&params).await.is_err());
    }

    // ── run_confluence_create: #1058 document (frontmatter) round-trip ──────

    /// Builds a document-path `ConfluenceCreateParams` with an optional override.
    fn confluence_create_doc_params(
        document: &str,
        space_key: Option<&str>,
    ) -> ConfluenceCreateParams {
        ConfluenceCreateParams {
            document: Some(document.to_string()),
            space_key: space_key.map(String::from),
            title: None,
            content: None,
            parent_id: None,
            format: None,
        }
    }

    async fn mount_space_and_page(server: &wiremock::MockServer, page_id: &str) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": [{"id": "98765"}]})),
            )
            .mount(server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "id": page_id })),
            )
            .mount(server)
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_confluence_create_from_document_resolves_space_and_title() {
        let _lock = env_lock();
        let server = wiremock::MockServer::start().await;
        mount_space_and_page(&server, "54321").await;
        let _env = EnvGuard::set(&server.uri());

        let doc = "---\ntype: confluence\ninstance: https://org.atlassian.net\npage_id: '7'\ntitle: Round-tripped\nspace_key: ENG\n---\n\nBody from document\n";
        let id = run_confluence_create(&confluence_create_doc_params(doc, None))
            .await
            .unwrap();
        assert_eq!(id, "54321");
        assert!(!id.contains("warning:"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_confluence_create_document_param_override_warns_in_text() {
        let _lock = env_lock();
        let server = wiremock::MockServer::start().await;
        mount_space_and_page(&server, "54321").await;
        let _env = EnvGuard::set(&server.uri());

        let doc = "---\ntype: confluence\ninstance: https://org.atlassian.net\ntitle: T\nspace_key: OLD\n---\n\nBody\n";
        let text = run_confluence_create(&confluence_create_doc_params(doc, Some("NEW")))
            .await
            .unwrap();
        assert!(text.contains("warning:"));
        assert!(text.contains("OLD"));
        assert!(text.contains("NEW"));
        assert!(text.contains("54321"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_confluence_create_document_and_content_errors() {
        let params = ConfluenceCreateParams {
            document: Some(
                "---\ntype: confluence\ninstance: https://o.net\ntitle: T\nspace_key: ENG\n---\n\nB\n"
                    .to_string(),
            ),
            space_key: None,
            title: None,
            content: Some("conflicting".to_string()),
            parent_id: None,
            format: None,
        };
        let err = run_confluence_create(&params).await.unwrap_err();
        assert!(err
            .to_string()
            .contains("Provide either `document` or `content`"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_confluence_create_document_rejects_jira_frontmatter() {
        let doc = "---\ntype: jira\ninstance: https://org.atlassian.net\nkey: PROJ-1\nsummary: T\n---\n\nB\n";
        let err = run_confluence_create(&confluence_create_doc_params(doc, None))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("JIRA"));
    }

    // The non-document (explicit-fields) path required-field errors short-circuit
    // before any client/HTTP, so they need no env or mock.

    #[tokio::test(flavor = "current_thread")]
    async fn run_confluence_create_requires_space_key() {
        let params = ConfluenceCreateParams {
            document: None,
            space_key: None,
            title: None,
            content: None,
            parent_id: None,
            format: None,
        };
        let err = run_confluence_create(&params).await.unwrap_err();
        assert!(err.to_string().contains("`space_key` is required"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_confluence_create_requires_title() {
        let params = ConfluenceCreateParams {
            document: None,
            space_key: Some("ENG".to_string()),
            title: None,
            content: None,
            parent_id: None,
            format: None,
        };
        let err = run_confluence_create(&params).await.unwrap_err();
        assert!(err.to_string().contains("`title` is required"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_confluence_create_requires_content() {
        let params = ConfluenceCreateParams {
            document: None,
            space_key: Some("ENG".to_string()),
            title: Some("T".to_string()),
            content: None,
            parent_id: None,
            format: None,
        };
        let err = run_confluence_create(&params).await.unwrap_err();
        assert!(err.to_string().contains("`content` is required"));
    }

    // ── run_confluence_write ───────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    async fn run_confluence_write_jfm_success() {
        let _lock = env_lock();
        let server = wiremock::MockServer::start().await;
        // GET to fetch current version
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "12345",
                "title": "Old",
                "status": "current",
                "spaceId": "98765",
                "version": {"number": 3},
                "body": {"atlas_doc_format": {"value": "{\"version\":1,\"type\":\"doc\",\"content\":[]}"}}
            })))
            .mount(&server)
            .await;
        // PUT to update
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;
        let _env = EnvGuard::set(&server.uri());

        let result = run_confluence_write("12345", "New body", ContentFormat::Jfm).await;
        assert!(result.is_ok(), "got: {result:?}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_confluence_write_adf_success() {
        let _lock = env_lock();
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "12345",
                "title": "Old",
                "status": "current",
                "spaceId": "98765",
                "version": {"number": 3},
                "body": {"atlas_doc_format": {"value": "{\"version\":1,\"type\":\"doc\",\"content\":[]}"}}
            })))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;
        let _env = EnvGuard::set(&server.uri());

        let adf_json = r#"{"version":1,"type":"doc","content":[]}"#;
        let result = run_confluence_write("12345", adf_json, ContentFormat::Adf).await;
        assert!(result.is_ok(), "got: {result:?}");
    }

    // ── run_confluence_delete ──────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    async fn run_confluence_delete_requires_confirm() {
        let params = ConfluenceDeleteParams {
            id: "12345".to_string(),
            confirm: false,
            purge: None,
        };
        let err = run_confluence_delete(&params).await.unwrap_err();
        assert!(err.to_string().contains("confirm"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_confluence_delete_success() {
        let _lock = env_lock();
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345"))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let _env = EnvGuard::set(&server.uri());

        let params = ConfluenceDeleteParams {
            id: "12345".to_string(),
            confirm: true,
            purge: None,
        };
        assert!(run_confluence_delete(&params).await.is_ok());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_confluence_delete_purge_success() {
        let _lock = env_lock();
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345"))
            .and(wiremock::matchers::query_param("purge", "true"))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let _env = EnvGuard::set(&server.uri());

        let params = ConfluenceDeleteParams {
            id: "12345".to_string(),
            confirm: true,
            purge: Some(true),
        };
        assert!(run_confluence_delete(&params).await.is_ok());
    }

    // ── run_confluence_download ────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    async fn run_confluence_download_requires_id_or_space() {
        let params = ConfluenceDownloadParams {
            id: None,
            space: None,
            output_dir: None,
            title_filter: None,
            concurrency: None,
            max_depth: None,
            format: None,
            include_attachments: None,
        };
        let err = run_confluence_download(params).await.unwrap_err();
        assert!(err.to_string().contains("`id` or `space`"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_confluence_download_single_page_returns_manifest() {
        let _lock = env_lock();
        let server = wiremock::MockServer::start().await;

        // Root page lookup
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "12345",
                "title": "Root Page",
                "status": "current",
                "spaceId": "98765",
                "version": {"number": 1},
                "body": {"atlas_doc_format": {"value": "{\"version\":1,\"type\":\"doc\",\"content\":[]}"}}
            })))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98765"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"key": "ENG"})),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/child/page",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .mount(&server)
            .await;

        let _env = EnvGuard::set(&server.uri());
        let tmp = tempfile::tempdir().unwrap();

        let params = ConfluenceDownloadParams {
            id: Some("12345".to_string()),
            space: None,
            output_dir: Some(tmp.path().to_string_lossy().to_string()),
            title_filter: None,
            concurrency: Some(1),
            max_depth: None,
            format: None,
            include_attachments: None,
        };

        let summary = run_confluence_download(params).await.unwrap();
        assert!(summary.contains("page_count: 1"));
        assert!(summary.contains("Root Page"));
    }

    // ── run_confluence_write JFM with frontmatter (covers title.as_str() branch) ────

    #[tokio::test(flavor = "current_thread")]
    async fn run_confluence_write_jfm_with_frontmatter_sends_title() {
        let _lock = env_lock();
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "12345",
                "title": "Old",
                "status": "current",
                "spaceId": "98765",
                "version": {"number": 3},
                "body": {"atlas_doc_format": {"value": "{\"version\":1,\"type\":\"doc\",\"content\":[]}"}}
            })))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;
        let _env = EnvGuard::set(&server.uri());

        let content = "---\ntype: confluence\ninstance: https://org.atlassian.net\npage_id: '12345'\ntitle: New Title\nspace_key: ENG\n---\n\nBody\n";
        let result = run_confluence_write("12345", content, ContentFormat::Jfm).await;
        assert!(result.is_ok(), "got: {result:?}");
    }

    // ── Tool handler bodies (direct invocation via Parameters) ────

    use rmcp::handler::server::wrapper::Parameters;

    fn make_server() -> OmniDevServer {
        OmniDevServer::new()
    }

    /// Clear env vars so `create_client()` fails cleanly — lets us drive the
    /// tool handler body all the way through the error path.
    fn clear_env() {
        std::env::remove_var(ATLASSIAN_INSTANCE_URL);
        std::env::remove_var(ATLASSIAN_EMAIL);
        std::env::remove_var(ATLASSIAN_API_TOKEN);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confluence_read_handler_invalid_format_returns_tool_error() {
        let server = make_server();
        let params = ConfluenceReadParams {
            id: "12345".to_string(),
            format: Some("xml".to_string()),
            output_file: None,
        };
        let result = server.confluence_read(Parameters(params)).await;
        let err = result.unwrap_err();
        assert!(
            err.message.contains("Invalid format"),
            "got: {}",
            err.message
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confluence_read_handler_success_path_via_mock() {
        let _lock = env_lock();
        let srv = wiremock::MockServer::start().await;
        mock_page(&srv, "12345").await;
        let _env = EnvGuard::set(&srv.uri());

        let server = make_server();
        let result = server
            .confluence_read(Parameters(ConfluenceReadParams {
                id: "12345".to_string(),
                format: Some("jfm".to_string()),
                output_file: None,
            }))
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confluence_search_handler_success_path_via_mock() {
        let _lock = env_lock();
        let srv = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/rest/api/content/search"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": [], "totalSize": 0})),
            )
            .mount(&srv)
            .await;
        let _env = EnvGuard::set(&srv.uri());

        let server = make_server();
        let result = server
            .confluence_search(Parameters(ConfluenceSearchParams {
                cql: "type = page".to_string(),
                limit: None,
            }))
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confluence_search_handler_error_path() {
        let _lock = env_lock();
        clear_env();
        let server = make_server();
        let result = server
            .confluence_search(Parameters(ConfluenceSearchParams {
                cql: "type = page".to_string(),
                limit: Some(5),
            }))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confluence_space_list_handler_success_via_mock() {
        let _lock = env_lock();
        let srv = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {
                            "id": "100",
                            "key": "ENG",
                            "name": "Engineering",
                            "type": "global",
                            "status": "current",
                            "homepageId": "200"
                        }
                    ]
                })),
            )
            .mount(&srv)
            .await;
        let _env = EnvGuard::set(&srv.uri());

        let server = make_server();
        let result = server
            .confluence_space_list(Parameters(ConfluenceSpaceListParams {
                keys: None,
                r#type: None,
                status: None,
                cursor: None,
                limit: None,
            }))
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confluence_space_list_handler_passes_filter_query_params() {
        let _lock = env_lock();
        let srv = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .and(wiremock::matchers::query_param("keys", "ENG,DEV"))
            .and(wiremock::matchers::query_param("type", "knowledge_base"))
            .and(wiremock::matchers::query_param("status", "archived"))
            .and(wiremock::matchers::query_param("cursor", "opaque"))
            .and(wiremock::matchers::query_param("limit", "5"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .expect(1)
            .mount(&srv)
            .await;
        let _env = EnvGuard::set(&srv.uri());

        let server = make_server();
        let result = server
            .confluence_space_list(Parameters(ConfluenceSpaceListParams {
                keys: Some(vec!["ENG".to_string(), "DEV".to_string()]),
                r#type: Some("knowledge_base".to_string()),
                status: Some("archived".to_string()),
                cursor: Some("opaque".to_string()),
                limit: Some(5),
            }))
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false));
    }

    /// Confluence Cloud returns template-derived space types (e.g. `onboarding`
    /// for the "Software Development" template) that are not in the documented
    /// `global | personal | collaboration | knowledge_base` set. The MCP handler
    /// must pass them through verbatim rather than rejecting client-side.
    #[tokio::test(flavor = "current_thread")]
    async fn confluence_space_list_handler_passes_unrecognised_type_through() {
        let _lock = env_lock();
        let srv = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .and(wiremock::matchers::query_param("type", "onboarding"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .expect(1)
            .mount(&srv)
            .await;
        let _env = EnvGuard::set(&srv.uri());

        let server = make_server();
        let result = server
            .confluence_space_list(Parameters(ConfluenceSpaceListParams {
                keys: None,
                r#type: Some("onboarding".to_string()),
                status: None,
                cursor: None,
                limit: None,
            }))
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confluence_space_list_handler_maps_api_error_to_tool_error() {
        let _lock = env_lock();
        let srv = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&srv)
            .await;
        let _env = EnvGuard::set(&srv.uri());

        let server = make_server();
        let result = server
            .confluence_space_list(Parameters(ConfluenceSpaceListParams {
                keys: None,
                r#type: None,
                status: None,
                cursor: None,
                limit: None,
            }))
            .await;
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("500"),
            "expected 500 in mapped tool error, got: {err}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confluence_space_list_handler_error_path_without_credentials() {
        let _lock = env_lock();
        clear_env();
        let server = make_server();
        let result = server
            .confluence_space_list(Parameters(ConfluenceSpaceListParams {
                keys: None,
                r#type: None,
                status: None,
                cursor: None,
                limit: None,
            }))
            .await;
        assert!(result.is_err());
    }

    // ── confluence_space_pages handler / fetch_space_pages_yaml ─────

    async fn mock_space_pages_endpoints(
        srv: &wiremock::MockServer,
        space_key: &str,
        space_id: &str,
        results: serde_json::Value,
    ) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .and(wiremock::matchers::query_param("keys", space_key))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": [{"id": space_id}]})),
            )
            .mount(srv)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!(
                "/wiki/api/v2/spaces/{space_id}/pages"
            )))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": results})),
            )
            .mount(srv)
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fetch_space_pages_yaml_returns_yaml() {
        let _lock = env_lock();
        let srv = wiremock::MockServer::start().await;
        mock_space_pages_endpoints(
            &srv,
            "ENG",
            "98765",
            serde_json::json!([
                {"id": "1", "title": "Home", "status": "current",
                 "authorId": "u1", "createdAt": "2024-01-02T03:04:05Z"}
            ]),
        )
        .await;
        let client = AtlassianClient::new(&srv.uri(), "u@t.com", "tok").unwrap();
        let api = ConfluenceApi::new(client);
        let yaml = fetch_space_pages_yaml(&api, "ENG", None, None, None, 25)
            .await
            .unwrap();
        assert!(yaml.contains("Home"));
        assert!(yaml.contains("authorId: u1"));
        assert!(yaml.contains("2024-01-02T03:04:05Z"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn download_attachment_yaml_writes_file_and_reports_metadata() {
        let _lock = env_lock();
        let srv = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/attachments/att-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "att-1",
                    "title": "diagram.png",
                    "mediaType": "image/png",
                    "downloadLink": "/download/attachments/12345/diagram.png"
                })),
            )
            .mount(&srv)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/download/attachments/12345/diagram.png",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"PNGDATA".to_vec()))
            .mount(&srv)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.png");
        let client = AtlassianClient::new(&srv.uri(), "u@t.com", "tok").unwrap();
        let api = ConfluenceApi::new(client);
        let yaml = download_attachment_yaml(&api, "att-1", Some(out.to_str().unwrap()))
            .await
            .unwrap();

        assert!(yaml.contains("id: att-1"));
        assert!(yaml.contains("title: diagram.png"));
        assert!(yaml.contains("media_type: image/png"));
        assert!(yaml.contains("bytes: 7"));
        assert_eq!(std::fs::read(&out).unwrap(), b"PNGDATA");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confluence_attachment_download_handler_success_via_mock() {
        let _lock = env_lock();
        let srv = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/attachments/att-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "att-1",
                    "title": "diagram.png",
                    "mediaType": "image/png",
                    "downloadLink": "/download/attachments/12345/diagram.png"
                })),
            )
            .mount(&srv)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/download/attachments/12345/diagram.png",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"PNGDATA".to_vec()))
            .mount(&srv)
            .await;
        let _env = EnvGuard::set(&srv.uri());

        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.png");
        let server = make_server();
        let result = server
            .confluence_attachment_download(Parameters(ConfluenceAttachmentDownloadParams {
                attachment_id: "att-1".to_string(),
                output_path: Some(out.to_string_lossy().into_owned()),
            }))
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false));
        assert_eq!(std::fs::read(&out).unwrap(), b"PNGDATA");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confluence_attachment_download_handler_error_path_without_credentials() {
        let _lock = env_lock();
        clear_env();
        let server = make_server();
        let result = server
            .confluence_attachment_download(Parameters(ConfluenceAttachmentDownloadParams {
                attachment_id: "att-1".to_string(),
                output_path: None,
            }))
            .await;
        assert!(result.is_err());
    }

    /// Drives the `create_dir_all` branch of `download_attachment_yaml` by
    /// pointing `output_path` at a not-yet-existing nested directory.
    #[tokio::test(flavor = "current_thread")]
    async fn download_attachment_yaml_creates_missing_parent_dirs() {
        let _lock = env_lock();
        let srv = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/attachments/att-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "att-1",
                    "title": "diagram.png",
                    "downloadLink": "/download/attachments/12345/diagram.png"
                })),
            )
            .mount(&srv)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/download/attachments/12345/diagram.png",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"PNGDATA".to_vec()))
            .mount(&srv)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("nested").join("sub").join("out.png");
        let client = AtlassianClient::new(&srv.uri(), "u@t.com", "tok").unwrap();
        let api = ConfluenceApi::new(client);
        download_attachment_yaml(&api, "att-1", Some(out.to_str().unwrap()))
            .await
            .unwrap();

        assert_eq!(std::fs::read(&out).unwrap(), b"PNGDATA");
    }

    #[test]
    fn resolve_attachment_path_defaults_to_tempdir() {
        let path = resolve_attachment_path(None, "x.txt").unwrap();
        assert_eq!(path.file_name().unwrap(), "x.txt");
        assert!(path.parent().unwrap().is_dir());
    }

    #[test]
    fn resolve_attachment_path_existing_dir_joins_title() {
        let dir = tempfile::tempdir().unwrap();
        let path = resolve_attachment_path(Some(dir.path().to_str().unwrap()), "x.txt").unwrap();
        assert_eq!(path, dir.path().join("x.txt"));
    }

    #[test]
    fn resolve_attachment_path_explicit_file_used_verbatim() {
        let path = resolve_attachment_path(Some("/tmp/foo/bar.bin"), "x.txt").unwrap();
        assert_eq!(path, PathBuf::from("/tmp/foo/bar.bin"));
    }

    /// Covers the Err branch of `resolve_space_id` inside
    /// `fetch_space_pages_yaml` — the happy-path test above only exercises
    /// the success branch of the `?` on the resolve call.
    #[tokio::test(flavor = "current_thread")]
    async fn fetch_space_pages_yaml_propagates_resolve_space_id_error() {
        let _lock = env_lock();
        let srv = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .mount(&srv)
            .await;
        let client = AtlassianClient::new(&srv.uri(), "u@t.com", "tok").unwrap();
        let api = ConfluenceApi::new(client);
        let err = fetch_space_pages_yaml(&api, "NOPE", None, None, None, 25)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confluence_space_pages_handler_success_via_mock() {
        let _lock = env_lock();
        let srv = wiremock::MockServer::start().await;
        mock_space_pages_endpoints(
            &srv,
            "ENG",
            "100",
            serde_json::json!([
                {"id": "1", "title": "Home", "status": "current",
                 "parentId": null, "authorId": "u1",
                 "createdAt": "2024-01-02T03:04:05Z"}
            ]),
        )
        .await;
        let _env = EnvGuard::set(&srv.uri());

        let server = make_server();
        let result = server
            .confluence_space_pages(Parameters(ConfluenceSpacePagesParams {
                space: "ENG".to_string(),
                status: None,
                sort: None,
                cursor: None,
                limit: None,
            }))
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confluence_space_pages_handler_passes_filter_query_params() {
        let _lock = env_lock();
        let srv = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .and(wiremock::matchers::query_param("keys", "ENG"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": [{"id": "55"}]})),
            )
            .mount(&srv)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/55/pages"))
            .and(wiremock::matchers::query_param("status", "archived"))
            .and(wiremock::matchers::query_param("sort", "-created-date"))
            .and(wiremock::matchers::query_param("cursor", "opaque"))
            .and(wiremock::matchers::query_param("limit", "5"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .expect(1)
            .mount(&srv)
            .await;
        let _env = EnvGuard::set(&srv.uri());

        let server = make_server();
        let result = server
            .confluence_space_pages(Parameters(ConfluenceSpacePagesParams {
                space: "ENG".to_string(),
                status: Some("archived".to_string()),
                sort: Some("-created-date".to_string()),
                cursor: Some("opaque".to_string()),
                limit: Some(5),
            }))
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confluence_space_pages_handler_maps_api_error_to_tool_error() {
        let _lock = env_lock();
        let srv = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": [{"id": "1"}]})),
            )
            .mount(&srv)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/1/pages"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&srv)
            .await;
        let _env = EnvGuard::set(&srv.uri());

        let server = make_server();
        let result = server
            .confluence_space_pages(Parameters(ConfluenceSpacePagesParams {
                space: "ENG".to_string(),
                status: None,
                sort: None,
                cursor: None,
                limit: None,
            }))
            .await;
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("500"),
            "expected 500 in mapped tool error, got: {err}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confluence_space_pages_handler_error_path_without_credentials() {
        let _lock = env_lock();
        clear_env();
        let server = make_server();
        let result = server
            .confluence_space_pages(Parameters(ConfluenceSpacePagesParams {
                space: "ENG".to_string(),
                status: None,
                sort: None,
                cursor: None,
                limit: None,
            }))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confluence_create_handler_invalid_format_returns_tool_error() {
        let server = make_server();
        let result = server
            .confluence_create(Parameters(ConfluenceCreateParams {
                document: None,
                space_key: Some("ENG".to_string()),
                title: Some("T".to_string()),
                content: Some("body".to_string()),
                parent_id: None,
                format: Some("xml".to_string()),
            }))
            .await;
        let err = result.unwrap_err();
        assert!(err.message.contains("Invalid format"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confluence_create_handler_error_path_without_credentials() {
        let _lock = env_lock();
        clear_env();
        let server = make_server();
        let result = server
            .confluence_create(Parameters(ConfluenceCreateParams {
                document: None,
                space_key: Some("ENG".to_string()),
                title: Some("T".to_string()),
                content: Some("body".to_string()),
                parent_id: None,
                format: None,
            }))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confluence_create_handler_success_path_via_mock() {
        let _lock = env_lock();
        let srv = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": [{"id": "98765"}]})),
            )
            .mount(&srv)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "54321"})),
            )
            .mount(&srv)
            .await;
        let _env = EnvGuard::set(&srv.uri());

        let server = make_server();
        let result = server
            .confluence_create(Parameters(ConfluenceCreateParams {
                document: None,
                space_key: Some("ENG".to_string()),
                title: Some("T".to_string()),
                content: Some("Body".to_string()),
                parent_id: None,
                format: None,
            }))
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confluence_write_handler_success_path_via_mock() {
        let _lock = env_lock();
        let srv = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "12345",
                "title": "Old",
                "status": "current",
                "spaceId": "98765",
                "version": {"number": 3},
                "body": {"atlas_doc_format": {"value": "{\"version\":1,\"type\":\"doc\",\"content\":[]}"}}
            })))
            .mount(&srv)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&srv)
            .await;
        let _env = EnvGuard::set(&srv.uri());

        let server = make_server();
        let result = server
            .confluence_write(Parameters(ConfluenceWriteParams {
                id: "12345".to_string(),
                content: "New body".to_string(),
                format: None,
            }))
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confluence_write_handler_invalid_format_returns_tool_error() {
        let server = make_server();
        let result = server
            .confluence_write(Parameters(ConfluenceWriteParams {
                id: "12345".to_string(),
                content: "body".to_string(),
                format: Some("xml".to_string()),
            }))
            .await;
        let err = result.unwrap_err();
        assert!(err.message.contains("Invalid format"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confluence_write_handler_error_path_without_credentials() {
        let _lock = env_lock();
        clear_env();
        let server = make_server();
        let result = server
            .confluence_write(Parameters(ConfluenceWriteParams {
                id: "12345".to_string(),
                content: "body".to_string(),
                format: None,
            }))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confluence_delete_handler_success_message() {
        let _lock = env_lock();
        let srv = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345"))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .mount(&srv)
            .await;
        let _env = EnvGuard::set(&srv.uri());

        let server = make_server();
        let result = server
            .confluence_delete(Parameters(ConfluenceDeleteParams {
                id: "12345".to_string(),
                confirm: true,
                purge: None,
            }))
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confluence_delete_handler_without_confirm_returns_tool_error() {
        let server = make_server();
        let result = server
            .confluence_delete(Parameters(ConfluenceDeleteParams {
                id: "12345".to_string(),
                confirm: false,
                purge: None,
            }))
            .await;
        assert!(result.is_err());
    }

    // ── parse_move_position ────────────────────────────────────────

    #[test]
    fn parse_move_position_default_is_append() {
        assert!(matches!(
            parse_move_position(None).unwrap(),
            MovePosition::Append
        ));
    }

    #[test]
    fn parse_move_position_case_insensitive() {
        assert!(matches!(
            parse_move_position(Some("APPEND")).unwrap(),
            MovePosition::Append
        ));
        assert!(matches!(
            parse_move_position(Some("Before")).unwrap(),
            MovePosition::Before
        ));
        assert!(matches!(
            parse_move_position(Some("after")).unwrap(),
            MovePosition::After
        ));
    }

    #[test]
    fn parse_move_position_invalid_errors() {
        let err = parse_move_position(Some("sideways")).unwrap_err();
        assert!(err.to_string().contains("Invalid position"));
    }

    // ── confluence_move handler ────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    async fn confluence_move_handler_invalid_position_returns_tool_error() {
        let server = make_server();
        let result = server
            .confluence_move(Parameters(ConfluenceMoveParams {
                page_id: "12345".to_string(),
                target_id: "456".to_string(),
                position: Some("sideways".to_string()),
            }))
            .await;
        let err = result.unwrap_err();
        assert!(err.message.contains("Invalid position"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confluence_move_handler_success_via_mock() {
        let _lock = env_lock();
        let srv = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/move/append/456",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"pageId": "12345"})),
            )
            .mount(&srv)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345"))
            .and(wiremock::matchers::query_param("include-ancestors", "true"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "12345",
                    "title": "Moved Page",
                    "status": "current",
                    "spaceId": "98765",
                    "parentId": "456",
                    "ancestors": [{"id": "456"}]
                })),
            )
            .mount(&srv)
            .await;
        let _env = EnvGuard::set(&srv.uri());

        let server = make_server();
        let result = server
            .confluence_move(Parameters(ConfluenceMoveParams {
                page_id: "12345".to_string(),
                target_id: "456".to_string(),
                position: None,
            }))
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confluence_move_handler_error_path_without_credentials() {
        let _lock = env_lock();
        clear_env();
        let server = make_server();
        let result = server
            .confluence_move(Parameters(ConfluenceMoveParams {
                page_id: "12345".to_string(),
                target_id: "456".to_string(),
                position: Some("append".to_string()),
            }))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confluence_download_handler_missing_id_and_space_returns_tool_error() {
        let server = make_server();
        let result = server
            .confluence_download(Parameters(ConfluenceDownloadParams {
                id: None,
                space: None,
                output_dir: None,
                title_filter: None,
                concurrency: None,
                max_depth: None,
                format: None,
                include_attachments: None,
            }))
            .await;
        let err = result.unwrap_err();
        assert!(err.message.contains("`id` or `space`"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confluence_download_handler_success_via_mock() {
        let _lock = env_lock();
        let srv = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "12345",
                "title": "Root",
                "status": "current",
                "spaceId": "98765",
                "version": {"number": 1},
                "body": {"atlas_doc_format": {"value": "{\"version\":1,\"type\":\"doc\",\"content\":[]}"}}
            })))
            .mount(&srv)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98765"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"key": "ENG"})),
            )
            .mount(&srv)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/child/page",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .mount(&srv)
            .await;
        let _env = EnvGuard::set(&srv.uri());

        let tmp = tempfile::tempdir().unwrap();
        let server = make_server();
        let result = server
            .confluence_download(Parameters(ConfluenceDownloadParams {
                id: Some("12345".to_string()),
                space: None,
                output_dir: Some(tmp.path().to_string_lossy().to_string()),
                title_filter: None,
                concurrency: Some(1),
                max_depth: None,
                format: None,
                include_attachments: None,
            }))
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false));
    }

    // ── router registration ────────────────────────────────────────

    #[test]
    fn tool_router_registers_all_confluence_tools() {
        let router = OmniDevServer::confluence_tool_router();
        for name in [
            "confluence_read",
            "confluence_search",
            "confluence_create",
            "confluence_write",
            "confluence_delete",
            "confluence_move",
            "confluence_download",
            "confluence_children",
            "confluence_history",
            "confluence_comment_list",
            "confluence_comment_add",
            "confluence_comment_add_inline",
            "confluence_comment_replies",
            "confluence_label_list",
            "confluence_label_add",
            "confluence_label_remove",
            "confluence_user_search",
            "confluence_attachment_upload",
            "confluence_attachment_list",
            "confluence_attachment_download",
            "confluence_attachment_delete",
            "confluence_space_list",
            "confluence_space_pages",
            "confluence_compare",
            "confluence_compare_section",
        ] {
            assert!(router.has_route(name), "missing tool: {name}");
        }
    }

    // ── confluence_history handler / fetch_history_yaml ────────────

    async fn mock_history_endpoints(
        server: &wiremock::MockServer,
        page_id: &str,
        version: u32,
        results: serde_json::Value,
    ) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!(
                "/wiki/api/v2/pages/{page_id}"
            )))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": page_id,
                    "title": "Sample",
                    "status": "current",
                    "spaceId": "1",
                    "version": {"number": version}
                })),
            )
            .mount(server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!(
                "/wiki/api/v2/pages/{page_id}/versions"
            )))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": results
                })),
            )
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn fetch_history_yaml_returns_yaml() {
        let server = wiremock::MockServer::start().await;
        mock_history_endpoints(
            &server,
            "12",
            2,
            serde_json::json!([
                {"number": 2, "createdAt": "2026-05-08T10:00:00Z", "authorId": "a", "message": "two", "minorEdit": false},
                {"number": 1, "createdAt": "2026-05-07T10:00:00Z", "authorId": "b", "message": "", "minorEdit": true},
            ]),
        )
        .await;
        let client = AtlassianClient::new(&server.uri(), "u@t.com", "tok").unwrap();
        let api = ConfluenceApi::new(client);
        let yaml = fetch_history_yaml(&api, "12", None, 20).await.unwrap();
        assert!(yaml.contains("page:"));
        assert!(yaml.contains("title: Sample"));
        assert!(yaml.contains("number: 2"));
        assert!(yaml.contains("truncated: false"));
    }

    #[tokio::test]
    async fn fetch_history_yaml_propagates_invalid_since() {
        let server = wiremock::MockServer::start().await;
        let client = AtlassianClient::new(&server.uri(), "u@t.com", "tok").unwrap();
        let api = ConfluenceApi::new(client);
        let err = fetch_history_yaml(&api, "12", Some("garbage"), 20)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Invalid `since`"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confluence_history_handler_success_via_mock() {
        let _lock = env_lock();
        let server = wiremock::MockServer::start().await;
        mock_history_endpoints(
            &server,
            "12",
            1,
            serde_json::json!([
                {"number": 1, "createdAt": "2026-05-06T10:00:00Z", "authorId": "a", "message": "first", "minorEdit": false},
            ]),
        )
        .await;
        let _env = EnvGuard::set(&server.uri());

        let result = make_server()
            .confluence_history(Parameters(ConfluenceHistoryParams {
                id: "12".to_string(),
                since: None,
                limit: Some(20),
            }))
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confluence_history_handler_invalid_since_returns_tool_error() {
        // Drives the `.map_err(tool_error)?` (line 760) by triggering an
        // error inside `fetch_history_yaml` via an invalid `since` value.
        let _lock = env_lock();
        let _env = EnvGuard::set("http://127.0.0.1:1");
        let result = make_server()
            .confluence_history(Parameters(ConfluenceHistoryParams {
                id: "12".to_string(),
                since: Some("garbage".to_string()),
                limit: None,
            }))
            .await;
        let err = result.unwrap_err();
        assert!(
            err.message.contains("Invalid `since`"),
            "got: {}",
            err.message
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confluence_history_handler_no_credentials_returns_tool_error() {
        let _lock = env_lock();
        clear_env();
        let result = make_server()
            .confluence_history(Parameters(ConfluenceHistoryParams {
                id: "12".to_string(),
                since: None,
                limit: None,
            }))
            .await;
        assert!(result.is_err());
    }

    // ── Phase 2d: children / comment / label / user-search tests ───

    fn phase2d_mock_client(uri: &str) -> AtlassianClient {
        AtlassianClient::new(uri, "user@test.com", "token").unwrap()
    }

    fn phase2d_mock_api(server: &wiremock::MockServer) -> ConfluenceApi {
        ConfluenceApi::new(phase2d_mock_client(&server.uri()))
    }

    // ── ChildrenEntry::from ────────────────────────────────────────

    #[test]
    fn children_entry_from_child_page_copies_fields() {
        let entry = ChildrenEntry::from(ChildPage {
            id: "1".to_string(),
            title: "Page".to_string(),
            status: "current".to_string(),
            parent_id: Some("100".to_string()),
            space_key: Some("ENG".to_string()),
        });
        assert_eq!(entry.id, "1");
        assert_eq!(entry.title, "Page");
        assert_eq!(entry.status, "current");
        assert_eq!(entry.parent_id.as_deref(), Some("100"));
        assert_eq!(entry.space_key.as_deref(), Some("ENG"));
        assert!(entry.children.is_empty());
    }

    #[test]
    fn children_entry_serialize_skips_empty() {
        let entry = ChildrenEntry {
            id: "1".to_string(),
            title: "P".to_string(),
            status: String::new(),
            parent_id: None,
            space_key: None,
            children: Vec::new(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains("status"));
        assert!(!json.contains("parent_id"));
        assert!(!json.contains("space_key"));
        assert!(!json.contains("children"));
    }

    // ── should_recurse ─────────────────────────────────────────────

    #[test]
    fn should_recurse_unlimited_when_max_is_zero() {
        assert!(should_recurse(1, 0));
        assert!(should_recurse(100, 0));
    }

    #[test]
    fn should_recurse_strictly_less_than_max() {
        assert!(should_recurse(1, 3));
        assert!(should_recurse(2, 3));
        assert!(!should_recurse(3, 3));
        assert!(!should_recurse(10, 3));
    }

    // ── to_entries ─────────────────────────────────────────────────

    #[test]
    fn to_entries_fills_missing_space_key() {
        let pages = vec![ChildPage {
            id: "1".to_string(),
            title: "P".to_string(),
            status: "current".to_string(),
            parent_id: None,
            space_key: None,
        }];
        let entries = to_entries(pages, Some("ENG"));
        assert_eq!(entries[0].space_key.as_deref(), Some("ENG"));
    }

    #[test]
    fn to_entries_preserves_existing_space_key() {
        let pages = vec![ChildPage {
            id: "1".to_string(),
            title: "P".to_string(),
            status: "current".to_string(),
            parent_id: None,
            space_key: Some("ORIG".to_string()),
        }];
        let entries = to_entries(pages, Some("OTHER"));
        assert_eq!(entries[0].space_key.as_deref(), Some("ORIG"));
    }

    #[test]
    fn to_entries_empty_input() {
        let entries = to_entries(Vec::new(), Some("ENG"));
        assert!(entries.is_empty());
    }

    // ── fetch_children_yaml ────────────────────────────────────────

    #[tokio::test]
    async fn fetch_children_yaml_requires_target() {
        let server = wiremock::MockServer::start().await;
        let api = phase2d_mock_api(&server);
        let err = fetch_children_yaml(&api, None, None, false, 0)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Provide either"));
    }

    #[tokio::test]
    async fn fetch_children_yaml_by_id_non_recursive() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/100/child/page",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"id": "1", "title": "Alpha", "status": "current"},
                        {"id": "2", "title": "Beta", "status": "current"}
                    ]
                })),
            )
            .mount(&server)
            .await;

        let yaml = fetch_children_yaml(&phase2d_mock_api(&server), Some("100"), None, false, 0)
            .await
            .unwrap();
        assert!(yaml.contains("Alpha"));
        assert!(yaml.contains("Beta"));
    }

    #[tokio::test]
    async fn fetch_children_yaml_by_space_propagates_space_key() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": [{"id": "77"}]})),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/77/pages"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{"id": "1", "title": "Home", "status": "current"}]
                })),
            )
            .mount(&server)
            .await;

        let yaml = fetch_children_yaml(&phase2d_mock_api(&server), None, Some("ENG"), false, 0)
            .await
            .unwrap();
        assert!(yaml.contains("space_key: ENG"));
    }

    #[tokio::test]
    async fn fetch_children_yaml_recursive_respects_max_depth() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/1/child/page",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{"id": "2", "title": "Child", "status": "current"}]
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/2/child/page",
            ))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        let yaml = fetch_children_yaml(&phase2d_mock_api(&server), Some("1"), None, true, 1)
            .await
            .unwrap();
        assert!(yaml.contains("Child"));
    }

    #[tokio::test]
    async fn fetch_children_yaml_recursive_walks_tree() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/1/child/page",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{"id": "2", "title": "Mid", "status": "current"}]
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/2/child/page",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{"id": "3", "title": "Leaf", "status": "current"}]
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/3/child/page",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .mount(&server)
            .await;

        let yaml = fetch_children_yaml(&phase2d_mock_api(&server), Some("1"), None, true, 0)
            .await
            .unwrap();
        assert!(yaml.contains("Mid"));
        assert!(yaml.contains("Leaf"));
    }

    #[tokio::test]
    async fn fetch_children_yaml_api_error_propagates() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/99/child/page",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;

        let err = fetch_children_yaml(&phase2d_mock_api(&server), Some("99"), None, false, 0)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    // ── list_comments_yaml ─────────────────────────────────────────

    #[tokio::test]
    async fn list_comments_yaml_returns_yaml_sequence() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/pages/12345/footer-comments",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {
                            "id": "c1",
                            "version": {"authorId": "alice", "createdAt": "2026-04-01T10:00:00Z"}
                        }
                    ]
                })),
            )
            .mount(&server)
            .await;

        let yaml = list_comments_yaml(
            &phase2d_mock_api(&server),
            "12345",
            CommentKindSelector::Footer,
            25,
        )
        .await
        .unwrap();
        assert!(yaml.contains("id: c1"));
        assert!(yaml.contains("alice"));
    }

    #[tokio::test]
    async fn list_comments_yaml_unlimited_when_limit_zero() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/pages/12345/footer-comments",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"id": "c1", "version": {"authorId": "a", "createdAt": "t"}},
                        {"id": "c2", "version": {"authorId": "b", "createdAt": "t"}}
                    ]
                })),
            )
            .mount(&server)
            .await;

        let yaml = list_comments_yaml(
            &phase2d_mock_api(&server),
            "12345",
            CommentKindSelector::Footer,
            0,
        )
        .await
        .unwrap();
        assert!(yaml.contains("id: c1"));
        assert!(yaml.contains("id: c2"));
    }

    #[tokio::test]
    async fn list_comments_yaml_truncates_to_limit() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/pages/12345/footer-comments",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"id": "c1", "version": {"authorId": "a", "createdAt": "t"}},
                        {"id": "c2", "version": {"authorId": "b", "createdAt": "t"}},
                        {"id": "c3", "version": {"authorId": "c", "createdAt": "t"}}
                    ]
                })),
            )
            .mount(&server)
            .await;

        let yaml = list_comments_yaml(
            &phase2d_mock_api(&server),
            "12345",
            CommentKindSelector::Footer,
            1,
        )
        .await
        .unwrap();
        assert!(yaml.contains("id: c1"));
        assert!(!yaml.contains("id: c2"));
    }

    #[tokio::test]
    async fn list_comments_yaml_api_error_propagates() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/pages/99/footer-comments",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;

        let err = list_comments_yaml(
            &phase2d_mock_api(&server),
            "99",
            CommentKindSelector::Footer,
            25,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn list_comments_yaml_inline_kind_hits_inline_endpoint_only() {
        // `Inline` must NOT hit `/footer-comments`; only `/inline-comments`.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/pages/12345/inline-comments",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"id": "i1", "version": {"authorId": "bob", "createdAt": "2026-04-02T10:00:00Z"}}
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let yaml = list_comments_yaml(
            &phase2d_mock_api(&server),
            "12345",
            CommentKindSelector::Inline,
            25,
        )
        .await
        .unwrap();
        assert!(yaml.contains("id: i1"));
        assert!(yaml.contains("kind: inline"));
    }

    #[tokio::test]
    async fn list_comments_yaml_all_kind_merges_and_sorts() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/pages/12345/footer-comments",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"id": "f1", "version": {"authorId": "alice", "createdAt": "2026-04-02T10:00:00Z"}}
                    ]
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/pages/12345/inline-comments",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"id": "i1", "version": {"authorId": "bob", "createdAt": "2026-04-01T10:00:00Z"}}
                    ]
                })),
            )
            .mount(&server)
            .await;

        let yaml = list_comments_yaml(
            &phase2d_mock_api(&server),
            "12345",
            CommentKindSelector::All,
            25,
        )
        .await
        .unwrap();
        // Inline (older) sorts before footer (newer).
        let i_pos = yaml.find("id: i1").expect("inline comment present");
        let f_pos = yaml.find("id: f1").expect("footer comment present");
        assert!(
            i_pos < f_pos,
            "inline (older) should precede footer (newer)"
        );
        assert!(yaml.contains("kind: inline"));
        assert!(yaml.contains("kind: footer"));
    }

    #[tokio::test]
    async fn list_comment_replies_yaml_returns_yaml_sequence() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/inline-comments/parent1/children",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"id": "r1", "version": {"authorId": "alice", "createdAt": "2026-04-01T10:00:00Z"}}
                    ]
                })),
            )
            .mount(&server)
            .await;

        let yaml = list_comment_replies_yaml(
            &phase2d_mock_api(&server),
            "parent1",
            CommentKind::Inline,
            25,
        )
        .await
        .unwrap();
        assert!(yaml.contains("id: r1"));
        assert!(yaml.contains("kind: inline"));
    }

    // ── add_comment_result ─────────────────────────────────────────

    #[tokio::test]
    async fn add_comment_result_converts_markdown_and_posts() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/wiki/api/v2/footer-comments"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "c9"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let yaml = add_comment_result(&phase2d_mock_api(&server), "12345", "Hello **world**")
            .await
            .unwrap();
        assert!(yaml.contains("ok: true"));
        assert!(yaml.contains("id: '12345'") || yaml.contains("id: \"12345\""));
        assert!(yaml.contains("Comment added"));
    }

    #[tokio::test]
    async fn add_comment_result_rejects_invalid_adf_nesting() {
        // Issue #714: invalid markdown body short-circuits before any HTTP
        // call. The footer-comments mock is intentionally absent so any
        // request would be a clear test failure.
        let server = wiremock::MockServer::start().await;
        let bad_jfm = ":::panel{type=info}\n:::expand{title=\"x\"}\nbody\n:::\n:::";
        let err = add_comment_result(&phase2d_mock_api(&server), "12345", bad_jfm)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid ADF nesting"));
        assert!(msg.contains("`expand` cannot be a child of `panel`"));
    }

    #[tokio::test]
    async fn add_comment_result_api_error_propagates() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/wiki/api/v2/footer-comments"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;

        let err = add_comment_result(&phase2d_mock_api(&server), "12345", "hello")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    // ── add_inline_comment_result ──────────────────────────────────

    /// Mounts a page fetch returning a page whose body contains `anchor_text`.
    async fn mock_page_with_anchor(server: &wiremock::MockServer, id: &str, anchor_text: &str) {
        let adf_value = format!(
            "{{\"version\":1,\"type\":\"doc\",\"content\":[{{\"type\":\"paragraph\",\"content\":[{{\"type\":\"text\",\"text\":{}}}]}}]}}",
            serde_json::Value::String(anchor_text.to_string())
        );
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!("/wiki/api/v2/pages/{id}")))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": id,
                    "title": "Mock Page",
                    "status": "current",
                    "spaceId": "98765",
                    "version": {"number": 1},
                    "body": {"atlas_doc_format": {"value": adf_value}}
                })),
            )
            .mount(server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98765"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"key": "ENG"})),
            )
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn add_inline_comment_result_resolves_anchor_and_posts() {
        let server = wiremock::MockServer::start().await;
        mock_page_with_anchor(&server, "12345", "the anchored phrase").await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/wiki/api/v2/inline-comments"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "ic1"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let yaml = add_inline_comment_result(
            &phase2d_mock_api(&server),
            "12345",
            "comment body",
            "the anchored phrase",
            None,
        )
        .await
        .unwrap();
        assert!(yaml.contains("ok: true"));
        assert!(yaml.contains("Inline comment added"));
        assert!(yaml.contains("occurrence 1 of 1"));
    }

    #[tokio::test]
    async fn add_inline_comment_result_anchor_not_found() {
        let server = wiremock::MockServer::start().await;
        mock_page_with_anchor(&server, "12345", "something else entirely").await;

        let err = add_inline_comment_result(
            &phase2d_mock_api(&server),
            "12345",
            "body",
            "the anchored phrase",
            None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn add_inline_comment_result_ambiguous_with_explicit_match_index() {
        // Page body has the anchor twice — agent picks occurrence 2.
        let server = wiremock::MockServer::start().await;
        mock_page_with_anchor(&server, "12345", "phrase here and phrase again").await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/wiki/api/v2/inline-comments"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "inlineCommentProperties": {
                    "textSelection": "phrase",
                    "textSelectionMatchCount": 2,
                    "textSelectionMatchIndex": 1
                }
            })))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "ic2"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let yaml = add_inline_comment_result(
            &phase2d_mock_api(&server),
            "12345",
            "comment body",
            "phrase",
            Some(2),
        )
        .await
        .unwrap();
        assert!(yaml.contains("occurrence 2 of 2"));
    }

    #[tokio::test]
    async fn add_inline_comment_result_rejects_invalid_adf_nesting() {
        // Body parses to invalid ADF — short-circuits before any HTTP call.
        let server = wiremock::MockServer::start().await;
        let bad_jfm = ":::panel{type=info}\n:::expand{title=\"x\"}\nbody\n:::\n:::";
        let err =
            add_inline_comment_result(&phase2d_mock_api(&server), "12345", bad_jfm, "anchor", None)
                .await
                .unwrap_err();
        assert!(err.to_string().contains("invalid ADF nesting"));
    }

    // ── list_labels_yaml ───────────────────────────────────────────

    #[tokio::test]
    async fn list_labels_yaml_returns_yaml_sequence() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345/labels"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"id": "1", "name": "architecture", "prefix": "global"},
                        {"id": "2", "name": "draft", "prefix": "global"}
                    ]
                })),
            )
            .mount(&server)
            .await;

        let yaml = list_labels_yaml(&phase2d_mock_api(&server), "12345", 0)
            .await
            .unwrap();
        assert!(yaml.contains("architecture"));
        assert!(yaml.contains("draft"));
    }

    #[tokio::test]
    async fn list_labels_yaml_respects_limit() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345/labels"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"id": "1", "name": "architecture", "prefix": "global"},
                        {"id": "2", "name": "draft", "prefix": "global"}
                    ]
                })),
            )
            .mount(&server)
            .await;

        let yaml = list_labels_yaml(&phase2d_mock_api(&server), "12345", 1)
            .await
            .unwrap();
        assert!(yaml.contains("architecture"));
        assert!(!yaml.contains("draft"));
    }

    #[tokio::test]
    async fn list_labels_yaml_api_error_propagates() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/404/labels"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;

        let err = list_labels_yaml(&phase2d_mock_api(&server), "404", 0)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    // ── add_labels_result ──────────────────────────────────────────

    #[tokio::test]
    async fn add_labels_result_posts_and_returns_confirmation() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/label",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{"prefix": "global", "name": "arch", "id": "1"}]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let yaml = add_labels_result(&phase2d_mock_api(&server), "12345", &["arch".to_string()])
            .await
            .unwrap();
        assert!(yaml.contains("ok: true"));
        assert!(yaml.contains("arch"));
        assert!(yaml.contains("Added 1 label"));
    }

    #[tokio::test]
    async fn add_labels_result_rejects_empty_labels() {
        let server = wiremock::MockServer::start().await;
        let err = add_labels_result(&phase2d_mock_api(&server), "12345", &[])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("at least one label"));
    }

    #[tokio::test]
    async fn add_labels_result_api_error_propagates() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/label",
            ))
            .respond_with(wiremock::ResponseTemplate::new(400).set_body_string("Bad Request"))
            .mount(&server)
            .await;

        let err = add_labels_result(&phase2d_mock_api(&server), "12345", &["x".to_string()])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("400"));
    }

    // ── remove_labels_result ───────────────────────────────────────

    #[tokio::test]
    async fn remove_labels_result_requires_confirm_true() {
        let server = wiremock::MockServer::start().await;
        let err = remove_labels_result(
            &phase2d_mock_api(&server),
            "12345",
            &["draft".to_string()],
            false,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("confirm: true"));
    }

    #[tokio::test]
    async fn remove_labels_result_deletes_each_label_with_confirm() {
        let server = wiremock::MockServer::start().await;
        for label in &["draft", "old"] {
            wiremock::Mock::given(wiremock::matchers::method("DELETE"))
                .and(wiremock::matchers::path(format!(
                    "/wiki/rest/api/content/12345/label/{label}"
                )))
                .respond_with(wiremock::ResponseTemplate::new(204))
                .expect(1)
                .mount(&server)
                .await;
        }

        let yaml = remove_labels_result(
            &phase2d_mock_api(&server),
            "12345",
            &["draft".to_string(), "old".to_string()],
            true,
        )
        .await
        .unwrap();
        assert!(yaml.contains("ok: true"));
        assert!(yaml.contains("Removed 2 label"));
    }

    #[tokio::test]
    async fn remove_labels_result_rejects_empty_labels() {
        let server = wiremock::MockServer::start().await;
        let err = remove_labels_result(&phase2d_mock_api(&server), "12345", &[], true)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("at least one label"));
    }

    #[tokio::test]
    async fn remove_labels_result_api_error_propagates() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/label/draft",
            ))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;

        let err = remove_labels_result(
            &phase2d_mock_api(&server),
            "12345",
            &["draft".to_string()],
            true,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    // ── search_users_yaml ──────────────────────────────────────────

    #[tokio::test]
    async fn search_users_yaml_returns_users() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/rest/api/search/user"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"user": {"accountId": "abc", "displayName": "Alice", "email": "a@x.com"}}
                    ]
                })),
            )
            .mount(&server)
            .await;

        let yaml = search_users_yaml(&phase2d_mock_client(&server.uri()), "alice", 25)
            .await
            .unwrap();
        assert!(yaml.contains("Alice"));
        assert!(yaml.contains("abc"));
    }

    #[tokio::test]
    async fn search_users_yaml_empty_results() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/rest/api/search/user"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .mount(&server)
            .await;

        let yaml = search_users_yaml(&phase2d_mock_client(&server.uri()), "nobody", 10)
            .await
            .unwrap();
        assert!(yaml.contains("total: 0"));
    }

    #[tokio::test]
    async fn search_users_yaml_api_error_propagates() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/rest/api/search/user"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;

        let err = search_users_yaml(&phase2d_mock_client(&server.uri()), "alice", 25)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    // ── confluence_compare / confluence_compare_section ────────────

    async fn mount_compare_endpoints(server: &wiremock::MockServer) {
        // Versions list (newest-first).
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12/versions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"number": 2, "createdAt": "2026-05-08T10:00:00Z", "authorId": "alice", "message": "v2", "minorEdit": false},
                        {"number": 1, "createdAt": "2026-05-07T10:00:00Z", "authorId": "bob", "message": "v1", "minorEdit": false},
                    ]
                })),
            )
            .mount(server)
            .await;

        // Page at version 1.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12"))
            .and(wiremock::matchers::query_param("version", "1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "12",
                "title": "Page v1",
                "status": "current",
                "spaceId": "98",
                "version": {"number": 1},
                "body": {"atlas_doc_format": {"value": r#"{"version":1,"type":"doc","content":[{"type":"heading","attrs":{"level":2},"content":[{"type":"text","text":"Background"}]},{"type":"paragraph","content":[{"type":"text","text":"version 12"}]}]}"#}}
            })))
            .mount(server)
            .await;

        // Page at version 2.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12"))
            .and(wiremock::matchers::query_param("version", "2"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "12",
                "title": "Page v2",
                "status": "current",
                "spaceId": "98",
                "version": {"number": 2},
                "body": {"atlas_doc_format": {"value": r#"{"version":1,"type":"doc","content":[{"type":"heading","attrs":{"level":2},"content":[{"type":"text","text":"Background"}]},{"type":"paragraph","content":[{"type":"text","text":"version 14"}]}]}"#}}
            })))
            .mount(server)
            .await;

        // Space lookup.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"key": "ENG"})),
            )
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn fetch_compare_yaml_returns_yaml() {
        let server = wiremock::MockServer::start().await;
        mount_compare_endpoints(&server).await;
        let client = AtlassianClient::new(&server.uri(), "u@t.com", "tok").unwrap();
        let api = ConfluenceApi::new(client);
        let params = ConfluenceCompareParams {
            id: "12".to_string(),
            from: None,
            to: None,
            detail: None,
            include: None,
            ignore_whitespace: None,
            min_change_chars: None,
            filter_sections: None,
            budget: None,
        };
        let yaml = fetch_compare_yaml(&api, &server.uri(), &params)
            .await
            .unwrap();
        assert!(yaml.contains("page:"));
        assert!(yaml.contains("/h2#background"));
        assert!(yaml.contains("number: 1"));
        assert!(yaml.contains("number: 2"));
    }

    #[tokio::test]
    async fn fetch_compare_yaml_invalid_detail_errors() {
        let server = wiremock::MockServer::start().await;
        let client = AtlassianClient::new(&server.uri(), "u@t.com", "tok").unwrap();
        let api = ConfluenceApi::new(client);
        let params = ConfluenceCompareParams {
            id: "12".to_string(),
            from: None,
            to: None,
            detail: Some("garbage".to_string()),
            include: None,
            ignore_whitespace: None,
            min_change_chars: None,
            filter_sections: None,
            budget: None,
        };
        let err = fetch_compare_yaml(&api, &server.uri(), &params)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Invalid detail"));
    }

    #[tokio::test]
    async fn fetch_compare_section_text_invalid_format_errors() {
        let server = wiremock::MockServer::start().await;
        let client = AtlassianClient::new(&server.uri(), "u@t.com", "tok").unwrap();
        let api = ConfluenceApi::new(client);
        // We need a valid cursor first to get past the decode step.
        let cursor = crate::atlassian::diff_format::Cursor {
            page_id: "12".to_string(),
            from_v: 1,
            to_v: 2,
            section_path: "/h2#background".to_string(),
        }
        .encode()
        .unwrap();
        let err = fetch_compare_section_text(&api, &cursor, Some("nonsense"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Invalid format"));
    }

    #[tokio::test]
    async fn fetch_compare_section_text_invalid_cursor_errors() {
        let server = wiremock::MockServer::start().await;
        let client = AtlassianClient::new(&server.uri(), "u@t.com", "tok").unwrap();
        let api = ConfluenceApi::new(client);
        let err = fetch_compare_section_text(&api, "!!!", None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Invalid cursor"));
    }

    #[tokio::test]
    async fn fetch_compare_section_text_returns_unified() {
        let server = wiremock::MockServer::start().await;
        mount_compare_endpoints(&server).await;
        let client = AtlassianClient::new(&server.uri(), "u@t.com", "tok").unwrap();
        let api = ConfluenceApi::new(client);
        let cursor = crate::atlassian::diff_format::Cursor {
            page_id: "12".to_string(),
            from_v: 1,
            to_v: 2,
            section_path: "/h2#background".to_string(),
        }
        .encode()
        .unwrap();
        let text = fetch_compare_section_text(&api, &cursor, Some("unified"))
            .await
            .unwrap();
        assert!(text.contains("/h2#background"));
        assert!(text.contains("version 12"));
        assert!(text.contains("version 14"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confluence_compare_handler_success_via_mock() {
        let _lock = env_lock();
        let server = wiremock::MockServer::start().await;
        mount_compare_endpoints(&server).await;
        let _env = EnvGuard::set(&server.uri());

        let result = make_server()
            .confluence_compare(Parameters(ConfluenceCompareParams {
                id: "12".to_string(),
                from: None,
                to: None,
                detail: None,
                include: None,
                ignore_whitespace: None,
                min_change_chars: None,
                filter_sections: None,
                budget: None,
            }))
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confluence_compare_section_handler_success_via_mock() {
        let _lock = env_lock();
        let server = wiremock::MockServer::start().await;
        mount_compare_endpoints(&server).await;
        let _env = EnvGuard::set(&server.uri());

        let cursor = crate::atlassian::diff_format::Cursor {
            page_id: "12".to_string(),
            from_v: 1,
            to_v: 2,
            section_path: "/h2#background".to_string(),
        }
        .encode()
        .unwrap();
        let result = make_server()
            .confluence_compare_section(Parameters(ConfluenceCompareSectionParams {
                cursor,
                format: None,
            }))
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confluence_compare_handler_no_credentials_returns_tool_error() {
        let _lock = env_lock();
        clear_env();
        let result = make_server()
            .confluence_compare(Parameters(ConfluenceCompareParams {
                id: "12".to_string(),
                from: None,
                to: None,
                detail: None,
                include: None,
                ignore_whitespace: None,
                min_change_chars: None,
                filter_sections: None,
                budget: None,
            }))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confluence_compare_section_handler_no_credentials_returns_tool_error() {
        let _lock = env_lock();
        clear_env();
        let result = make_server()
            .confluence_compare_section(Parameters(ConfluenceCompareSectionParams {
                cursor: "irrelevant".to_string(),
                format: None,
            }))
            .await;
        assert!(result.is_err());
    }
}
