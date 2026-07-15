//! MCP tool handlers for JIRA operations (extensions: sprints, watchers,
//! worklogs, fields, boards, attachments, projects, changelogs, delete).
//!
//! Each tool is a thin handler that constructs an [`AtlassianClient`] via
//! [`create_client`] and delegates to a `*_yaml` helper. The helpers do the
//! actual work — call the client method, serialise the result as YAML — and
//! are the unit-tested surface (via `wiremock`).
//!
//! The split exists because [`create_client`] reads credentials from the
//! filesystem; the `*_yaml` helpers accept an [`AtlassianClient`] directly so
//! tests can wire one to a [`wiremock::MockServer`].

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock as Content},
    schemars, tool, tool_router, ErrorData as McpError,
};
use serde::{Deserialize, Serialize};

use super::catalogue_cache::CatalogueCache;
use super::dry_run::dry_run_request_yaml;
use super::error::tool_error;
use super::server::OmniDevServer;
use crate::atlassian::client::AtlassianClient;
use crate::atlassian::jira_types::{AgileBoard, JiraAttachment, JiraProject};
use crate::cli::atlassian::helpers::create_client;
use crate::utils::path::attachment_filename;

// ─────────────────────────────────────────────────────────────────────────
// Attachment tools
// ─────────────────────────────────────────────────────────────────────────

/// Image MIME types recognised by [`attachment_images_yaml`].
const IMAGE_MIME_TYPES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/svg+xml",
    "image/webp",
];

/// Result of downloading one attachment to disk.
#[derive(Debug, Clone, Serialize)]
struct DownloadedAttachment {
    /// JIRA attachment ID.
    id: String,
    /// Original filename.
    filename: String,
    /// MIME type (e.g., `image/png`).
    mime_type: String,
    /// File size in bytes.
    size: u64,
    /// Absolute path on disk where the attachment was written.
    path: String,
}

/// Result returned by [`attachment_download_yaml`] / [`attachment_images_yaml`].
#[derive(Debug, Clone, Serialize)]
struct DownloadResult {
    /// Output directory (absolute path) where files were written.
    output_dir: String,
    /// Files downloaded.
    files: Vec<DownloadedAttachment>,
}

/// Parameters for the `jira_attachment_download` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AttachmentDownloadParams {
    /// JIRA issue key (e.g., `PROJ-123`).
    pub key: String,
    /// Output directory. Defaults to a fresh temp directory whose path is
    /// returned in the result so the assistant can read the files via its
    /// filesystem tool.
    #[serde(default)]
    pub output_dir: Option<String>,
    /// Case-insensitive filename substring filter.
    #[serde(default)]
    pub filter: Option<String>,
}

/// Parameters for the `jira_attachment_images` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AttachmentImagesParams {
    /// JIRA issue key (e.g., `PROJ-123`).
    pub key: String,
    /// Output directory. Defaults to a fresh temp directory.
    #[serde(default)]
    pub output_dir: Option<String>,
}

/// Parameters for the `jira_attachment_upload` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AttachmentUploadParams {
    /// JIRA issue key (e.g., `PROJ-123`).
    pub key: String,
    /// Local filesystem path(s) to the file(s) to upload. Each is streamed
    /// from disk (never fully buffered in memory) and rides a single
    /// multipart request.
    pub file_paths: Vec<String>,
}

/// Parameters for the `jira_attachment_delete` tool.
///
/// `confirm` must be `true` for the deletion to proceed. This is the
/// MCP-side guard for an irreversible operation; the assistant must
/// explicitly opt in.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AttachmentDeleteParams {
    /// Attachment ID to delete.
    pub attachment_id: String,
    /// Must be set to `true` — destructive guard.
    pub confirm: bool,
}

/// Result returned by [`attachment_upload_yaml`].
#[derive(Debug, Serialize)]
struct UploadResult {
    /// Issue the files were attached to.
    key: String,
    /// Metadata for each created attachment.
    uploaded: Vec<JiraAttachment>,
}

/// Downloads matching attachments to disk and returns YAML metadata
/// describing each downloaded file (including its on-disk path).
pub(crate) async fn attachment_download_yaml(
    client: &AtlassianClient,
    key: &str,
    output_dir: &Path,
    filter: Option<&str>,
) -> Result<String> {
    let attachments = client.get_attachments(key).await?;
    let selected = match filter {
        Some(pattern) => {
            let needle = pattern.to_lowercase();
            attachments
                .into_iter()
                .filter(|a| a.filename.to_lowercase().contains(&needle))
                .collect::<Vec<_>>()
        }
        None => attachments,
    };
    download_to_yaml(client, output_dir, &selected).await
}

/// Downloads only image attachments and returns YAML metadata.
pub(crate) async fn attachment_images_yaml(
    client: &AtlassianClient,
    key: &str,
    output_dir: &Path,
) -> Result<String> {
    let attachments = client.get_attachments(key).await?;
    let selected: Vec<JiraAttachment> = attachments
        .into_iter()
        .filter(|a| IMAGE_MIME_TYPES.contains(&a.mime_type.as_str()))
        .collect();
    download_to_yaml(client, output_dir, &selected).await
}

async fn download_to_yaml(
    client: &AtlassianClient,
    output_dir: &Path,
    attachments: &[JiraAttachment],
) -> Result<String> {
    fs::create_dir_all(output_dir).with_context(|| {
        format!(
            "Failed to create output directory: {}",
            output_dir.display()
        )
    })?;
    let mut files = Vec::with_capacity(attachments.len());
    for a in attachments {
        let bytes = client.get_bytes(&a.content_url).await?;
        let path = output_dir.join(attachment_filename(&a.filename, &a.id));
        fs::write(&path, &bytes).with_context(|| format!("Failed to write {}", path.display()))?;
        files.push(DownloadedAttachment {
            id: a.id.clone(),
            filename: a.filename.clone(),
            mime_type: a.mime_type.clone(),
            size: a.size,
            path: path.to_string_lossy().into_owned(),
        });
    }
    let result = DownloadResult {
        output_dir: output_dir.to_string_lossy().into_owned(),
        files,
    };
    serde_yaml::to_string(&result).context("Failed to serialize download result as YAML")
}

/// Resolves [`AttachmentDownloadParams::output_dir`] / [`AttachmentImagesParams::output_dir`]
/// to a usable [`PathBuf`], creating a fresh tempdir when the caller did not
/// supply a path.
fn resolve_output_dir(requested: Option<&str>) -> Result<PathBuf> {
    if let Some(dir) = requested {
        Ok(PathBuf::from(dir))
    } else {
        let tmp = tempfile::Builder::new()
            .prefix("omni-dev-jira-attachment-")
            .tempdir()
            .context("Failed to create temp dir for attachment download")?;
        Ok(tmp.keep())
    }
}

/// Uploads the given files to an issue and returns YAML metadata for each
/// created attachment.
pub(crate) async fn attachment_upload_yaml(
    client: &AtlassianClient,
    key: &str,
    file_paths: &[String],
) -> Result<String> {
    let files: Vec<PathBuf> = file_paths.iter().map(PathBuf::from).collect();
    let uploaded = client.upload_attachments(key, &files).await?;
    let result = UploadResult {
        key: key.to_string(),
        uploaded,
    };
    serde_yaml::to_string(&result).context("Failed to serialize upload result as YAML")
}

/// Deletes an attachment by ID (guarded by `confirm`) and returns YAML
/// `{status: ok}` on success.
pub(crate) async fn attachment_delete_yaml(
    client: &AtlassianClient,
    attachment_id: &str,
    confirm: bool,
) -> Result<String> {
    if !confirm {
        return Err(anyhow!(
            "Refusing to delete attachment {attachment_id}: pass `confirm: true` to authorise this irreversible operation."
        ));
    }
    client.delete_attachment(attachment_id).await?;
    serde_yaml::to_string(&STATUS_OK).context("Failed to serialize status as YAML")
}

// ─────────────────────────────────────────────────────────────────────────
// Board tools
// ─────────────────────────────────────────────────────────────────────────

/// Parameters for the `jira_board_list` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BoardListParams {
    /// Filter by project key.
    #[serde(default)]
    pub project: Option<String>,
    /// Filter by board type (e.g., `scrum`, `kanban`).
    #[serde(default)]
    pub board_type: Option<String>,
    /// Maximum number of boards to return. `0` means unlimited (default 50).
    #[serde(default = "default_limit_50")]
    pub limit: u32,
}

/// Parameters for the `jira_board_issues` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BoardIssuesParams {
    /// Board ID.
    pub board_id: u64,
    /// Optional JQL to further filter issues.
    #[serde(default)]
    pub jql: Option<String>,
    /// Maximum number of issues to return. `0` means unlimited (default 50).
    #[serde(default = "default_limit_50")]
    pub limit: u32,
}

pub(crate) async fn board_list_yaml(
    client: &AtlassianClient,
    cache: &CatalogueCache,
    project: Option<&str>,
    board_type: Option<&str>,
    limit: u32,
) -> Result<String> {
    let cached = cache.boards(client).await?;
    let effective_limit = if limit == 0 {
        usize::MAX
    } else {
        limit as usize
    };
    let boards: Vec<AgileBoard> = cached
        .boards
        .iter()
        .filter(|b| project.map_or(true, |p| b.project_key.as_deref() == Some(p)))
        .filter(|b| board_type.map_or(true, |t| b.board_type == t))
        .take(effective_limit)
        .cloned()
        .collect();
    let total = boards.len() as u32;
    let view = crate::atlassian::jira_types::AgileBoardList { boards, total };
    serde_yaml::to_string(&view).context("Failed to serialize boards as YAML")
}

pub(crate) async fn board_issues_yaml(
    client: &AtlassianClient,
    board_id: u64,
    jql: Option<&str>,
    limit: u32,
) -> Result<String> {
    let result = client.get_board_issues(board_id, jql, limit).await?;
    serde_yaml::to_string(&result).context("Failed to serialize board issues as YAML")
}

// ─────────────────────────────────────────────────────────────────────────
// Sprint tools
// ─────────────────────────────────────────────────────────────────────────

/// Parameters for the `jira_sprint_list` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SprintListParams {
    /// Board ID.
    pub board_id: u64,
    /// Filter by state (`active`, `future`, `closed`).
    #[serde(default)]
    pub state: Option<String>,
    /// Maximum number of sprints to return. `0` means unlimited (default 50).
    #[serde(default = "default_limit_50")]
    pub limit: u32,
}

/// Parameters for the `jira_sprint_issues` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SprintIssuesParams {
    /// Sprint ID.
    pub sprint_id: u64,
    /// Optional JQL to further filter issues.
    #[serde(default)]
    pub jql: Option<String>,
    /// Maximum number of issues to return. `0` means unlimited (default 50).
    #[serde(default = "default_limit_50")]
    pub limit: u32,
}

/// Parameters for the `jira_sprint_add` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SprintAddParams {
    /// Sprint ID.
    pub sprint_id: u64,
    /// Issue keys to add to the sprint.
    pub issue_keys: Vec<String>,
}

/// Parameters for the `jira_sprint_create` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SprintCreateParams {
    /// Origin board ID.
    pub board_id: u64,
    /// Sprint name.
    pub name: String,
    /// ISO 8601 start date.
    #[serde(default)]
    pub start_date: Option<String>,
    /// ISO 8601 end date.
    #[serde(default)]
    pub end_date: Option<String>,
    /// Sprint goal.
    #[serde(default)]
    pub goal: Option<String>,
}

/// Parameters for the `jira_sprint_update` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SprintUpdateParams {
    /// Sprint ID.
    pub sprint_id: u64,
    /// New sprint name.
    #[serde(default)]
    pub name: Option<String>,
    /// New sprint state (`future`, `active`, `closed`).
    #[serde(default)]
    pub state: Option<String>,
    /// New start date.
    #[serde(default)]
    pub start_date: Option<String>,
    /// New end date.
    #[serde(default)]
    pub end_date: Option<String>,
    /// New goal.
    #[serde(default)]
    pub goal: Option<String>,
}

/// Status payload returned by mutating sprint tools that don't have a
/// natural body (like add-to-sprint).
#[derive(Debug, Serialize)]
struct StatusOk {
    status: &'static str,
}

const STATUS_OK: StatusOk = StatusOk { status: "ok" };

pub(crate) async fn sprint_list_yaml(
    client: &AtlassianClient,
    board_id: u64,
    state: Option<&str>,
    limit: u32,
) -> Result<String> {
    let result = client.get_sprints(board_id, state, limit).await?;
    serde_yaml::to_string(&result).context("Failed to serialize sprints as YAML")
}

pub(crate) async fn sprint_issues_yaml(
    client: &AtlassianClient,
    sprint_id: u64,
    jql: Option<&str>,
    limit: u32,
) -> Result<String> {
    let result = client.get_sprint_issues(sprint_id, jql, limit).await?;
    serde_yaml::to_string(&result).context("Failed to serialize sprint issues as YAML")
}

pub(crate) async fn sprint_add_yaml(
    client: &AtlassianClient,
    sprint_id: u64,
    issue_keys: &[String],
) -> Result<String> {
    let refs: Vec<&str> = issue_keys.iter().map(String::as_str).collect();
    client.add_issues_to_sprint(sprint_id, &refs).await?;
    serde_yaml::to_string(&STATUS_OK).context("Failed to serialize status as YAML")
}

pub(crate) async fn sprint_create_yaml(
    client: &AtlassianClient,
    board_id: u64,
    name: &str,
    start_date: Option<&str>,
    end_date: Option<&str>,
    goal: Option<&str>,
) -> Result<String> {
    let sprint = client
        .create_sprint(board_id, name, start_date, end_date, goal)
        .await?;
    serde_yaml::to_string(&sprint).context("Failed to serialize sprint as YAML")
}

pub(crate) async fn sprint_update_yaml(
    client: &AtlassianClient,
    sprint_id: u64,
    name: Option<&str>,
    state: Option<&str>,
    start_date: Option<&str>,
    end_date: Option<&str>,
    goal: Option<&str>,
) -> Result<String> {
    client
        .update_sprint(sprint_id, name, state, start_date, end_date, goal)
        .await?;
    serde_yaml::to_string(&STATUS_OK).context("Failed to serialize status as YAML")
}

/// Parameters for the `jira_sprint_delete` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SprintDeleteParams {
    /// Sprint ID.
    pub sprint_id: u64,
    /// Must be `true` to authorise the irreversible delete; the tool refuses
    /// (without calling the API) when `false`.
    #[serde(default)]
    pub confirm: bool,
}

pub(crate) async fn sprint_delete_yaml(
    client: &AtlassianClient,
    sprint_id: u64,
    confirm: bool,
) -> Result<String> {
    if !confirm {
        return Err(anyhow!(
            "Refusing to delete sprint {sprint_id}: pass `confirm: true` to authorise this irreversible operation."
        ));
    }
    client.delete_sprint(sprint_id).await?;
    serde_yaml::to_string(&STATUS_OK).context("Failed to serialize status as YAML")
}

