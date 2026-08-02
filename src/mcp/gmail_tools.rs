//! MCP tool handlers for Gmail read (and label-list) operations.
//!
//! Each tool builds a fresh [`GmailClient`] via
//! [`crate::cli::gmail::helpers::create_client`] and then delegates to the
//! same API façade (`MessagesApi`, `ThreadsApi`, `LabelsApi`) that the CLI
//! uses under `src/cli/gmail/`. Tool outputs are YAML serialisations of the
//! typed response structs, matching the CLI `-o yaml` output.
//!
//! `gmail auth login` has no MCP equivalent — it's an interactive browser
//! flow with no non-interactive analogue. `gmail_label_modify`
//! (`label add`/`remove`) is deferred to a fast-follow issue: the issue's
//! own "Initial tools" list names exactly the five tools below, and the
//! mutating tool's confirm-gating deserves its own focused review.

use anyhow::{Context, Result};
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock as Content},
    schemars, tool, tool_router, ErrorData as McpError,
};
use serde::{Deserialize, Serialize};

use crate::cli::gmail::helpers::create_client;
use crate::gmail::auth;
use crate::gmail::client::GmailClient;
use crate::gmail::labels_api::LabelsApi;
use crate::gmail::messages_api::{MessageFormat, MessagesApi, DEFAULT_SEARCH_LIMIT};
use crate::gmail::threads_api::{ThreadFormat, ThreadsApi};

use super::error::tool_error;
use super::git_tools::build_truncated_result;
use super::output_file::write_to_file_yaml;
use super::server::OmniDevServer;

// ── Parameter structs ───────────────────────────────────────────────

/// Parameters for `gmail_auth_status` (none).
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct GmailAuthStatusParams {}

/// Parameters for the `gmail_search` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GmailSearchParams {
    /// Gmail search query, same syntax as the Gmail search box (e.g.
    /// `label:finance after:2026/01/01`). Required.
    pub query: String,
    /// Maximum results. Defaults to 50 when omitted; `0` explicitly means
    /// fetch every match up to the hard cap (10000).
    #[serde(default)]
    pub limit: Option<usize>,
    /// When `true`, enrich each hit with From/Subject/Date/snippet via one
    /// extra `messages.get` request per hit. Defaults to `false` (ids-only,
    /// the quota-safe default — `messages.get` costs 5 units against
    /// Gmail's 250 units/user/second budget).
    #[serde(default)]
    pub enrich: Option<bool>,
    /// Bounds concurrent `messages.get` calls when `enrich` is true (has no
    /// effect otherwise). Defaults to 4, clamped to 1-50 regardless of what
    /// is requested — Gmail's quota is 250 units/user/second and
    /// `messages.get` costs 5 units, so a higher value could burst past it.
    #[serde(default)]
    pub concurrency: Option<usize>,
}

/// Parameters for the `gmail_message_read` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GmailMessageReadParams {
    /// Gmail message id. Required.
    pub message_id: String,
    /// `minimal` (ids/labels only), `metadata` (headers + snippet), `full`
    /// (default; parsed MIME structure), or `raw` (base64url RFC 2822
    /// source). Matches Gmail's own wire values verbatim.
    #[serde(default)]
    pub format: Option<String>,
    /// When set, writes the rendered message to this path and returns a
    /// short YAML summary (path/bytes/format) instead of the inline body —
    /// use for large messages/attachments that would blow past the context
    /// window.
    #[serde(default)]
    pub output_file: Option<String>,
}

/// Parameters for the `gmail_thread_read` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GmailThreadReadParams {
    /// Gmail thread id. Required.
    pub thread_id: String,
}

/// Parameters for `gmail_label_list` (none).
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct GmailLabelListParams {}

// ── Tool handlers ────────────────────────────────────────────────────

