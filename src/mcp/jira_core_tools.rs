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
use crate::atlassian::adf_validated::ValidatedAdfDocument;
use crate::atlassian::client::{
    AtlassianClient, JiraTransition, JiraVisibility, JiraVisibilityType,
};
use crate::atlassian::convert::markdown_to_adf;
use crate::atlassian::create::{create_resolved_jira_issue, prepend_warnings, resolve_jira_create};
use crate::atlassian::custom_fields::{
    apply_user_field_overrides, convert_textarea_string_values, resolve_custom_fields,
};
use crate::atlassian::document::{issue_to_jfm_document, JfmDocument};
use crate::cli::atlassian::helpers::create_client;

use super::catalogue_cache::CatalogueCache;
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
    /// Full JFM document (YAML frontmatter + markdown body), e.g. the output
    /// of `jira_read` with the frontmatter edited. When provided, `project`,
    /// `summary`, `issue_type`, labels and custom fields are taken from the
    /// frontmatter (the project derives from `key:` when no `project:` is set)
    /// and the body becomes the description — so the read → edit → create
    /// round-trip works without re-specifying fields. The `project`/`summary`/
    /// `issue_type` parameters below still override their frontmatter
    /// counterparts (a warning is returned when they do); passing `description`
    /// or `custom_fields` together with `document` is an error (put custom fields
    /// in the document's `custom_fields:` frontmatter). See resource
    /// `omni-dev://specs/jfm`.
    #[serde(default)]
    pub document: Option<String>,
    /// Project key (e.g., `PROJ`). Required unless `document` carries a
    /// `project:` (or a `key:` it can be derived from). Overrides frontmatter.
    #[serde(default)]
    pub project: Option<String>,
    /// Issue summary / title. Required unless `document` carries one.
    /// Overrides frontmatter.
    #[serde(default)]
    pub summary: Option<String>,
    /// Optional description in JFM markdown — see resource
    /// `omni-dev://specs/jfm` for syntax. JFM is GitHub-style markdown,
    /// NOT JIRA wiki markup (use `##` not `h2.`, triple-backtick fences not
    /// `{code}`, backtick inline code not `{{...}}`). Rejected when `document`
    /// is provided (the document body is the description).
    #[serde(default)]
    pub description: Option<String>,
    /// Issue type (defaults to `Task`). Overrides frontmatter.
    #[serde(default)]
    pub issue_type: Option<String>,
    /// Custom fields to set at create time, as a map of field name *or*
    /// canonical id (e.g. `"Story Points"` or `"customfield_10016"`) to its
    /// value. Names are resolved against the project/issue-type create screen
    /// (`createmeta`), so pass the name back from a `400`
    /// "`<Field> is required`" error directly. Values are natural JSON: a
    /// string or number for scalar/number/date fields, a string for
    /// select/option fields (sent as `{"value": ...}`), an array of strings
    /// for multi-selects. Use this for fields a project requires at create
    /// time — without them JIRA rejects the create with HTTP 400.
    #[serde(default)]
    pub custom_fields: Option<std::collections::BTreeMap<String, serde_json::Value>>,
}

/// Parameters for the `jira_write` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct JiraWriteParams {
    /// JIRA issue key (e.g., `PROJ-123`).
    pub key: String,
    /// New description body. Interpreted per `format`. Omit to leave the
    /// existing description unchanged (useful when only updating
    /// `assignee`/`reporter`/`fields`).
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
    /// Assignee `accountId`. The empty string `""` clears the assignee;
    /// `"-1"` triggers JIRA automatic assignment. Use `jira_user_search` to
    /// resolve a name or email to an `accountId`.
    #[serde(default)]
    pub assignee: Option<String>,
    /// Reporter `accountId`. Same conventions as `assignee` (`""` clears,
    /// `"-1"` is JIRA automatic).
    #[serde(default)]
    pub reporter: Option<String>,
    /// Additional `fields` keys merged into the issue update payload.
    /// Keys must already be canonical JIRA field ids (e.g. `priority`,
    /// `labels`, `customfield_10010`). Values must already be in the API's
    /// JSON shape (e.g. `{"name": "High"}` for priority,
    /// `["a", "b"]` for labels) — with one ergonomic exception: a string
    /// value targeting a rich-text textarea custom field (e.g.
    /// `{"customfield_19300": "- bullet\n- bullet"}`) is auto-converted
    /// from JFM markdown to ADF, and the empty string `""` clears such a
    /// field. Pass a JSON object instead of a string to bypass conversion
    /// (raw ADF). Setting `assignee` or `reporter` here collides with the
    /// typed parameters and is rejected — pass the typed parameter instead.
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

