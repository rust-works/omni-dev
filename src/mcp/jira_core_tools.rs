//! MCP tool handlers for JIRA operations.
//!
//! Each handler constructs an [`AtlassianClient`] via the shared
//! `create_client()` helper, dispatches to the underlying client method, and
//! serializes the result as YAML (the AI-friendly default per ADR-0020 /
//! ADR-0021). Errors from the client — including missing credentials — are
//! surfaced via [`super::error::tool_error`] rather than panicking.

use anyhow::{Context, Result};
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    schemars, tool, tool_router, ErrorData as McpError,
};
use serde::Deserialize;

use crate::atlassian::adf::AdfDocument;
use crate::atlassian::client::{
    AtlassianClient, JiraTransition, JiraVisibility, JiraVisibilityType,
};
use crate::atlassian::convert::markdown_to_adf;
use crate::atlassian::custom_fields::apply_user_field_overrides;
use crate::atlassian::document::{issue_to_jfm_document, JfmDocument};
use crate::cli::atlassian::helpers::create_client;

use super::error::tool_error;
use super::output_file::write_to_file_yaml;
use super::server::OmniDevServer;

// ── parameter types ────────────────────────────────────────────────────────

/// Parameters for the `jira_read` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct JiraReadParams {
    /// JIRA issue key (e.g., `PROJ-123`).
    pub key: String,
    /// Output format — `jfm` (default) returns JFM markdown with YAML
    /// frontmatter; `adf` returns the raw ADF description payload as JSON.
    #[serde(default)]
    pub format: Option<String>,
    /// When set, writes the rendered content to this path and returns a
    /// short YAML summary (path/bytes/format) instead of the inline body.
    /// Useful for large issues that would otherwise blow past the context
    /// window — the assistant can then read the file with offset/limit.
    #[serde(default)]
    pub output_file: Option<String>,
}

/// Parameters for the `jira_search` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct JiraSearchParams {
    /// JQL query string (e.g., `project = PROJ AND status = Open`).
    pub jql: String,
    /// Maximum number of results. Defaults to 20; `0` means unlimited.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Additional fields to request (informational; standard fields are
    /// always returned).
    #[serde(default)]
    pub fields: Option<Vec<String>>,
}

/// Parameters for the `jira_create` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct JiraCreateParams {
    /// Project key (e.g., `PROJ`).
    pub project: String,
    /// Issue summary / title.
    pub summary: String,
    /// Optional description in JFM markdown — see resource
    /// `omni-dev://specs/jfm` for syntax. JFM is GitHub-style markdown,
    /// NOT JIRA wiki markup (use `##` not `h2.`, triple-backtick fences not
    /// `{code}`, backtick inline code not `{{...}}`).
    #[serde(default)]
    pub description: Option<String>,
    /// Issue type (defaults to `Task`).
    #[serde(default)]
    pub issue_type: Option<String>,
}

/// Parameters for the `jira_write` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct JiraWriteParams {
    /// JIRA issue key (e.g., `PROJ-123`).
    pub key: String,
    /// New description body. Interpreted per `format`. Omit to leave the
    /// existing description unchanged (useful when only updating
    /// `parent`/`assignee`/`reporter`/`fields`).
    ///
    /// For `format = "jfm"` (the default), this is GitHub-style markdown,
    /// NOT JIRA wiki markup. Use `##` not `h2.`, triple-backtick fences not
    /// `{code}`, backtick inline code not `{{...}}`. Full reference:
    /// MCP resource `omni-dev://specs/jfm`.
    #[serde(default)]
    pub content: Option<String>,
    /// Content format — `jfm` (default) parses Markdown/JFM; `adf` accepts
    /// a raw ADF JSON document.
    #[serde(default)]
    pub format: Option<String>,
    /// Parent issue key (e.g., `PROJ-100`). When set, establishes a
    /// parent-child hierarchy on the issue (Epic → Story, Story → Sub-task,
    /// etc.). Distinct from `jira_link` actions, which create
    /// "Composition"-style links rather than the system parent field.
    #[serde(default)]
    pub parent: Option<String>,
    /// Assignee `accountId`. The empty string `""` clears the assignee;
    /// `"-1"` triggers JIRA automatic assignment. Use `jira_user_search` to
    /// resolve a name or email to an `accountId`.
    #[serde(default)]
    pub assignee: Option<String>,
    /// Reporter `accountId`. Same conventions as `assignee` (`""` clears,
    /// `"-1"` is JIRA automatic).
    #[serde(default)]
    pub reporter: Option<String>,
    /// Additional `fields` keys merged into the issue update payload as-is.
    /// Keys must already be canonical JIRA field ids (e.g. `priority`,
    /// `labels`, `customfield_10010`). Values must already be in the API's
    /// JSON shape (e.g. `{"name": "High"}` for priority,
    /// `["a", "b"]` for labels). Setting `assignee` or `reporter` here
    /// collides with the typed parameters and is rejected — pass the typed
    /// parameter instead.
    #[serde(default)]
    pub fields: Option<std::collections::BTreeMap<String, serde_json::Value>>,
}

/// Parameters for the `jira_transition` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct JiraTransitionParams {
    /// JIRA issue key (e.g., `PROJ-123`).
    pub key: String,
    /// Transition name (case-insensitive) or numeric id, e.g. `"In Progress"`
    /// or `"31"`. Required unless `list` is true.
    #[serde(default)]
    pub transition: Option<String>,
    /// Optional comment to add after the transition.
    #[serde(default)]
    pub comment: Option<String>,
    /// If true, returns the available transitions without applying one.
    #[serde(default)]
    pub list: Option<bool>,
}

/// Parameters for the `jira_transition_list` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct JiraTransitionListParams {
    /// JIRA issue key (e.g., `PROJ-123`).
    pub key: String,
}

/// Parameters for the `jira_comment` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct JiraCommentParams {
    /// JIRA issue key (e.g., `PROJ-123`).
    pub key: String,
    /// `list` to fetch comments; `add` to post a new one.
    pub action: String,
    /// Comment body (JFM markdown — see resource `omni-dev://specs/jfm`).
    /// Required for `action = "add"`.
    #[serde(default)]
    pub body: Option<String>,
    /// Maximum number of comments to return. `0` means unlimited.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Visibility restriction payload for the `jira_comment_edit` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct JiraVisibilityParam {
    /// Restriction kind — `"group"` or `"role"`.
    #[serde(rename = "type")]
    pub ty: String,
    /// Group name or project role name.
    pub value: String,
}

/// Parameters for the `jira_comment_edit` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct JiraCommentEditParams {
    /// JIRA issue key (e.g., `PROJ-123`).
    pub key: String,
    /// Comment ID to update.
    pub comment_id: String,
    /// New comment body (JFM markdown — see resource `omni-dev://specs/jfm`).
    pub body: String,
    /// Optional visibility restriction. Many JIRA configurations only allow
    /// the comment author to change visibility — JIRA's response is surfaced
    /// as-is when permission is denied.
    #[serde(default)]
    pub visibility: Option<JiraVisibilityParam>,
}

/// Parameters for the `jira_link` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct JiraLinkParams {
    /// Action: `list`, `types`, `create`, `remove`, or `parent`.
    pub action: String,
    /// Issue key. Required for `list`; for `create`, this is the source
    /// (inward) issue; for `parent`, this is the child issue. Ignored for
    /// `types`.
    #[serde(default)]
    pub key: Option<String>,
    /// Target issue key. Required for `create` (the outward issue) and for
    /// `parent` (the parent issue).
    #[serde(default)]
    pub target: Option<String>,
    /// Link type name (for `create`), e.g., `Blocks`.
    #[serde(default)]
    pub link_type: Option<String>,
    /// Link id (for `remove`).
    #[serde(default)]
    pub link_id: Option<String>,
}

/// Parameters for the `jira_dev` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct JiraDevParams {
    /// JIRA issue key (e.g., `PROJ-123`).
    pub key: String,
}

/// Parameters for the `jira_user_search` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct JiraUserSearchParams {
    /// Search text — matched against display name and email substrings.
    pub query: String,
    /// Maximum number of results (`0` = unlimited). Defaults to 25.
    #[serde(default)]
    pub limit: Option<u32>,
}

// ── format helpers ─────────────────────────────────────────────────────────

/// Output format for JIRA read/write operations.
#[derive(Debug)]
enum ReadFormat {
    Jfm,
    Adf,
}