// ─────────────────────────────────────────────────────────────────────────
// Project version tools
// ─────────────────────────────────────────────────────────────────────────

/// Parameters for the `jira_version_list` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct VersionListParams {
    /// Project key (e.g., `PROJ`).
    pub project: String,
    /// Filter to only released (`true`) or only unreleased (`false`) versions.
    /// Omit for both.
    #[serde(default)]
    pub released: Option<bool>,
    /// Filter to only archived (`true`) or only non-archived (`false`)
    /// versions. Omit for both.
    #[serde(default)]
    pub archived: Option<bool>,
}

/// Parameters for the `jira_version_create` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct VersionCreateParams {
    /// Project key (e.g., `PROJ`).
    pub project: String,
    /// Version name (e.g., `1.0.0`).
    pub name: String,
    /// Version description.
    #[serde(default)]
    pub description: Option<String>,
    /// Release date (ISO 8601, `YYYY-MM-DD`). Validated client-side.
    #[serde(default)]
    pub release_date: Option<String>,
    /// Start date (ISO 8601, `YYYY-MM-DD`). Validated client-side.
    #[serde(default)]
    pub start_date: Option<String>,
    /// Whether the version is released. Defaults to `false`.
    #[serde(default)]
    pub released: bool,
    /// Whether the version is archived. Defaults to `false`.
    #[serde(default)]
    pub archived: bool,
}

pub(crate) async fn version_list_yaml(
    client: &AtlassianClient,
    project: &str,
    released: Option<bool>,
    archived: Option<bool>,
) -> Result<String> {
    let result = client
        .get_project_versions(project, released, archived)
        .await?;
    serde_yaml::to_string(&result).context("Failed to serialize project versions as YAML")
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn version_create_yaml(
    client: &AtlassianClient,
    project: &str,
    name: &str,
    description: Option<&str>,
    release_date: Option<&str>,
    start_date: Option<&str>,
    released: bool,
    archived: bool,
) -> Result<String> {
    let version = client
        .create_project_version(
            project,
            name,
            description,
            release_date,
            start_date,
            released,
            archived,
        )
        .await?;
    serde_yaml::to_string(&version).context("Failed to serialize project version as YAML")
}

/// Parameters for the `jira_version_release` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct VersionReleaseParams {
    /// Version ID (from `jira_version_list`).
    pub version_id: String,
    /// Release date (ISO 8601, `YYYY-MM-DD`). Validated client-side.
    #[serde(default)]
    pub release_date: Option<String>,
}

/// Parameters for the `jira_version_archive` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct VersionArchiveParams {
    /// Version ID (from `jira_version_list`).
    pub version_id: String,
}

/// Parameters for the `jira_version_rename` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct VersionRenameParams {
    /// Version ID (from `jira_version_list`).
    pub version_id: String,
    /// New version name.
    pub name: String,
    /// New description (optional).
    #[serde(default)]
    pub description: Option<String>,
}

/// Parameters for the `jira_version_delete` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct VersionDeleteParams {
    /// Version ID (from `jira_version_list`).
    pub version_id: String,
    /// Reassign the `fixVersion` of affected issues to this version id before
    /// deleting (otherwise the references are dropped).
    #[serde(default)]
    pub move_fix_issues_to: Option<String>,
    /// Reassign the `affectedVersion` of affected issues to this version id.
    #[serde(default)]
    pub move_affected_issues_to: Option<String>,
    /// Must be `true` to authorise the irreversible delete; the tool refuses
    /// (without calling the API) when `false`.
    #[serde(default)]
    pub confirm: bool,
}

pub(crate) async fn version_release_yaml(
    client: &AtlassianClient,
    version_id: &str,
    release_date: Option<&str>,
) -> Result<String> {
    client
        .update_project_version(version_id, None, None, Some(true), release_date, None, None)
        .await?;
    serde_yaml::to_string(&STATUS_OK).context("Failed to serialize status as YAML")
}

pub(crate) async fn version_archive_yaml(
    client: &AtlassianClient,
    version_id: &str,
) -> Result<String> {
    client
        .update_project_version(version_id, None, None, None, None, Some(true), None)
        .await?;
    serde_yaml::to_string(&STATUS_OK).context("Failed to serialize status as YAML")
}

pub(crate) async fn version_rename_yaml(
    client: &AtlassianClient,
    version_id: &str,
    name: &str,
    description: Option<&str>,
) -> Result<String> {
    client
        .update_project_version(version_id, Some(name), description, None, None, None, None)
        .await?;
    serde_yaml::to_string(&STATUS_OK).context("Failed to serialize status as YAML")
}

pub(crate) async fn version_delete_yaml(
    client: &AtlassianClient,
    version_id: &str,
    move_fix_issues_to: Option<&str>,
    move_affected_issues_to: Option<&str>,
    confirm: bool,
) -> Result<String> {
    if !confirm {
        return Err(anyhow!(
            "Refusing to delete version {version_id}: pass `confirm: true` to authorise this irreversible operation."
        ));
    }
    client
        .delete_project_version(version_id, move_fix_issues_to, move_affected_issues_to)
        .await?;
    serde_yaml::to_string(&STATUS_OK).context("Failed to serialize status as YAML")
}

// ─────────────────────────────────────────────────────────────────────────
// Component tools
// ─────────────────────────────────────────────────────────────────────────

/// Parameters for the `jira_component_list` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ComponentListParams {
    /// Project key (e.g., `PROJ`).
    pub project: String,
}

/// Parameters for the `jira_component_create` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ComponentCreateParams {
    /// Project key (e.g., `PROJ`).
    pub project: String,
    /// Component name.
    pub name: String,
    /// Component description.
    #[serde(default)]
    pub description: Option<String>,
}

/// Parameters for the `jira_component_update` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ComponentUpdateParams {
    /// Component ID (from `jira_component_list`).
    pub component_id: String,
    /// New name. Omit to leave unchanged.
    #[serde(default)]
    pub name: Option<String>,
    /// New description. Omit to leave unchanged.
    #[serde(default)]
    pub description: Option<String>,
}

/// Parameters for the `jira_component_delete` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ComponentDeleteParams {
    /// Component ID (from `jira_component_list`).
    pub component_id: String,
    /// Reassign issues referencing this component to this component id before
    /// deleting (otherwise the references are dropped).
    #[serde(default)]
    pub move_issues_to: Option<String>,
    /// Must be `true` to authorise the irreversible delete; the tool refuses
    /// (without calling the API) when `false`.
    #[serde(default)]
    pub confirm: bool,
}

pub(crate) async fn component_list_yaml(client: &AtlassianClient, project: &str) -> Result<String> {
    let components = client.get_project_components(project).await?;
    serde_yaml::to_string(&components).context("Failed to serialize components as YAML")
}

pub(crate) async fn component_create_yaml(
    client: &AtlassianClient,
    project: &str,
    name: &str,
    description: Option<&str>,
) -> Result<String> {
    let component = client.create_component(project, name, description).await?;
    serde_yaml::to_string(&component).context("Failed to serialize component as YAML")
}

pub(crate) async fn component_update_yaml(
    client: &AtlassianClient,
    component_id: &str,
    name: Option<&str>,
    description: Option<&str>,
) -> Result<String> {
    if name.is_none() && description.is_none() {
        return Err(anyhow!(
            "Nothing to update: supply at least one of `name` or `description`."
        ));
    }
    client
        .update_component(component_id, name, description)
        .await?;
    serde_yaml::to_string(&STATUS_OK).context("Failed to serialize status as YAML")
}

pub(crate) async fn component_delete_yaml(
    client: &AtlassianClient,
    component_id: &str,
    move_issues_to: Option<&str>,
    confirm: bool,
) -> Result<String> {
    if !confirm {
        return Err(anyhow!(
            "Refusing to delete component {component_id}: pass `confirm: true` to authorise this irreversible operation."
        ));
    }
    client
        .delete_component(component_id, move_issues_to)
        .await?;
    serde_yaml::to_string(&STATUS_OK).context("Failed to serialize status as YAML")
}

// ─────────────────────────────────────────────────────────────────────────
// Watcher tools
// ─────────────────────────────────────────────────────────────────────────

/// Parameters for the `jira_watcher_list` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WatcherListParams {
    /// JIRA issue key (e.g., `PROJ-123`).
    pub key: String,
}

/// Parameters for the `jira_watcher_add` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WatcherMutateParams {
    /// JIRA issue key (e.g., `PROJ-123`).
    pub key: String,
    /// Atlassian `accountId` of the user (not a display name or email). Use
    /// `jira_user_search` to resolve a name or email to an `accountId`.
    pub account_id: String,
}

/// Parameters for the `jira_watcher_remove` tool.
///
/// `confirm` must be `true` for the removal to proceed. This is the
/// MCP-side guard for a destructive operation; the assistant must
/// explicitly opt in.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WatcherRemoveParams {
    /// JIRA issue key (e.g., `PROJ-123`).
    pub key: String,
    /// Atlassian `accountId` of the user (not a display name or email). Use
    /// `jira_user_search` to resolve a name or email to an `accountId`.
    pub account_id: String,
    /// Must be set to `true` — destructive guard.
    pub confirm: bool,
}

pub(crate) async fn watcher_list_yaml(client: &AtlassianClient, key: &str) -> Result<String> {
    let watchers = client.get_watchers(key).await?;
    serde_yaml::to_string(&watchers).context("Failed to serialize watchers as YAML")
}

pub(crate) async fn watcher_add_yaml(
    client: &AtlassianClient,
    key: &str,
    account_id: &str,
) -> Result<String> {
    client.add_watcher(key, account_id).await?;
    serde_yaml::to_string(&STATUS_OK).context("Failed to serialize status as YAML")
}

pub(crate) async fn watcher_remove_yaml(
    client: &AtlassianClient,
    key: &str,
    account_id: &str,
    confirm: bool,
) -> Result<String> {
    if !confirm {
        return Err(anyhow!(
            "Refusing to remove watcher {account_id} from {key}: pass `confirm: true` to authorise this destructive operation."
        ));
    }
    client.remove_watcher(key, account_id).await?;
    serde_yaml::to_string(&STATUS_OK).context("Failed to serialize status as YAML")
}

// ─────────────────────────────────────────────────────────────────────────
// Link tools
// ─────────────────────────────────────────────────────────────────────────

/// Parameters for the `jira_link_list` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LinkListParams {
    /// JIRA issue key (e.g., `PROJ-123`).
    pub key: String,
}

/// Parameters for the `jira_link_types` tool (none).
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct LinkTypesParams {}

/// Parameters for the `jira_link_remove` tool.
///
/// `confirm` must be `true` for the removal to proceed. This is the
/// MCP-side guard for a destructive operation; the assistant must
/// explicitly opt in.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LinkRemoveParams {
    /// Link ID to remove (returned by `jira_link_list`).
    pub link_id: String,
    /// Must be set to `true` — destructive guard.
    pub confirm: bool,
    /// When true, validate and return the would-be request (method, path)
    /// without removing the link (and without requiring `confirm`). Defaults
    /// to `false`.
    #[serde(default)]
    pub dry_run: bool,
}

pub(crate) async fn link_list_yaml(client: &AtlassianClient, key: &str) -> Result<String> {
    let links = client.get_issue_links(key).await?;
    serde_yaml::to_string(&links).context("Failed to serialize issue links as YAML")
}

pub(crate) async fn link_types_yaml(
    client: &AtlassianClient,
    cache: &CatalogueCache,
) -> Result<String> {
    let types = cache.link_types(client).await?;
    serde_yaml::to_string(&*types).context("Failed to serialize link types as YAML")
}

pub(crate) async fn link_remove_yaml(
    client: &AtlassianClient,
    link_id: &str,
    confirm: bool,
    dry_run: bool,
) -> Result<String> {
    if dry_run {
        // A dry-run only previews the request, so it neither mutates nor
        // requires the destructive `confirm` guard.
        return dry_run_request_yaml("DELETE", format!("/rest/api/3/issueLink/{link_id}"), None);
    }
    if !confirm {
        return Err(anyhow!(
            "Refusing to remove link {link_id}: pass `confirm: true` to authorise this destructive operation."
        ));
    }
    client.remove_issue_link(link_id).await?;
    serde_yaml::to_string(&STATUS_OK).context("Failed to serialize status as YAML")
}

/// Parameters for the `jira_link_remote_list` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LinkRemoteListParams {
    /// JIRA issue key (e.g., `PROJ-123`).
    pub key: String,
}

pub(crate) async fn link_remote_list_yaml(client: &AtlassianClient, key: &str) -> Result<String> {
    let links = client.get_remote_issue_links(key).await?;
    serde_yaml::to_string(&links).context("Failed to serialize remote issue links as YAML")
}

/// Parameters for the `jira_link_remote_create` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LinkRemoteCreateParams {
    /// JIRA issue key (e.g., `PROJ-123`).
    pub key: String,
    /// External URL to link to.
    pub url: String,
    /// Link title (display text).
    pub title: String,
    /// Optional summary shown under the title.
    #[serde(default)]
    pub summary: Option<String>,
    /// Optional relationship label (e.g., `relates to`).
    #[serde(default)]
    pub relationship: Option<String>,
    /// Optional global id — reusing an existing one updates that link rather
    /// than creating a duplicate.
    #[serde(default)]
    pub global_id: Option<String>,
    /// When true, validate and return the would-be request without creating
    /// the link. Defaults to `false`.
    #[serde(default)]
    pub dry_run: bool,
}

/// Parameters for the `jira_link_remote_delete` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LinkRemoteDeleteParams {
    /// JIRA issue key (e.g., `PROJ-123`).
    pub key: String,
    /// Remote link ID to delete (from `jira_link_remote_list`).
    pub link_id: String,
    /// Must be `true` — destructive guard.
    pub confirm: bool,
    /// When true, preview the request without deleting (and without requiring
    /// `confirm`). Defaults to `false`.
    #[serde(default)]
    pub dry_run: bool,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn link_remote_create_yaml(
    client: &AtlassianClient,
    key: &str,
    url: &str,
    title: &str,
    summary: Option<&str>,
    relationship: Option<&str>,
    global_id: Option<&str>,
    dry_run: bool,
) -> Result<String> {
    if dry_run {
        let mut object = serde_json::json!({ "url": url, "title": title });
        if let Some(s) = summary {
            object["summary"] = serde_json::Value::String(s.to_string());
        }
        let mut body = serde_json::json!({ "object": object });
        if let Some(r) = relationship {
            body["relationship"] = serde_json::Value::String(r.to_string());
        }
        if let Some(g) = global_id {
            body["globalId"] = serde_json::Value::String(g.to_string());
        }
        return dry_run_request_yaml(
            "POST",
            format!("/rest/api/3/issue/{key}/remotelink"),
            Some(body),
        );
    }
    let id = client
        .create_remote_issue_link(key, url, title, summary, relationship, global_id)
        .await?;
    serde_yaml::to_string(&serde_json::json!({ "status": "ok", "id": id }))
        .context("Failed to serialize status as YAML")
}

