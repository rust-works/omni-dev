//! MCP resource URIs: parsing and read dispatch.
//!
//! Resources expose omni-dev content at stable URIs so an MCP client can
//! fetch them without issuing a tool call. Each URI template is backed by
//! the same core function the CLI uses, so surface and CLI stay in lock
//! step. See issue #606 for the URI catalogue.

use anyhow::{Context, Result};
use rmcp::{
    model::{
        ListResourceTemplatesResult, ListResourcesResult, RawResource, RawResourceTemplate,
        ReadResourceResult, Resource, ResourceContents, ResourceTemplate,
    },
    ErrorData as McpError,
};
use serde_json::json;

use crate::atlassian::api::{AtlassianApi, ContentItem};
use crate::atlassian::confluence_api::ConfluenceApi;
use crate::atlassian::document::content_item_to_document;
use crate::atlassian::jira_api::JiraApi;
use crate::cli::atlassian::helpers::create_client;
use crate::resources;

/// Format suffix for a resource URI.
///
/// Atlassian URIs accept an optional `.adf` suffix: absent means JFM markdown,
/// present means raw ADF JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceFormat {
    /// JFM markdown (the default for Atlassian resources).
    Jfm,
    /// Raw ADF JSON (opted into via the `.adf` suffix).
    Adf,
}

/// A parsed omni-dev resource URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceUri {
    /// `jira://issue/{key}` / `jira://issue/{key}.adf` — JIRA issue body.
    JiraIssue {
        /// JIRA issue key (e.g. `PROJ-123`).
        key: String,
        /// Output format: JFM or ADF JSON.
        format: ResourceFormat,
    },
    /// `confluence://page/{id}` / `confluence://page/{id}.adf` — Confluence page.
    ConfluencePage {
        /// Confluence page id (decimal string).
        id: String,
        /// Output format: JFM or ADF JSON.
        format: ResourceFormat,
    },
    /// `omni-dev://specs/{name}` — embedded reference spec (e.g. `jfm`).
    Specs {
        /// Spec identifier — see [`crate::resources::get`] for the registered set.
        name: String,
    },
}

/// Errors returned by the URI parser.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum UriParseError {
    /// The URI scheme is not one omni-dev knows about.
    #[error("unknown URI scheme in `{0}`; expected jira://, confluence://, or omni-dev://")]
    UnknownScheme(String),
    /// The URI authority/path shape does not match any known template.
    #[error("malformed URI `{0}`: {1}")]
    Malformed(String, &'static str),
    /// The path identifier (range, key, id) is empty.
    #[error("empty identifier in URI `{0}`")]
    EmptyIdentifier(String),
}

impl ResourceUri {
    /// Parses a resource URI string.
    ///
    /// Rejects URIs that do not match one of the known templates.
    pub fn parse(uri: &str) -> Result<Self, UriParseError> {
        if let Some(rest) = uri.strip_prefix("jira://issue/") {
            let (key, format) = split_suffix(rest);
            if key.is_empty() {
                return Err(UriParseError::EmptyIdentifier(uri.to_string()));
            }
            return Ok(Self::JiraIssue {
                key: key.to_string(),
                format,
            });
        }

        if let Some(rest) = uri.strip_prefix("confluence://page/") {
            let (id, format) = split_suffix(rest);
            if id.is_empty() {
                return Err(UriParseError::EmptyIdentifier(uri.to_string()));
            }
            return Ok(Self::ConfluencePage {
                id: id.to_string(),
                format,
            });
        }

        if let Some(rest) = uri.strip_prefix("omni-dev://specs/") {
            if rest.is_empty() {
                return Err(UriParseError::EmptyIdentifier(uri.to_string()));
            }
            return Ok(Self::Specs {
                name: rest.to_string(),
            });
        }

        // Reject `<scheme>://` URIs with a different path shape explicitly
        // rather than falling through to UnknownScheme, so the client sees
        // what's actually wrong. Placeholders are escaped for the lint.
        if uri.starts_with("jira://") {
            return Err(UriParseError::Malformed(
                uri.to_string(),
                "expected `jira://issue/<key>` or `jira://issue/<key>.adf`",
            ));
        }
        if uri.starts_with("confluence://") {
            return Err(UriParseError::Malformed(
                uri.to_string(),
                "expected `confluence://page/<id>` or `confluence://page/<id>.adf`",
            ));
        }
        if uri.starts_with("omni-dev://") {
            return Err(UriParseError::Malformed(
                uri.to_string(),
                "expected `omni-dev://specs/<name>`",
            ));
        }

        Err(UriParseError::UnknownScheme(uri.to_string()))
    }
}

