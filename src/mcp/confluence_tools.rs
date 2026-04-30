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
use crate::atlassian::api::{AtlassianApi, ContentItem};
use crate::atlassian::client::AtlassianClient;
use crate::atlassian::confluence_api::{ChildPage, ConfluenceApi};
use crate::atlassian::convert::markdown_to_adf;
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
    /// Target Confluence space key (e.g., `"ENG"`).
    pub space_key: String,
    /// Page title.
    pub title: String,
    /// Page body. Parsed according to `format`.
    pub content: String,
    /// Optional parent page ID for nesting under an existing page.
    #[serde(default)]
    pub parent_id: Option<String>,
    /// Format of `content`: `"jfm"` (default markdown) or `"adf"` (raw ADF JSON).
    #[serde(default)]
    pub format: Option<String>,
}

/// Parameters for the `confluence_write` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfluenceWriteParams {
    /// Confluence page ID.
    pub id: String,
    /// New page body.
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

/// Parameters for the `confluence_comment_list` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfluenceCommentListParams {
    /// Confluence page ID.
    pub id: String,
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
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfluenceLabelRemoveParams {
    /// Confluence page ID.
    pub id: String,
    /// Labels to remove from the page.
    pub labels: Vec<String>,
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
fn parse_write_content(content: &str, format: ContentFormat) -> Result<(AdfDocument, String)> {
    match format {
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
                Ok((adf, title))
            } else {
                let adf = markdown_to_adf(content)?;
                Ok((adf, String::new()))
            }
        }
        ContentFormat::Adf => {
            let adf = AdfDocument::from_json_str(content)?;
            Ok((adf, String::new()))
        }
    }
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

/// Builds the YAML output for the `confluence_comment_list` tool.
///
/// `limit` of 0 returns every comment; otherwise the list is truncated to
/// the requested size (matching the CLI `--limit` semantics).
pub async fn list_comments_yaml(api: &ConfluenceApi, id: &str, limit: usize) -> Result<String> {
    let mut comments = api.get_page_comments(id).await?;
    if limit > 0 {
        comments.truncate(limit);
    }
    to_yaml(&comments)
}