pub(crate) async fn link_remote_delete_yaml(
    client: &AtlassianClient,
    key: &str,
    link_id: &str,
    confirm: bool,
    dry_run: bool,
) -> Result<String> {
    if dry_run {
        return dry_run_request_yaml(
            "DELETE",
            format!("/rest/api/3/issue/{key}/remotelink/{link_id}"),
            None,
        );
    }
    if !confirm {
        return Err(anyhow!(
            "Refusing to delete remote link {link_id} on {key}: pass `confirm: true` to authorise this destructive operation."
        ));
    }
    client.delete_remote_issue_link(key, link_id).await?;
    serde_yaml::to_string(&STATUS_OK).context("Failed to serialize status as YAML")
}

/// Parameters for the `jira_link_create` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LinkCreateParams {
    /// Link type name, e.g. `Blocks` (use `jira_link_types` to list options).
    pub link_type: String,
    /// Source (inward) issue key — e.g. for `Blocks`, the issue doing the blocking.
    pub inward: String,
    /// Target (outward) issue key — e.g. for `Blocks`, the issue being blocked.
    pub outward: String,
    /// When true, validate and return the would-be request (method, path,
    /// body) without creating the link. Defaults to `false`.
    #[serde(default)]
    pub dry_run: bool,
}

pub(crate) async fn link_create_yaml(
    client: &AtlassianClient,
    link_type: &str,
    inward: &str,
    outward: &str,
    dry_run: bool,
) -> Result<String> {
    if dry_run {
        return dry_run_request_yaml(
            "POST",
            "/rest/api/3/issueLink".to_string(),
            Some(serde_json::json!({
                "type": { "name": link_type },
                "inwardIssue": { "key": inward },
                "outwardIssue": { "key": outward },
            })),
        );
    }
    client.create_issue_link(link_type, inward, outward).await?;
    serde_yaml::to_string(&STATUS_OK).context("Failed to serialize status as YAML")
}

/// Parameters for the `jira_link_parent` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LinkParentParams {
    /// Parent issue key (e.g. the epic).
    pub parent: String,
    /// Child issue key to place under the parent.
    pub child: String,
    /// When true, validate and return the would-be request (method, path,
    /// body) without setting the parent. Defaults to `false`.
    #[serde(default)]
    pub dry_run: bool,
}

pub(crate) async fn link_parent_yaml(
    client: &AtlassianClient,
    parent: &str,
    child: &str,
    dry_run: bool,
) -> Result<String> {
    if dry_run {
        return dry_run_request_yaml(
            "PUT",
            format!("/rest/api/3/issue/{child}"),
            Some(serde_json::json!({ "fields": { "parent": { "key": parent } } })),
        );
    }
    client.set_issue_parent(child, parent).await?;
    serde_yaml::to_string(&STATUS_OK).context("Failed to serialize status as YAML")
}

// ─────────────────────────────────────────────────────────────────────────
// Worklog tools
// ─────────────────────────────────────────────────────────────────────────

/// Parameters for the `jira_worklog_list` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorklogListParams {
    /// JIRA issue key (e.g., `PROJ-123`).
    pub key: String,
    /// Maximum number of worklogs to return. `0` means unlimited (default 50).
    #[serde(default = "default_limit_50")]
    pub limit: u32,
}

/// Parameters for the `jira_worklog_add` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorklogAddParams {
    /// JIRA issue key (e.g., `PROJ-123`).
    pub key: String,
    /// Time spent (e.g., `1h 30m`, `2d`).
    pub time_spent: String,
    /// ISO 8601 timestamp when the work started.
    #[serde(default)]
    pub started: Option<String>,
    /// Optional plain-text comment.
    #[serde(default)]
    pub comment: Option<String>,
}

pub(crate) async fn worklog_list_yaml(
    client: &AtlassianClient,
    key: &str,
    limit: u32,
) -> Result<String> {
    let result = client.get_worklogs(key, limit).await?;
    serde_yaml::to_string(&result).context("Failed to serialize worklogs as YAML")
}

/// Parameters for the `jira_worklog_update` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorklogUpdateParams {
    /// JIRA issue key (e.g., `PROJ-123`).
    pub key: String,
    /// Worklog ID to update (from `jira_worklog_list`).
    pub worklog_id: String,
    /// New time spent (e.g., `1h 30m`, `2d`). Omit to leave unchanged.
    #[serde(default)]
    pub time_spent: Option<String>,
    /// New ISO 8601 start timestamp. Omit to leave unchanged.
    #[serde(default)]
    pub started: Option<String>,
    /// New plain-text comment. Omit to leave unchanged.
    #[serde(default)]
    pub comment: Option<String>,
}

/// Parameters for the `jira_worklog_delete` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorklogDeleteParams {
    /// JIRA issue key (e.g., `PROJ-123`).
    pub key: String,
    /// Worklog ID to delete (from `jira_worklog_list`).
    pub worklog_id: String,
    /// Must be `true` to authorise the irreversible delete; the tool refuses
    /// (without calling the API) when `false`.
    #[serde(default)]
    pub confirm: bool,
}

pub(crate) async fn worklog_add_yaml(
    client: &AtlassianClient,
    key: &str,
    time_spent: &str,
    started: Option<&str>,
    comment: Option<&str>,
) -> Result<String> {
    client
        .add_worklog(key, time_spent, started, comment)
        .await?;
    serde_yaml::to_string(&STATUS_OK).context("Failed to serialize status as YAML")
}

pub(crate) async fn worklog_update_yaml(
    client: &AtlassianClient,
    key: &str,
    worklog_id: &str,
    time_spent: Option<&str>,
    started: Option<&str>,
    comment: Option<&str>,
) -> Result<String> {
    if time_spent.is_none() && started.is_none() && comment.is_none() {
        return Err(anyhow!(
            "Nothing to update: supply at least one of `time_spent`, `started`, or `comment`."
        ));
    }
    client
        .update_worklog(key, worklog_id, time_spent, started, comment)
        .await?;
    serde_yaml::to_string(&STATUS_OK).context("Failed to serialize status as YAML")
}

pub(crate) async fn worklog_delete_yaml(
    client: &AtlassianClient,
    key: &str,
    worklog_id: &str,
    confirm: bool,
) -> Result<String> {
    if !confirm {
        return Err(anyhow!(
            "Refusing to delete worklog {worklog_id} on {key}: pass `confirm: true` to authorise this irreversible operation."
        ));
    }
    client.delete_worklog(key, worklog_id).await?;
    serde_yaml::to_string(&STATUS_OK).context("Failed to serialize status as YAML")
}

// ─────────────────────────────────────────────────────────────────────────
// Label tools
// ─────────────────────────────────────────────────────────────────────────

/// Parameters for the `jira_label_add` / `jira_label_remove` tools.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LabelMutateParams {
    /// JIRA issue key (e.g., `PROJ-123`).
    pub key: String,
    /// Labels to add or remove. JIRA labels cannot contain spaces.
    pub labels: Vec<String>,
}

pub(crate) async fn label_add_yaml(
    client: &AtlassianClient,
    key: &str,
    labels: &[String],
) -> Result<String> {
    if labels.is_empty() {
        return Err(anyhow!("No labels supplied."));
    }
    client.modify_issue_labels(key, labels, &[]).await?;
    serde_yaml::to_string(&STATUS_OK).context("Failed to serialize status as YAML")
}

pub(crate) async fn label_remove_yaml(
    client: &AtlassianClient,
    key: &str,
    labels: &[String],
) -> Result<String> {
    if labels.is_empty() {
        return Err(anyhow!("No labels supplied."));
    }
    client.modify_issue_labels(key, &[], labels).await?;
    serde_yaml::to_string(&STATUS_OK).context("Failed to serialize status as YAML")
}

// ─────────────────────────────────────────────────────────────────────────
// Field tools
// ─────────────────────────────────────────────────────────────────────────

/// Parameters for the `jira_field_list` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FieldListParams {
    /// Filter to fields whose name contains this substring (case-insensitive).
    #[serde(default)]
    pub search: Option<String>,
}

/// Parameters for the `jira_field_options` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FieldOptionsParams {
    /// Field ID (e.g., `customfield_10001`).
    pub field_id: String,
    /// Optional context ID. When omitted, the first context is auto-discovered.
    #[serde(default)]
    pub context_id: Option<String>,
}

pub(crate) async fn field_list_yaml(
    client: &AtlassianClient,
    cache: &CatalogueCache,
    search: Option<&str>,
) -> Result<String> {
    let cached = cache.fields(client).await?;
    let view: Vec<_> = match search {
        Some(needle) => {
            let needle_lower = needle.to_lowercase();
            cached
                .iter()
                .filter(|f| f.name.to_lowercase().contains(&needle_lower))
                .cloned()
                .collect()
        }
        None => cached.iter().cloned().collect(),
    };
    serde_yaml::to_string(&view).context("Failed to serialize fields as YAML")
}

pub(crate) async fn field_options_yaml(
    client: &AtlassianClient,
    field_id: &str,
    context_id: Option<&str>,
) -> Result<String> {
    let options = client.get_field_options(field_id, context_id).await?;
    serde_yaml::to_string(&options).context("Failed to serialize field options as YAML")
}

// ─────────────────────────────────────────────────────────────────────────
// Project tools
// ─────────────────────────────────────────────────────────────────────────

/// Parameters for the `jira_project_list` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProjectListParams {
    /// Maximum number of projects to return. `0` means unlimited (default 50).
    #[serde(default = "default_limit_50")]
    pub limit: u32,
}

/// Parameters for the `jira_project_create_meta` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProjectCreateMetaParams {
    /// Project key (e.g., `PROJ`).
    pub project: String,
    /// Issue type name (e.g., `Task`, `Bug`).
    pub issue_type: String,
}

pub(crate) async fn project_list_yaml(
    client: &AtlassianClient,
    cache: &CatalogueCache,
    limit: u32,
) -> Result<String> {
    let cached = cache.projects(client).await?;
    let effective_limit = if limit == 0 {
        usize::MAX
    } else {
        limit as usize
    };
    let projects: Vec<JiraProject> = cached
        .projects
        .iter()
        .take(effective_limit)
        .cloned()
        .collect();
    let total = projects.len() as u32;
    let view = crate::atlassian::jira_types::JiraProjectList { projects, total };
    serde_yaml::to_string(&view).context("Failed to serialize projects as YAML")
}

pub(crate) async fn project_create_meta_yaml(
    client: &AtlassianClient,
    project: &str,
    issue_type: &str,
) -> Result<String> {
    let meta = client.get_project_create_meta(project, issue_type).await?;
    serde_yaml::to_string(&meta).context("Failed to serialize create metadata as YAML")
}

// ─────────────────────────────────────────────────────────────────────────
// Changelog tool
// ─────────────────────────────────────────────────────────────────────────

/// Parameters for the `jira_changelog` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ChangelogParams {
    /// JIRA issue key (e.g., `PROJ-123`).
    pub key: String,
    /// Maximum number of entries to return. `0` means unlimited (default 50).
    #[serde(default = "default_limit_50")]
    pub limit: u32,
}

pub(crate) async fn changelog_yaml(
    client: &AtlassianClient,
    key: &str,
    limit: u32,
) -> Result<String> {
    let entries = client.get_changelog(key, limit).await?;
    serde_yaml::to_string(&entries).context("Failed to serialize changelog as YAML")
}

// ─────────────────────────────────────────────────────────────────────────
// Delete tool (destructive)
// ─────────────────────────────────────────────────────────────────────────

/// Parameters for the `jira_delete` tool.
///
/// `confirm` must be `true` for the deletion to proceed. This is the
/// MCP-side guard for an irreversible operation; the assistant must
/// explicitly opt in.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DeleteParams {
    /// JIRA issue key to delete (e.g., `PROJ-123`).
    pub key: String,
    /// Must be set to `true` — destructive guard.
    pub confirm: bool,
}

pub(crate) async fn delete_yaml(
    client: &AtlassianClient,
    key: &str,
    confirm: bool,
) -> Result<String> {
    if !confirm {
        return Err(anyhow!(
            "Refusing to delete {key}: pass `confirm: true` to authorise this irreversible operation."
        ));
    }
    client.delete_issue(key).await?;
    serde_yaml::to_string(&STATUS_OK).context("Failed to serialize status as YAML")
}

// ─────────────────────────────────────────────────────────────────────────
// Defaults
// ─────────────────────────────────────────────────────────────────────────

const fn default_limit_50() -> u32 {
    50
}

// ─────────────────────────────────────────────────────────────────────────
// Tool router
// ─────────────────────────────────────────────────────────────────────────

#[allow(missing_docs)] // #[tool_router] generates a pub `jira_tool_router` fn.
#[tool_router(router = jira_tool_router, vis = "pub")]
impl OmniDevServer {
    // ── attachments ────────────────────────────────────────────────