/// Creates a JIRA issue and returns the new issue key as YAML.
///
/// Two modes: from a full JFM `document` (frontmatter resolved like the CLI,
/// with full label/custom-field parity via the shared create path), or from
/// explicit `project`/`summary` fields plus an optional `custom_fields` map.
/// When the document path shadows a frontmatter value with an explicit
/// parameter, a `warning:` line is prepended to the returned text (and logged)
/// so the assistant has a signal.
async fn run_jira_create(client: &AtlassianClient, params: &JiraCreateParams) -> Result<String> {
    if let Some(document) = params.document.as_deref() {
        if params.description.is_some() {
            anyhow::bail!(
                "Provide either `document` or `description`, not both — the document body \
                 becomes the description"
            );
        }
        if params.custom_fields.is_some() {
            anyhow::bail!(
                "Provide either `document` or `custom_fields`, not both — put custom fields in \
                 the document's `custom_fields:` frontmatter"
            );
        }

        let resolved = resolve_jira_create(
            document,
            params.project.as_deref(),
            params.summary.as_deref(),
            params.issue_type.as_deref(),
            vec![],
        )?;
        for shadowed in &resolved.shadowed {
            tracing::warn!("{}", shadowed.warning_line());
        }

        let created = create_resolved_jira_issue(
            client,
            &resolved.project,
            &resolved.issue_type,
            &resolved.summary,
            &resolved.adf,
            &resolved.labels,
            &resolved.custom_scalars,
            &resolved.custom_sections,
        )
        .await?;

        return Ok(prepend_warnings(&resolved.shadowed, yaml_result(&created)?));
    }

    let project = params.project.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "`project` is required (or provide a `document` whose frontmatter carries \
             `project:`/`key:`)"
        )
    })?;
    let summary = params.summary.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "`summary` is required (or provide a `document` whose frontmatter carries `summary:`)"
        )
    })?;
    let issue_type = params.issue_type.as_deref().unwrap_or("Task");

    let adf = match params.description.as_deref() {
        Some(md) if !md.is_empty() => Some(ValidatedAdfDocument::try_new(markdown_to_adf(md)?)?),
        _ => None,
    };

    let resolved = match params.custom_fields.as_ref() {
        Some(fields) if !fields.is_empty() => {
            // The CLI's resolver works on YAML scalars (numbers/bools/strings)
            // so it can coerce inline `NAME=VALUE` flags; bridge the JSON
            // values from the MCP request into the same shape before reusing
            // it. A `serde_json::Value` always serialises into a
            // `serde_yaml::Value` (JSON is a subset of YAML), so `.ok()` never
            // actually drops a field — this mirrors the same conversion in
            // `document.rs::json_to_yaml`.
            let scalars: std::collections::BTreeMap<String, serde_yaml::Value> = fields
                .iter()
                .filter_map(|(name, value)| {
                    serde_yaml::to_value(value)
                        .ok()
                        .map(|yaml| (name.clone(), yaml))
                })
                .collect();
            let createmeta = client.get_createmeta(project, issue_type).await?;
            resolve_custom_fields(&scalars, &[], &createmeta)?
        }
        _ => std::collections::BTreeMap::new(),
    };

    let created = client
        .create_issue_with_custom_fields(project, issue_type, summary, adf.as_ref(), &[], &resolved)
        .await?;
    yaml_result(&created)
}