#[allow(missing_docs)] // #[tool_router] generates a pub `gmail_tool_router` fn.
#[tool_router(router = gmail_tool_router, vis = "pub")]
impl OmniDevServer {
    /// Reports whether Gmail OAuth2 credentials are configured.
    ///
    /// Presence flags only — never calls the Gmail API and never returns
    /// secret values.
    #[tool(
        description = "Report whether Gmail OAuth2 credentials are configured \
                       (GMAIL_CLIENT_ID/GMAIL_CLIENT_SECRET/refresh token present) and which \
                       scope was granted at login (readonly vs. modify). Returns presence flags \
                       and the granted scope only — NEVER the client secret, refresh token, or \
                       access token. Unlike the CLI `omni-dev gmail auth status`, this tool does \
                       not call the Gmail API and cannot confirm the refresh token is still \
                       accepted (a testing-mode Google Cloud project's refresh tokens expire \
                       after 7 days — use the CLI status command to actually verify). \
                       Read-only, no parameters. Mirrors `omni-dev gmail auth status`."
    )]
    pub async fn gmail_auth_status(
        &self,
        Parameters(_params): Parameters<GmailAuthStatusParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = run_auth_status().map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: search Gmail messages.
    #[tool(
        description = "Search Gmail messages with a Gmail query (same syntax as the Gmail \
                       search box, e.g. `label:finance after:2026/01/01`). Returns only \
                       id/threadId per hit by default (ids-only, quota-safe). Set `enrich: true` \
                       to add From/Subject/Date/snippet — this costs one extra `messages.get` \
                       request per hit, bounded by `concurrency` (default 4). `limit` defaults \
                       to 50 when omitted; pass `0` explicitly to auto-paginate up to a hard cap \
                       (10000) — expensive combined with `enrich: true`, use deliberately. \
                       Read-only. Mirrors `omni-dev gmail search`. Output is YAML."
    )]
    pub async fn gmail_search(
        &self,
        Parameters(params): Parameters<GmailSearchParams>,
    ) -> Result<CallToolResult, McpError> {
        let client = create_client().map_err(tool_error)?;
        let yaml = run_search(&client, &params).await.map_err(tool_error)?;
        Ok(build_truncated_result(yaml))
    }

    /// Tool: read a single Gmail message.
    #[tool(
        description = "Read a single Gmail message by id. `format` is `minimal` (ids/labels \
                       only), `metadata` (headers + snippet only), `full` (default; parsed MIME \
                       structure), or `raw` (base64url-encoded RFC 2822 source) — Gmail's own \
                       wire values verbatim. When `output_file` is set, writes the rendered \
                       message to that path and returns a short YAML summary instead of the \
                       inline body — use it for large messages or ones with attachments that \
                       would exceed the response size limit. Read-only. \
                       Mirrors `omni-dev gmail read`. Output is YAML."
    )]
    pub async fn gmail_message_read(
        &self,
        Parameters(params): Parameters<GmailMessageReadParams>,
    ) -> Result<CallToolResult, McpError> {
        let client = create_client().map_err(tool_error)?;
        let wrote_to_file = params.output_file.is_some();
        let text = run_message_read(&client, &params)
            .await
            .map_err(tool_error)?;
        if wrote_to_file {
            Ok(CallToolResult::success(vec![Content::text(text)]))
        } else {
            Ok(build_truncated_result(text))
        }
    }

    /// Tool: read a Gmail thread.
    #[tool(
        description = "Read a full Gmail thread (conversation) by id, including every message \
                       in it. A thread is N messages, each potentially carrying attachments — \
                       the single highest-risk payload on the whole Gmail surface for exceeding \
                       the response size limit, so large threads are automatically truncated \
                       with a marker. Read-only. \
                       Mirrors `omni-dev gmail thread`. Output is YAML."
    )]
    pub async fn gmail_thread_read(
        &self,
        Parameters(params): Parameters<GmailThreadReadParams>,
    ) -> Result<CallToolResult, McpError> {
        let client = create_client().map_err(tool_error)?;
        let yaml = run_thread_read(&client, &params)
            .await
            .map_err(tool_error)?;
        Ok(build_truncated_result(yaml))
    }

    /// Tool: list Gmail labels.
    #[tool(
        description = "List every label on the Gmail mailbox (system labels like INBOX/TRASH \
                       and user-created ones), with unread/total message counts. Adding or \
                       removing labels on messages is CLI-only in this release \
                       (`omni-dev gmail label add`/`remove`) — no MCP tool mutates labels yet. \
                       Read-only, no parameters. \
                       Mirrors `omni-dev gmail label list`. Output is YAML."
    )]
    pub async fn gmail_label_list(
        &self,
        Parameters(_params): Parameters<GmailLabelListParams>,
    ) -> Result<CallToolResult, McpError> {
        let client = create_client().map_err(tool_error)?;
        let yaml = run_label_list(&client).await.map_err(tool_error)?;
        Ok(build_truncated_result(yaml))
    }
}