impl ReadFormat {
    fn parse(raw: Option<&str>) -> Result<Self> {
        match raw.map(str::to_ascii_lowercase).as_deref() {
            None | Some("jfm") => Ok(Self::Jfm),
            Some("adf") => Ok(Self::Adf),
            Some(other) => anyhow::bail!("unknown format {other:?} (expected 'jfm' or 'adf')"),
        }
    }

    /// String label used in [`super::output_file::WriteFileSummary`].
    fn label(&self) -> &'static str {
        match self {
            Self::Jfm => "jfm",
            Self::Adf => "adf",
        }
    }
}

fn yaml_result<T: serde::Serialize>(data: &T) -> Result<String> {
    serde_yaml::to_string(data).context("Failed to serialize result as YAML")
}

fn ok_text(text: String) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![Content::text(text)]))
}

// ── internal `run_*` implementations ───────────────────────────────────────
//
// Split out from the tool handlers so they can be tested against a
// wiremock-backed [`AtlassianClient`] without needing real credentials.

/// Fetches a JIRA issue and renders it in `format`. When `output_file` is
/// set, writes the rendered content to disk and returns a YAML summary
/// instead of the body.
async fn run_jira_read(
    client: &AtlassianClient,
    instance_url: &str,
    key: &str,
    format: ReadFormat,
    output_file: Option<&str>,
) -> Result<String> {
    let issue = client.get_issue(key).await?;
    let rendered = render_jira_issue(&issue, instance_url, &format)?;
    match output_file {
        Some(path) => write_to_file_yaml(path, &rendered, format.label()),
        None => Ok(rendered),
    }
}

/// Renders a fetched [`crate::atlassian::client::JiraIssue`] in the requested
/// format. Split out from [`run_jira_read`] so the rendering branch can be
/// unit-tested without going through the HTTP client.
fn render_jira_issue(
    issue: &crate::atlassian::client::JiraIssue,
    instance_url: &str,
    format: &ReadFormat,
) -> Result<String> {
    match format {
        ReadFormat::Jfm => issue_to_jfm_document(issue, instance_url)?.render(),
        ReadFormat::Adf => {
            let adf = issue
                .description_adf
                .clone()
                .unwrap_or(serde_json::Value::Null);
            serde_json::to_string_pretty(&adf).context("Failed to serialize ADF JSON")
        }
    }
}

/// Searches JIRA issues and returns the result as YAML.
async fn run_jira_search(client: &AtlassianClient, jql: &str, limit: u32) -> Result<String> {
    let result = client.search_issues(jql, limit).await?;
    yaml_result(&result)
}

/// Creates a JIRA issue and returns the new issue key.
async fn run_jira_create(
    client: &AtlassianClient,
    project: &str,
    summary: &str,
    description: Option<&str>,
    issue_type: &str,
) -> Result<String> {
    let adf = match description {
        Some(md) if !md.is_empty() => Some(markdown_to_adf(md)?),
        _ => None,
    };
    let created = client
        .create_issue(project, issue_type, summary, adf.as_ref(), &[])
        .await?;
    yaml_result(&created)
}

/// Updates a JIRA issue. Any combination of description (`content`),
/// `parent`, `assignee`, `reporter`, and arbitrary `fields` may be supplied;
/// absent inputs leave the corresponding JIRA values untouched. At least one
/// of these must be supplied.
#[allow(clippy::too_many_arguments)]
async fn run_jira_write(
    client: &AtlassianClient,
    key: &str,
    content: Option<&str>,
    format: ReadFormat,
    parent: Option<&str>,
    assignee: Option<&str>,
    reporter: Option<&str>,
    extra_fields: Option<&std::collections::BTreeMap<String, serde_json::Value>>,
) -> Result<String> {
    let adf: Option<AdfDocument> = match content {
        Some(c) => Some(match format {
            ReadFormat::Jfm => {
                if c.starts_with("---\n") {
                    let doc = JfmDocument::parse(c)?;
                    markdown_to_adf(&doc.body)?
                } else {
                    markdown_to_adf(c)?
                }
            }
            ReadFormat::Adf => serde_json::from_str(c).context("Failed to parse ADF JSON")?,
        }),
        None => None,
    };

    let mut merged: std::collections::BTreeMap<String, serde_json::Value> =
        std::collections::BTreeMap::new();
    if let Some(extras) = extra_fields {
        for (k, v) in extras {
            merged.insert(k.clone(), v.clone());
        }
    }

    apply_user_field_overrides(
        &mut merged,
        assignee,
        reporter,
        "the same key inside `fields`",
    )?;

    if adf.is_none() && merged.is_empty() && parent.is_none() {
        anyhow::bail!(
            "no changes supplied for {key}: provide `content`, `parent`, `assignee`, `reporter`, or `fields`"
        );
    }

    client
        .update_issue_with_custom_fields(key, adf.as_ref(), None, parent, &merged)
        .await?;
    Ok(format!("Updated {key}.\n"))
}

/// Lists or executes a transition on an issue. When `list` is true (or
/// `transition` is absent), returns the available transitions as YAML.
async fn run_jira_transition(
    client: &AtlassianClient,
    key: &str,
    transition: Option<&str>,
    comment: Option<&str>,
    list: bool,
) -> Result<String> {
    let transitions = client.get_transitions(key).await?;

    if list || transition.is_none() {
        return yaml_result(&transitions);
    }

    let target = transition.unwrap_or_default();
    let matched = resolve_transition(target, &transitions)?.clone();
    client
        .do_transition(key, &matched.id)
        .await
        .with_context(|| {
            format!(
                "failed to transition {key} to \"{name}\" (id: {id}); the workflow may require \
                 additional fields (assignee, resolution, screen-driven field) or this transition \
                 may not be valid from the current status — try `list = true` to confirm availability",
                name = matched.name,
                id = matched.id,
            )
        })?;

    if let Some(body) = comment.filter(|s| !s.is_empty()) {
        let adf = markdown_to_adf(body)?;
        client.add_comment(key, &adf).await?;
    }

    Ok(format!(
        "Transitioned {key} to \"{name}\" (id: {id}).\n",
        name = matched.name,
        id = matched.id
    ))
}

/// Lists available transitions for an issue and returns them as YAML.
async fn run_jira_transition_list(client: &AtlassianClient, key: &str) -> Result<String> {
    let transitions = client.get_transitions(key).await?;
    yaml_result(&transitions)
}

/// Resolves a transition by exact id or case-insensitive name match.
///
/// Replicated here rather than imported from the CLI module because the
/// CLI's `resolve_transition` is private to its module.
fn resolve_transition<'a>(
    target: &str,
    transitions: &'a [JiraTransition],
) -> Result<&'a JiraTransition> {
    if let Some(t) = transitions.iter().find(|t| t.id == target) {
        return Ok(t);
    }
    let target_lower = target.to_lowercase();
    let matches: Vec<_> = transitions
        .iter()
        .filter(|t| t.name.to_lowercase() == target_lower)
        .collect();
    match matches.len() {
        0 => {
            let names: Vec<_> = transitions
                .iter()
                .map(|t| format!("\"{}\" (id: {})", t.name, t.id))
                .collect();
            anyhow::bail!(
                "No transition matching \"{target}\" found. Available: {}",
                if names.is_empty() {
                    "none".to_string()
                } else {
                    names.join(", ")
                }
            );
        }
        1 => Ok(matches[0]),
        _ => {
            let dupes: Vec<_> = matches
                .iter()
                .map(|t| format!("\"{}\" (id: {})", t.name, t.id))
                .collect();
            anyhow::bail!(
                "Ambiguous transition \"{target}\": {}. Use the id instead.",
                dupes.join(", ")
            );
        }
    }
}

/// List or add a comment on an issue.
async fn run_jira_comment(
    client: &AtlassianClient,
    key: &str,
    action: &str,
    body: Option<&str>,
    limit: u32,
) -> Result<String> {
    match action {
        "list" => {
            let comments = client.get_comments(key, limit).await?;
            yaml_result(&comments)
        }
        "add" => {
            let text =
                body.ok_or_else(|| anyhow::anyhow!("`body` is required when action is \"add\""))?;
            let adf = markdown_to_adf(text)?;
            client.add_comment(key, &adf).await?;
            Ok(format!("Comment added to {key}.\n"))
        }
        other => {
            anyhow::bail!("unknown comment action {other:?} (expected \"list\" or \"add\")")
        }
    }
}