/// Posts a comment to a Confluence page.
///
/// The markdown `content` is converted to ADF before posting.
pub async fn add_comment_result(api: &ConfluenceApi, id: &str, content: &str) -> Result<String> {
    let adf: AdfDocument = markdown_to_adf(content).context("Failed to convert markdown to ADF")?;
    api.add_page_comment(id, &adf).await?;

    let result = MutationResult {
        ok: true,
        message: format!("Comment added to page {id}."),
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
) -> Result<String> {
    if labels.is_empty() {
        anyhow::bail!("`labels` must contain at least one label");
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
        description = "Create a new Confluence page. Returns the new page's ID. \
                       Mirrors `omni-dev atlassian confluence create`."
    )]
    pub async fn confluence_create(
        &self,
        Parameters(params): Parameters<ConfluenceCreateParams>,
    ) -> Result<CallToolResult, McpError> {
        let format = parse_format(params.format.as_deref()).map_err(tool_error)?;
        let page_id = run_confluence_create(&params, format)
            .await
            .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(page_id)]))
    }

    /// Tool: update a Confluence page's body (and optionally title).
    #[tool(description = "Overwrite a Confluence page's body. \
                       Accepts JFM markdown (default) or ADF JSON. \
                       Mirrors `omni-dev atlassian confluence write --force`.")]
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

    /// Tool: recursively download a Confluence page tree.
    #[tool(
        description = "Recursively download a Confluence page or an entire space into a directory. \
                       Either `id` (root page) or `space` (space key) must be provided. \
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

    /// Lists footer comments on a Confluence page.
    #[tool(
        description = "List footer comments on a Confluence page (auto-paginated). \
                       `limit` of 0 returns every comment. Mirrors \
                       `omni-dev atlassian confluence comment list`."
    )]
    pub async fn confluence_comment_list(
        &self,
        Parameters(params): Parameters<ConfluenceCommentListParams>,
    ) -> Result<CallToolResult, McpError> {
        let (client, _url) = create_client().map_err(tool_error)?;
        let api = ConfluenceApi::new(client);
        let yaml = list_comments_yaml(&api, &params.id, params.limit.unwrap_or(25))
            .await
            .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Posts a comment to a Confluence page.
    #[tool(
        description = "Post a markdown comment to a Confluence page. The content is \
                       converted to ADF before posting. Mirrors \
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
        description = "Remove one or more labels from a Confluence page. Mirrors \
                       `omni-dev atlassian confluence label remove`."
    )]
    pub async fn confluence_label_remove(
        &self,
        Parameters(params): Parameters<ConfluenceLabelRemoveParams>,
    ) -> Result<CallToolResult, McpError> {
        let (client, _url) = create_client().map_err(tool_error)?;
        let api = ConfluenceApi::new(client);
        let yaml = remove_labels_result(&api, &params.id, &params.labels)
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

async fn run_confluence_create(
    params: &ConfluenceCreateParams,
    format: ContentFormat,
) -> Result<String> {
    let adf = match format {
        ContentFormat::Jfm => markdown_to_adf(&params.content)?,
        ContentFormat::Adf => AdfDocument::from_json_str(&params.content)?,
    };

    let (client, _instance_url) = create_client()?;
    let api = ConfluenceApi::new(client);
    let id = api
        .create_page(
            &params.space_key,
            &params.title,
            &adf,
            params.parent_id.as_deref(),
        )
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
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock()
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
            space_key: "ENG".to_string(),
            title: "New Page".to_string(),
            content: "Body".to_string(),
            parent_id: None,
            format: None,
        };
        let id = run_confluence_create(&params, ContentFormat::Jfm)
            .await
            .unwrap();
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
            space_key: "ENG".to_string(),
            title: "ADF Page".to_string(),
            content: r#"{"version":1,"type":"doc","content":[]}"#.to_string(),
            parent_id: Some("11111".to_string()),
            format: Some("adf".to_string()),
        };
        let id = run_confluence_create(&params, ContentFormat::Adf)
            .await
            .unwrap();
        assert_eq!(id, "999");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_confluence_create_invalid_adf_errors() {
        let _lock = env_lock();
        let server = wiremock::MockServer::start().await;
        let _env = EnvGuard::set(&server.uri());

        let params = ConfluenceCreateParams {
            space_key: "ENG".to_string(),
            title: "Bad".to_string(),
            content: "not json".to_string(),
            parent_id: None,
            format: Some("adf".to_string()),
        };
        assert!(run_confluence_create(&params, ContentFormat::Adf)
            .await
            .is_err());
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
    async fn confluence_create_handler_invalid_format_returns_tool_error() {
        let server = make_server();
        let result = server
            .confluence_create(Parameters(ConfluenceCreateParams {
                space_key: "ENG".to_string(),
                title: "T".to_string(),
                content: "body".to_string(),
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
                space_key: "ENG".to_string(),
                title: "T".to_string(),
                content: "body".to_string(),
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
                space_key: "ENG".to_string(),
                title: "T".to_string(),
                content: "Body".to_string(),
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
            "confluence_download",
            "confluence_children",
            "confluence_comment_list",
            "confluence_comment_add",
            "confluence_label_list",
            "confluence_label_add",
            "confluence_label_remove",
            "confluence_user_search",
        ] {
            assert!(router.has_route(name), "missing tool: {name}");
        }
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

        let yaml = list_comments_yaml(&phase2d_mock_api(&server), "12345", 25)
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

        let yaml = list_comments_yaml(&phase2d_mock_api(&server), "12345", 0)
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

        let yaml = list_comments_yaml(&phase2d_mock_api(&server), "12345", 1)
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

        let err = list_comments_yaml(&phase2d_mock_api(&server), "99", 25)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
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
    async fn remove_labels_result_deletes_each_label() {
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
        )
        .await
        .unwrap();
        assert!(yaml.contains("ok: true"));
        assert!(yaml.contains("Removed 2 label"));
    }

    #[tokio::test]
    async fn remove_labels_result_rejects_empty_labels() {
        let server = wiremock::MockServer::start().await;
        let err = remove_labels_result(&phase2d_mock_api(&server), "12345", &[])
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

        let err = remove_labels_result(&phase2d_mock_api(&server), "12345", &["draft".to_string()])
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
}