// ── Internal run_* implementations ──────────────────────────────────
//
// Split out from the tool handlers, taking an already-resolved
// `&GmailClient`, so they can be tested against a wiremock-backed client
// without needing real credentials (mirrors `run_jira_read` in
// `jira_core_tools.rs`; unlike the Datadog tools, Gmail has no
// `GMAIL_API_URL`-style env override to redirect `create_client()` itself).

/// Renders the credential-presence summary as YAML.
///
/// Pure: never touches the network and never reads any secret values.
fn run_auth_status() -> Result<String> {
    let status = auth::status();
    serde_yaml::to_string(&status).context("Failed to serialize Gmail auth status")
}

async fn run_search(client: &GmailClient, params: &GmailSearchParams) -> Result<String> {
    let limit = params.limit.unwrap_or(DEFAULT_SEARCH_LIMIT);
    let api = MessagesApi::new(client);
    if params.enrich.unwrap_or(false) {
        let concurrency = params.concurrency.unwrap_or(4);
        let summaries = api
            .search_summaries(Some(&params.query), &[], limit, concurrency)
            .await?;
        yaml_result(&summaries)
    } else {
        let list = api.search_all(Some(&params.query), &[], limit).await?;
        yaml_result(&list.messages)
    }
}

async fn run_message_read(client: &GmailClient, params: &GmailMessageReadParams) -> Result<String> {
    let format = parse_message_format(params.format.as_deref())?;
    let message = MessagesApi::new(client)
        .get(&params.message_id, format, &[])
        .await?;
    let yaml = yaml_result(&message)?;
    match params.output_file.as_deref() {
        Some(path) => write_to_file_yaml(path, &yaml, message_format_label(format)),
        None => Ok(yaml),
    }
}

async fn run_thread_read(client: &GmailClient, params: &GmailThreadReadParams) -> Result<String> {
    let thread = ThreadsApi::new(client)
        .get(&params.thread_id, ThreadFormat::Full)
        .await?;
    yaml_result(&thread)
}

async fn run_label_list(client: &GmailClient) -> Result<String> {
    let response = LabelsApi::new(client).list().await?;
    yaml_result(&response.labels)
}

/// Parses an MCP-supplied message format string.
///
/// Accepts the same names as the CLI `--detail` arg (`minimal`/`metadata`/
/// `full`/`raw` — Gmail's own wire values verbatim); `None` defaults to
/// `full`.
fn parse_message_format(raw: Option<&str>) -> Result<MessageFormat> {
    match raw.map(str::to_ascii_lowercase).as_deref() {
        None | Some("full") => Ok(MessageFormat::Full),
        Some("minimal") => Ok(MessageFormat::Minimal),
        Some("metadata") => Ok(MessageFormat::Metadata),
        Some("raw") => Ok(MessageFormat::Raw),
        Some(other) => anyhow::bail!(
            "unknown format {other:?} (expected 'minimal', 'metadata', 'full', or 'raw')"
        ),
    }
}