/// Updates a JIRA issue. Any combination of description (`content`),
/// `assignee`, `reporter`, and arbitrary `fields` may be supplied; absent
/// inputs leave the corresponding JIRA values untouched. At least one of
/// these must be supplied.
#[allow(clippy::too_many_arguments)]
async fn run_jira_write(
    client: &AtlassianClient,
    cache: &CatalogueCache,
    key: &str,
    content: Option<&str>,
    format: ReadFormat,
    assignee: Option<&str>,
    reporter: Option<&str>,
    extra_fields: Option<&std::collections::BTreeMap<String, serde_json::Value>>,
) -> Result<String> {
    let adf: Option<ValidatedAdfDocument> = match content {
        Some(c) => {
            let raw: AdfDocument = match format {
                ReadFormat::Jfm => {
                    if c.starts_with("---\n") {
                        let doc = JfmDocument::parse(c)?;
                        markdown_to_adf(&doc.body)?
                    } else {
                        markdown_to_adf(c)?
                    }
                }
                ReadFormat::Adf => serde_json::from_str(c).context("Failed to parse ADF JSON")?,
            };
            Some(ValidatedAdfDocument::try_new(raw)?)
        }
        None => None,
    };

    let mut merged: std::collections::BTreeMap<String, serde_json::Value> =
        std::collections::BTreeMap::new();
    if let Some(extras) = extra_fields {
        for (k, v) in extras {
            merged.insert(k.clone(), v.clone());
        }
    }

    // Issue #866: when callers supply string values for rich-text custom
    // fields, treat them as JFM and convert to ADF before sending. The
    // editmeta lookup is skipped entirely when no string values are present,
    // so the existing object-payload path takes zero extra HTTP calls.
    if merged
        .values()
        .any(|v| matches!(v, serde_json::Value::String(_)))
    {
        match cache.editmeta(client, key).await {
            Ok(editmeta) => {
                convert_textarea_string_values(&mut merged, &editmeta)?;
            }
            Err(err) => {
                tracing::debug!(
                    "editmeta lookup for {key} failed; passing `fields` through unchanged: {err:#}"
                );
            }
        }
    }

    apply_user_field_overrides(
        &mut merged,
        assignee,
        reporter,
        "the same key inside `fields`",
    )?;

    if adf.is_none() && merged.is_empty() {
        anyhow::bail!(
            "no changes supplied for {key}: provide `content`, `assignee`, `reporter`, or `fields`"
        );
    }

    client
        .update_issue_with_custom_fields(key, adf.as_ref(), None, &merged)
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
        let adf = ValidatedAdfDocument::try_new(markdown_to_adf(body)?)?;
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
            let adf = ValidatedAdfDocument::try_new(markdown_to_adf(text)?)?;
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
    let adf = ValidatedAdfDocument::try_new(markdown_to_adf(body)?)?;
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
        description = "Create a new JIRA issue, from explicit fields or from a full JFM `document` \
                       (frontmatter + body, e.g. the output of `jira_read`). With a `document`, \
                       `project`/`summary`/`issue_type`, labels and custom fields come from the \
                       frontmatter (project is derived from `key:` when no `project:` is set) and \
                       the body becomes the description — enabling the read → edit → create \
                       round-trip. Explicit `project`/`summary`/`issue_type` override frontmatter \
                       and a warning is returned when they do. Without a `document`, `custom_fields` \
                       is an optional map of field name or canonical id (e.g. `{\"Story Points\": 8}` \
                       or `{\"Planned / Unplanned Work\": \"Unplanned\"}`) to value, resolved against \
                       the create screen and shaped for the API — use it to satisfy fields a project \
                       requires at create time (otherwise JIRA returns HTTP 400). Returns the new \
                       issue key and self URL as YAML."
    )]
    pub async fn jira_create(
        &self,
        Parameters(params): Parameters<JiraCreateParams>,
    ) -> Result<CallToolResult, McpError> {
        let (client, _instance_url) = create_client().map_err(tool_error)?;
        let text = run_jira_create(&client, &params)
            .await
            .map_err(tool_error)?;
        ok_text(text)
    }

    /// Tool: update a JIRA issue's description, assignee, reporter, or
    /// arbitrary fields.
    #[tool(
        description = "Update a JIRA issue. `content` updates the description (JFM markdown by \
                       default, or raw ADF JSON when `format = \"adf\"`); omit it to leave the \
                       description unchanged. JFM is GitHub-style markdown — see resource \
                       `omni-dev://specs/jfm` for syntax. To set the parent for hierarchy \
                       (Epic → Story, Story → Sub-task) use the `jira_link_parent` tool — \
                       the canonical hierarchy surface. `assignee`/`reporter` \
                       accept an `accountId` (use the empty string `\"\"` to clear, `\"-1\"` for \
                       JIRA automatic assignment); call `jira_user_search` first if you only \
                       have a name or email. `fields` is an escape hatch — a map of canonical \
                       JIRA field id to its API JSON value (e.g. `{\"priority\": {\"name\": \
                       \"High\"}}`) — for fields without a typed parameter. String values \
                       targeting rich-text custom fields (e.g. Acceptance Criteria) are \
                       auto-converted from JFM to ADF; pass the empty string `\"\"` to clear \
                       such a field. Pass a JSON object value to bypass conversion (raw ADF). \
                       At least one of `content`, `assignee`, `reporter`, or `fields` must be \
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
            &self.catalogue_cache,
            &params.key,
            params.content.as_deref(),
            format,
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

    /// Fresh cache per test — keeps editmeta call counts deterministic and
    /// avoids cross-test bleed.
    fn mock_cache() -> CatalogueCache {
        CatalogueCache::new(std::time::Duration::from_secs(60))
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

    /// Builds `JiraCreateParams` for the explicit-fields (non-document) path.
    fn jira_create_params(
        project: Option<&str>,
        summary: Option<&str>,
        description: Option<&str>,
        issue_type: Option<&str>,
    ) -> JiraCreateParams {
        JiraCreateParams {
            document: None,
            project: project.map(String::from),
            summary: summary.map(String::from),
            description: description.map(String::from),
            issue_type: issue_type.map(String::from),
            custom_fields: None,
        }
    }

    /// Builds `JiraCreateParams` for the document path with an optional project override.
    fn jira_create_doc_params(document: &str, project: Option<&str>) -> JiraCreateParams {
        JiraCreateParams {
            document: Some(document.to_string()),
            project: project.map(String::from),
            summary: None,
            description: None,
            issue_type: None,
            custom_fields: None,
        }
    }

    async fn mount_create_ok(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/rest/api/3/issue"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": "100",
                "key": "PROJ-100",
                "self": "https://example.atlassian.net/rest/api/3/issue/100"
            })))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn run_jira_create_returns_new_key() {
        let server = MockServer::start().await;
        mount_create_ok(&server).await;

        let client = mock_client(&server.uri());
        let yaml = run_jira_create(
            &client,
            &jira_create_params(
                Some("PROJ"),
                Some("A task"),
                Some("Body text"),
                Some("Task"),
            ),
        )
        .await
        .unwrap();
        assert!(yaml.contains("PROJ-100"));
    }

    #[tokio::test]
    async fn run_jira_create_without_description_omits_body() {
        let server = MockServer::start().await;
        mount_create_ok(&server).await;

        let client = mock_client(&server.uri());
        run_jira_create(
            &client,
            &jira_create_params(Some("PROJ"), Some("Terse"), None, Some("Task")),
        )
        .await
        .unwrap();
    }

    /// Issue #714: a body whose ADF would violate Confluence's content
    /// model is rejected locally before any HTTP call. Uses an unreachable
    /// URL to assert the validation `?` short-circuits before the wire.
    const BAD_ADF_JFM: &str = ":::panel{type=info}\n:::expand{title=\"x\"}\nbody\n:::\n:::";

    #[tokio::test]
    async fn run_jira_create_rejects_invalid_adf_nesting() {
        let client = mock_client("http://127.0.0.1:1");
        let err = run_jira_create(
            &client,
            &jira_create_params(Some("PROJ"), Some("Title"), Some(BAD_ADF_JFM), Some("Task")),
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid ADF nesting"));
        assert!(msg.contains("`expand` cannot be a child of `panel`"));
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
        let err = run_jira_create(
            &client,
            &jira_create_params(Some("PROJ"), Some("Title"), None, Some("Task")),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("400"));
    }

    #[tokio::test]
    async fn run_jira_create_requires_project_without_document() {
        let client = mock_client("http://127.0.0.1:1");
        let err = run_jira_create(
            &client,
            &jira_create_params(None, Some("Title"), None, Some("Task")),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("`project` is required"));
    }

    #[tokio::test]
    async fn run_jira_create_requires_summary_without_document() {
        let client = mock_client("http://127.0.0.1:1");
        let err = run_jira_create(
            &client,
            &jira_create_params(Some("PROJ"), None, None, Some("Task")),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("`summary` is required"));
    }

    // ── run_jira_create: #1058 document (frontmatter) round-trip ─────────────

    #[tokio::test]
    async fn run_jira_create_from_document_derives_project_from_key() {
        let server = MockServer::start().await;
        mount_create_ok(&server).await;

        let doc = "---\ntype: jira\ninstance: https://org.atlassian.net\nkey: PROJ-7\nsummary: Round-tripped\n---\n\nBody from document\n";
        let client = mock_client(&server.uri());
        let yaml = run_jira_create(&client, &jira_create_doc_params(doc, None))
            .await
            .unwrap();
        assert!(yaml.contains("PROJ-100"));
        // No override → no warning line.
        assert!(!yaml.contains("warning:"));
    }

    #[tokio::test]
    async fn run_jira_create_from_document_with_labels() {
        let server = MockServer::start().await;
        // Labels add no createmeta round-trip (single POST); assert the labels
        // reach the wire via a partial-body match.
        Mock::given(method("POST"))
            .and(path("/rest/api/3/issue"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({ "fields": { "labels": ["backend"] } }),
            ))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": "100", "key": "PROJ-100",
                "self": "https://example.atlassian.net/rest/api/3/issue/100"
            })))
            .mount(&server)
            .await;

        let doc = "---\ntype: jira\ninstance: https://org.atlassian.net\nproject: PROJ\nsummary: Labelled\nlabels:\n  - backend\n---\n\nBody\n";
        let client = mock_client(&server.uri());
        let yaml = run_jira_create(&client, &jira_create_doc_params(doc, None))
            .await
            .unwrap();
        assert!(yaml.contains("PROJ-100"));
    }

    #[tokio::test]
    async fn run_jira_create_from_document_custom_fields_use_createmeta() {
        // Full parity: a `custom_fields:` map drives a createmeta GET to
        // resolve the human field name to an ID, then the create POST carries
        // the resolved `customfield_*`.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/createmeta"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "projects": [{
                    "key": "PROJ",
                    "issuetypes": [{
                        "name": "Task",
                        "fields": {
                            "customfield_10010": {
                                "name": "Story Points",
                                "schema": { "type": "number" }
                            }
                        }
                    }]
                }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rest/api/3/issue"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({ "fields": { "customfield_10010": 5 } }),
            ))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": "100", "key": "PROJ-100",
                "self": "https://example.atlassian.net/rest/api/3/issue/100"
            })))
            .mount(&server)
            .await;

        let doc = "---\ntype: jira\ninstance: https://org.atlassian.net\nproject: PROJ\nsummary: With CF\ncustom_fields:\n  Story Points: 5\n---\n\nBody\n";
        let client = mock_client(&server.uri());
        let yaml = run_jira_create(&client, &jira_create_doc_params(doc, None))
            .await
            .unwrap();
        assert!(yaml.contains("PROJ-100"));
    }

    #[tokio::test]
    async fn run_jira_create_document_param_override_warns_in_text() {
        let server = MockServer::start().await;
        mount_create_ok(&server).await;

        let doc = "---\ntype: jira\ninstance: https://org.atlassian.net\nproject: OLD\nsummary: T\n---\n\nBody\n";
        let client = mock_client(&server.uri());
        let yaml = run_jira_create(&client, &jira_create_doc_params(doc, Some("NEW")))
            .await
            .unwrap();
        assert!(yaml.contains("warning:"));
        assert!(yaml.contains("OLD"));
        assert!(yaml.contains("NEW"));
        assert!(yaml.contains("PROJ-100"));
    }

    #[tokio::test]
    async fn run_jira_create_document_without_project_or_key_errors() {
        let doc = "---\ntype: jira\ninstance: https://org.atlassian.net\nsummary: No project\n---\n\nBody\n";
        let client = mock_client("http://127.0.0.1:1");
        let err = run_jira_create(&client, &jira_create_doc_params(doc, None))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Project key is required"));
    }

    #[tokio::test]
    async fn run_jira_create_document_and_description_errors() {
        let doc = "---\ntype: jira\ninstance: https://org.atlassian.net\nproject: PROJ\nsummary: T\n---\n\nBody\n";
        let client = mock_client("http://127.0.0.1:1");
        let params = JiraCreateParams {
            document: Some(doc.to_string()),
            project: None,
            summary: None,
            description: Some("conflicting body".to_string()),
            issue_type: None,
            custom_fields: None,
        };
        let err = run_jira_create(&client, &params).await.unwrap_err();
        assert!(err
            .to_string()
            .contains("Provide either `document` or `description`"));
    }

    #[tokio::test]
    async fn run_jira_create_document_and_custom_fields_errors() {
        let doc = "---\ntype: jira\ninstance: https://org.atlassian.net\nproject: PROJ\nsummary: T\n---\n\nBody\n";
        let client = mock_client("http://127.0.0.1:1");
        let mut fields = std::collections::BTreeMap::new();
        fields.insert("Story Points".to_string(), serde_json::json!(5));
        let params = JiraCreateParams {
            document: Some(doc.to_string()),
            project: None,
            summary: None,
            description: None,
            issue_type: None,
            custom_fields: Some(fields),
        };
        let err = run_jira_create(&client, &params).await.unwrap_err();
        assert!(err
            .to_string()
            .contains("Provide either `document` or `custom_fields`"));
    }

    #[tokio::test]
    async fn run_jira_create_document_rejects_confluence_frontmatter() {
        let doc = "---\ntype: confluence\ninstance: https://org.atlassian.net\ntitle: P\nspace_key: ENG\n---\n\nBody\n";
        let client = mock_client("http://127.0.0.1:1");
        let err = run_jira_create(&client, &jira_create_doc_params(doc, None))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Confluence"));
    }

    #[tokio::test]
    async fn run_jira_create_with_custom_fields_resolves_via_createmeta() {
        // Issue #1052: a custom field passed by human name is resolved to its
        // id via createmeta and shaped for the API ({"value": ...} for an
        // option field) before the create POST.
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/createmeta"))
            .and(query_param("projectKeys", "PROJ"))
            .and(query_param("issuetypeNames", "Epic"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "projects": [{
                    "issuetypes": [{
                        "fields": {
                            "customfield_10001": {
                                "name": "Planned / Unplanned Work",
                                "schema": {
                                    "type": "option",
                                    "custom": "com.atlassian.jira.plugin.system.customfieldtypes:select"
                                }
                            }
                        }
                    }]
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/rest/api/3/issue"))
            .and(body_json(serde_json::json!({
                "fields": {
                    "project": {"key": "PROJ"},
                    "issuetype": {"name": "Epic"},
                    "summary": "An epic",
                    "customfield_10001": {"value": "Unplanned"}
                }
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": "100",
                "key": "PROJ-100",
                "self": "https://example.atlassian.net/rest/api/3/issue/100"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let mut fields = std::collections::BTreeMap::new();
        fields.insert(
            "Planned / Unplanned Work".to_string(),
            serde_json::Value::String("Unplanned".to_string()),
        );
        let params = JiraCreateParams {
            document: None,
            project: Some("PROJ".to_string()),
            summary: Some("An epic".to_string()),
            description: None,
            issue_type: Some("Epic".to_string()),
            custom_fields: Some(fields),
        };
        let yaml = run_jira_create(&client, &params).await.unwrap();
        assert!(yaml.contains("PROJ-100"));
    }

    #[tokio::test]
    async fn run_jira_create_without_custom_fields_skips_createmeta() {
        // The fast path must not hit createmeta when no custom fields are
        // requested — only the create POST. An empty map behaves like None.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/3/issue"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": "100",
                "key": "PROJ-100",
                "self": "https://example.atlassian.net/rest/api/3/issue/100"
            })))
            .expect(1)
            .mount(&server)
            .await;
        // No createmeta mock is mounted, so any GET to it surfaces a
        // wiremock unmatched-request panic and fails the test.

        let client = mock_client(&server.uri());
        let params = JiraCreateParams {
            document: None,
            project: Some("PROJ".to_string()),
            summary: Some("Terse".to_string()),
            description: None,
            issue_type: Some("Task".to_string()),
            custom_fields: Some(std::collections::BTreeMap::new()),
        };
        run_jira_create(&client, &params).await.unwrap();
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
            &mock_cache(),
            "PROJ-1",
            Some("New body\n"),
            ReadFormat::Jfm,
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
            &mock_cache(),
            "PROJ-1",
            Some(content),
            ReadFormat::Jfm,
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
            &mock_cache(),
            "PROJ-1",
            Some(r#"{"version":1,"type":"doc","content":[]}"#),
            ReadFormat::Adf,
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
            &mock_cache(),
            "PROJ-1",
            Some("not json"),
            ReadFormat::Adf,
            None,
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("Failed to parse ADF JSON"));
    }

    #[tokio::test]
    async fn run_jira_write_rejects_invalid_adf_nesting() {
        // Issue #714: validation runs before the network call.
        let client = mock_client("http://127.0.0.1:1");
        let err = run_jira_write(
            &client,
            &mock_cache(),
            "PROJ-1",
            Some(BAD_ADF_JFM),
            ReadFormat::Jfm,
            None,
            None,
            None,
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid ADF nesting"));
        assert!(msg.contains("`expand` cannot be a child of `panel`"));
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
            &mock_cache(),
            "PROJ-1",
            Some("Body"),
            ReadFormat::Jfm,
            None,
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("400"));
    }

    #[tokio::test]
    async fn run_jira_write_enriches_adf_required_error() {
        let server = MockServer::start().await;
        let body = serde_json::json!({
            "errorMessages": [],
            "errors": {
                "customfield_19300": "Operation value must be an Atlassian Document (see the Atlassian Document Format)"
            }
        });
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .respond_with(ResponseTemplate::new(400).set_body_json(body))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());

        let mut extras = std::collections::BTreeMap::new();
        extras.insert(
            "customfield_19300".to_string(),
            serde_json::Value::String("plain string, not ADF".to_string()),
        );

        let err = run_jira_write(
            &client,
            &mock_cache(),
            "PROJ-1",
            None,
            ReadFormat::Jfm,
            None,
            None,
            Some(&extras),
        )
        .await
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("Field `customfield_19300`"), "got: {msg}");
        assert!(
            msg.contains("rich-text content in ADF format"),
            "got: {msg}"
        );
        assert!(msg.contains("To fix:"), "got: {msg}");
        assert!(msg.contains("JFM markdown"), "got: {msg}");
        assert!(msg.contains("omni-dev://specs/jfm"), "got: {msg}");
        assert!(
            msg.contains("Operation value must be an Atlassian Document"),
            "got: {msg}"
        );
    }

    #[tokio::test]
    async fn run_jira_write_falls_back_for_non_adf_400() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "errorMessages": ["Something else"],
                "errors": {"summary": "Summary is required."}
            })))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let err = run_jira_write(
            &client,
            &mock_cache(),
            "PROJ-1",
            Some("Body"),
            ReadFormat::Jfm,
            None,
            None,
            None,
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("400"), "got: {msg}");
        assert!(
            !msg.contains("rich-text content in ADF format"),
            "got: {msg}"
        );
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
            &mock_cache(),
            "PROJ-1",
            Some(""),
            ReadFormat::Jfm,
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
            &mock_cache(),
            "PROJ-1",
            None,
            ReadFormat::Jfm,
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
            &mock_cache(),
            "PROJ-1",
            None,
            ReadFormat::Jfm,
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
            &mock_cache(),
            "PROJ-1",
            None,
            ReadFormat::Jfm,
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
            &mock_cache(),
            "PROJ-1",
            None,
            ReadFormat::Jfm,
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
            &mock_cache(),
            "PROJ-1",
            None,
            ReadFormat::Jfm,
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
            &mock_cache(),
            "PROJ-1",
            None,
            ReadFormat::Jfm,
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
            &mock_cache(),
            "PROJ-1",
            None,
            ReadFormat::Jfm,
            None,
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no changes supplied"));
    }

    // ── run_jira_write: JFM→ADF for rich-text fields (issue #866) ─────────

    fn editmeta_textarea_body() -> serde_json::Value {
        serde_json::json!({
            "fields": {
                "customfield_19300": {
                    "name": "Acceptance Criteria",
                    "schema": {
                        "type": "string",
                        "custom": "com.atlassian.jira.plugin.system.customfieldtypes:textarea"
                    }
                },
                "customfield_10010": {
                    "name": "Plain String",
                    "schema": {"type": "string"}
                }
            }
        })
    }

    async fn mount_editmeta_textarea(server: &MockServer, key: &str) {
        Mock::given(method("GET"))
            .and(path(format!("/rest/api/3/issue/{key}/editmeta")))
            .respond_with(ResponseTemplate::new(200).set_body_json(editmeta_textarea_body()))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn run_jira_write_textarea_string_converts_to_adf() {
        let server = MockServer::start().await;
        mount_editmeta_textarea(&server, "PROJ-1").await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .and(body_json(serde_json::json!({
                "fields": {
                    "customfield_19300": {
                        "version": 1,
                        "type": "doc",
                        "content": [{
                            "type": "bulletList",
                            "content": [
                                {"type": "listItem", "content": [{
                                    "type": "paragraph",
                                    "content": [{"type": "text", "text": "one"}]
                                }]},
                                {"type": "listItem", "content": [{
                                    "type": "paragraph",
                                    "content": [{"type": "text", "text": "two"}]
                                }]}
                            ]
                        }]
                    }
                }
            })))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let mut extras = std::collections::BTreeMap::new();
        extras.insert(
            "customfield_19300".to_string(),
            serde_json::Value::String("- one\n- two".to_string()),
        );
        run_jira_write(
            &client,
            &mock_cache(),
            "PROJ-1",
            None,
            ReadFormat::Jfm,
            None,
            None,
            Some(&extras),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_jira_write_textarea_object_passes_through() {
        // Object value should not trigger editmeta lookup at all — the
        // prefilter only fires when at least one value is a string. We
        // intentionally do NOT mock editmeta; if the code regresses and
        // calls it, wiremock will surface an unmatched-request panic.
        let raw_adf = serde_json::json!({
            "version": 1,
            "type": "doc",
            "content": [{"type": "paragraph", "content": [{"type": "text", "text": "hi"}]}]
        });
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .and(body_json(serde_json::json!({
                "fields": {"customfield_19300": raw_adf.clone()}
            })))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let mut extras = std::collections::BTreeMap::new();
        extras.insert("customfield_19300".to_string(), raw_adf);
        run_jira_write(
            &client,
            &mock_cache(),
            "PROJ-1",
            None,
            ReadFormat::Jfm,
            None,
            None,
            Some(&extras),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_jira_write_textarea_empty_string_clears_field() {
        let server = MockServer::start().await;
        mount_editmeta_textarea(&server, "PROJ-1").await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .and(body_json(serde_json::json!({
                "fields": {"customfield_19300": null}
            })))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let mut extras = std::collections::BTreeMap::new();
        extras.insert(
            "customfield_19300".to_string(),
            serde_json::Value::String(String::new()),
        );
        run_jira_write(
            &client,
            &mock_cache(),
            "PROJ-1",
            None,
            ReadFormat::Jfm,
            None,
            None,
            Some(&extras),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_jira_write_non_textarea_string_passes_through() {
        // String value targeting a non-textarea field: editmeta is consulted
        // (lazy prefilter sees a string), confirms it's not rich-text, and
        // the original payload is sent as-is.
        let server = MockServer::start().await;
        mount_editmeta_textarea(&server, "PROJ-1").await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .and(body_json(serde_json::json!({
                "fields": {"customfield_10010": "plain text"}
            })))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let mut extras = std::collections::BTreeMap::new();
        extras.insert(
            "customfield_10010".to_string(),
            serde_json::Value::String("plain text".to_string()),
        );
        run_jira_write(
            &client,
            &mock_cache(),
            "PROJ-1",
            None,
            ReadFormat::Jfm,
            None,
            None,
            Some(&extras),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_jira_write_editmeta_fetch_failure_passes_through() {
        // editmeta returns 500 — code must fall back to passthrough and let
        // the API surface its own error from the PUT. The PUT mock here
        // succeeds (204), proving that the original string body reached the
        // wire even though editmeta was unavailable.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/PROJ-1/editmeta"))
            .respond_with(ResponseTemplate::new(500).set_body_string("editmeta down"))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .and(body_json(serde_json::json!({
                "fields": {"customfield_19300": "- one"}
            })))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let mut extras = std::collections::BTreeMap::new();
        extras.insert(
            "customfield_19300".to_string(),
            serde_json::Value::String("- one".to_string()),
        );
        run_jira_write(
            &client,
            &mock_cache(),
            "PROJ-1",
            None,
            ReadFormat::Jfm,
            None,
            None,
            Some(&extras),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_jira_write_textarea_invalid_adf_nesting_errors() {
        // Invalid ADF nesting (panel→expand) for a textarea field must
        // short-circuit before the PUT. No PUT mock is registered.
        let server = MockServer::start().await;
        mount_editmeta_textarea(&server, "PROJ-1").await;

        let client = mock_client(&server.uri());
        let mut extras = std::collections::BTreeMap::new();
        extras.insert(
            "customfield_19300".to_string(),
            serde_json::Value::String(BAD_ADF_JFM.to_string()),
        );
        let err = run_jira_write(
            &client,
            &mock_cache(),
            "PROJ-1",
            None,
            ReadFormat::Jfm,
            None,
            None,
            Some(&extras),
        )
        .await
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("Acceptance Criteria"), "got: {msg}");
        assert!(msg.contains("ADF nesting validation"), "got: {msg}");
        assert!(
            msg.contains("`expand` cannot be a child of `panel`"),
            "got: {msg}"
        );
    }

    #[tokio::test]
    async fn run_jira_write_unknown_field_id_passes_through() {
        // Field id missing from editmeta — pass the value through unchanged,
        // let the API tell the caller. Editmeta returns a non-matching
        // schema; PUT receives the raw string.
        let server = MockServer::start().await;
        mount_editmeta_textarea(&server, "PROJ-1").await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .and(body_json(serde_json::json!({
                "fields": {"customfield_99999": "some text"}
            })))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let mut extras = std::collections::BTreeMap::new();
        extras.insert(
            "customfield_99999".to_string(),
            serde_json::Value::String("some text".to_string()),
        );
        run_jira_write(
            &client,
            &mock_cache(),
            "PROJ-1",
            None,
            ReadFormat::Jfm,
            None,
            None,
            Some(&extras),
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
    async fn run_jira_transition_rejects_invalid_comment_adf_nesting() {
        // Issue #714: when transition succeeds but the optional comment body
        // produces invalid ADF, the validation `?` rejects it. The transition
        // POST has already happened; the comment POST does not (no mock).
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

        let client = mock_client(&server.uri());
        let err = run_jira_transition(&client, "PROJ-1", Some("Done"), Some(BAD_ADF_JFM), false)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid ADF nesting"));
        assert!(msg.contains("`expand` cannot be a child of `panel`"));
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
    async fn run_jira_comment_add_rejects_invalid_adf_nesting() {
        // Issue #714: invalid body short-circuits before the network call.
        let client = mock_client("http://127.0.0.1:1");
        let err = run_jira_comment(&client, "PROJ-1", "add", Some(BAD_ADF_JFM), 0)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid ADF nesting"));
        assert!(msg.contains("`expand` cannot be a child of `panel`"));
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
    async fn run_jira_comment_edit_rejects_invalid_adf_nesting() {
        // Issue #714: invalid body short-circuits before the network call.
        let client = mock_client("http://127.0.0.1:1");
        let err = run_jira_comment_edit(&client, "PROJ-1", "100", BAD_ADF_JFM, None)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid ADF nesting"));
        assert!(msg.contains("`expand` cannot be a child of `panel`"));
    }

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