/// Edits an existing comment on an issue.
async fn run_jira_comment_edit(
    client: &AtlassianClient,
    key: &str,
    comment_id: &str,
    body: &str,
    visibility: Option<&JiraVisibilityParam>,
) -> Result<String> {
    let adf = markdown_to_adf(body)?;
    let visibility = visibility.map(parse_visibility).transpose()?;
    let updated = client
        .update_comment(key, comment_id, &adf, visibility.as_ref())
        .await?;
    yaml_result(&updated)
}

fn parse_visibility(param: &JiraVisibilityParam) -> Result<JiraVisibility> {
    let ty = match param.ty.to_ascii_lowercase().as_str() {
        "group" => JiraVisibilityType::Group,
        "role" => JiraVisibilityType::Role,
        other => {
            anyhow::bail!("unknown visibility type {other:?} (expected \"group\" or \"role\")")
        }
    };
    Ok(JiraVisibility {
        ty,
        value: param.value.clone(),
    })
}

/// Manages issue links.
async fn run_jira_link(
    client: &AtlassianClient,
    action: &str,
    key: Option<&str>,
    target: Option<&str>,
    link_type: Option<&str>,
    link_id: Option<&str>,
) -> Result<String> {
    match action {
        "list" => {
            let k = key.ok_or_else(|| anyhow::anyhow!("`key` is required for link list"))?;
            let links = client.get_issue_links(k).await?;
            yaml_result(&links)
        }
        "types" => {
            let types = client.get_link_types().await?;
            yaml_result(&types)
        }
        "create" => {
            let inward = key.ok_or_else(|| {
                anyhow::anyhow!("`key` (source issue) is required for link create")
            })?;
            let outward = target.ok_or_else(|| {
                anyhow::anyhow!("`target` is required for link create")
            })?;
            let lt = link_type.ok_or_else(|| {
                anyhow::anyhow!("`link_type` is required for link create")
            })?;
            client.create_issue_link(lt, inward, outward).await?;
            Ok(format!("Linked {inward} → {outward} ({lt}).\n"))
        }
        "remove" => {
            let id = link_id.ok_or_else(|| {
                anyhow::anyhow!("`link_id` is required for link remove")
            })?;
            client.remove_issue_link(id).await?;
            Ok(format!("Removed link {id}.\n"))
        }
        "parent" => {
            // Sets the JIRA system `parent` field — distinct from `create`,
            // which produces a relationship link (Blocks, Composition, etc.).
            // `key` is the child; `target` is the parent.
            let child = key.ok_or_else(|| {
                anyhow::anyhow!("`key` (child issue) is required for link parent")
            })?;
            let parent_key = target.ok_or_else(|| {
                anyhow::anyhow!("`target` (parent issue) is required for link parent")
            })?;
            client.set_issue_parent(child, parent_key).await?;
            Ok(format!("Set parent of {child} to {parent_key}.\n"))
        }
        other => anyhow::bail!(
            "unknown link action {other:?} (expected \"list\", \"types\", \"create\", \"remove\", or \"parent\")"
        ),
    }
}

/// Returns development status (PRs, branches, repositories) for an issue.
async fn run_jira_dev(client: &AtlassianClient, key: &str) -> Result<String> {
    let status = client.get_dev_status(key, None, None).await?;
    yaml_result(&status)
}

/// Searches JIRA users by name/email substring and returns the result as
/// YAML.
async fn run_jira_user_search(client: &AtlassianClient, query: &str, limit: u32) -> Result<String> {
    let result = client.search_jira_users(query, limit).await?;
    yaml_result(&result)
}

// ── tool router ────────────────────────────────────────────────────────────