/// String label used in [`super::output_file::WriteFileSummary`].
fn message_format_label(format: MessageFormat) -> &'static str {
    match format {
        MessageFormat::Minimal => "minimal",
        MessageFormat::Metadata => "metadata",
        MessageFormat::Full => "full",
        MessageFormat::Raw => "raw",
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
    use crate::gmail::auth::{GmailCredentials, GmailScope};
    use crate::gmail::test_support::EnvGuard;
    use crate::utils::secret::Secret;

    fn test_credentials() -> GmailCredentials {
        GmailCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: GmailScope::ReadOnly,
        }
    }

    async fn client_with_bootstrapped_token(server: &wiremock::MockServer) -> GmailClient {
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

        let mut client = GmailClient::new(&server.uri(), &test_credentials()).unwrap();
        crate::gmail::client::test_support::replace_session(
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

    // ── parse_message_format ─────────────────────────────────────────

    #[test]
    fn parse_message_format_defaults_to_full() {
        assert!(matches!(
            parse_message_format(None).unwrap(),
            MessageFormat::Full
        ));
    }

    #[test]
    fn parse_message_format_accepts_known_strings() {
        assert!(matches!(
            parse_message_format(Some("minimal")).unwrap(),
            MessageFormat::Minimal
        ));
        assert!(matches!(
            parse_message_format(Some("metadata")).unwrap(),
            MessageFormat::Metadata
        ));
        assert!(matches!(
            parse_message_format(Some("full")).unwrap(),
            MessageFormat::Full
        ));
        assert!(matches!(
            parse_message_format(Some("raw")).unwrap(),
            MessageFormat::Raw
        ));
    }

    #[test]
    fn parse_message_format_is_case_insensitive() {
        assert!(matches!(
            parse_message_format(Some("METADATA")).unwrap(),
            MessageFormat::Metadata
        ));
    }

    #[test]
    fn parse_message_format_rejects_unknown_value() {
        let err = parse_message_format(Some("meta")).unwrap_err();
        assert!(err.to_string().contains("format"));
    }

    // ── message_format_label ────────────────────────────────────────

    #[test]
    fn message_format_label_matches_wire_values() {
        assert_eq!(message_format_label(MessageFormat::Minimal), "minimal");
        assert_eq!(message_format_label(MessageFormat::Metadata), "metadata");
        assert_eq!(message_format_label(MessageFormat::Full), "full");
        assert_eq!(message_format_label(MessageFormat::Raw), "raw");
    }

    // ── run_auth_status ────────────────────────────────────────────────

    #[test]
    fn run_auth_status_reports_unconfigured_state() {
        let guard = EnvGuard::take();
        let _dir = guard.clear_credentials();

        let yaml = run_auth_status().unwrap();
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
                "GMAIL_CLIENT_ID":"client-visible",
                "GMAIL_CLIENT_SECRET":"sekret-do-not-leak",
                "GMAIL_REFRESH_TOKEN":"sekret-refresh-do-not-leak",
                "GMAIL_SCOPE":"https://www.googleapis.com/auth/gmail.readonly"
            }}"#,
        )
        .unwrap();
        std::env::remove_var(auth::GMAIL_CLIENT_ID);
        std::env::remove_var(auth::GMAIL_CLIENT_SECRET);
        std::env::remove_var(auth::GMAIL_REFRESH_TOKEN);
        std::env::remove_var(auth::GMAIL_SCOPE);

        let yaml = run_auth_status().unwrap();
        assert!(yaml.contains("has_client_id: true"));
        assert!(yaml.contains("client-visible") || yaml.contains("has_client_id: true"));
        assert!(!yaml.contains("sekret-do-not-leak"));
        assert!(!yaml.contains("sekret-refresh-do-not-leak"));
    }

    // ── run_search ───────────────────────────────────────────────────

    #[tokio::test]
    async fn run_search_defaults_to_ids_only_and_makes_no_hydration_call() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "messages": [{"id": "m1", "threadId": "t1"}]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;
        // No mock for GET .../messages/m1 — if `run_search` hydrated by
        // default, wiremock would 404 and the call would fail.

        let yaml = run_search(
            &client,
            &GmailSearchParams {
                query: "label:finance".to_string(),
                limit: Some(10),
                enrich: None,
                concurrency: None,
            },
        )
        .await
        .unwrap();
        assert!(yaml.contains("id: m1"));
        assert!(yaml.contains("threadId: t1"));
    }

    #[tokio::test]
    async fn run_search_omitted_limit_defaults_to_50_not_hard_cap() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages"))
            .and(wiremock::matchers::query_param("maxResults", "50"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "messages": [{"id": "m1", "threadId": "t1"}]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;
        // No mock matching any other `maxResults` value — if an omitted
        // `limit` fell back to 0 (fetch-to-HARD_CAP) instead of the CLI's
        // 50, the request above wouldn't match and wiremock would 404.

        let yaml = run_search(
            &client,
            &GmailSearchParams {
                query: "label:finance".to_string(),
                limit: None,
                enrich: None,
                concurrency: None,
            },
        )
        .await
        .unwrap();
        assert!(yaml.contains("id: m1"));
    }

    #[tokio::test]
    async fn run_search_enrich_true_hydrates_each_hit() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "messages": [{"id": "m1", "threadId": "t1"}]
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages/m1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "m1",
                    "snippet": "Hi there",
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let yaml = run_search(
            &client,
            &GmailSearchParams {
                query: "label:finance".to_string(),
                limit: Some(10),
                enrich: Some(true),
                concurrency: Some(2),
            },
        )
        .await
        .unwrap();
        assert!(yaml.contains("Hi there"));
    }

    #[tokio::test]
    async fn run_search_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("forbidden"))
            .mount(&server)
            .await;

        let err = run_search(
            &client,
            &GmailSearchParams {
                query: "*".to_string(),
                limit: None,
                enrich: None,
                concurrency: None,
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    // ── run_message_read ───────────────────────────────────────────────

    #[tokio::test]
    async fn run_message_read_returns_yaml_object() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages/m1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "m1",
                    "snippet": "Hi",
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let yaml = run_message_read(
            &client,
            &GmailMessageReadParams {
                message_id: "m1".to_string(),
                format: None,
                output_file: None,
            },
        )
        .await
        .unwrap();
        assert!(yaml.contains("id: m1"));
    }

    #[tokio::test]
    async fn run_message_read_writes_to_output_file() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages/m1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "m1"})),
            )
            .mount(&server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("message.yaml");
        let summary_yaml = run_message_read(
            &client,
            &GmailMessageReadParams {
                message_id: "m1".to_string(),
                format: None,
                output_file: Some(path.to_str().unwrap().to_string()),
            },
        )
        .await
        .unwrap();

        assert!(summary_yaml.contains("bytes:"));
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("id: m1"));
    }

    #[tokio::test]
    async fn run_message_read_rejects_invalid_format() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;

        let err = run_message_read(
            &client,
            &GmailMessageReadParams {
                message_id: "m1".to_string(),
                format: Some("bogus".to_string()),
                output_file: None,
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("format"));
    }

    // ── run_thread_read ────────────────────────────────────────────────

    #[tokio::test]
    async fn run_thread_read_returns_yaml_with_messages() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/threads/t1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "t1",
                    "messages": [{"id": "m1", "threadId": "t1"}],
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let yaml = run_thread_read(
            &client,
            &GmailThreadReadParams {
                thread_id: "t1".to_string(),
            },
        )
        .await
        .unwrap();
        assert!(yaml.contains("id: t1"));
    }

    // ── run_label_list ─────────────────────────────────────────────────

    #[tokio::test]
    async fn run_label_list_returns_yaml_array() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/labels"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "labels": [{"id": "INBOX", "name": "INBOX", "type": "system"}]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let yaml = run_label_list(&client).await.unwrap();
        assert!(yaml.contains("INBOX"));
    }

    // ── Tool handler bodies (smoke + auth-status full path) ───────────

    #[tokio::test(flavor = "current_thread")]
    async fn gmail_auth_status_handler_returns_yaml_no_secrets() {
        let guard = EnvGuard::take();
        let dir = guard.clear_credentials();
        let omni_dir = dir.path().join(".omni-dev");
        std::fs::create_dir_all(&omni_dir).unwrap();
        std::fs::write(
            omni_dir.join("settings.json"),
            r#"{"env":{
                "GMAIL_CLIENT_ID":"client-1",
                "GMAIL_CLIENT_SECRET":"sekret-secret",
                "GMAIL_REFRESH_TOKEN":"sekret-refresh"
            }}"#,
        )
        .unwrap();
        std::env::remove_var(auth::GMAIL_CLIENT_ID);
        std::env::remove_var(auth::GMAIL_CLIENT_SECRET);
        std::env::remove_var(auth::GMAIL_REFRESH_TOKEN);

        let server = OmniDevServer::new();
        let result = server
            .gmail_auth_status(Parameters(GmailAuthStatusParams::default()))
            .await
            .unwrap();
        let body = handler_text(&result);
        assert!(body.contains("has_client_id: true"));
        assert!(!body.contains("sekret-secret"));
        assert!(!body.contains("sekret-refresh"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn gmail_search_handler_propagates_credentials_error() {
        let guard = EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = OmniDevServer::new();
        let err = server
            .gmail_search(Parameters(GmailSearchParams {
                query: "*".to_string(),
                limit: None,
                enrich: None,
                concurrency: None,
            }))
            .await
            .unwrap_err();
        assert!(err.message.contains("not configured"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn gmail_message_read_handler_rejects_invalid_format() {
        // Even with credentials missing, format parsing happens after
        // client creation in this handler, so this exercises the
        // credentials-error path — the invalid-format path is covered by
        // `run_message_read_rejects_invalid_format` above (client-level).
        let guard = EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = OmniDevServer::new();
        let err = server
            .gmail_message_read(Parameters(GmailMessageReadParams {
                message_id: "m1".to_string(),
                format: Some("bogus".to_string()),
                output_file: None,
            }))
            .await
            .unwrap_err();
        assert!(err.message.contains("not configured"));
    }
}