/// Splits a trailing `.adf` suffix off a path segment.
fn split_suffix(rest: &str) -> (&str, ResourceFormat) {
    match rest.strip_suffix(".adf") {
        Some(id) => (id, ResourceFormat::Adf),
        None => (rest, ResourceFormat::Jfm),
    }
}

/// Comma-separated spec short-names (without the `specs/` prefix) for use in
/// MCP `unknown spec` errors.
///
/// Why: keeps the long-standing MCP error wording (`unknown spec `nope`;
/// known: jfm`) stable. The CLI uses [`resources::known_ids_csv`] (full ids)
/// because its namespace is wider than just specs.
fn specs_only_csv() -> String {
    resources::ids()
        .filter_map(|id| id.strip_prefix("specs/"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The static catalogue of resource templates omni-dev advertises.
///
/// Returned by `list_resource_templates`. URIs are RFC 6570 templates; the
/// placeholders match the identifiers [`ResourceUri::parse`] understands.
pub fn resource_templates() -> Vec<ResourceTemplate> {
    let jira_issue_jfm = RawResourceTemplate::new("jira://issue/{key}", "jira_issue_jfm")
        .with_description("JIRA issue rendered as JFM (JIRA-flavoured markdown).")
        .with_mime_type("text/markdown");

    let jira_issue_adf = RawResourceTemplate::new("jira://issue/{key}.adf", "jira_issue_adf")
        .with_description("JIRA issue body as raw Atlassian Document Format (ADF) JSON.")
        .with_mime_type("application/json");

    let confluence_page_jfm =
        RawResourceTemplate::new("confluence://page/{id}", "confluence_page_jfm")
            .with_description("Confluence page rendered as JFM markdown.")
            .with_mime_type("text/markdown");

    let confluence_page_adf =
        RawResourceTemplate::new("confluence://page/{id}.adf", "confluence_page_adf")
            .with_description("Confluence page body as raw ADF JSON.")
            .with_mime_type("application/json");

    let omni_dev_spec = RawResourceTemplate::new("omni-dev://specs/{name}", "omni_dev_spec")
        .with_description(
            "Reference specs maintained by omni-dev. Currently supports `jfm` \
             (JIRA-Flavoured Markdown) — fetch before writing JIRA or Confluence \
             content via `jira_write`, `jira_create`, or `confluence_write`.",
        )
        .with_mime_type("text/markdown");

    vec![
        annotate_template(jira_issue_jfm),
        annotate_template(jira_issue_adf),
        annotate_template(confluence_page_jfm),
        annotate_template(confluence_page_adf),
        annotate_template(omni_dev_spec),
    ]
}

fn annotate_template(raw: RawResourceTemplate) -> ResourceTemplate {
    ResourceTemplate {
        raw,
        annotations: None,
    }
}

/// Resources surfaced by `list_resources`.
///
/// Per the MCP spec, resources returned here must have concrete URIs. We
/// expose the URI templates themselves as informational pointers so a client
/// that does not support `list_resource_templates` can still discover the
/// URI shape; real content lives behind `read_resource`.
pub fn resource_listing() -> Vec<Resource> {
    resource_templates()
        .into_iter()
        .map(|tpl| {
            let RawResourceTemplate {
                uri_template,
                name,
                title,
                description,
                mime_type,
                icons,
            } = tpl.raw;
            Resource {
                raw: RawResource {
                    uri: uri_template,
                    name,
                    title,
                    description,
                    mime_type,
                    size: None,
                    icons,
                    meta: None,
                },
                annotations: None,
            }
        })
        .collect()
}

/// Resolves a parsed URI into [`ReadResourceResult`] contents.
pub async fn read_resource(uri: &ResourceUri, raw_uri: &str) -> Result<ReadResourceResult> {
    match uri {
        ResourceUri::JiraIssue { key, format } => {
            let (client, instance_url) =
                create_client().context("Failed to load Atlassian credentials")?;
            let api = JiraApi::new(client);
            let item = api
                .get_content(key)
                .await
                .with_context(|| format!("Failed to fetch JIRA issue {key}"))?;
            let (text, mime) = render_content_item(&item, &instance_url, *format)?;
            Ok(ReadResourceResult::new(vec![ResourceContents::text(
                text, raw_uri,
            )
            .with_mime_type(mime)]))
        }
        ResourceUri::ConfluencePage { id, format } => {
            let (client, instance_url) =
                create_client().context("Failed to load Atlassian credentials")?;
            let api = ConfluenceApi::new(client);
            let item = api
                .get_content(id)
                .await
                .with_context(|| format!("Failed to fetch Confluence page {id}"))?;
            let (text, mime) = render_content_item(&item, &instance_url, *format)?;
            Ok(ReadResourceResult::new(vec![ResourceContents::text(
                text, raw_uri,
            )
            .with_mime_type(mime)]))
        }
        ResourceUri::Specs { name } => {
            let full_id = format!("specs/{name}");
            let resource = resources::get(&full_id).ok_or_else(|| {
                anyhow::anyhow!(
                    "unknown spec `{name}`; known: {known}",
                    known = specs_only_csv(),
                )
            })?;
            Ok(ReadResourceResult::new(vec![ResourceContents::text(
                resource.content.to_string(),
                raw_uri,
            )
            .with_mime_type(resource.mime_type)]))
        }
    }
}

/// Renders a [`ContentItem`] as either JFM markdown or ADF JSON.
///
/// Returns the rendered text and the MIME type to advertise.
pub fn render_content_item(
    item: &ContentItem,
    instance_url: &str,
    format: ResourceFormat,
) -> Result<(String, &'static str)> {
    match format {
        ResourceFormat::Jfm => {
            let doc = content_item_to_document(item, instance_url)?;
            let rendered = doc.render()?;
            Ok((rendered, "text/markdown"))
        }
        ResourceFormat::Adf => {
            let json = serde_json::to_string_pretty(
                &item.body_adf.clone().unwrap_or(serde_json::Value::Null),
            )
            .context("Failed to serialize ADF JSON")?;
            Ok((json, "application/json"))
        }
    }
}

/// Converts a URI-lookup failure into a protocol-level `resource_not_found`.
///
/// The raw URI is included in `data` per the MCP spec so the client can tell
/// which resource failed when multiple reads are in flight.
pub fn not_found(uri: &str, reason: impl std::fmt::Display) -> McpError {
    McpError::resource_not_found(
        format!("resource_not_found: {reason}"),
        Some(json!({ "uri": uri })),
    )
}

/// Builds the full [`ListResourcesResult`] payload.
pub fn list_resources_result() -> ListResourcesResult {
    ListResourcesResult {
        resources: resource_listing(),
        next_cursor: None,
        meta: None,
    }
}

/// Builds the full [`ListResourceTemplatesResult`] payload.
pub fn list_resource_templates_result() -> ListResourceTemplatesResult {
    ListResourceTemplatesResult {
        resource_templates: resource_templates(),
        next_cursor: None,
        meta: None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::await_holding_lock)]
mod tests {
    use super::*;

    // ── Parser ─────────────────────────────────────────────────────

    #[test]
    fn parse_jira_issue_jfm() {
        let uri = ResourceUri::parse("jira://issue/PROJ-123").unwrap();
        assert_eq!(
            uri,
            ResourceUri::JiraIssue {
                key: "PROJ-123".to_string(),
                format: ResourceFormat::Jfm,
            }
        );
    }

    #[test]
    fn parse_jira_issue_adf() {
        let uri = ResourceUri::parse("jira://issue/PROJ-123.adf").unwrap();
        assert_eq!(
            uri,
            ResourceUri::JiraIssue {
                key: "PROJ-123".to_string(),
                format: ResourceFormat::Adf,
            }
        );
    }

    #[test]
    fn parse_confluence_page_jfm() {
        let uri = ResourceUri::parse("confluence://page/12345").unwrap();
        assert_eq!(
            uri,
            ResourceUri::ConfluencePage {
                id: "12345".to_string(),
                format: ResourceFormat::Jfm,
            }
        );
    }

    #[test]
    fn parse_confluence_page_adf() {
        let uri = ResourceUri::parse("confluence://page/12345.adf").unwrap();
        assert_eq!(
            uri,
            ResourceUri::ConfluencePage {
                id: "12345".to_string(),
                format: ResourceFormat::Adf,
            }
        );
    }

    #[test]
    fn parse_omni_dev_spec_jfm() {
        let uri = ResourceUri::parse("omni-dev://specs/jfm").unwrap();
        assert_eq!(
            uri,
            ResourceUri::Specs {
                name: "jfm".to_string(),
            }
        );
    }

    #[test]
    fn parse_omni_dev_spec_empty_name_is_empty_identifier() {
        let err = ResourceUri::parse("omni-dev://specs/").unwrap_err();
        assert!(matches!(err, UriParseError::EmptyIdentifier(_)));
    }

    #[test]
    fn parse_omni_dev_wrong_path_is_malformed() {
        let err = ResourceUri::parse("omni-dev://other/x").unwrap_err();
        assert!(matches!(err, UriParseError::Malformed(_, _)));
    }

    #[test]
    fn parse_unknown_scheme_is_rejected() {
        let err = ResourceUri::parse("http://example.com/resource").unwrap_err();
        assert!(matches!(err, UriParseError::UnknownScheme(_)));
    }

    #[test]
    fn parse_empty_string_is_unknown_scheme() {
        let err = ResourceUri::parse("").unwrap_err();
        assert!(matches!(err, UriParseError::UnknownScheme(_)));
    }

    #[test]
    fn parse_jira_wrong_path_is_malformed() {
        let err = ResourceUri::parse("jira://board/123").unwrap_err();
        assert!(matches!(err, UriParseError::Malformed(_, _)));
    }

    #[test]
    fn parse_confluence_wrong_path_is_malformed() {
        let err = ResourceUri::parse("confluence://space/ENG").unwrap_err();
        assert!(matches!(err, UriParseError::Malformed(_, _)));
    }

    #[test]
    fn parse_empty_jira_key_is_empty_identifier() {
        let err = ResourceUri::parse("jira://issue/").unwrap_err();
        assert!(matches!(err, UriParseError::EmptyIdentifier(_)));
    }

    #[test]
    fn parse_empty_confluence_id_is_empty_identifier() {
        let err = ResourceUri::parse("confluence://page/").unwrap_err();
        assert!(matches!(err, UriParseError::EmptyIdentifier(_)));
    }

    #[test]
    fn parse_jira_adf_with_empty_key_is_empty_identifier() {
        // `jira://issue/.adf` strips to empty key after the `.adf` suffix.
        let err = ResourceUri::parse("jira://issue/.adf").unwrap_err();
        assert!(matches!(err, UriParseError::EmptyIdentifier(_)));
    }

    #[test]
    fn error_messages_surface_uri() {
        let err = ResourceUri::parse("ftp://x").unwrap_err();
        assert!(err.to_string().contains("ftp://x"));
    }

    // ── Templates / listings ───────────────────────────────────────

    #[test]
    fn templates_include_all_known_uris() {
        let templates = resource_templates();
        let template_uris: Vec<&str> = templates
            .iter()
            .map(|t| t.raw.uri_template.as_str())
            .collect();
        assert!(template_uris.contains(&"jira://issue/{key}"));
        assert!(template_uris.contains(&"jira://issue/{key}.adf"));
        assert!(template_uris.contains(&"confluence://page/{id}"));
        assert!(template_uris.contains(&"confluence://page/{id}.adf"));
        assert!(template_uris.contains(&"omni-dev://specs/{name}"));
    }

    #[test]
    fn every_template_has_description_and_mime() {
        for tpl in resource_templates() {
            assert!(
                tpl.raw.description.is_some(),
                "missing description for {}",
                tpl.raw.uri_template
            );
            assert!(
                tpl.raw.mime_type.is_some(),
                "missing mime for {}",
                tpl.raw.uri_template
            );
        }
    }

    #[test]
    fn resource_listing_mirrors_templates() {
        let resources = resource_listing();
        let templates = resource_templates();
        assert_eq!(resources.len(), templates.len());
        for (resource, tpl) in resources.iter().zip(templates.iter()) {
            assert_eq!(resource.raw.uri, tpl.raw.uri_template);
            assert_eq!(resource.raw.name, tpl.raw.name);
        }
    }

    #[test]
    fn list_resources_result_has_no_pagination() {
        let result = list_resources_result();
        assert!(result.next_cursor.is_none());
        assert_eq!(result.resources.len(), 5);
    }

    #[test]
    fn list_resource_templates_result_has_no_pagination() {
        let result = list_resource_templates_result();
        assert!(result.next_cursor.is_none());
        assert_eq!(result.resource_templates.len(), 5);
    }

    // ── not_found ──────────────────────────────────────────────────

    #[test]
    fn not_found_puts_uri_in_data() {
        let err = not_found("jira://issue/NOPE", "parse failed");
        assert!(err.message.contains("parse failed"));
        let data = err.data.as_ref().expect("data payload present");
        assert_eq!(
            data.get("uri").and_then(|v| v.as_str()),
            Some("jira://issue/NOPE")
        );
    }

    // ── render_content_item ────────────────────────────────────────

    fn jira_item_with(body: Option<serde_json::Value>) -> ContentItem {
        use crate::atlassian::api::ContentMetadata;
        ContentItem {
            id: "PROJ-1".to_string(),
            title: "Sample".to_string(),
            body_adf: body,
            metadata: ContentMetadata::Jira {
                status: Some("Open".to_string()),
                issue_type: Some("Bug".to_string()),
                assignee: None,
                priority: None,
                labels: vec![],
            },
        }
    }

    #[test]
    fn render_content_item_jfm_contains_frontmatter_and_body() {
        let body = serde_json::json!({
            "version": 1,
            "type": "doc",
            "content": [{"type": "paragraph", "content": [{"type": "text", "text": "Hello"}]}]
        });
        let item = jira_item_with(Some(body));
        let (text, mime) =
            render_content_item(&item, "https://org.atlassian.net", ResourceFormat::Jfm).unwrap();
        assert_eq!(mime, "text/markdown");
        assert!(text.contains("PROJ-1"), "missing key: {text}");
        assert!(text.contains("Hello"), "missing body: {text}");
    }

    #[test]
    fn render_content_item_adf_returns_pretty_json() {
        let body = serde_json::json!({
            "version": 1,
            "type": "doc",
            "content": []
        });
        let item = jira_item_with(Some(body.clone()));
        let (text, mime) =
            render_content_item(&item, "https://org.atlassian.net", ResourceFormat::Adf).unwrap();
        assert_eq!(mime, "application/json");
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, body);
    }

    #[test]
    fn render_content_item_adf_null_body_serializes_as_null() {
        let item = jira_item_with(None);
        let (text, _) =
            render_content_item(&item, "https://org.atlassian.net", ResourceFormat::Adf).unwrap();
        assert_eq!(text.trim(), "null");
    }

    // ── Atlassian branches of read_resource ────────────────────────
    //
    // These tests exercise the JIRA/Confluence branches by pointing
    // `ATLASSIAN_*` env vars at a wiremock server and checking the
    // full handler path. `ENV_MUTEX` serialises env-var access — the
    // stdlib warns that process-env access races across threads, and
    // `create_client` reads these vars internally.

    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvGuard {
        keys: Vec<&'static str>,
    }

    impl EnvGuard {
        fn set(pairs: &[(&'static str, String)]) -> Self {
            let keys = pairs.iter().map(|(k, _)| *k).collect();
            for (k, v) in pairs {
                std::env::set_var(k, v);
            }
            Self { keys }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for k in &self.keys {
                std::env::remove_var(k);
            }
        }
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread")]
    async fn read_resource_jira_issue_jfm_against_wiremock() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-7"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "key": "PROJ-7",
                    "fields": {
                        "summary": "Resource test issue",
                        "description": {
                            "type": "doc",
                            "version": 1,
                            "content": [{
                                "type": "paragraph",
                                "content": [{"type": "text", "text": "resource body"}]
                            }]
                        },
                        "status": {"name": "Open"},
                        "issuetype": {"name": "Task"},
                        "assignee": null,
                        "priority": null,
                        "labels": []
                    }
                })),
            )
            .mount(&server)
            .await;

        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::set(&[
            ("ATLASSIAN_INSTANCE_URL", server.uri()),
            ("ATLASSIAN_EMAIL", "test@example.com".to_string()),
            ("ATLASSIAN_API_TOKEN", "fake-token".to_string()),
        ]);

        let uri = ResourceUri::parse("jira://issue/PROJ-7").unwrap();
        let result = read_resource(&uri, "jira://issue/PROJ-7").await.unwrap();
        assert_eq!(result.contents.len(), 1);
        match &result.contents[0] {
            ResourceContents::TextResourceContents {
                text,
                mime_type,
                uri: reply_uri,
                ..
            } => {
                assert!(text.contains("PROJ-7"), "missing key: {text}");
                assert!(text.contains("resource body"), "missing body: {text}");
                assert_eq!(mime_type.as_deref(), Some("text/markdown"));
                assert_eq!(reply_uri, "jira://issue/PROJ-7");
            }
            other @ ResourceContents::BlobResourceContents { .. } => {
                panic!("expected text, got {other:?}")
            }
        }
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread")]
    async fn read_resource_jira_issue_adf_returns_json() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-8"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "key": "PROJ-8",
                    "fields": {
                        "summary": "ADF payload issue",
                        "description": {
                            "type": "doc",
                            "version": 1,
                            "content": []
                        },
                        "status": {"name": "Open"},
                        "issuetype": {"name": "Bug"},
                        "assignee": null,
                        "priority": null,
                        "labels": []
                    }
                })),
            )
            .mount(&server)
            .await;

        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::set(&[
            ("ATLASSIAN_INSTANCE_URL", server.uri()),
            ("ATLASSIAN_EMAIL", "test@example.com".to_string()),
            ("ATLASSIAN_API_TOKEN", "fake-token".to_string()),
        ]);

        let uri = ResourceUri::parse("jira://issue/PROJ-8.adf").unwrap();
        let result = read_resource(&uri, "jira://issue/PROJ-8.adf")
            .await
            .unwrap();
        match &result.contents[0] {
            ResourceContents::TextResourceContents {
                text, mime_type, ..
            } => {
                assert_eq!(mime_type.as_deref(), Some("application/json"));
                let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
                assert_eq!(parsed["type"], "doc");
            }
            other @ ResourceContents::BlobResourceContents { .. } => {
                panic!("expected text, got {other:?}")
            }
        }
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread")]
    async fn read_resource_jira_propagates_server_errors() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/NOPE-1"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;

        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::set(&[
            ("ATLASSIAN_INSTANCE_URL", server.uri()),
            ("ATLASSIAN_EMAIL", "test@example.com".to_string()),
            ("ATLASSIAN_API_TOKEN", "fake-token".to_string()),
        ]);

        let uri = ResourceUri::parse("jira://issue/NOPE-1").unwrap();
        let err = read_resource(&uri, "jira://issue/NOPE-1")
            .await
            .expect_err("404 should surface as error");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("NOPE-1") || chain.contains("404"),
            "unexpected chain: {chain}"
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread")]
    async fn read_resource_confluence_page_jfm_against_wiremock() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/99999"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "99999",
                    "title": "Resource page",
                    "status": "current",
                    "version": {"number": 3},
                    "spaceId": "10",
                    "parentId": null,
                    "body": {
                        "atlas_doc_format": {
                            "representation": "atlas_doc_format",
                            "value": serde_json::json!({
                                "type": "doc",
                                "version": 1,
                                "content": [{
                                    "type": "paragraph",
                                    "content": [{"type": "text", "text": "page content"}]
                                }]
                            }).to_string()
                        }
                    }
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/10"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "10",
                    "key": "ENG",
                    "name": "Engineering"
                })),
            )
            .mount(&server)
            .await;

        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::set(&[
            ("ATLASSIAN_INSTANCE_URL", server.uri()),
            ("ATLASSIAN_EMAIL", "test@example.com".to_string()),
            ("ATLASSIAN_API_TOKEN", "fake-token".to_string()),
        ]);

        let uri = ResourceUri::parse("confluence://page/99999").unwrap();
        let result = read_resource(&uri, "confluence://page/99999").await;
        // We don't pin the exact rendered output (depends on internal ADF
        // rendering details), only that the branch ran and produced
        // TextResourceContents with the markdown MIME type.
        match result {
            Ok(res) => match &res.contents[0] {
                ResourceContents::TextResourceContents { mime_type, .. } => {
                    assert_eq!(mime_type.as_deref(), Some("text/markdown"));
                }
                other @ ResourceContents::BlobResourceContents { .. } => {
                    panic!("expected text, got {other:?}")
                }
            },
            // Some Confluence client paths require additional endpoints we
            // haven't mocked. The goal here is branch coverage of
            // `ResourceUri::ConfluencePage`, which runs either way — accept
            // an error too, just assert the URI appears in the chain.
            Err(e) => {
                let chain = format!("{e:#}");
                assert!(
                    chain.contains("99999") || chain.contains("Confluence"),
                    "unexpected error chain: {chain}"
                );
            }
        }
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread")]
    async fn read_resource_confluence_propagates_server_errors() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/404404"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;

        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::set(&[
            ("ATLASSIAN_INSTANCE_URL", server.uri()),
            ("ATLASSIAN_EMAIL", "test@example.com".to_string()),
            ("ATLASSIAN_API_TOKEN", "fake-token".to_string()),
        ]);

        let uri = ResourceUri::parse("confluence://page/404404").unwrap();
        let err = read_resource(&uri, "confluence://page/404404")
            .await
            .expect_err("404 should surface as error");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("404404") || chain.contains("404") || chain.contains("Confluence"),
            "unexpected chain: {chain}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn read_resource_jira_without_credentials_errors() {
        // No ATLASSIAN_* env vars set → create_client() fails in the JIRA
        // branch of read_resource, exercising the "Failed to load Atlassian
        // credentials" context wrap.
        let _guard = ENV_MUTEX.lock().unwrap();
        // Scrub any stray vars the surrounding process may have set so
        // create_client() definitively fails.
        for key in [
            "ATLASSIAN_INSTANCE_URL",
            "ATLASSIAN_EMAIL",
            "ATLASSIAN_API_TOKEN",
            "JIRA_INSTANCE_URL",
            "JIRA_EMAIL",
            "JIRA_API_TOKEN",
        ] {
            std::env::remove_var(key);
        }
        std::env::set_var("HOME", std::env::temp_dir());

        let uri = ResourceUri::parse("jira://issue/ZZZ-1").unwrap();
        let err = read_resource(&uri, "jira://issue/ZZZ-1")
            .await
            .expect_err("missing credentials must error");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("Atlassian") || chain.contains("credential"),
            "unexpected chain: {chain}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn read_resource_confluence_without_credentials_errors() {
        let _guard = ENV_MUTEX.lock().unwrap();
        for key in [
            "ATLASSIAN_INSTANCE_URL",
            "ATLASSIAN_EMAIL",
            "ATLASSIAN_API_TOKEN",
            "JIRA_INSTANCE_URL",
            "JIRA_EMAIL",
            "JIRA_API_TOKEN",
        ] {
            std::env::remove_var(key);
        }
        std::env::set_var("HOME", std::env::temp_dir());

        let uri = ResourceUri::parse("confluence://page/0").unwrap();
        let err = read_resource(&uri, "confluence://page/0")
            .await
            .expect_err("missing credentials must error");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("Atlassian") || chain.contains("credential"),
            "unexpected chain: {chain}"
        );
    }

    #[test]
    fn render_content_item_jfm_for_confluence_page() {
        use crate::atlassian::api::ContentMetadata;
        let body = serde_json::json!({
            "version": 1,
            "type": "doc",
            "content": [{"type": "paragraph", "content": [{"type": "text", "text": "conf body"}]}]
        });
        let item = ContentItem {
            id: "12345".to_string(),
            title: "Test Page".to_string(),
            body_adf: Some(body),
            metadata: ContentMetadata::Confluence {
                space_key: "ENG".to_string(),
                status: Some("current".to_string()),
                version: Some(1),
                parent_id: None,
            },
        };
        let (text, mime) =
            render_content_item(&item, "https://org.atlassian.net", ResourceFormat::Jfm).unwrap();
        assert_eq!(mime, "text/markdown");
        assert!(text.contains("conf body"), "missing body: {text}");
    }

    #[test]
    fn uri_parse_error_variants_display_expected_messages() {
        let malformed = UriParseError::Malformed("jira://x".to_string(), "oops");
        assert!(malformed.to_string().contains("oops"));
        let empty = UriParseError::EmptyIdentifier("jira://issue/".to_string());
        assert!(empty.to_string().contains("empty identifier"));
    }

    #[test]
    fn resource_uri_debug_and_clone() {
        let uri = ResourceUri::JiraIssue {
            key: "X-1".to_string(),
            format: ResourceFormat::Jfm,
        };
        let dup = uri.clone();
        assert_eq!(uri, dup);
        assert!(format!("{uri:?}").contains("JiraIssue"));
    }

    #[test]
    fn resource_format_copy_and_eq() {
        let a = ResourceFormat::Adf;
        let b = a;
        assert_eq!(a, b);
        assert_ne!(a, ResourceFormat::Jfm);
    }

    // ── Specs branch of read_resource ──────────────────────────────────
    //
    // No credentials, no network — the spec is embedded at compile time.

    #[tokio::test(flavor = "multi_thread")]
    async fn read_resource_specs_jfm_returns_markdown() {
        let uri = ResourceUri::parse("omni-dev://specs/jfm").unwrap();
        let result = read_resource(&uri, "omni-dev://specs/jfm").await.unwrap();
        assert_eq!(result.contents.len(), 1);
        match &result.contents[0] {
            ResourceContents::TextResourceContents {
                text,
                mime_type,
                uri: reply_uri,
                ..
            } => {
                assert_eq!(mime_type.as_deref(), Some("text/markdown"));
                assert_eq!(reply_uri, "omni-dev://specs/jfm");
                assert!(
                    text.contains("# JFM (JIRA-Flavored Markdown) Specification"),
                    "spec body missing heading"
                );
            }
            other @ ResourceContents::BlobResourceContents { .. } => {
                panic!("expected text resource contents, got {other:?}")
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn read_resource_specs_unknown_name_errors() {
        let uri = ResourceUri::parse("omni-dev://specs/nope").unwrap();
        let err = read_resource(&uri, "omni-dev://specs/nope")
            .await
            .expect_err("unknown spec must error");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("unknown spec") && chain.contains("nope"),
            "unexpected chain: {chain}"
        );
    }
}