#[allow(missing_docs)] // #[tool_router] generates a pub `jira_core_tool_router` fn.
#[tool_router(router = jira_core_tool_router, vis = "pub")]
impl OmniDevServer {
    /// Tool: fetch a JIRA issue as JFM markdown or raw ADF JSON.
    #[tool(
        description = "Fetch a JIRA issue. Returns JFM markdown (default, AI-friendly) \
                       or raw ADF JSON when `format = \"adf\"`. When `output_file` is set, \
                       the content is written to that path and the tool returns a short \
                       YAML summary (path/bytes/format) — useful for large issues."
    )]
    pub async fn jira_read(
        &self,
        Parameters(params): Parameters<JiraReadParams>,
    ) -> Result<CallToolResult, McpError> {
        let format = ReadFormat::parse(params.format.as_deref()).map_err(tool_error)?;
        let (client, instance_url) = create_client().map_err(tool_error)?;
        let text = run_jira_read(
            &client,
            &instance_url,
            &params.key,
            format,
            params.output_file.as_deref(),
        )
        .await
        .map_err(tool_error)?;
        ok_text(text)
    }

    /// Tool: search JIRA issues by JQL.
    #[tool(description = "Search JIRA issues using a JQL query. Returns matching issues as YAML.")]
    pub async fn jira_search(
        &self,
        Parameters(params): Parameters<JiraSearchParams>,
    ) -> Result<CallToolResult, McpError> {
        let limit = params.limit.unwrap_or(20);
        // `fields` is accepted but not forwarded: the underlying API only
        // returns the standard set. Keep it on the schema so callers can
        // grow into richer selections later without a breaking change.
        let _ = params.fields;
        let (client, _instance_url) = create_client().map_err(tool_error)?;
        let text = run_jira_search(&client, &params.jql, limit)
            .await
            .map_err(tool_error)?;
        ok_text(text)
    }

    /// Tool: create a new JIRA issue.
    #[tool(
        description = "Create a new JIRA issue. Returns the new issue key and self URL as YAML."
    )]
    pub async fn jira_create(
        &self,
        Parameters(params): Parameters<JiraCreateParams>,
    ) -> Result<CallToolResult, McpError> {
        let issue_type = params.issue_type.as_deref().unwrap_or("Task");
        let (client, _instance_url) = create_client().map_err(tool_error)?;
        let text = run_jira_create(
            &client,
            &params.project,
            &params.summary,
            params.description.as_deref(),
            issue_type,
        )
        .await
        .map_err(tool_error)?;
        ok_text(text)
    }

    /// Tool: update a JIRA issue's description, parent, assignee, reporter,
    /// or arbitrary fields.
    #[tool(
        description = "Update a JIRA issue. `content` updates the description (JFM markdown by \
                       default, or raw ADF JSON when `format = \"adf\"`); omit it to leave the \
                       description unchanged. JFM is GitHub-style markdown — see resource \
                       `omni-dev://specs/jfm` for syntax. `parent` sets the system parent field \
                       for hierarchy (Epic → Story, Story → Sub-task) — distinct from \
                       `jira_link` actions, which create relationship links (Blocks, \
                       Composition, etc.) rather than the parent field. `assignee`/`reporter` \
                       accept an `accountId` (use the empty string `\"\"` to clear, `\"-1\"` for \
                       JIRA automatic assignment); call `jira_user_search` first if you only \
                       have a name or email. `fields` is an escape hatch — a map of canonical \
                       JIRA field id to its API JSON value (e.g. `{\"priority\": {\"name\": \
                       \"High\"}}`) — for fields without a typed parameter. At least one of \
                       `content`, `parent`, `assignee`, `reporter`, or `fields` must be \
                       supplied."
    )]
    pub async fn jira_write(
        &self,
        Parameters(params): Parameters<JiraWriteParams>,
    ) -> Result<CallToolResult, McpError> {
        let format = ReadFormat::parse(params.format.as_deref()).map_err(tool_error)?;
        let (client, _instance_url) = create_client().map_err(tool_error)?;
        let text = run_jira_write(
            &client,
            &params.key,
            params.content.as_deref(),
            format,
            params.parent.as_deref(),
            params.assignee.as_deref(),
            params.reporter.as_deref(),
            params.fields.as_ref(),
        )
        .await
        .map_err(tool_error)?;
        ok_text(text)
    }

    /// Tool: list or execute a transition on a JIRA issue.
    #[tool(
        description = "Transition a JIRA issue to a new workflow status. Most common usage: \
                       pass the transition name in `transition`, e.g. `transition: \"In Progress\"`. \
                       The numeric id also works, e.g. `transition: \"31\"`. Names are matched \
                       case-insensitively. If unsure which transitions are valid from the issue's \
                       current status, call this tool first with `list = true` (or omit `transition`) \
                       to get the available `{id, name}` pairs as YAML, then call again with one of \
                       those names. Optionally posts `comment` (JFM markdown) after the transition succeeds."
    )]
    pub async fn jira_transition(
        &self,
        Parameters(params): Parameters<JiraTransitionParams>,
    ) -> Result<CallToolResult, McpError> {
        let list = params.list.unwrap_or(false);
        let (client, _instance_url) = create_client().map_err(tool_error)?;
        let text = run_jira_transition(
            &client,
            &params.key,
            params.transition.as_deref(),
            params.comment.as_deref(),
            list,
        )
        .await
        .map_err(tool_error)?;
        ok_text(text)
    }

    /// Tool: list workflow transitions available from the issue's current status.
    #[tool(
        description = "List the workflow transitions available from a JIRA issue's current status. \
                       Returns YAML with `{id, name, to_status, has_screen}` for each transition. \
                       Faster and lighter than `jira_read` when you only need the transition ids \
                       and names to feed into `jira_transition`. Equivalent to `jira_transition` \
                       with `list = true`, but exposed as a single-purpose tool for discoverability."
    )]
    pub async fn jira_transition_list(
        &self,
        Parameters(params): Parameters<JiraTransitionListParams>,
    ) -> Result<CallToolResult, McpError> {
        let (client, _instance_url) = create_client().map_err(tool_error)?;
        let text = run_jira_transition_list(&client, &params.key)
            .await
            .map_err(tool_error)?;
        ok_text(text)
    }

    /// Tool: list or add comments on a JIRA issue.
    #[tool(
        description = "Manage JIRA issue comments. `action = \"list\"` returns comments as YAML; \
                       `action = \"add\"` posts the given `body` (JFM markdown)."
    )]
    pub async fn jira_comment(
        &self,
        Parameters(params): Parameters<JiraCommentParams>,
    ) -> Result<CallToolResult, McpError> {
        let limit = params.limit.unwrap_or(0);
        let (client, _instance_url) = create_client().map_err(tool_error)?;
        let text = run_jira_comment(
            &client,
            &params.key,
            &params.action,
            params.body.as_deref(),
            limit,
        )
        .await
        .map_err(tool_error)?;
        ok_text(text)
    }

    /// Tool: edit an existing JIRA comment.
    #[tool(
        description = "Edit an existing JIRA comment. `body` is JFM markdown (see resource \
                       `omni-dev://specs/jfm`) and replaces the current comment text. Optional \
                       `visibility = {type: \"group\"|\"role\", value: <name>}` updates the \
                       restriction. JIRA enforces stricter permissions on edit than on add (often \
                       only the original author can edit) — when JIRA refuses, its error message \
                       is surfaced verbatim. Returns the updated comment metadata as YAML."
    )]
    pub async fn jira_comment_edit(
        &self,
        Parameters(params): Parameters<JiraCommentEditParams>,
    ) -> Result<CallToolResult, McpError> {
        let (client, _instance_url) = create_client().map_err(tool_error)?;
        let text = run_jira_comment_edit(
            &client,
            &params.key,
            &params.comment_id,
            &params.body,
            params.visibility.as_ref(),
        )
        .await
        .map_err(tool_error)?;
        ok_text(text)
    }

    /// Tool: manage issue links.
    #[tool(
        description = "Manage JIRA issue links and hierarchy. Actions: \"list\" (needs `key`), \
                       \"types\", \"create\" (needs `key`, `target`, `link_type`), \
                       \"remove\" (needs `link_id`), \"parent\" (needs `key` = child, \
                       `target` = parent — sets the system parent field for Epic → Story / \
                       Story → Sub-task hierarchy, distinct from relationship links)."
    )]
    pub async fn jira_link(
        &self,
        Parameters(params): Parameters<JiraLinkParams>,
    ) -> Result<CallToolResult, McpError> {
        let (client, _instance_url) = create_client().map_err(tool_error)?;
        let text = run_jira_link(
            &client,
            &params.action,
            params.key.as_deref(),
            params.target.as_deref(),
            params.link_type.as_deref(),
            params.link_id.as_deref(),
        )
        .await
        .map_err(tool_error)?;
        ok_text(text)
    }

    /// Tool: fetch development status (PRs, branches, repositories) for an issue.
    #[tool(
        description = "Fetch development status for a JIRA issue: linked pull requests, \
                       branches, and repositories as YAML."
    )]
    pub async fn jira_dev(
        &self,
        Parameters(params): Parameters<JiraDevParams>,
    ) -> Result<CallToolResult, McpError> {
        let (client, _instance_url) = create_client().map_err(tool_error)?;
        let text = run_jira_dev(&client, &params.key)
            .await
            .map_err(tool_error)?;
        ok_text(text)
    }

    /// Tool: search JIRA users by name or email substring.
    #[tool(
        description = "Search JIRA users by display-name or email substring. Returns matches as \
                       YAML — each entry includes `account_id`, `display_name`, `email_address` \
                       (often redacted by GDPR), `active`, and `account_type`. Use the returned \
                       `account_id` as input to `jira_write`'s `assignee` or `reporter` \
                       parameter. `limit` defaults to 25; pass `0` for unlimited. Atlassian \
                       matches substrings on display name and email — try a shorter or alternate \
                       spelling if the first attempt returns nothing."
    )]
    pub async fn jira_user_search(
        &self,
        Parameters(params): Parameters<JiraUserSearchParams>,
    ) -> Result<CallToolResult, McpError> {
        let limit = params.limit.unwrap_or(25);
        let (client, _instance_url) = create_client().map_err(tool_error)?;
        let text = run_jira_user_search(&client, &params.query, limit)
            .await
            .map_err(tool_error)?;
        ok_text(text)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // ── helpers ────────────────────────────────────────────────────────────

    fn mock_client(base_url: &str) -> AtlassianClient {
        AtlassianClient::new(base_url, "user@test.com", "token").unwrap()
    }

    // ── ReadFormat::parse ──────────────────────────────────────────────────

    #[test]
    fn read_format_defaults_to_jfm() {
        assert!(matches!(ReadFormat::parse(None).unwrap(), ReadFormat::Jfm));
    }

    #[test]
    fn read_format_parses_case_insensitively() {
        assert!(matches!(
            ReadFormat::parse(Some("JFM")).unwrap(),
            ReadFormat::Jfm
        ));
        assert!(matches!(
            ReadFormat::parse(Some("Adf")).unwrap(),
            ReadFormat::Adf
        ));
    }

    #[test]
    fn read_format_rejects_unknown() {
        let err = ReadFormat::parse(Some("xml")).unwrap_err();
        assert!(err.to_string().contains("unknown format"));
    }

    // ── run_jira_read ──────────────────────────────────────────────────────

    async fn mount_issue_response(server: &MockServer, key: &str, body: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path(format!("/rest/api/3/issue/{key}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    fn sample_issue_body() -> serde_json::Value {
        serde_json::json!({
            "key": "PROJ-1",
            "fields": {
                "summary": "Sample",
                "description": {
                    "version": 1,
                    "type": "doc",
                    "content": [{
                        "type": "paragraph",
                        "content": [{"type": "text", "text": "Hello from JIRA"}]
                    }]
                },
                "status": {"name": "Open"},
                "issuetype": {"name": "Task"},
                "assignee": {"displayName": "Alice"},
                "priority": null,
                "labels": ["backend"]
            }
        })
    }

    #[tokio::test]
    async fn run_jira_read_jfm_emits_frontmatter_and_body() {
        let server = MockServer::start().await;
        mount_issue_response(&server, "PROJ-1", sample_issue_body()).await;
        let client = mock_client(&server.uri());

        let rendered = run_jira_read(&client, &server.uri(), "PROJ-1", ReadFormat::Jfm, None)
            .await
            .unwrap();
        assert!(rendered.contains("key: PROJ-1"));
        assert!(rendered.contains("summary: Sample"));
        assert!(rendered.contains("Hello from JIRA"));
    }

    #[tokio::test]
    async fn run_jira_read_adf_returns_raw_json() {
        let server = MockServer::start().await;
        mount_issue_response(&server, "PROJ-1", sample_issue_body()).await;
        let client = mock_client(&server.uri());

        let json = run_jira_read(&client, &server.uri(), "PROJ-1", ReadFormat::Adf, None)
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "doc");
    }

    #[tokio::test]
    async fn run_jira_read_adf_null_description_emits_null_json() {
        let server = MockServer::start().await;
        mount_issue_response(
            &server,
            "PROJ-2",
            serde_json::json!({
                "key": "PROJ-2",
                "fields": {"summary": "No body"}
            }),
        )
        .await;
        let client = mock_client(&server.uri());

        let json = run_jira_read(&client, &server.uri(), "PROJ-2", ReadFormat::Adf, None)
            .await
            .unwrap();
        assert_eq!(json.trim(), "null");
    }

    #[tokio::test]
    async fn run_jira_read_propagates_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/NOPE-1"))
            .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());

        let err = run_jira_read(&client, &server.uri(), "NOPE-1", ReadFormat::Jfm, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn run_jira_read_jfm_writes_to_output_file() {
        let server = MockServer::start().await;
        mount_issue_response(&server, "PROJ-1", sample_issue_body()).await;
        let client = mock_client(&server.uri());

        let tmp = tempfile::tempdir().unwrap();
        let out_path = tmp.path().join("issue.md");
        let path_str = out_path.to_str().unwrap();

        let summary = run_jira_read(
            &client,
            &server.uri(),
            "PROJ-1",
            ReadFormat::Jfm,
            Some(path_str),
        )
        .await
        .unwrap();

        assert!(summary.contains(&format!("path: {path_str}")));
        assert!(summary.contains("format: jfm"));
        assert!(summary.contains("bytes:"));
        // Inline content must NOT leak into the summary.
        assert!(!summary.contains("Hello from JIRA"));

        let written = std::fs::read_to_string(&out_path).unwrap();
        assert!(written.contains("key: PROJ-1"));
        assert!(written.contains("Hello from JIRA"));
    }

    #[tokio::test]
    async fn run_jira_read_adf_writes_to_output_file() {
        let server = MockServer::start().await;
        mount_issue_response(&server, "PROJ-1", sample_issue_body()).await;
        let client = mock_client(&server.uri());

        let tmp = tempfile::tempdir().unwrap();
        let out_path = tmp.path().join("issue.json");
        let path_str = out_path.to_str().unwrap();

        let summary = run_jira_read(
            &client,
            &server.uri(),
            "PROJ-1",
            ReadFormat::Adf,
            Some(path_str),
        )
        .await
        .unwrap();

        assert!(summary.contains("format: adf"));
        let written = std::fs::read_to_string(&out_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(parsed["type"], "doc");
    }

    #[tokio::test]
    async fn run_jira_read_output_file_invalid_path_errors() {
        let server = MockServer::start().await;
        mount_issue_response(&server, "PROJ-1", sample_issue_body()).await;
        let client = mock_client(&server.uri());

        let err = run_jira_read(
            &client,
            &server.uri(),
            "PROJ-1",
            ReadFormat::Jfm,
            Some("/nonexistent_dir_zxq/out.md"),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    #[test]
    fn read_format_label_matches_expected_strings() {
        assert_eq!(ReadFormat::Jfm.label(), "jfm");
        assert_eq!(ReadFormat::Adf.label(), "adf");
    }

    // ── render_jira_issue ──────────────────────────────────────────────────

    fn issue_with_description(
        adf: Option<serde_json::Value>,
    ) -> crate::atlassian::client::JiraIssue {
        crate::atlassian::client::JiraIssue {
            key: "PROJ-1".to_string(),
            summary: "S".to_string(),
            description_adf: adf,
            status: None,
            issue_type: None,
            assignee: None,
            priority: None,
            labels: vec![],
            custom_fields: vec![],
        }
    }

    #[test]
    fn render_jira_issue_jfm_propagates_adf_parse_error() {
        // A JSON string is valid JSON but cannot deserialize into AdfDocument,
        // so issue_to_jfm_document errors out — exercising the `?` partial.
        let issue = issue_with_description(Some(serde_json::Value::String("not adf".into())));
        let err = render_jira_issue(&issue, "https://org", &ReadFormat::Jfm).unwrap_err();
        assert!(
            err.to_string().contains("Failed to parse ADF"),
            "got: {err}"
        );
    }

    #[test]
    fn render_jira_issue_adf_serialises_null_when_description_absent() {
        let issue = issue_with_description(None);
        let json = render_jira_issue(&issue, "https://org", &ReadFormat::Adf).unwrap();
        assert_eq!(json.trim(), "null");
    }

    #[tokio::test]
    async fn run_jira_read_propagates_render_error() {
        // Mock returns a JIRA description that's a JSON string (not an ADF
        // doc). render_jira_issue errors out, exercising the outer `?` on
        // `render_jira_issue(...)?` in run_jira_read.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "key": "PROJ-1",
                "fields": {
                    "summary": "Bad ADF",
                    "description": "this is not adf"
                }
            })))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());

        let err = run_jira_read(&client, &server.uri(), "PROJ-1", ReadFormat::Jfm, None)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("Failed to parse ADF"),
            "got: {err}"
        );
    }

    // ── run_jira_search ────────────────────────────────────────────────────

    #[tokio::test]
    async fn run_jira_search_yaml_output_includes_keys() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/3/search/jql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "issues": [
                    {"key": "PROJ-1", "fields": {"summary": "First"}},
                    {"key": "PROJ-2", "fields": {"summary": "Second"}}
                ],
                "total": 2
            })))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let yaml = run_jira_search(&client, "project = PROJ", 20)
            .await
            .unwrap();
        assert!(yaml.contains("PROJ-1"));
        assert!(yaml.contains("PROJ-2"));
        assert!(yaml.contains("total: 2"));
    }

    #[tokio::test]
    async fn run_jira_search_propagates_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/3/search/jql"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad jql"))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let err = run_jira_search(&client, "!bad!", 20).await.unwrap_err();
        assert!(err.to_string().contains("400"));
    }

    // ── run_jira_create ────────────────────────────────────────────────────

    #[tokio::test]
    async fn run_jira_create_returns_new_key() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/3/issue"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": "100",
                "key": "PROJ-100",
                "self": "https://example.atlassian.net/rest/api/3/issue/100"
            })))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let yaml = run_jira_create(&client, "PROJ", "A task", Some("Body text"), "Task")
            .await
            .unwrap();
        assert!(yaml.contains("PROJ-100"));
    }

    #[tokio::test]
    async fn run_jira_create_without_description_omits_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/3/issue"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": "100",
                "key": "PROJ-100",
                "self": "https://example.atlassian.net/rest/api/3/issue/100"
            })))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        run_jira_create(&client, "PROJ", "Terse", None, "Task")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_jira_create_propagates_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/3/issue"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = run_jira_create(&client, "PROJ", "Title", None, "Task")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("400"));
    }

    // ── run_jira_write ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn run_jira_write_jfm_from_markdown_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        run_jira_write(
            &client,
            "PROJ-1",
            Some("New body\n"),
            ReadFormat::Jfm,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_jira_write_jfm_from_frontmatter_strips_it() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let content = "---\ntype: jira\ninstance: https://org.atlassian.net\nkey: PROJ-1\nsummary: T\n---\n\nBody\n";
        run_jira_write(
            &client,
            "PROJ-1",
            Some(content),
            ReadFormat::Jfm,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_jira_write_adf_parses_raw_json() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        run_jira_write(
            &client,
            "PROJ-1",
            Some(r#"{"version":1,"type":"doc","content":[]}"#),
            ReadFormat::Adf,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_jira_write_adf_rejects_invalid_json() {
        let client = mock_client("http://127.0.0.1:1");
        let err = run_jira_write(
            &client,
            "PROJ-1",
            Some("not json"),
            ReadFormat::Adf,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("Failed to parse ADF JSON"));
    }

    #[tokio::test]
    async fn run_jira_write_propagates_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .respond_with(ResponseTemplate::new(400).set_body_string("Bad"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = run_jira_write(
            &client,
            "PROJ-1",
            Some("Body"),
            ReadFormat::Jfm,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("400"));
    }

    #[tokio::test]
    async fn run_jira_write_with_assignee_emits_account_id_payload() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .and(body_json(serde_json::json!({
                "fields": {
                    "description": {"version": 1, "type": "doc", "content": []},
                    "assignee": {"accountId": "abc123"}
                }
            })))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        run_jira_write(
            &client,
            "PROJ-1",
            Some(""),
            ReadFormat::Jfm,
            None,
            Some("abc123"),
            None,
            None,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_jira_write_with_assignee_dash_one_means_auto() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .and(body_json(serde_json::json!({
                "fields": {"assignee": {"accountId": "-1"}}
            })))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        run_jira_write(
            &client,
            "PROJ-1",
            None,
            ReadFormat::Jfm,
            None,
            Some("-1"),
            None,
            None,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_jira_write_with_empty_assignee_clears() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .and(body_json(serde_json::json!({
                "fields": {"assignee": null}
            })))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        run_jira_write(
            &client,
            "PROJ-1",
            None,
            ReadFormat::Jfm,
            None,
            Some(""),
            None,
            None,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_jira_write_with_reporter_emits_account_id_payload() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .and(body_json(serde_json::json!({
                "fields": {"reporter": {"accountId": "rep123"}}
            })))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        run_jira_write(
            &client,
            "PROJ-1",
            None,
            ReadFormat::Jfm,
            None,
            None,
            Some("rep123"),
            None,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_jira_write_extra_fields_merge_into_payload() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .and(body_json(serde_json::json!({
                "fields": {
                    "priority": {"name": "High"},
                    "labels": ["a", "b"]
                }
            })))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let mut extra = std::collections::BTreeMap::new();
        extra.insert("priority".to_string(), serde_json::json!({"name": "High"}));
        extra.insert("labels".to_string(), serde_json::json!(["a", "b"]));
        run_jira_write(
            &client,
            "PROJ-1",
            None,
            ReadFormat::Jfm,
            None,
            None,
            None,
            Some(&extra),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_jira_write_assignee_collision_with_fields_errors() {
        let client = mock_client("http://127.0.0.1:1");
        let mut extra = std::collections::BTreeMap::new();
        extra.insert(
            "assignee".to_string(),
            serde_json::json!({"accountId": "x"}),
        );
        let err = run_jira_write(
            &client,
            "PROJ-1",
            None,
            ReadFormat::Jfm,
            None,
            Some("y"),
            None,
            Some(&extra),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("collides"));
    }

    #[tokio::test]
    async fn run_jira_write_reporter_collision_with_fields_errors() {
        let client = mock_client("http://127.0.0.1:1");
        let mut extra = std::collections::BTreeMap::new();
        extra.insert(
            "reporter".to_string(),
            serde_json::json!({"accountId": "x"}),
        );
        let err = run_jira_write(
            &client,
            "PROJ-1",
            None,
            ReadFormat::Jfm,
            None,
            None,
            Some("y"),
            Some(&extra),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("collides"));
    }

    #[tokio::test]
    async fn run_jira_write_no_changes_errors() {
        let client = mock_client("http://127.0.0.1:1");
        let err = run_jira_write(
            &client,
            "PROJ-1",
            None,
            ReadFormat::Jfm,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no changes supplied"));
    }

    #[tokio::test]
    async fn run_jira_write_parent_only_sends_parent_key() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "fields": {"parent": {"key": "EPIC-1"}}
            })))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        run_jira_write(
            &client,
            "PROJ-1",
            None,
            ReadFormat::Jfm,
            Some("EPIC-1"),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_jira_write_content_and_parent_sends_both() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "fields": {
                    "description": {
                        "version": 1,
                        "type": "doc",
                        "content": [{
                            "type": "paragraph",
                            "content": [{"type": "text", "text": "Body"}]
                        }]
                    },
                    "parent": {"key": "EPIC-1"}
                }
            })))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        run_jira_write(
            &client,
            "PROJ-1",
            Some("Body"),
            ReadFormat::Jfm,
            Some("EPIC-1"),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    }

    // ── run_jira_transition ────────────────────────────────────────────────

    async fn mount_transitions(server: &MockServer, key: &str, body: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path(format!("/rest/api/3/issue/{key}/transitions")))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn run_jira_transition_list_returns_yaml() {
        let server = MockServer::start().await;
        mount_transitions(
            &server,
            "PROJ-1",
            serde_json::json!({
                "transitions": [
                    {"id": "11", "name": "In Progress"},
                    {"id": "21", "name": "Done"}
                ]
            }),
        )
        .await;

        let client = mock_client(&server.uri());
        let yaml = run_jira_transition(&client, "PROJ-1", None, None, true)
            .await
            .unwrap();
        assert!(yaml.contains("In Progress"));
        assert!(yaml.contains("Done"));
    }

    #[tokio::test]
    async fn run_jira_transition_missing_transition_lists_available() {
        let server = MockServer::start().await;
        mount_transitions(
            &server,
            "PROJ-1",
            serde_json::json!({"transitions": [{"id": "11", "name": "In Progress"}]}),
        )
        .await;

        let client = mock_client(&server.uri());
        let yaml = run_jira_transition(&client, "PROJ-1", None, None, false)
            .await
            .unwrap();
        assert!(yaml.contains("In Progress"));
    }

    #[tokio::test]
    async fn run_jira_transition_executes_by_name() {
        let server = MockServer::start().await;
        mount_transitions(
            &server,
            "PROJ-1",
            serde_json::json!({
                "transitions": [
                    {"id": "11", "name": "In Progress"},
                    {"id": "21", "name": "Done"}
                ]
            }),
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/rest/api/3/issue/PROJ-1/transitions"))
            .and(body_json(serde_json::json!({"transition": {"id": "21"}})))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let result = run_jira_transition(&client, "PROJ-1", Some("Done"), None, false)
            .await
            .unwrap();
        assert!(result.contains("Transitioned"));
        assert!(result.contains("Done"));
    }

    #[tokio::test]
    async fn run_jira_transition_posts_comment_when_provided() {
        let server = MockServer::start().await;
        mount_transitions(
            &server,
            "PROJ-1",
            serde_json::json!({"transitions": [{"id": "21", "name": "Done"}]}),
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/rest/api/3/issue/PROJ-1/transitions"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rest/api/3/issue/PROJ-1/comment"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({"id": "c1"})))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        run_jira_transition(&client, "PROJ-1", Some("Done"), Some("nice"), false)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_jira_transition_get_transitions_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/PROJ-1/transitions"))
            .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = run_jira_transition(&client, "PROJ-1", None, None, true)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn run_jira_transition_do_transition_api_error() {
        let server = MockServer::start().await;
        mount_transitions(
            &server,
            "PROJ-1",
            serde_json::json!({"transitions": [{"id": "21", "name": "Done"}]}),
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/rest/api/3/issue/PROJ-1/transitions"))
            .respond_with(ResponseTemplate::new(400).set_body_string("Bad"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = run_jira_transition(&client, "PROJ-1", Some("Done"), None, false)
            .await
            .unwrap_err();
        let chain = format!("{err:#}");
        assert!(
            chain.contains("400"),
            "expected 400 in error chain, got: {chain}"
        );
        assert!(
            chain.contains("list = true"),
            "expected hint about `list = true` in error chain, got: {chain}",
        );
        assert!(
            chain.contains("workflow may require"),
            "expected hint about workflow requirements in error chain, got: {chain}",
        );
    }

    #[tokio::test]
    async fn run_jira_transition_add_comment_api_error() {
        let server = MockServer::start().await;
        mount_transitions(
            &server,
            "PROJ-1",
            serde_json::json!({"transitions": [{"id": "21", "name": "Done"}]}),
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/rest/api/3/issue/PROJ-1/transitions"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rest/api/3/issue/PROJ-1/comment"))
            .respond_with(ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = run_jira_transition(&client, "PROJ-1", Some("Done"), Some("nice"), false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn run_jira_transition_unknown_name_errors() {
        let server = MockServer::start().await;
        mount_transitions(
            &server,
            "PROJ-1",
            serde_json::json!({"transitions": [{"id": "11", "name": "In Progress"}]}),
        )
        .await;
        let client = mock_client(&server.uri());
        let err = run_jira_transition(&client, "PROJ-1", Some("Nope"), None, false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("No transition matching"));
    }

    // ── run_jira_transition_list ───────────────────────────────────────────

    #[tokio::test]
    async fn run_jira_transition_list_happy_path() {
        let server = MockServer::start().await;
        mount_transitions(
            &server,
            "PROJ-1",
            serde_json::json!({
                "transitions": [
                    {
                        "id": "21",
                        "name": "In Progress",
                        "hasScreen": false,
                        "to": {
                            "id": "3",
                            "name": "In Progress",
                            "statusCategory": {"key": "indeterminate"}
                        }
                    },
                    {
                        "id": "31",
                        "name": "Done",
                        "hasScreen": false,
                        "to": {
                            "id": "10000",
                            "name": "Done",
                            "statusCategory": {"key": "done"}
                        }
                    }
                ]
            }),
        )
        .await;

        let client = mock_client(&server.uri());
        let yaml = run_jira_transition_list(&client, "PROJ-1").await.unwrap();
        assert!(yaml.contains("In Progress"));
        assert!(yaml.contains("Done"));
        assert!(yaml.contains("to_status:"));
        assert!(yaml.contains("category: indeterminate"));
        assert!(yaml.contains("category: done"));
        assert!(yaml.contains("has_screen: false"));
    }

    #[tokio::test]
    async fn run_jira_transition_list_issue_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/NOPE-1/transitions"))
            .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let err = run_jira_transition_list(&client, "NOPE-1")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn run_jira_transition_list_empty() {
        let server = MockServer::start().await;
        mount_transitions(&server, "PROJ-1", serde_json::json!({"transitions": []})).await;

        let client = mock_client(&server.uri());
        let yaml = run_jira_transition_list(&client, "PROJ-1").await.unwrap();
        assert_eq!(yaml, "[]\n");
    }

    // ── resolve_transition ─────────────────────────────────────────────────

    fn t(id: &str, name: &str) -> JiraTransition {
        JiraTransition {
            id: id.to_string(),
            name: name.to_string(),
            to_status: None,
            has_screen: None,
        }
    }

    #[test]
    fn resolve_transition_exact_id_wins() {
        let ts = [t("Done", "Anything"), t("99", "Done")];
        assert_eq!(resolve_transition("Done", &ts).unwrap().name, "Anything");
    }

    #[test]
    fn resolve_transition_case_insensitive_name() {
        let ts = [t("11", "Done")];
        assert_eq!(resolve_transition("done", &ts).unwrap().id, "11");
    }

    #[test]
    fn resolve_transition_empty_list() {
        let err = resolve_transition("Done", &[]).unwrap_err();
        assert!(err.to_string().contains("none"));
    }

    #[test]
    fn resolve_transition_ambiguous_errors() {
        let ts = [t("11", "Done"), t("22", "done")];
        let err = resolve_transition("Done", &ts).unwrap_err();
        assert!(err.to_string().contains("Ambiguous"));
    }

    // ── run_jira_comment ───────────────────────────────────────────────────

    #[tokio::test]
    async fn run_jira_comment_list_returns_yaml() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/PROJ-1/comment"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "startAt": 0,
                "maxResults": 100,
                "total": 1,
                "comments": [{
                    "id": "1",
                    "author": {"displayName": "Alice"},
                    "created": "2026-04-01T10:00:00.000+0000",
                    "body": null
                }]
            })))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let yaml = run_jira_comment(&client, "PROJ-1", "list", None, 0)
            .await
            .unwrap();
        assert!(yaml.contains("Alice"));
    }

    #[tokio::test]
    async fn run_jira_comment_list_forwards_limit() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/PROJ-1/comment"))
            .and(query_param("maxResults", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "startAt": 0, "maxResults": 2, "total": 0, "comments": []
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        run_jira_comment(&client, "PROJ-1", "list", None, 2)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_jira_comment_add_posts_comment() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/3/issue/PROJ-1/comment"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({"id": "1"})))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        run_jira_comment(&client, "PROJ-1", "add", Some("hello"), 0)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_jira_comment_add_without_body_errors() {
        let client = mock_client("http://127.0.0.1:1");
        let err = run_jira_comment(&client, "PROJ-1", "add", None, 0)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("`body` is required"));
    }

    #[tokio::test]
    async fn run_jira_comment_unknown_action_errors() {
        let client = mock_client("http://127.0.0.1:1");
        let err = run_jira_comment(&client, "PROJ-1", "delete", None, 0)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown comment action"));
    }

    #[tokio::test]
    async fn run_jira_comment_list_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/NOPE-1/comment"))
            .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = run_jira_comment(&client, "NOPE-1", "list", None, 0)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn run_jira_comment_add_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/3/issue/PROJ-1/comment"))
            .respond_with(ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = run_jira_comment(&client, "PROJ-1", "add", Some("hi"), 0)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    // ── run_jira_comment_edit ──────────────────────────────────────────────

    #[tokio::test]
    async fn run_jira_comment_edit_success() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/issue/PROJ-1/comment/100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "100",
                "author": {"displayName": "Me"},
                "created": "2026-04-01T10:00:00.000+0000",
                "updated": "2026-05-10T12:00:00.000+0000",
                "body": null
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let yaml = run_jira_comment_edit(&client, "PROJ-1", "100", "hello", None)
            .await
            .unwrap();
        assert!(yaml.contains("id: '100'") || yaml.contains("id: \"100\""));
        assert!(yaml.contains("2026-05-10T12:00:00"));
    }

    #[tokio::test]
    async fn run_jira_comment_edit_with_visibility() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/issue/PROJ-1/comment/100"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "visibility": {"type": "group", "identifier": "jira-administrators"}
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "100",
                "author": {"displayName": "Me"},
                "created": "2026-04-01T10:00:00.000+0000",
                "updated": "2026-05-10T12:00:00.000+0000",
                "body": null
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let visibility = JiraVisibilityParam {
            ty: "group".to_string(),
            value: "jira-administrators".to_string(),
        };
        run_jira_comment_edit(&client, "PROJ-1", "100", "hello", Some(&visibility))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_jira_comment_edit_forbidden_surfaces_jira_message() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/issue/PROJ-1/comment/100"))
            .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
                "errorMessages": ["You do not have permission to edit this comment"]
            })))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let err = run_jira_comment_edit(&client, "PROJ-1", "100", "hello", None)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("403"));
        assert!(msg.contains("permission to edit"));
    }

    #[tokio::test]
    async fn run_jira_comment_edit_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/issue/PROJ-1/comment/9999"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "errorMessages": ["Comment not found"]
            })))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let err = run_jira_comment_edit(&client, "PROJ-1", "9999", "hello", None)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("404"));
        assert!(msg.contains("Comment not found"));
    }

    #[tokio::test]
    async fn run_jira_comment_edit_unknown_visibility_type() {
        let client = mock_client("http://127.0.0.1:1");
        let visibility = JiraVisibilityParam {
            ty: "user".to_string(),
            value: "alice".to_string(),
        };
        let err = run_jira_comment_edit(&client, "PROJ-1", "100", "hello", Some(&visibility))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown visibility type"));
    }

    #[test]
    fn parse_visibility_group() {
        let v = parse_visibility(&JiraVisibilityParam {
            ty: "group".to_string(),
            value: "jira-users".to_string(),
        })
        .unwrap();
        assert!(matches!(v.ty, JiraVisibilityType::Group));
        assert_eq!(v.value, "jira-users");
    }

    #[test]
    fn parse_visibility_role() {
        let v = parse_visibility(&JiraVisibilityParam {
            ty: "role".to_string(),
            value: "Administrators".to_string(),
        })
        .unwrap();
        assert!(matches!(v.ty, JiraVisibilityType::Role));
        assert_eq!(v.value, "Administrators");
    }

    #[test]
    fn parse_visibility_case_insensitive() {
        let v = parse_visibility(&JiraVisibilityParam {
            ty: "ROLE".to_string(),
            value: "Administrators".to_string(),
        })
        .unwrap();
        assert!(matches!(v.ty, JiraVisibilityType::Role));
    }

    // ── run_jira_link ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn run_jira_link_types_returns_yaml() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issueLinkType"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "issueLinkTypes": [
                    {"id": "1", "name": "Blocks", "inward": "is blocked by", "outward": "blocks"}
                ]
            })))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = run_jira_link(&client, "types", None, None, None, None)
            .await
            .unwrap();
        assert!(yaml.contains("Blocks"));
    }

    #[tokio::test]
    async fn run_jira_link_list_returns_yaml() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "key": "PROJ-1",
                "fields": {
                    "summary": "s",
                    "issuelinks": []
                }
            })))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        run_jira_link(&client, "list", Some("PROJ-1"), None, None, None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_jira_link_list_requires_key() {
        let client = mock_client("http://127.0.0.1:1");
        let err = run_jira_link(&client, "list", None, None, None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("`key` is required"));
    }

    #[tokio::test]
    async fn run_jira_link_create_posts_link() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/3/issueLink"))
            .respond_with(ResponseTemplate::new(201))
            .expect(1)
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        run_jira_link(
            &client,
            "create",
            Some("PROJ-1"),
            Some("PROJ-2"),
            Some("Blocks"),
            None,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_jira_link_create_requires_target() {
        let client = mock_client("http://127.0.0.1:1");
        let err = run_jira_link(
            &client,
            "create",
            Some("PROJ-1"),
            None,
            Some("Blocks"),
            None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("`target` is required"));
    }

    #[tokio::test]
    async fn run_jira_link_create_requires_link_type() {
        let client = mock_client("http://127.0.0.1:1");
        let err = run_jira_link(
            &client,
            "create",
            Some("PROJ-1"),
            Some("PROJ-2"),
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("`link_type` is required"));
    }

    #[tokio::test]
    async fn run_jira_link_create_requires_key() {
        let client = mock_client("http://127.0.0.1:1");
        let err = run_jira_link(
            &client,
            "create",
            None,
            Some("PROJ-2"),
            Some("Blocks"),
            None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("`key` (source issue) is required"));
    }

    #[tokio::test]
    async fn run_jira_link_remove_deletes() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/rest/api/3/issueLink/42"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        run_jira_link(&client, "remove", None, None, None, Some("42"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_jira_link_remove_requires_link_id() {
        let client = mock_client("http://127.0.0.1:1");
        let err = run_jira_link(&client, "remove", None, None, None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("`link_id` is required"));
    }

    #[tokio::test]
    async fn run_jira_link_unknown_action_errors() {
        let client = mock_client("http://127.0.0.1:1");
        let err = run_jira_link(&client, "frob", None, None, None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown link action"));
    }

    #[tokio::test]
    async fn run_jira_link_parent_sets_parent_field() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/issue/PROJ-2"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "fields": {"parent": {"key": "EPIC-1"}}
            })))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let out = run_jira_link(
            &client,
            "parent",
            Some("PROJ-2"),
            Some("EPIC-1"),
            None,
            None,
        )
        .await
        .unwrap();
        assert!(out.contains("Set parent of PROJ-2 to EPIC-1"));
    }

    #[tokio::test]
    async fn run_jira_link_parent_requires_key() {
        let client = mock_client("http://127.0.0.1:1");
        let err = run_jira_link(&client, "parent", None, Some("EPIC-1"), None, None)
            .await
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("`key` (child issue) is required for link parent"));
    }

    #[tokio::test]
    async fn run_jira_link_parent_requires_target() {
        let client = mock_client("http://127.0.0.1:1");
        let err = run_jira_link(&client, "parent", Some("PROJ-2"), None, None, None)
            .await
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("`target` (parent issue) is required for link parent"));
    }

    #[tokio::test]
    async fn run_jira_link_parent_propagates_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/issue/PROJ-2"))
            .respond_with(ResponseTemplate::new(400).set_body_string("Not allowed"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = run_jira_link(
            &client,
            "parent",
            Some("PROJ-2"),
            Some("EPIC-1"),
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("400"));
    }

    #[tokio::test]
    async fn run_jira_link_list_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/NOPE-1"))
            .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = run_jira_link(&client, "list", Some("NOPE-1"), None, None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn run_jira_link_types_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issueLinkType"))
            .respond_with(ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = run_jira_link(&client, "types", None, None, None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn run_jira_link_create_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/3/issueLink"))
            .respond_with(ResponseTemplate::new(400).set_body_string("Bad"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = run_jira_link(
            &client,
            "create",
            Some("PROJ-1"),
            Some("PROJ-2"),
            Some("Blocks"),
            None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("400"));
    }

    #[tokio::test]
    async fn run_jira_link_remove_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/rest/api/3/issueLink/99"))
            .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = run_jira_link(&client, "remove", None, None, None, Some("99"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    // ── run_jira_dev ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn run_jira_dev_returns_yaml_for_empty_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"id": "10001", "key": "PROJ-1", "fields": {}}),
                ),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/rest/dev-status/1.0/issue/summary"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "summary": {
                    "pullrequest": {"overall": {"count": 0}, "byInstanceType": {}},
                    "branch": {"overall": {"count": 0}, "byInstanceType": {}},
                    "repository": {"overall": {"count": 0}, "byInstanceType": {}}
                }
            })))
            .mount(&server)
            .await;
        // No providers in summary → get_dev_status falls back to "GitHub",
        // then queries the detail endpoint for each data type.
        Mock::given(method("GET"))
            .and(path("/rest/dev-status/1.0/issue/detail"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "detail": [{
                    "pullRequests": [],
                    "branches": [],
                    "repositories": []
                }]
            })))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let yaml = run_jira_dev(&client, "PROJ-1").await.unwrap();
        // Empty vectors are skipped by `serde(skip_serializing_if)`, so the
        // result is an empty YAML mapping.
        assert_eq!(yaml.trim(), "{}");
    }

    #[tokio::test]
    async fn run_jira_dev_includes_populated_categories() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"id": "10001", "key": "PROJ-1", "fields": {}}),
                ),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/rest/dev-status/1.0/issue/summary"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "summary": {
                    "pullrequest": {"overall": {"count": 0}, "byInstanceType": {}},
                    "branch": {"overall": {"count": 0}, "byInstanceType": {}},
                    "repository": {"overall": {"count": 0}, "byInstanceType": {}}
                }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/rest/dev-status/1.0/issue/detail"))
            .and(query_param("dataType", "pullrequest"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "detail": [{
                    "pullRequests": [{
                        "id": "#42",
                        "name": "Fix bug",
                        "status": "OPEN",
                        "url": "https://github.com/o/r/pull/42",
                        "repositoryName": "o/r",
                        "source": {"branch": "fix"},
                        "destination": {"branch": "main"}
                    }],
                    "branches": [],
                    "repositories": []
                }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/rest/dev-status/1.0/issue/detail"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "detail": [{"pullRequests": [], "branches": [], "repositories": []}]
            })))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let yaml = run_jira_dev(&client, "PROJ-1").await.unwrap();
        assert!(yaml.contains("pull_requests"));
        assert!(yaml.contains("Fix bug"));
    }

    #[tokio::test]
    async fn run_jira_dev_propagates_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/NOPE-1"))
            .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = run_jira_dev(&client, "NOPE-1").await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    // ── tool_error wrapping ────────────────────────────────────────────────

    #[test]
    fn tool_error_wraps_anyhow_chain() {
        let err: anyhow::Error = anyhow::anyhow!("root").context("middle").context("top");
        let mcp = tool_error(err);
        assert!(mcp.message.contains("top"));
        assert!(mcp.message.contains("Caused by: middle"));
        assert!(mcp.message.contains("Caused by: root"));
    }

    // ── run_jira_user_search ───────────────────────────────────────────────

    #[tokio::test]
    async fn run_jira_user_search_yaml_includes_account_id_and_active() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/user/search"))
            .and(query_param("query", "alice"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {
                    "accountId": "abc123",
                    "displayName": "Alice Smith",
                    "emailAddress": "alice@example.com",
                    "active": true,
                    "accountType": "atlassian"
                }
            ])))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let yaml = run_jira_user_search(&client, "alice", 25).await.unwrap();
        assert!(yaml.contains("account_id: abc123"));
        assert!(yaml.contains("display_name: Alice Smith"));
        assert!(yaml.contains("active: true"));
        assert!(yaml.contains("count: 1"));
    }

    #[tokio::test]
    async fn run_jira_user_search_no_matches_yields_empty_users() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/user/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .expect(1)
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let yaml = run_jira_user_search(&client, "nobody", 25).await.unwrap();
        assert!(yaml.contains("count: 0"));
    }

    #[tokio::test]
    async fn run_jira_user_search_propagates_403() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/user/search"))
            .respond_with(ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = run_jira_user_search(&client, "alice", 25)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    // ── ok_text / yaml_result helpers ──────────────────────────────────────

    #[test]
    fn yaml_result_serializes_vec() {
        let v = vec![1_u32, 2, 3];
        let s = yaml_result(&v).unwrap();
        assert_eq!(s, "- 1\n- 2\n- 3\n");
    }

    #[test]
    fn ok_text_returns_success_result() {
        let result = ok_text("hello".to_string()).unwrap();
        assert!(!result.is_error.unwrap_or(false));
        assert_eq!(result.content.len(), 1);
    }
}