    /// Tool: download all attachments on an issue (optionally filtered by
    /// filename substring) to disk and return YAML metadata describing the
    /// downloaded files.
    #[tool(
        description = "Download attachments on a JIRA issue to disk. Returns YAML metadata \
                       (id, filename, mime_type, size, on-disk path) for each downloaded file. \
                       If `output_dir` is omitted, files are written to a fresh temp directory \
                       whose path is in the result; the assistant can then read them via the \
                       filesystem tool. Mirrors `omni-dev atlassian jira attachment download`."
    )]
    pub async fn jira_attachment_download(
        &self,
        Parameters(params): Parameters<AttachmentDownloadParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            let dir = resolve_output_dir(params.output_dir.as_deref())?;
            attachment_download_yaml(&client, &params.key, &dir, params.filter.as_deref()).await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: download only image attachments on an issue and return YAML
    /// metadata describing the files.
    #[tool(
        description = "Download image attachments (PNG, JPEG, GIF, SVG, WebP) on a JIRA issue \
                       to disk. Returns YAML metadata for each downloaded image. If \
                       `output_dir` is omitted, files are written to a fresh temp directory. \
                       Mirrors `omni-dev atlassian jira attachment images`."
    )]
    pub async fn jira_attachment_images(
        &self,
        Parameters(params): Parameters<AttachmentImagesParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            let dir = resolve_output_dir(params.output_dir.as_deref())?;
            attachment_images_yaml(&client, &params.key, &dir).await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: upload one or more local files as attachments to an issue.
    #[tool(
        description = "Upload one or more local files as attachments to a JIRA issue. Provide \
                       `file_paths` as absolute paths on the MCP server's filesystem; each is \
                       streamed from disk and rides a single multipart request. Returns YAML \
                       metadata (id, filename, mime_type, size, content_url) for each created \
                       attachment. Mirrors `omni-dev atlassian jira attachment upload`."
    )]
    pub async fn jira_attachment_upload(
        &self,
        Parameters(params): Parameters<AttachmentUploadParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            attachment_upload_yaml(&client, &params.key, &params.file_paths).await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: delete a JIRA attachment by ID. **Irreversible** — caller must
    /// pass `confirm: true`.
    #[tool(
        description = "Delete a JIRA attachment by ID. **DESTRUCTIVE AND IRREVERSIBLE** (JIRA \
                       has no trash). You must explicitly pass `confirm: true` for the deletion \
                       to proceed; otherwise the tool returns an error without contacting the \
                       API. Returns YAML `{status: ok}` on success. Mirrors `omni-dev atlassian \
                       jira attachment delete --force`."
    )]
    pub async fn jira_attachment_delete(
        &self,
        Parameters(params): Parameters<AttachmentDeleteParams>,
    ) -> Result<CallToolResult, McpError> {
        // Reject before loading credentials so a missing-config environment can
        // still see the destructive guard.
        if !params.confirm {
            return Err(tool_error(anyhow!(
                "Refusing to delete attachment {}: pass `confirm: true` to authorise this irreversible operation.",
                params.attachment_id
            )));
        }
        let yaml = (async {
            let (client, _) = create_client()?;
            attachment_delete_yaml(&client, &params.attachment_id, params.confirm).await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    // ── boards ─────────────────────────────────────────────────────

    /// Tool: list agile boards.
    #[tool(
        description = "List JIRA agile boards, optionally filtered by project key and/or board \
                       type (`scrum`/`kanban`). Returns YAML. Mirrors `omni-dev atlassian jira \
                       board list`."
    )]
    pub async fn jira_board_list(
        &self,
        Parameters(params): Parameters<BoardListParams>,
    ) -> Result<CallToolResult, McpError> {
        let cache = self.catalogue_cache.clone();
        let yaml = (async {
            let (client, _) = create_client()?;
            board_list_yaml(
                &client,
                &cache,
                params.project.as_deref(),
                params.board_type.as_deref(),
                params.limit,
            )
            .await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: list issues on an agile board.
    #[tool(
        description = "List issues on a JIRA agile board (every issue on the board, across its \
                       backlog and all its sprints). Accepts an optional JQL filter. Returns YAML. \
                       To scope to a single sprint's issues instead, use `jira_sprint_issues`. \
                       Mirrors `omni-dev atlassian jira board issues`."
    )]
    pub async fn jira_board_issues(
        &self,
        Parameters(params): Parameters<BoardIssuesParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            board_issues_yaml(
                &client,
                params.board_id,
                params.jql.as_deref(),
                params.limit,
            )
            .await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    // ── sprints ────────────────────────────────────────────────────

    /// Tool: list sprints on a board.
    #[tool(
        description = "List sprints on an agile board, optionally filtered by state \
                       (`active`/`future`/`closed`). Returns YAML. Mirrors `omni-dev atlassian \
                       jira sprint list`."
    )]
    pub async fn jira_sprint_list(
        &self,
        Parameters(params): Parameters<SprintListParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            sprint_list_yaml(
                &client,
                params.board_id,
                params.state.as_deref(),
                params.limit,
            )
            .await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: list issues in a sprint.
    #[tool(
        description = "List issues in one JIRA sprint (get sprint IDs from `jira_sprint_list`). \
                       Accepts an optional JQL filter. Returns YAML. For every issue on a board \
                       regardless of sprint, use `jira_board_issues` instead. \
                       Mirrors `omni-dev atlassian jira sprint issues`."
    )]
    pub async fn jira_sprint_issues(
        &self,
        Parameters(params): Parameters<SprintIssuesParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            sprint_issues_yaml(
                &client,
                params.sprint_id,
                params.jql.as_deref(),
                params.limit,
            )
            .await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: add issues to a sprint.
    #[tool(
        description = "Add one or more issues to a JIRA sprint by issue key. Returns YAML \
                       `{status: ok}` on success. Mirrors `omni-dev atlassian jira sprint add`."
    )]
    pub async fn jira_sprint_add(
        &self,
        Parameters(params): Parameters<SprintAddParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            sprint_add_yaml(&client, params.sprint_id, &params.issue_keys).await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: create a sprint.
    #[tool(
        description = "Create a new sprint on a JIRA agile board. Returns YAML for the created \
                       sprint. Mirrors `omni-dev atlassian jira sprint create`."
    )]
    pub async fn jira_sprint_create(
        &self,
        Parameters(params): Parameters<SprintCreateParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            sprint_create_yaml(
                &client,
                params.board_id,
                &params.name,
                params.start_date.as_deref(),
                params.end_date.as_deref(),
                params.goal.as_deref(),
            )
            .await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: update sprint metadata or state.
    #[tool(
        description = "Update sprint name, state (`future`/`active`/`closed`), dates, or goal. \
                       Returns YAML `{status: ok}`. Mirrors `omni-dev atlassian jira sprint \
                       update`."
    )]
    pub async fn jira_sprint_update(
        &self,
        Parameters(params): Parameters<SprintUpdateParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            sprint_update_yaml(
                &client,
                params.sprint_id,
                params.name.as_deref(),
                params.state.as_deref(),
                params.start_date.as_deref(),
                params.end_date.as_deref(),
                params.goal.as_deref(),
            )
            .await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: delete a sprint.
    #[tool(
        description = "Delete a JIRA sprint (by `sprint_id`). Irreversible: pass `confirm: true` \
                       to authorise — without it the tool refuses and makes no API call. Returns \
                       YAML `{status: ok}`. Mirrors `omni-dev atlassian jira sprint delete`."
    )]
    pub async fn jira_sprint_delete(
        &self,
        Parameters(params): Parameters<SprintDeleteParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            sprint_delete_yaml(&client, params.sprint_id, params.confirm).await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    // ── project versions ───────────────────────────────────────────

    /// Tool: list project versions.
    #[tool(
        description = "List versions for a JIRA project, optionally filtered by `released` \
                       and `archived` flags. Returns YAML. Mirrors `omni-dev atlassian jira \
                       version list`."
    )]
    pub async fn jira_version_list(
        &self,
        Parameters(params): Parameters<VersionListParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            version_list_yaml(&client, &params.project, params.released, params.archived).await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: create a project version.
    #[tool(
        description = "Create a new version in a JIRA project. Dates must be `YYYY-MM-DD` and \
                       are validated client-side. Returns YAML for the created version. \
                       Mirrors `omni-dev atlassian jira version create`."
    )]
    pub async fn jira_version_create(
        &self,
        Parameters(params): Parameters<VersionCreateParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            version_create_yaml(
                &client,
                &params.project,
                &params.name,
                params.description.as_deref(),
                params.release_date.as_deref(),
                params.start_date.as_deref(),
                params.released,
                params.archived,
            )
            .await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: mark a project version as released.
    #[tool(
        description = "Mark a JIRA project version as released (by `version_id` from \
                       `jira_version_list`). Optional `release_date` (`YYYY-MM-DD`, validated \
                       client-side). Returns YAML `{status: ok}`. Mirrors `omni-dev atlassian \
                       jira version release`."
    )]
    pub async fn jira_version_release(
        &self,
        Parameters(params): Parameters<VersionReleaseParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            version_release_yaml(&client, &params.version_id, params.release_date.as_deref()).await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: archive a project version.
    #[tool(
        description = "Archive a JIRA project version (by `version_id` from `jira_version_list`). \
                       Returns YAML `{status: ok}`. Mirrors `omni-dev atlassian jira version \
                       archive`."
    )]
    pub async fn jira_version_archive(
        &self,
        Parameters(params): Parameters<VersionArchiveParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            version_archive_yaml(&client, &params.version_id).await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: rename a project version.
    #[tool(
        description = "Rename a JIRA project version (by `version_id` from `jira_version_list`), \
                       optionally updating its `description`. Returns YAML `{status: ok}`. \
                       Mirrors `omni-dev atlassian jira version rename`."
    )]
    pub async fn jira_version_rename(
        &self,
        Parameters(params): Parameters<VersionRenameParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            version_rename_yaml(
                &client,
                &params.version_id,
                &params.name,
                params.description.as_deref(),
            )
            .await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: delete a project version.
    #[tool(
        description = "Delete a JIRA project version (by `version_id` from `jira_version_list`). \
                       Optionally reassign affected issues first via `move_fix_issues_to` / \
                       `move_affected_issues_to` (target version ids). Irreversible: pass \
                       `confirm: true` to authorise — without it the tool refuses and makes no \
                       API call. Returns YAML `{status: ok}`. Mirrors `omni-dev atlassian jira \
                       version delete`."
    )]
    pub async fn jira_version_delete(
        &self,
        Parameters(params): Parameters<VersionDeleteParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            version_delete_yaml(
                &client,
                &params.version_id,
                params.move_fix_issues_to.as_deref(),
                params.move_affected_issues_to.as_deref(),
                params.confirm,
            )
            .await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    // ── components ─────────────────────────────────────────────────

    /// Tool: list a project's components.
    #[tool(
        description = "List a JIRA project's components. Returns YAML. Mirrors `omni-dev \
                       atlassian jira component list`."
    )]
    pub async fn jira_component_list(
        &self,
        Parameters(params): Parameters<ComponentListParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            component_list_yaml(&client, &params.project).await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: create a project component.
    #[tool(
        description = "Create a JIRA project component (`project`, `name`, optional \
                       `description`). Returns YAML for the created component. Mirrors \
                       `omni-dev atlassian jira component create`."
    )]
    pub async fn jira_component_create(
        &self,
        Parameters(params): Parameters<ComponentCreateParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            component_create_yaml(
                &client,
                &params.project,
                &params.name,
                params.description.as_deref(),
            )
            .await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: update a component's name/description.
    #[tool(
        description = "Update a JIRA component's `name` and/or `description` (by `component_id` \
                       from `jira_component_list`; supply at least one field). Returns YAML \
                       `{status: ok}`. Mirrors `omni-dev atlassian jira component update`."
    )]
    pub async fn jira_component_update(
        &self,
        Parameters(params): Parameters<ComponentUpdateParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            component_update_yaml(
                &client,
                &params.component_id,
                params.name.as_deref(),
                params.description.as_deref(),
            )
            .await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: delete a component.
    #[tool(
        description = "Delete a JIRA component (by `component_id` from `jira_component_list`). \
                       Optionally reassign referencing issues via `move_issues_to`. \
                       Irreversible: pass `confirm: true` to authorise — without it the tool \
                       refuses and makes no API call. Returns YAML `{status: ok}`. Mirrors \
                       `omni-dev atlassian jira component delete`."
    )]
    pub async fn jira_component_delete(
        &self,
        Parameters(params): Parameters<ComponentDeleteParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            component_delete_yaml(
                &client,
                &params.component_id,
                params.move_issues_to.as_deref(),
                params.confirm,
            )
            .await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    // ── watchers ───────────────────────────────────────────────────

    /// Tool: list watchers on an issue.
    #[tool(
        description = "List watchers on a JIRA issue. Returns YAML with watch_count and an \
                       array of watcher accounts. Mirrors `omni-dev atlassian jira watcher list`."
    )]
    pub async fn jira_watcher_list(
        &self,
        Parameters(params): Parameters<WatcherListParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            watcher_list_yaml(&client, &params.key).await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: add a watcher to an issue.
    #[tool(
        description = "Add a user (by Atlassian `accountId`, not a name or email — resolve one \
                       with `jira_user_search`) as a watcher on a JIRA issue. \
                       Returns YAML `{status: ok}`. Mirrors `omni-dev atlassian jira watcher add`."
    )]
    pub async fn jira_watcher_add(
        &self,
        Parameters(params): Parameters<WatcherMutateParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            watcher_add_yaml(&client, &params.key, &params.account_id).await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: remove a watcher from an issue.
    #[tool(
        description = "Remove a user (by Atlassian `accountId`, not a name or email — resolve \
                       one with `jira_user_search`) from the watchers of a JIRA \
                       issue. Destructive operation: callers must explicitly pass \
                       `confirm: true` for the removal to proceed; otherwise the tool \
                       refuses with an error. Returns YAML `{status: ok}`. Mirrors \
                       `omni-dev atlassian jira watcher remove`."
    )]
    pub async fn jira_watcher_remove(
        &self,
        Parameters(params): Parameters<WatcherRemoveParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            watcher_remove_yaml(&client, &params.key, &params.account_id, params.confirm).await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    // ── links ──────────────────────────────────────────────────────

    /// Tool: list inward and outward links on an issue.
    #[tool(
        description = "List inward and outward links on a JIRA issue. Returns YAML with one \
                       entry per link (id, link_type, direction, linked_issue_key, \
                       linked_issue_summary). Mirrors `omni-dev atlassian jira link list`."
    )]
    pub async fn jira_link_list(
        &self,
        Parameters(params): Parameters<LinkListParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            link_list_yaml(&client, &params.key).await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: list the configured issue link-type catalogue.
    #[tool(
        description = "List the configured JIRA issue link-type catalogue (id, name, inward, \
                       outward). Global per JIRA instance — returns the configured types, not \
                       the ones used in any particular issue. Returns YAML. Mirrors `omni-dev \
                       atlassian jira link types`."
    )]
    pub async fn jira_link_types(
        &self,
        Parameters(_params): Parameters<LinkTypesParams>,
    ) -> Result<CallToolResult, McpError> {
        let cache = self.catalogue_cache.clone();
        let yaml = (async {
            let (client, _) = create_client()?;
            link_types_yaml(&client, &cache).await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: remove an issue link by its link ID.
    #[tool(
        description = "Remove a JIRA issue link by its link ID (use `jira_link_list` or \
                       `jira_read` to discover IDs). Destructive operation: callers \
                       must explicitly pass `confirm: true` for the removal to proceed; \
                       otherwise the tool refuses with an error. Returns YAML \
                       `{status: ok}`. Set `dry_run: true` to preview the request that would be \
                       sent (method, path) without removing the link — no `confirm` needed for a \
                       dry-run. Mirrors `omni-dev atlassian jira link remove`."
    )]
    pub async fn jira_link_remove(
        &self,
        Parameters(params): Parameters<LinkRemoveParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            link_remove_yaml(&client, &params.link_id, params.confirm, params.dry_run).await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: list remote (external URL) links on an issue.
    #[tool(
        description = "List remote (external URL) links on a JIRA issue — links pointing out \
                       to non-JIRA resources (Confluence pages, Bitbucket PRs, external \
                       trackers). Read-only. Returns YAML with one entry per remote link \
                       (id, optional global_id, optional relationship, object.{url, title, \
                       summary, icon}). Mirrors `omni-dev atlassian jira link remote list`."
    )]
    pub async fn jira_link_remote_list(
        &self,
        Parameters(params): Parameters<LinkRemoteListParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            link_remote_list_yaml(&client, &params.key).await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: add a remote (external URL) link to an issue.
    #[tool(
        description = "Add a remote (external URL) link to a JIRA issue — pointing out to a \
                       non-JIRA resource. `url` and `title` are required; `summary`, \
                       `relationship`, and `global_id` are optional (reusing an existing \
                       `global_id` updates that link instead of duplicating it). Returns YAML \
                       `{status: ok, id}`. Set `dry_run: true` to preview the request without \
                       creating it. Mirrors `omni-dev atlassian jira link remote create`."
    )]
    pub async fn jira_link_remote_create(
        &self,
        Parameters(params): Parameters<LinkRemoteCreateParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            link_remote_create_yaml(
                &client,
                &params.key,
                &params.url,
                &params.title,
                params.summary.as_deref(),
                params.relationship.as_deref(),
                params.global_id.as_deref(),
                params.dry_run,
            )
            .await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: delete a remote (external URL) link from an issue.
    #[tool(
        description = "Delete a remote (external URL) link from a JIRA issue (by `key` + \
                       `link_id` from `jira_link_remote_list`). Irreversible: pass \
                       `confirm: true` to authorise — without it the tool refuses and makes no \
                       API call. Set `dry_run: true` to preview the request without deleting \
                       (and without requiring `confirm`). Returns YAML `{status: ok}`. Mirrors \
                       `omni-dev atlassian jira link remote delete`."
    )]
    pub async fn jira_link_remote_delete(
        &self,
        Parameters(params): Parameters<LinkRemoteDeleteParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            link_remote_delete_yaml(
                &client,
                &params.key,
                &params.link_id,
                params.confirm,
                params.dry_run,
            )
            .await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: create a typed link between two issues.
    #[tool(
        description = "Create a typed link between two JIRA issues (e.g. `Blocks`, \
                       `Relates`). `link_type` is the type name (list options with \
                       `jira_link_types`); `inward` is the source issue and `outward` \
                       the target. To set hierarchy (Epic → Story / Story → Sub-task) \
                       use `jira_link_parent` instead. Returns YAML `{status: ok}`. \
                       Set `dry_run: true` to preview the request that would be sent (method, \
                       path, body) without creating the link. \
                       Mirrors `omni-dev atlassian jira link create`."
    )]
    pub async fn jira_link_create(
        &self,
        Parameters(params): Parameters<LinkCreateParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            link_create_yaml(
                &client,
                &params.link_type,
                &params.inward,
                &params.outward,
                params.dry_run,
            )
            .await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: set an issue's parent (hierarchy link).
    #[tool(
        description = "Set a JIRA issue's parent — the system `parent` field for \
                       Epic → Story / Story → Sub-task hierarchy, distinct from the \
                       relationship links created by `jira_link_create`. `parent` is \
                       the parent issue key (e.g. the epic); `child` is the issue placed \
                       under it. Returns YAML `{status: ok}`. Set `dry_run: true` to preview \
                       the request that would be sent (method, path, body) without setting the \
                       parent. Mirrors `omni-dev atlassian jira link parent`."
    )]
    pub async fn jira_link_parent(
        &self,
        Parameters(params): Parameters<LinkParentParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            link_parent_yaml(&client, &params.parent, &params.child, params.dry_run).await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    // ── worklogs ───────────────────────────────────────────────────

    /// Tool: list worklog entries for an issue.
    #[tool(
        description = "List worklog entries on a JIRA issue. Returns YAML. Mirrors `omni-dev \
                       atlassian jira worklog list`."
    )]
    pub async fn jira_worklog_list(
        &self,
        Parameters(params): Parameters<WorklogListParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            worklog_list_yaml(&client, &params.key, params.limit).await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: log time on an issue.
    #[tool(
        description = "Log time on a JIRA issue. `time_spent` accepts JIRA's duration format \
                       (e.g., `1h 30m`, `2d`). Returns YAML `{status: ok}`. Mirrors `omni-dev \
                       atlassian jira worklog add`."
    )]
    pub async fn jira_worklog_add(
        &self,
        Parameters(params): Parameters<WorklogAddParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            worklog_add_yaml(
                &client,
                &params.key,
                &params.time_spent,
                params.started.as_deref(),
                params.comment.as_deref(),
            )
            .await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: edit a worklog entry on an issue.
    #[tool(
        description = "Edit an existing JIRA worklog (by `key` + `worklog_id`; get the id from \
                       `jira_worklog_list`). Supply at least one of `time_spent` (JIRA duration, \
                       e.g. `1h 30m`), `started` (ISO 8601), or `comment`; omitted fields keep \
                       their current value. Returns YAML `{status: ok}`. Mirrors `omni-dev \
                       atlassian jira worklog edit`."
    )]
    pub async fn jira_worklog_update(
        &self,
        Parameters(params): Parameters<WorklogUpdateParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            worklog_update_yaml(
                &client,
                &params.key,
                &params.worklog_id,
                params.time_spent.as_deref(),
                params.started.as_deref(),
                params.comment.as_deref(),
            )
            .await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: delete a worklog entry from an issue.
    #[tool(
        description = "Delete a JIRA worklog (by `key` + `worklog_id`; get the id from \
                       `jira_worklog_list`). Irreversible: pass `confirm: true` to authorise — \
                       without it the tool refuses and makes no API call. Returns YAML \
                       `{status: ok}`. Mirrors `omni-dev atlassian jira worklog delete`."
    )]
    pub async fn jira_worklog_delete(
        &self,
        Parameters(params): Parameters<WorklogDeleteParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            worklog_delete_yaml(&client, &params.key, &params.worklog_id, params.confirm).await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    // ── labels ─────────────────────────────────────────────────────

    /// Tool: add labels to an issue incrementally.
    #[tool(
        description = "Add one or more labels to a JIRA issue incrementally (leaves the issue's \
                       other labels untouched, unlike `jira_edit` with a full `labels` array). \
                       JIRA labels cannot contain spaces. Returns YAML `{status: ok}`. Mirrors \
                       `omni-dev atlassian jira label add`."
    )]
    pub async fn jira_label_add(
        &self,
        Parameters(params): Parameters<LabelMutateParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            label_add_yaml(&client, &params.key, &params.labels).await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: remove labels from an issue incrementally.
    #[tool(
        description = "Remove one or more labels from a JIRA issue incrementally (leaves the \
                       issue's other labels untouched). Returns YAML `{status: ok}`. Mirrors \
                       `omni-dev atlassian jira label remove`."
    )]
    pub async fn jira_label_remove(
        &self,
        Parameters(params): Parameters<LabelMutateParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            label_remove_yaml(&client, &params.key, &params.labels).await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    // ── fields ─────────────────────────────────────────────────────

    /// Tool: list JIRA field definitions.
    #[tool(
        description = "List JIRA field definitions, optionally filtered by name substring. \
                       Returns YAML. Mirrors `omni-dev atlassian jira field list`. \
                       The `schema_type` is `\"richtext\"` for ADF-required custom fields \
                       (e.g. Acceptance Criteria); `schema_custom`, when present, holds \
                       the raw plugin URI (e.g. \
                       `com.atlassian.jira.plugin.system.customfieldtypes:textarea`)."
    )]
    pub async fn jira_field_list(
        &self,
        Parameters(params): Parameters<FieldListParams>,
    ) -> Result<CallToolResult, McpError> {
        let cache = self.catalogue_cache.clone();
        let yaml = (async {
            let (client, _) = create_client()?;
            field_list_yaml(&client, &cache, params.search.as_deref()).await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: list options for a custom field.
    #[tool(
        description = "List allowed option values for a JIRA custom field. If `context_id` is \
                       omitted, the first context for the field is auto-discovered. Returns \
                       YAML. Mirrors `omni-dev atlassian jira field options`."
    )]
    pub async fn jira_field_options(
        &self,
        Parameters(params): Parameters<FieldOptionsParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            field_options_yaml(&client, &params.field_id, params.context_id.as_deref()).await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    // ── projects ───────────────────────────────────────────────────

    /// Tool: list JIRA projects.
    #[tool(
        description = "List JIRA projects. Returns YAML. Mirrors `omni-dev atlassian jira \
                       project list`."
    )]
    pub async fn jira_project_list(
        &self,
        Parameters(params): Parameters<ProjectListParams>,
    ) -> Result<CallToolResult, McpError> {
        let cache = self.catalogue_cache.clone();
        let yaml = (async {
            let (client, _) = create_client()?;
            project_list_yaml(&client, &cache, params.limit).await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: introspect the create screen for a project + issue type.
    #[tool(
        description = "Introspect which fields a JIRA issue type needs: for each field on the \
                       create screen, returns `required`, `schema_type`, allowed values \
                       (resolving option/select/cascading-select), and any default. Collapses the \
                       create→HTTP 400→`jira_field_list`→`jira_field_options` recovery loop into a \
                       single pre-flight call. Returns YAML. Mirrors `omni-dev atlassian jira \
                       project create-meta`."
    )]
    pub async fn jira_project_create_meta(
        &self,
        Parameters(params): Parameters<ProjectCreateMetaParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            project_create_meta_yaml(&client, &params.project, &params.issue_type).await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    // ── changelog ──────────────────────────────────────────────────

    /// Tool: get the change history for an issue.
    #[tool(
        description = "Get the change history for a JIRA issue. Returns YAML with one entry \
                       per change (author, timestamp, items). The author is an Atlassian \
                       account ID — resolve it to a display name with `jira_user_get`. \
                       Mirrors `omni-dev atlassian jira changelog`."
    )]
    pub async fn jira_changelog(
        &self,
        Parameters(params): Parameters<ChangelogParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = (async {
            let (client, _) = create_client()?;
            changelog_yaml(&client, &params.key, params.limit).await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    // ── delete (destructive) ───────────────────────────────────────

    /// Tool: delete a JIRA issue. **Irreversible** — caller must pass
    /// `confirm: true`.
    #[tool(
        description = "Delete a JIRA issue. **DESTRUCTIVE AND IRREVERSIBLE.** You must \
                       explicitly pass `confirm: true` for the deletion to proceed; otherwise \
                       the tool returns an error without contacting the API. Returns YAML \
                       `{status: ok}` on success. Mirrors `omni-dev atlassian jira delete`."
    )]
    pub async fn jira_delete(
        &self,
        Parameters(params): Parameters<DeleteParams>,
    ) -> Result<CallToolResult, McpError> {
        // Reject before loading credentials so a missing-config environment can
        // still see the destructive guard.
        if !params.confirm {
            return Err(tool_error(anyhow!(
                "Refusing to delete {}: pass `confirm: true` to authorise this irreversible operation.",
                params.key
            )));
        }
        let yaml = (async {
            let (client, _) = create_client()?;
            delete_yaml(&client, &params.key, params.confirm).await
        })
        .await
        .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn mock_client(base_url: &str) -> AtlassianClient {
        AtlassianClient::new(base_url, "u@t.com", "tok").unwrap()
    }

    // ── attachment helpers ─────────────────────────────────────────

    #[tokio::test]
    async fn attachment_download_yaml_success_writes_files() {
        let server = MockServer::start().await;
        let content_url = format!("{}/attachment/1", server.uri());

        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "fields": {
                    "attachment": [
                        {"id": "1", "filename": "note.txt", "mimeType": "text/plain", "size": 5, "content": content_url}
                    ]
                }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/attachment/1"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"hello".as_slice()))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let dir = tempfile::tempdir().unwrap();
        let yaml = attachment_download_yaml(&client, "PROJ-1", dir.path(), None)
            .await
            .unwrap();
        assert!(yaml.contains("note.txt"));
        assert!(yaml.contains(dir.path().to_str().unwrap()));
        assert!(dir.path().join("note.txt").exists());
    }

    #[tokio::test]
    async fn attachment_download_yaml_sanitizes_traversal_filename() {
        let server = MockServer::start().await;
        let content_url = format!("{}/attachment/1", server.uri());

        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "fields": {
                    "attachment": [
                        {"id": "1", "filename": "../../escape.txt", "mimeType": "text/plain", "size": 4, "content": content_url}
                    ]
                }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/attachment/1"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"EVIL".as_slice()))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let dir = tempfile::tempdir().unwrap();
        attachment_download_yaml(&client, "PROJ-1", dir.path(), None)
            .await
            .unwrap();
        assert!(dir.path().join("escape.txt").exists());
        let escaped = dir.path().parent().unwrap().parent().unwrap();
        assert!(!escaped.join("escape.txt").exists());
    }

    #[tokio::test]
    async fn attachment_download_yaml_empty_filename_falls_back_to_id() {
        let server = MockServer::start().await;
        let content_url = format!("{}/attachment/1", server.uri());

        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "fields": {
                    "attachment": [
                        {"id": "1", "filename": "", "mimeType": "text/plain", "size": 4, "content": content_url}
                    ]
                }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/attachment/1"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"data".as_slice()))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let dir = tempfile::tempdir().unwrap();
        attachment_download_yaml(&client, "PROJ-1", dir.path(), None)
            .await
            .unwrap();
        assert!(dir.path().join("attachment-1").exists());
    }

    #[tokio::test]
    async fn attachment_download_yaml_filter_excludes_non_matching() {
        let server = MockServer::start().await;
        let content_url = format!("{}/attachment/1", server.uri());

        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/PROJ-2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "fields": {
                    "attachment": [
                        {"id": "1", "filename": "screenshot.png", "mimeType": "image/png", "size": 1, "content": content_url},
                        {"id": "2", "filename": "report.pdf", "mimeType": "application/pdf", "size": 2, "content": "http://nowhere/2"}
                    ]
                }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/attachment/1"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"x".as_slice()))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let dir = tempfile::tempdir().unwrap();
        let yaml = attachment_download_yaml(&client, "PROJ-2", dir.path(), Some("screen"))
            .await
            .unwrap();
        assert!(yaml.contains("screenshot.png"));
        assert!(!yaml.contains("report.pdf"));
    }

    #[tokio::test]
    async fn attachment_download_yaml_propagates_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/NOPE-1"))
            .respond_with(ResponseTemplate::new(404).set_body_string("nope"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let dir = tempfile::tempdir().unwrap();
        let err = attachment_download_yaml(&client, "NOPE-1", dir.path(), None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn attachment_images_yaml_filters_to_images() {
        let server = MockServer::start().await;
        let img_url = format!("{}/attachment/img", server.uri());
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/PROJ-3"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "fields": {
                    "attachment": [
                        {"id": "img", "filename": "photo.png", "mimeType": "image/png", "size": 4, "content": img_url},
                        {"id": "doc", "filename": "spec.pdf", "mimeType": "application/pdf", "size": 8, "content": "http://nowhere/doc"}
                    ]
                }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/attachment/img"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"\x89PNG".as_slice()))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let dir = tempfile::tempdir().unwrap();
        let yaml = attachment_images_yaml(&client, "PROJ-3", dir.path())
            .await
            .unwrap();
        assert!(yaml.contains("photo.png"));
        assert!(!yaml.contains("spec.pdf"));
        assert!(dir.path().join("photo.png").exists());
    }

    #[tokio::test]
    async fn attachment_images_yaml_propagates_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/NOPE-2"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let dir = tempfile::tempdir().unwrap();
        let err = attachment_images_yaml(&client, "NOPE-2", dir.path())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    #[tokio::test]
    async fn attachment_download_yaml_create_dir_failure_includes_path() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "fields": {"attachment": []}
            })))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        // A path under /dev/null cannot be created.
        let bad = Path::new("/dev/null/does/not/exist");
        let err = attachment_download_yaml(&client, "PROJ-1", bad, None)
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Failed to create output directory"),
            "got: {msg}"
        );
    }

    #[test]
    fn resolve_output_dir_uses_supplied_path() {
        let dir = resolve_output_dir(Some("/tmp/foo")).unwrap();
        assert_eq!(dir, PathBuf::from("/tmp/foo"));
    }

    #[test]
    fn resolve_output_dir_creates_tempdir_when_missing() {
        let dir = resolve_output_dir(None).unwrap();
        assert!(dir.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn attachment_upload_yaml_returns_created_attachments() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/3/issue/PROJ-1/attachments"))
            .and(header("X-Atlassian-Token", "no-check"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "10001", "filename": "log.txt", "mimeType": "text/plain", "size": 5, "content": "https://example.com/10001"}
            ])))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("log.txt");
        fs::write(&file, b"hello").unwrap();

        let client = mock_client(&server.uri());
        let yaml =
            attachment_upload_yaml(&client, "PROJ-1", &[file.to_string_lossy().into_owned()])
                .await
                .unwrap();
        assert!(yaml.contains("key: PROJ-1"));
        assert!(yaml.contains("log.txt"));
        assert!(yaml.contains("10001"));
    }

    #[tokio::test]
    async fn attachment_delete_yaml_requires_confirm() {
        let client = mock_client("https://example.atlassian.net");
        let err = attachment_delete_yaml(&client, "10042", false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("confirm: true"));
    }

    #[tokio::test]
    async fn attachment_delete_yaml_with_confirm_calls_api() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/rest/api/3/attachment/10042"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let yaml = attachment_delete_yaml(&client, "10042", true)
            .await
            .unwrap();
        assert!(yaml.contains("status: ok"));
    }

    // ── attachment tool handlers (create_client boundary) ───────────

    #[tokio::test(flavor = "current_thread")]
    async fn jira_attachment_upload_handler_success_via_mock() {
        let guard = crate::atlassian::auth::test_util::EnvGuard::take();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/3/issue/PROJ-1/attachments"))
            .and(header("X-Atlassian-Token", "no-check"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "10001", "filename": "log.txt", "mimeType": "text/plain", "size": 5, "content": "https://example.com/10001"}
            ])))
            .mount(&server)
            .await;
        let _home = guard.set_credentials(&server.uri());

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("log.txt");
        fs::write(&file, b"hello").unwrap();

        let srv = OmniDevServer::new();
        let result = srv
            .jira_attachment_upload(Parameters(AttachmentUploadParams {
                key: "PROJ-1".to_string(),
                file_paths: vec![file.to_string_lossy().into_owned()],
            }))
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn jira_attachment_delete_handler_without_confirm_returns_tool_error() {
        // Rejects before create_client, so no env/mock is needed.
        let srv = OmniDevServer::new();
        let result = srv
            .jira_attachment_delete(Parameters(AttachmentDeleteParams {
                attachment_id: "10042".to_string(),
                confirm: false,
            }))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn jira_attachment_delete_handler_success_via_mock() {
        let guard = crate::atlassian::auth::test_util::EnvGuard::take();
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/rest/api/3/attachment/10042"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let _home = guard.set_credentials(&server.uri());

        let srv = OmniDevServer::new();
        let result = srv
            .jira_attachment_delete(Parameters(AttachmentDeleteParams {
                attachment_id: "10042".to_string(),
                confirm: true,
            }))
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false));
    }

    // ── boards ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn board_list_yaml_returns_yaml() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/agile/1.0/board"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "values": [{"id": 1, "name": "B", "type": "scrum", "location": {"projectKey": "PROJ"}}],
                "isLast": true
            })))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let cache = CatalogueCache::new(std::time::Duration::from_secs(60));
        let yaml = board_list_yaml(&client, &cache, None, None, 50)
            .await
            .unwrap();
        assert!(yaml.contains("scrum"));
        assert!(yaml.contains("projectKey: PROJ") || yaml.contains("project_key: PROJ"));
    }

    #[tokio::test]
    async fn board_list_yaml_limit_zero_returns_all_filtered_clientside() {
        // limit=0 hits the unlimited branch in `effective_limit`; the cache
        // fetches with limit=0, and the helper applies the project +
        // board_type filters client-side against the cached full list.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/agile/1.0/board"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "values": [
                    {"id": 1, "name": "Alpha",  "type": "scrum",  "location": {"projectKey": "PROJ"}},
                    {"id": 2, "name": "Beta",   "type": "kanban", "location": {"projectKey": "PROJ"}},
                    {"id": 3, "name": "Gamma",  "type": "scrum",  "location": {"projectKey": "OTHER"}}
                ],
                "isLast": true
            })))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let cache = CatalogueCache::new(std::time::Duration::from_secs(60));

        let yaml = board_list_yaml(&client, &cache, Some("PROJ"), Some("scrum"), 0)
            .await
            .unwrap();
        // Only Alpha matches both filters.
        assert!(yaml.contains("Alpha"));
        assert!(!yaml.contains("Beta"));
        assert!(!yaml.contains("Gamma"));

        // Project-only filter exercises the project-Some lambda body.
        let yaml = board_list_yaml(&client, &cache, Some("PROJ"), None, 0)
            .await
            .unwrap();
        assert!(yaml.contains("Alpha"));
        assert!(yaml.contains("Beta"));
        assert!(!yaml.contains("Gamma"));

        // Board-type-only filter exercises the board_type-Some lambda body.
        let yaml = board_list_yaml(&client, &cache, None, Some("kanban"), 0)
            .await
            .unwrap();
        assert!(!yaml.contains("Alpha"));
        assert!(yaml.contains("Beta"));
        assert!(!yaml.contains("Gamma"));
    }

    #[tokio::test]
    async fn board_list_yaml_propagates_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/agile/1.0/board"))
            .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let cache = CatalogueCache::new(std::time::Duration::from_secs(60));
        let err = board_list_yaml(&client, &cache, None, None, 50)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn board_issues_yaml_returns_yaml() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/agile/1.0/board/7/issue"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "issues": [{"key": "PROJ-7", "fields": {"summary": "Task"}}],
                "isLast": true
            })))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = board_issues_yaml(&client, 7, None, 50).await.unwrap();
        assert!(yaml.contains("PROJ-7"));
    }

    #[tokio::test]
    async fn board_issues_yaml_propagates_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/agile/1.0/board/9/issue"))
            .respond_with(ResponseTemplate::new(404).set_body_string("nope"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = board_issues_yaml(&client, 9, None, 50).await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    // ── sprints ────────────────────────────────────────────────────

    #[tokio::test]
    async fn sprint_list_yaml_returns_yaml() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/agile/1.0/board/1/sprint"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "values": [{"id": 10, "name": "S1", "state": "active"}],
                "isLast": true
            })))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = sprint_list_yaml(&client, 1, None, 50).await.unwrap();
        assert!(yaml.contains("S1"));
        assert!(yaml.contains("active"));
    }

    #[tokio::test]
    async fn sprint_list_yaml_propagates_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/agile/1.0/board/1/sprint"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = sprint_list_yaml(&client, 1, None, 50).await.unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    #[tokio::test]
    async fn sprint_issues_yaml_returns_yaml() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/agile/1.0/sprint/5/issue"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "issues": [{"key": "PROJ-5", "fields": {"summary": "Sprint task"}}],
                "isLast": true
            })))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = sprint_issues_yaml(&client, 5, None, 50).await.unwrap();
        assert!(yaml.contains("PROJ-5"));
    }

    #[tokio::test]
    async fn sprint_issues_yaml_propagates_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/agile/1.0/sprint/5/issue"))
            .respond_with(ResponseTemplate::new(404).set_body_string("nope"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = sprint_issues_yaml(&client, 5, None, 50).await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn sprint_add_yaml_returns_status_ok() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/agile/1.0/sprint/3/issue"))
            .and(body_json(
                serde_json::json!({"issues": ["PROJ-1", "PROJ-2"]}),
            ))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = sprint_add_yaml(&client, 3, &["PROJ-1".to_string(), "PROJ-2".to_string()])
            .await
            .unwrap();
        assert!(yaml.contains("status"));
        assert!(yaml.contains("ok"));
    }

    #[tokio::test]
    async fn sprint_add_yaml_propagates_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/agile/1.0/sprint/3/issue"))
            .respond_with(ResponseTemplate::new(403).set_body_string("denied"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = sprint_add_yaml(&client, 3, &["PROJ-1".to_string()])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn sprint_create_yaml_returns_yaml() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/agile/1.0/sprint"))
            .and(body_json(serde_json::json!({
                "originBoardId": 1, "name": "Sprint 1", "goal": "Win"
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": 99, "name": "Sprint 1", "state": "future", "goal": "Win"
            })))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = sprint_create_yaml(&client, 1, "Sprint 1", None, None, Some("Win"))
            .await
            .unwrap();
        assert!(yaml.contains("id: 99"));
        assert!(yaml.contains("Sprint 1"));
    }

    #[tokio::test]
    async fn sprint_create_yaml_propagates_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/agile/1.0/sprint"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = sprint_create_yaml(&client, 1, "S", None, None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("400"));
    }

    #[tokio::test]
    async fn sprint_update_yaml_returns_status_ok() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/agile/1.0/sprint/99"))
            .and(body_json(serde_json::json!({"state": "closed"})))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = sprint_update_yaml(&client, 99, None, Some("closed"), None, None, None)
            .await
            .unwrap();
        assert!(yaml.contains("status: ok"));
    }

    #[tokio::test]
    async fn sprint_update_yaml_propagates_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/agile/1.0/sprint/99"))
            .respond_with(ResponseTemplate::new(404).set_body_string("nope"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = sprint_update_yaml(&client, 99, Some("Renamed"), None, None, None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn sprint_delete_yaml_without_confirm_refuses() {
        let client = mock_client("http://127.0.0.1:1");
        let err = sprint_delete_yaml(&client, 42, false).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Refusing to delete sprint 42"));
        assert!(msg.contains("confirm: true"));
    }

    #[tokio::test]
    async fn sprint_delete_yaml_with_confirm_deletes() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/rest/agile/1.0/sprint/42"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = sprint_delete_yaml(&client, 42, true).await.unwrap();
        assert!(yaml.contains("ok"));
    }

    // ── project versions ───────────────────────────────────────────

    #[tokio::test]
    async fn version_list_yaml_returns_yaml() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/project/PROJ/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "1", "name": "1.0.0", "released": true, "archived": false}
            ])))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = version_list_yaml(&client, "PROJ", None, None)
            .await
            .unwrap();
        assert!(yaml.contains("1.0.0"));
        assert!(yaml.contains("project_key: PROJ"));
    }

    #[tokio::test]
    async fn version_list_yaml_filters_released() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/project/PROJ/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "1", "name": "1.0.0", "released": true, "archived": false},
                {"id": "2", "name": "2.0.0", "released": false, "archived": false},
            ])))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = version_list_yaml(&client, "PROJ", Some(false), None)
            .await
            .unwrap();
        assert!(yaml.contains("2.0.0"));
        assert!(!yaml.contains("1.0.0"));
    }

    #[tokio::test]
    async fn version_list_yaml_propagates_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/project/NONE/versions"))
            .respond_with(ResponseTemplate::new(404).set_body_string("missing"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = version_list_yaml(&client, "NONE", None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn version_create_yaml_returns_yaml() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/3/version"))
            .and(body_json(serde_json::json!({
                "project": "PROJ",
                "name": "1.0.0",
                "released": false,
                "archived": false,
                "releaseDate": "2026-06-01"
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": "100",
                "name": "1.0.0",
                "released": false,
                "archived": false,
                "releaseDate": "2026-06-01"
            })))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = version_create_yaml(
            &client,
            "PROJ",
            "1.0.0",
            None,
            Some("2026-06-01"),
            None,
            false,
            false,
        )
        .await
        .unwrap();
        assert!(yaml.contains("id: '100'"));
        assert!(yaml.contains("1.0.0"));
    }

    #[tokio::test]
    async fn version_create_yaml_propagates_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/3/version"))
            .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = version_create_yaml(&client, "PROJ", "1.0", None, None, None, false, false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn version_create_yaml_validates_dates_client_side() {
        let server = MockServer::start().await;
        // No mock — request must short-circuit before HTTP.
        let client = mock_client(&server.uri());
        let err = version_create_yaml(
            &client,
            "PROJ",
            "1.0",
            None,
            Some("2026/06/01"),
            None,
            false,
            false,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("YYYY-MM-DD"));
    }

    #[tokio::test]
    async fn version_release_yaml_returns_status_ok() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/version/100"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "100"})),
            )
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = version_release_yaml(&client, "100", Some("2026-06-01"))
            .await
            .unwrap();
        assert!(yaml.contains("ok"));
    }

    #[tokio::test]
    async fn version_archive_yaml_returns_status_ok() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/version/100"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "100"})),
            )
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = version_archive_yaml(&client, "100").await.unwrap();
        assert!(yaml.contains("ok"));
    }

    #[tokio::test]
    async fn version_rename_yaml_returns_status_ok() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/version/100"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "100"})),
            )
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = version_rename_yaml(&client, "100", "2.0", Some("desc"))
            .await
            .unwrap();
        assert!(yaml.contains("ok"));
    }

    #[tokio::test]
    async fn version_delete_yaml_without_confirm_refuses() {
        let client = mock_client("http://127.0.0.1:1");
        let err = version_delete_yaml(&client, "100", None, None, false)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Refusing to delete version 100"));
        assert!(msg.contains("confirm: true"));
    }

    #[tokio::test]
    async fn version_delete_yaml_with_confirm_deletes() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/rest/api/3/version/100"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = version_delete_yaml(&client, "100", None, None, true)
            .await
            .unwrap();
        assert!(yaml.contains("ok"));
    }

    // ── components ─────────────────────────────────────────────────

    #[tokio::test]
    async fn component_list_yaml_returns_yaml() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/project/PROJ/components"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "1", "name": "Backend"}
            ])))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = component_list_yaml(&client, "PROJ").await.unwrap();
        assert!(yaml.contains("Backend"));
    }

    #[tokio::test]
    async fn component_create_yaml_returns_component() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/3/component"))
            .respond_with(
                ResponseTemplate::new(201)
                    .set_body_json(serde_json::json!({"id": "1", "name": "Backend"})),
            )
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = component_create_yaml(&client, "PROJ", "Backend", None)
            .await
            .unwrap();
        assert!(yaml.contains("Backend"));
    }

    #[tokio::test]
    async fn component_update_yaml_without_fields_errors() {
        let client = mock_client("http://127.0.0.1:1");
        let err = component_update_yaml(&client, "1", None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Nothing to update"));
    }

    #[tokio::test]
    async fn component_delete_yaml_without_confirm_refuses() {
        let client = mock_client("http://127.0.0.1:1");
        let err = component_delete_yaml(&client, "1", None, false)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Refusing to delete component 1"));
        assert!(msg.contains("confirm: true"));
    }

    #[tokio::test]
    async fn component_delete_yaml_with_confirm_deletes() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/rest/api/3/component/1"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = component_delete_yaml(&client, "1", None, true)
            .await
            .unwrap();
        assert!(yaml.contains("ok"));
    }

    // ── watchers ───────────────────────────────────────────────────

    #[tokio::test]
    async fn watcher_list_yaml_returns_yaml() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/PROJ-1/watchers"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "watchCount": 1,
                "watchers": [{"accountId": "abc", "displayName": "Alice"}]
            })))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = watcher_list_yaml(&client, "PROJ-1").await.unwrap();
        assert!(yaml.contains("Alice"));
        assert!(yaml.contains("watch_count"));
    }

    #[tokio::test]
    async fn watcher_list_yaml_propagates_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/NOPE/watchers"))
            .respond_with(ResponseTemplate::new(404).set_body_string("missing"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = watcher_list_yaml(&client, "NOPE").await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn watcher_add_yaml_returns_status_ok() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/3/issue/PROJ-1/watchers"))
            .and(body_json(serde_json::json!("abc")))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = watcher_add_yaml(&client, "PROJ-1", "abc").await.unwrap();
        assert!(yaml.contains("ok"));
    }

    #[tokio::test]
    async fn watcher_add_yaml_propagates_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/3/issue/PROJ-1/watchers"))
            .respond_with(ResponseTemplate::new(403).set_body_string("denied"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = watcher_add_yaml(&client, "PROJ-1", "abc")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn watcher_remove_yaml_requires_confirm_true() {
        let server = MockServer::start().await;
        let client = mock_client(&server.uri());
        let err = watcher_remove_yaml(&client, "PROJ-1", "abc", false)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("confirm: true"));
    }

    #[tokio::test]
    async fn watcher_remove_yaml_with_confirm_calls_api() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/rest/api/3/issue/PROJ-1/watchers"))
            .and(query_param("accountId", "abc"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = watcher_remove_yaml(&client, "PROJ-1", "abc", true)
            .await
            .unwrap();
        assert!(yaml.contains("ok"));
    }

    #[tokio::test]
    async fn watcher_remove_yaml_propagates_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/rest/api/3/issue/PROJ-1/watchers"))
            .respond_with(ResponseTemplate::new(403).set_body_string("denied"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = watcher_remove_yaml(&client, "PROJ-1", "abc", true)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    // ── links ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn link_list_yaml_returns_yaml() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "key": "PROJ-1",
                "fields": {
                    "issuelinks": [
                        {
                            "id": "10001",
                            "type": {"name": "Blocks", "inward": "is blocked by", "outward": "blocks"},
                            "outwardIssue": {"key": "PROJ-2", "fields": {"summary": "downstream"}}
                        },
                        {
                            "id": "10002",
                            "type": {"name": "Relates", "inward": "relates to", "outward": "relates to"},
                            "inwardIssue": {"key": "PROJ-3", "fields": {"summary": "upstream"}}
                        }
                    ]
                }
            })))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = link_list_yaml(&client, "PROJ-1").await.unwrap();
        assert!(yaml.contains("Blocks"));
        assert!(yaml.contains("PROJ-2"));
        assert!(yaml.contains("PROJ-3"));
        assert!(yaml.contains("inward"));
        assert!(yaml.contains("outward"));
    }

    #[tokio::test]
    async fn link_list_yaml_propagates_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/NOPE-1"))
            .respond_with(ResponseTemplate::new(404).set_body_string("missing"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = link_list_yaml(&client, "NOPE-1").await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn link_remote_list_yaml_returns_yaml() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/PROJ-1/remotelink"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {
                    "id": 10001,
                    "globalId": "system=https://example.atlassian.net/wiki&id=12345",
                    "relationship": "mentioned in",
                    "object": {
                        "url": "https://example.atlassian.net/wiki/page/1",
                        "title": "Design doc",
                        "icon": {"url16x16": "https://example.atlassian.net/icons/page.png"}
                    }
                }
            ])))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = link_remote_list_yaml(&client, "PROJ-1").await.unwrap();
        assert!(yaml.contains("10001"));
        assert!(yaml.contains("mentioned in"));
        assert!(yaml.contains("Design doc"));
        assert!(yaml.contains("global_id"));
    }

    #[tokio::test]
    async fn link_remote_list_yaml_propagates_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/NOPE-1/remotelink"))
            .respond_with(ResponseTemplate::new(404).set_body_string("missing"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = link_remote_list_yaml(&client, "NOPE-1").await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn link_remote_create_yaml_returns_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/3/issue/PROJ-1/remotelink"))
            .respond_with(
                ResponseTemplate::new(201).set_body_json(serde_json::json!({"id": 10010})),
            )
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = link_remote_create_yaml(
            &client,
            "PROJ-1",
            "https://x/doc",
            "Doc",
            None,
            None,
            None,
            false,
        )
        .await
        .unwrap();
        assert!(yaml.contains("ok"));
        assert!(yaml.contains("10010"));
    }

    #[tokio::test]
    async fn link_remote_create_yaml_dry_run_previews_without_api() {
        // No mock: a real POST would fail the connection.
        let client = mock_client("http://127.0.0.1:1");
        let yaml = link_remote_create_yaml(
            &client,
            "PROJ-1",
            "https://x/doc",
            "Doc",
            Some("s"),
            Some("relates to"),
            None,
            true,
        )
        .await
        .unwrap();
        assert!(yaml.contains("POST"));
        assert!(yaml.contains("/rest/api/3/issue/PROJ-1/remotelink"));
    }

    #[tokio::test]
    async fn link_remote_delete_yaml_requires_confirm_true() {
        let client = mock_client("http://127.0.0.1:1");
        let err = link_remote_delete_yaml(&client, "PROJ-1", "10010", false, false)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Refusing to delete remote link 10010 on PROJ-1"));
        assert!(msg.contains("confirm: true"));
    }

    #[tokio::test]
    async fn link_remote_delete_yaml_with_confirm_calls_api() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/rest/api/3/issue/PROJ-1/remotelink/10010"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = link_remote_delete_yaml(&client, "PROJ-1", "10010", true, false)
            .await
            .unwrap();
        assert!(yaml.contains("ok"));
    }

    #[tokio::test]
    async fn link_remote_delete_yaml_dry_run_previews_without_confirm_or_api() {
        let client = mock_client("http://127.0.0.1:1");
        let yaml = link_remote_delete_yaml(&client, "PROJ-1", "10010", false, true)
            .await
            .unwrap();
        assert!(yaml.contains("DELETE"));
        assert!(yaml.contains("/rest/api/3/issue/PROJ-1/remotelink/10010"));
    }

    #[tokio::test]
    async fn link_types_yaml_returns_yaml() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issueLinkType"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "issueLinkTypes": [
                    {"id": "1", "name": "Blocks", "inward": "is blocked by", "outward": "blocks"},
                    {"id": "2", "name": "Duplicates", "inward": "is duplicated by", "outward": "duplicates"}
                ]
            })))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let cache = CatalogueCache::new(std::time::Duration::from_secs(60));
        let yaml = link_types_yaml(&client, &cache).await.unwrap();
        assert!(yaml.contains("Blocks"));
        assert!(yaml.contains("Duplicates"));
    }

    #[tokio::test]
    async fn link_types_yaml_propagates_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issueLinkType"))
            .respond_with(ResponseTemplate::new(403).set_body_string("denied"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let cache = CatalogueCache::new(std::time::Duration::from_secs(60));
        let err = link_types_yaml(&client, &cache).await.unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn link_remove_yaml_requires_confirm_true() {
        let server = MockServer::start().await;
        let client = mock_client(&server.uri());
        let err = link_remove_yaml(&client, "12345", false, false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("confirm: true"));
    }

    #[tokio::test]
    async fn link_remove_yaml_with_confirm_calls_api() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/rest/api/3/issueLink/12345"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = link_remove_yaml(&client, "12345", true, false)
            .await
            .unwrap();
        assert!(yaml.contains("ok"));
    }

    #[tokio::test]
    async fn link_remove_yaml_propagates_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/rest/api/3/issueLink/99"))
            .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = link_remove_yaml(&client, "99", true, false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn link_create_yaml_calls_api() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/3/issueLink"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = link_create_yaml(&client, "Blocks", "PROJ-1", "PROJ-2", false)
            .await
            .unwrap();
        assert!(yaml.contains("ok"));
    }

    #[tokio::test]
    async fn link_create_yaml_propagates_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/3/issueLink"))
            .respond_with(ResponseTemplate::new(400).set_body_string("Bad"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = link_create_yaml(&client, "Bad", "PROJ-1", "PROJ-2", false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("400"));
    }

    #[tokio::test]
    async fn link_parent_yaml_calls_api() {
        let server = MockServer::start().await;
        // The child issue (`PROJ-2`) is the one PUT-updated with the parent field.
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/issue/PROJ-2"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = link_parent_yaml(&client, "EPIC-1", "PROJ-2", false)
            .await
            .unwrap();
        assert!(yaml.contains("ok"));
    }

    #[tokio::test]
    async fn link_parent_yaml_propagates_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/issue/PROJ-2"))
            .respond_with(ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = link_parent_yaml(&client, "EPIC-1", "PROJ-2", false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    // ── link dry_run (issue #1048) ──────────────────────────────────
    //
    // The unreachable `http://127.0.0.1:1` client is the short-circuit proof:
    // a dry-run returns the would-be request instead of making a network call.

    #[tokio::test]
    async fn link_remove_yaml_dry_run_previews_without_confirm_or_api() {
        let client = mock_client("http://127.0.0.1:1");
        // `confirm = false`: a dry-run neither requires the destructive guard
        // nor performs the DELETE.
        let yaml = link_remove_yaml(&client, "12345", false, true)
            .await
            .unwrap();
        assert!(yaml.contains("dry_run: true"));
        assert!(yaml.contains("method: DELETE"));
        assert!(yaml.contains("path: /rest/api/3/issueLink/12345"));
    }

    #[tokio::test]
    async fn link_create_yaml_dry_run_previews_without_api() {
        let client = mock_client("http://127.0.0.1:1");
        let yaml = link_create_yaml(&client, "Blocks", "PROJ-1", "PROJ-2", true)
            .await
            .unwrap();
        assert!(yaml.contains("dry_run: true"));
        assert!(yaml.contains("method: POST"));
        assert!(yaml.contains("path: /rest/api/3/issueLink"));
        assert!(yaml.contains("Blocks"));
        assert!(yaml.contains("PROJ-1"));
        assert!(yaml.contains("PROJ-2"));
    }

    #[tokio::test]
    async fn link_parent_yaml_dry_run_previews_without_api() {
        let client = mock_client("http://127.0.0.1:1");
        let yaml = link_parent_yaml(&client, "EPIC-1", "PROJ-2", true)
            .await
            .unwrap();
        assert!(yaml.contains("dry_run: true"));
        assert!(yaml.contains("method: PUT"));
        // The child issue (`PROJ-2`) is the one PUT-updated with the parent field.
        assert!(yaml.contains("path: /rest/api/3/issue/PROJ-2"));
        assert!(yaml.contains("EPIC-1"));
    }

    // ── worklogs ───────────────────────────────────────────────────

    #[tokio::test]
    async fn worklog_list_yaml_returns_yaml() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/PROJ-1/worklog"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "total": 1,
                "worklogs": [{
                    "id": "w1",
                    "author": {"displayName": "Alice"},
                    "timeSpent": "1h",
                    "timeSpentSeconds": 3600,
                    "started": "2026-04-21T00:00:00.000+0000"
                }]
            })))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = worklog_list_yaml(&client, "PROJ-1", 50).await.unwrap();
        assert!(yaml.contains("Alice"));
        assert!(yaml.contains("1h"));
    }

    #[tokio::test]
    async fn worklog_list_yaml_propagates_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/PROJ-1/worklog"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = worklog_list_yaml(&client, "PROJ-1", 50).await.unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    #[tokio::test]
    async fn worklog_add_yaml_returns_status_ok() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/3/issue/PROJ-1/worklog"))
            .and(header("content-type", "application/json"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = worklog_add_yaml(&client, "PROJ-1", "1h", None, Some("did stuff"))
            .await
            .unwrap();
        assert!(yaml.contains("ok"));
    }

    #[tokio::test]
    async fn worklog_add_yaml_propagates_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/3/issue/PROJ-1/worklog"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = worklog_add_yaml(&client, "PROJ-1", "1h", None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("400"));
    }

    #[tokio::test]
    async fn worklog_update_yaml_returns_status_ok() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/issue/PROJ-1/worklog/100"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "100"})),
            )
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = worklog_update_yaml(&client, "PROJ-1", "100", Some("2h"), None, None)
            .await
            .unwrap();
        assert!(yaml.contains("ok"));
    }

    #[tokio::test]
    async fn worklog_update_yaml_without_fields_errors_before_call() {
        let client = mock_client("http://127.0.0.1:1");
        let err = worklog_update_yaml(&client, "PROJ-1", "100", None, None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Nothing to update"));
    }

    #[tokio::test]
    async fn worklog_delete_yaml_without_confirm_refuses() {
        let client = mock_client("http://127.0.0.1:1");
        let err = worklog_delete_yaml(&client, "PROJ-1", "100", false)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Refusing to delete worklog 100 on PROJ-1"));
        assert!(msg.contains("confirm: true"));
    }

    #[tokio::test]
    async fn worklog_delete_yaml_with_confirm_deletes() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/rest/api/3/issue/PROJ-1/worklog/100"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = worklog_delete_yaml(&client, "PROJ-1", "100", true)
            .await
            .unwrap();
        assert!(yaml.contains("ok"));
    }

    // ── labels ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn label_add_yaml_sends_update_verb() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .and(body_json(serde_json::json!({
                "update": {"labels": [{"add": "backend"}]}
            })))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = label_add_yaml(&client, "PROJ-1", &["backend".to_string()])
            .await
            .unwrap();
        assert!(yaml.contains("ok"));
    }

    #[tokio::test]
    async fn label_remove_yaml_sends_update_verb() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .and(body_json(serde_json::json!({
                "update": {"labels": [{"remove": "stale"}]}
            })))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = label_remove_yaml(&client, "PROJ-1", &["stale".to_string()])
            .await
            .unwrap();
        assert!(yaml.contains("ok"));
    }

    #[tokio::test]
    async fn label_add_yaml_empty_errors() {
        let client = mock_client("http://127.0.0.1:1");
        let err = label_add_yaml(&client, "PROJ-1", &[]).await.unwrap_err();
        assert!(err.to_string().contains("No labels"));
    }

    // ── fields ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn field_list_yaml_returns_yaml() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/field"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "summary", "name": "Summary", "custom": false},
                {"id": "customfield_1", "name": "Story Points", "custom": true},
                {
                    "id": "customfield_19300",
                    "name": "Acceptance Criteria",
                    "custom": true,
                    "schema": {
                        "type": "string",
                        "custom": "com.atlassian.jira.plugin.system.customfieldtypes:textarea"
                    }
                }
            ])))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let cache = CatalogueCache::new(std::time::Duration::from_secs(60));
        let yaml = field_list_yaml(&client, &cache, None).await.unwrap();
        assert!(yaml.contains("Summary"));
        assert!(yaml.contains("Story Points"));
        assert!(yaml.contains("Acceptance Criteria"));
        assert!(yaml.contains("schema_type: richtext"));
        assert!(yaml
            .contains("schema_custom: com.atlassian.jira.plugin.system.customfieldtypes:textarea"));
    }

    #[tokio::test]
    async fn field_list_yaml_filter_narrows_results() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/field"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "summary", "name": "Summary", "custom": false},
                {"id": "customfield_1", "name": "Story Points", "custom": true}
            ])))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let cache = CatalogueCache::new(std::time::Duration::from_secs(60));
        let yaml = field_list_yaml(&client, &cache, Some("story"))
            .await
            .unwrap();
        assert!(yaml.contains("Story Points"));
        assert!(!yaml.contains("Summary"));
    }

    #[tokio::test]
    async fn field_list_yaml_propagates_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/field"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let cache = CatalogueCache::new(std::time::Duration::from_secs(60));
        let err = field_list_yaml(&client, &cache, None).await.unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    #[tokio::test]
    async fn field_options_yaml_returns_yaml_with_explicit_context() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/field/customfield_1/context/ctx-1/option"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "values": [
                    {"id": "1", "value": "Low"},
                    {"id": "2", "value": "High"}
                ]
            })))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = field_options_yaml(&client, "customfield_1", Some("ctx-1"))
            .await
            .unwrap();
        assert!(yaml.contains("Low"));
        assert!(yaml.contains("High"));
    }

    #[tokio::test]
    async fn field_options_yaml_auto_discovers_context() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/field/customfield_2/context"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "values": [{"id": "ctx-7"}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/field/customfield_2/context/ctx-7/option"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "values": [{"id": "1", "value": "Solo"}]
            })))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = field_options_yaml(&client, "customfield_2", None)
            .await
            .unwrap();
        assert!(yaml.contains("Solo"));
    }

    #[tokio::test]
    async fn field_options_yaml_propagates_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/field/customfield_1/context/ctx-1/option"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = field_options_yaml(&client, "customfield_1", Some("ctx-1"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("400"));
    }

    // ── projects ───────────────────────────────────────────────────

    #[tokio::test]
    async fn project_list_yaml_returns_yaml() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/project/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "values": [{"id": "10001", "key": "PROJ", "name": "Project"}],
                "total": 1,
                "isLast": true
            })))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let cache = CatalogueCache::new(std::time::Duration::from_secs(60));
        let yaml = project_list_yaml(&client, &cache, 50).await.unwrap();
        assert!(yaml.contains("PROJ"));
    }

    #[tokio::test]
    async fn project_list_yaml_limit_zero_returns_all() {
        // limit=0 takes the `usize::MAX` branch in `effective_limit`.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/project/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "values": [
                    {"id": "10001", "key": "PROJ", "name": "Project"},
                    {"id": "10002", "key": "OTHER", "name": "Other"}
                ],
                "total": 2,
                "isLast": true
            })))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let cache = CatalogueCache::new(std::time::Duration::from_secs(60));
        let yaml = project_list_yaml(&client, &cache, 0).await.unwrap();
        assert!(yaml.contains("PROJ"));
        assert!(yaml.contains("OTHER"));
    }

    #[tokio::test]
    async fn project_list_yaml_propagates_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/project/search"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let cache = CatalogueCache::new(std::time::Duration::from_secs(60));
        let err = project_list_yaml(&client, &cache, 50).await.unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    #[tokio::test]
    async fn project_create_meta_yaml_returns_yaml() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/createmeta"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "projects": [{
                    "issuetypes": [{
                        "fields": {
                            "customfield_1": {
                                "name": "Work Type",
                                "required": true,
                                "schema": {"type": "option"},
                                "allowedValues": [{"id": "1", "value": "Planned"}]
                            }
                        }
                    }]
                }]
            })))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = project_create_meta_yaml(&client, "PROJ", "Task")
            .await
            .unwrap();
        assert!(yaml.contains("Work Type"));
        assert!(yaml.contains("required: true"));
        assert!(yaml.contains("Planned"));
    }

    #[tokio::test]
    async fn project_create_meta_yaml_propagates_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/createmeta"))
            .respond_with(ResponseTemplate::new(404).set_body_string("nope"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = project_create_meta_yaml(&client, "PROJ", "Task")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    /// Drives the production `jira_project_create_meta` tool handler end-to-end
    /// — `OmniDevServer::new()` -> `create_client()` (reading credentials from
    /// env) -> `project_create_meta_yaml` — with credentials pointed at a mock
    /// server behind the canonical `EnvGuard`, covering the env-reading handler
    /// wrapper the helper tests above bypass.
    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::await_holding_lock)]
    async fn jira_project_create_meta_handler_returns_success() {
        use crate::atlassian::auth::test_util::EnvGuard;
        use rmcp::handler::server::wrapper::Parameters;

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/createmeta"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "projects": [{
                    "issuetypes": [{
                        "fields": {
                            "summary": {
                                "name": "Summary",
                                "required": true,
                                "schema": {"type": "string"}
                            }
                        }
                    }]
                }]
            })))
            .mount(&server)
            .await;

        let guard = EnvGuard::take();
        let _home = guard.set_credentials(&server.uri());

        let srv = OmniDevServer::new();
        let result = srv
            .jira_project_create_meta(Parameters(ProjectCreateMetaParams {
                project: "PROJ".to_string(),
                issue_type: "Task".to_string(),
            }))
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false));
    }

    // ── changelog ──────────────────────────────────────────────────

    #[tokio::test]
    async fn changelog_yaml_returns_yaml() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/PROJ-1/changelog"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "values": [{
                    "id": "1",
                    "author": {"displayName": "Alice"},
                    "created": "2026-04-21T00:00:00.000+0000",
                    "items": [{"field": "status", "fromString": "Open", "toString": "In Progress"}]
                }],
                "isLast": true
            })))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = changelog_yaml(&client, "PROJ-1", 50).await.unwrap();
        assert!(yaml.contains("Alice"));
        assert!(yaml.contains("In Progress"));
    }

    #[tokio::test]
    async fn changelog_yaml_propagates_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/PROJ-1/changelog"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = changelog_yaml(&client, "PROJ-1", 50).await.unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    // ── delete ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn delete_yaml_requires_confirm_true() {
        let client = mock_client("http://nowhere.invalid");
        let err = delete_yaml(&client, "PROJ-1", false).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Refusing to delete"));
        assert!(msg.contains("confirm: true"));
    }

    #[tokio::test]
    async fn delete_yaml_with_confirm_calls_api() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = delete_yaml(&client, "PROJ-1", true).await.unwrap();
        assert!(yaml.contains("ok"));
    }

    #[tokio::test]
    async fn delete_yaml_propagates_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .respond_with(ResponseTemplate::new(404).set_body_string("missing"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = delete_yaml(&client, "PROJ-1", true).await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    // ── #[tool] handler wrappers (create_client boundary) ──────────
    //
    // The `*_yaml` helpers above are unit-tested directly; these drive the
    // thin `#[tool]` wrappers end-to-end through `create_client()` (env +
    // wiremock), covering the param-unpacking + result-wrapping glue.

    /// Points credentials at `server` and returns both the env guard and the
    /// redirected-`HOME` tempdir; the caller binds both so they stay alive for
    /// the handler call.
    fn handler_env(
        server: &MockServer,
    ) -> (
        crate::atlassian::auth::test_util::EnvGuard,
        tempfile::TempDir,
    ) {
        let guard = crate::atlassian::auth::test_util::EnvGuard::take();
        let home = guard.set_credentials(&server.uri());
        (guard, home)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn jira_worklog_update_handler_success() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/issue/PROJ-1/worklog/100"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "100"})),
            )
            .mount(&server)
            .await;
        let (_g, _home) = handler_env(&server);
        let r = OmniDevServer::new()
            .jira_worklog_update(Parameters(WorklogUpdateParams {
                key: "PROJ-1".to_string(),
                worklog_id: "100".to_string(),
                time_spent: Some("2h".to_string()),
                started: None,
                comment: None,
            }))
            .await
            .unwrap();
        assert!(!r.is_error.unwrap_or(false));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn jira_worklog_delete_handler_success() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/rest/api/3/issue/PROJ-1/worklog/100"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let (_g, _home) = handler_env(&server);
        let r = OmniDevServer::new()
            .jira_worklog_delete(Parameters(WorklogDeleteParams {
                key: "PROJ-1".to_string(),
                worklog_id: "100".to_string(),
                confirm: true,
            }))
            .await
            .unwrap();
        assert!(!r.is_error.unwrap_or(false));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn jira_version_release_handler_success() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/version/10000"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "10000"})),
            )
            .mount(&server)
            .await;
        let (_g, _home) = handler_env(&server);
        let r = OmniDevServer::new()
            .jira_version_release(Parameters(VersionReleaseParams {
                version_id: "10000".to_string(),
                release_date: Some("2026-06-01".to_string()),
            }))
            .await
            .unwrap();
        assert!(!r.is_error.unwrap_or(false));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn jira_version_archive_handler_success() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/version/10000"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "10000"})),
            )
            .mount(&server)
            .await;
        let (_g, _home) = handler_env(&server);
        let r = OmniDevServer::new()
            .jira_version_archive(Parameters(VersionArchiveParams {
                version_id: "10000".to_string(),
            }))
            .await
            .unwrap();
        assert!(!r.is_error.unwrap_or(false));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn jira_version_rename_handler_success() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/version/10000"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "10000"})),
            )
            .mount(&server)
            .await;
        let (_g, _home) = handler_env(&server);
        let r = OmniDevServer::new()
            .jira_version_rename(Parameters(VersionRenameParams {
                version_id: "10000".to_string(),
                name: "2.0".to_string(),
                description: None,
            }))
            .await
            .unwrap();
        assert!(!r.is_error.unwrap_or(false));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn jira_version_delete_handler_success() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/rest/api/3/version/10000"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let (_g, _home) = handler_env(&server);
        let r = OmniDevServer::new()
            .jira_version_delete(Parameters(VersionDeleteParams {
                version_id: "10000".to_string(),
                move_fix_issues_to: None,
                move_affected_issues_to: None,
                confirm: true,
            }))
            .await
            .unwrap();
        assert!(!r.is_error.unwrap_or(false));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn jira_link_remote_create_handler_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/3/issue/PROJ-1/remotelink"))
            .respond_with(
                ResponseTemplate::new(201).set_body_json(serde_json::json!({"id": 10010})),
            )
            .mount(&server)
            .await;
        let (_g, _home) = handler_env(&server);
        let r = OmniDevServer::new()
            .jira_link_remote_create(Parameters(LinkRemoteCreateParams {
                key: "PROJ-1".to_string(),
                url: "https://x/doc".to_string(),
                title: "Doc".to_string(),
                summary: None,
                relationship: None,
                global_id: None,
                dry_run: false,
            }))
            .await
            .unwrap();
        assert!(!r.is_error.unwrap_or(false));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn jira_link_remote_delete_handler_success() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/rest/api/3/issue/PROJ-1/remotelink/10010"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let (_g, _home) = handler_env(&server);
        let r = OmniDevServer::new()
            .jira_link_remote_delete(Parameters(LinkRemoteDeleteParams {
                key: "PROJ-1".to_string(),
                link_id: "10010".to_string(),
                confirm: true,
                dry_run: false,
            }))
            .await
            .unwrap();
        assert!(!r.is_error.unwrap_or(false));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn jira_sprint_delete_handler_success() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/rest/agile/1.0/sprint/42"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let (_g, _home) = handler_env(&server);
        let r = OmniDevServer::new()
            .jira_sprint_delete(Parameters(SprintDeleteParams {
                sprint_id: 42,
                confirm: true,
            }))
            .await
            .unwrap();
        assert!(!r.is_error.unwrap_or(false));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn jira_label_add_handler_success() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let (_g, _home) = handler_env(&server);
        let r = OmniDevServer::new()
            .jira_label_add(Parameters(LabelMutateParams {
                key: "PROJ-1".to_string(),
                labels: vec!["backend".to_string()],
            }))
            .await
            .unwrap();
        assert!(!r.is_error.unwrap_or(false));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn jira_label_remove_handler_success() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let (_g, _home) = handler_env(&server);
        let r = OmniDevServer::new()
            .jira_label_remove(Parameters(LabelMutateParams {
                key: "PROJ-1".to_string(),
                labels: vec!["stale".to_string()],
            }))
            .await
            .unwrap();
        assert!(!r.is_error.unwrap_or(false));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn jira_component_list_handler_success() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/project/PROJ/components"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "10000", "name": "Backend"}
            ])))
            .mount(&server)
            .await;
        let (_g, _home) = handler_env(&server);
        let r = OmniDevServer::new()
            .jira_component_list(Parameters(ComponentListParams {
                project: "PROJ".to_string(),
            }))
            .await
            .unwrap();
        assert!(!r.is_error.unwrap_or(false));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn jira_component_create_handler_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/3/component"))
            .respond_with(
                ResponseTemplate::new(201)
                    .set_body_json(serde_json::json!({"id": "10000", "name": "Backend"})),
            )
            .mount(&server)
            .await;
        let (_g, _home) = handler_env(&server);
        let r = OmniDevServer::new()
            .jira_component_create(Parameters(ComponentCreateParams {
                project: "PROJ".to_string(),
                name: "Backend".to_string(),
                description: None,
            }))
            .await
            .unwrap();
        assert!(!r.is_error.unwrap_or(false));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn jira_component_update_handler_success() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/component/10000"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "10000", "name": "Renamed"})),
            )
            .mount(&server)
            .await;
        let (_g, _home) = handler_env(&server);
        let r = OmniDevServer::new()
            .jira_component_update(Parameters(ComponentUpdateParams {
                component_id: "10000".to_string(),
                name: Some("Renamed".to_string()),
                description: None,
            }))
            .await
            .unwrap();
        assert!(!r.is_error.unwrap_or(false));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn jira_component_delete_handler_success() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/rest/api/3/component/10000"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let (_g, _home) = handler_env(&server);
        let r = OmniDevServer::new()
            .jira_component_delete(Parameters(ComponentDeleteParams {
                component_id: "10000".to_string(),
                move_issues_to: None,
                confirm: true,
            }))
            .await
            .unwrap();
        assert!(!r.is_error.unwrap_or(false));
    }

    // ── small util ─────────────────────────────────────────────────

    #[test]
    fn default_limit_is_50() {
        assert_eq!(default_limit_50(), 50);
    }
}
