//! Atlassian Cloud REST API client.
//!
//! Provides HTTP access to JIRA Cloud REST API v3 for reading and
//! writing issues. Uses Basic Auth (email + API token).

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use base64::Engine;
use reqwest::Client;
use tokio_util::io::ReaderStream;

use crate::atlassian::adf::AdfDocument;
use crate::atlassian::adf_validated::ValidatedAdfDocument;
use crate::atlassian::confluence_types::{
    ConfluenceContentSearchResponse, ConfluenceSearchResult, ConfluenceSearchResults,
    ConfluenceUserGetEntry, ConfluenceUserGetResults, ConfluenceUserRecord,
    ConfluenceUserSearchResponse, ConfluenceUserSearchResult, ConfluenceUserSearchResults,
};
use crate::atlassian::convert::adf_to_markdown;
use crate::atlassian::error::AtlassianError;
use crate::atlassian::jira_types::{
    AgileBoard, AgileBoardList, AgileBoardListResponse, AgileIssueListResponse, AgileSprint,
    AgileSprintEntry, AgileSprintList, AgileSprintListResponse, CreateMeta, CreateMetaField,
    DevStatusCommit, DevStatusResponse, DevStatusSummaryCategory, DevStatusSummaryResponse,
    EditMeta, EditMetaField, FieldSelection, JiraAllowedValueRaw, JiraAttachment,
    JiraAttachmentEntry, JiraAttachmentIssueResponse, JiraChangelogEntry, JiraChangelogItem,
    JiraChangelogResponse, JiraComment, JiraCommentEntry, JiraCommentsResponse,
    JiraCreateMetaFullResponse, JiraCreateMetaResponse, JiraCreateMetaSchemaRaw,
    JiraCreateResponse, JiraCreatedIssue, JiraDevBranch, JiraDevCommit, JiraDevProvider,
    JiraDevPullRequest, JiraDevRepository, JiraDevStatus, JiraDevStatusCount, JiraDevStatusSummary,
    JiraEditMetaField, JiraEditMetaResponse, JiraField, JiraFieldContextsResponse, JiraFieldEntry,
    JiraFieldOption, JiraFieldOptionsResponse, JiraIssue, JiraIssueEnvelope, JiraIssueIdResponse,
    JiraIssueLink, JiraIssueLinksResponse, JiraLinkType, JiraLinkTypesResponse, JiraProject,
    JiraProjectList, JiraProjectSearchResponse, JiraProjectVersion, JiraProjectVersionEntry,
    JiraProjectVersionList, JiraRemoteIssueLink, JiraRemoteIssueLinkEntry, JiraRemoteIssueLinkIcon,
    JiraRemoteIssueLinkObject, JiraSearchResponse, JiraSearchResult, JiraTransition,
    JiraTransitionEntry, JiraTransitionToStatus, JiraTransitionsResponse, JiraUser,
    JiraUserGetResults, JiraUserRecord, JiraUserSearchEntry, JiraUserSearchResult,
    JiraUserSearchResults, JiraVisibility, JiraWatcherList, JiraWorklog, JiraWorklogList,
    JiraWorklogResponse, TEXTAREA_CUSTOM_TYPE,
};
use crate::request_log;
use crate::utils::http::{retry_429, REQUEST_TIMEOUT};

/// Internal page size for auto-pagination. Individual API calls request
/// this many items per page; the `limit` parameter controls the total.
const PAGE_SIZE: u32 = 100;

/// JIRA's standard error envelope returned by REST API v3 on validation
/// failures: `{ "errorMessages": [...], "errors": { "<field_id>": "<msg>" } }`.
#[derive(serde::Deserialize)]
struct JiraErrorEnvelope {
    #[serde(default, rename = "errorMessages")]
    _error_messages: Vec<String>,
    #[serde(default)]
    errors: std::collections::BTreeMap<String, String>,
}

/// Builds an `anyhow::Error` for a non-success JIRA write response.
///
/// On HTTP 400, parses `body` as JIRA's standard
/// `{ "errorMessages": [...], "errors": {...} }` envelope and looks for
/// per-field errors whose message indicates the field requires an ADF
/// document (substring `"atlassian document"`, case-insensitive). When at
/// least one such field is found, returns
/// [`AtlassianError::JiraAdfFieldRequired`] naming the offending field
/// IDs. All other status codes (and 400 responses with no detected
/// ADF-required message) fall back to [`AtlassianError::ApiRequestFailed`].
fn jira_write_error(status: u16, body: String) -> anyhow::Error {
    if status == 400 {
        if let Ok(parsed) = serde_json::from_str::<JiraErrorEnvelope>(&body) {
            let needle = "atlassian document";
            let matching: Vec<(&String, &String)> = parsed
                .errors
                .iter()
                .filter(|(_, msg)| msg.to_ascii_lowercase().contains(needle))
                .collect();
            if !matching.is_empty() {
                let fields: Vec<String> = matching.iter().map(|(k, _)| (*k).clone()).collect();
                let original_message = matching[0].1.clone();
                return AtlassianError::JiraAdfFieldRequired {
                    fields,
                    original_message,
                    body,
                }
                .into();
            }
        }
    }
    AtlassianError::ApiRequestFailed { status, body }.into()
}

/// Shared HTTP client for Atlassian Cloud REST APIs.
///
/// Backs every JIRA, Confluence, and Agile helper exposed by this crate.
/// Construct directly via [`AtlassianClient::new`] (instance URL + email + API
/// token) or, more commonly, via [`AtlassianClient::from_credentials`] which
/// accepts an [`AtlassianCredentials`](crate::atlassian::auth::AtlassianCredentials)
/// resolved from the `ATLASSIAN_INSTANCE_URL`, `ATLASSIAN_EMAIL`, and
/// `ATLASSIAN_API_TOKEN` environment variables (falling back to
/// `~/.omni-dev/settings.json`) by
/// [`load_credentials`](crate::atlassian::auth::load_credentials).
///
/// Authenticates every request with HTTP Basic auth: a precomputed
/// `Authorization: Basic <base64(email:api_token)>` header is attached to all
/// outbound calls. Requests time out after 30s and automatically retry up to
/// three times on HTTP 429, honoring any `Retry-After` header.
pub struct AtlassianClient {
    client: Client,
    instance_url: String,
    auth_header: String,
}

/// Maps a raw `(schema.type, schema.custom)` pair from the JIRA field API into
/// the value omni-dev surfaces as `schema_type`. Rich-text custom fields are
/// reported as `"richtext"` so callers can detect ADF-required fields without
/// inspecting the plugin URI; all other fields pass through unchanged.
fn map_schema_type(raw_type: Option<String>, raw_custom: Option<&str>) -> Option<String> {
    if raw_custom == Some(TEXTAREA_CUSTOM_TYPE) {
        return Some("richtext".to_string());
    }
    raw_type
}

/// Builds an [`EditMeta`] from a raw JIRA field-metadata map.
///
/// The editmeta, createmeta, and `expand=transitions.fields` transitions
/// responses all carry fields in the same [`JiraEditMetaField`] shape, so this
/// is the single normalization point for all three.
fn edit_meta_from_raw_fields(
    raw: std::collections::BTreeMap<String, JiraEditMetaField>,
) -> EditMeta {
    let fields = raw
        .into_iter()
        .map(|(id, field)| {
            let allowed_values = field.allowed_value_strings();
            (
                id,
                EditMetaField {
                    name: field.name.unwrap_or_default(),
                    schema: field.schema.into(),
                    allowed_values,
                },
            )
        })
        .collect();
    EditMeta { fields }
}

/// Maps a raw transitions-response entry to the public [`JiraTransition`],
/// dropping any expanded screen-field metadata (captured separately by
/// [`AtlassianClient::get_transitions_with_fields`]).
fn transition_from_entry(t: JiraTransitionEntry) -> JiraTransition {
    JiraTransition {
        id: t.id,
        name: t.name,
        to_status: t.to.map(|to| JiraTransitionToStatus {
            id: to.id,
            name: to.name,
            category: to.status_category.and_then(|sc| sc.key),
        }),
        has_screen: t.has_screen,
    }
}

/// Validates that a date string is `YYYY-MM-DD`.
///
/// Surfaces a clear error before the request is sent, so callers don't
/// have to interpret JIRA's opaque 400s on malformed dates.
fn validate_iso_date(date: Option<&str>, field: &str) -> Result<()> {
    let Some(d) = date else { return Ok(()) };
    chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d")
        .with_context(|| format!("{field} must be YYYY-MM-DD, got {d:?}"))?;
    Ok(())
}

/// Builds the `error` string stored on a stub user record when a single
/// account-ID lookup fails. Includes a short body snippet when the API
/// returned one so callers can distinguish "not found" from "no permission".
fn user_lookup_error(status: u16, body: &str) -> String {
    let snippet = body.trim();
    if snippet.is_empty() {
        format!("HTTP {status}")
    } else {
        let snippet: String = snippet.chars().take(200).collect();
        format!("HTTP {status}: {snippet}")
    }
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::items_after_test_module
)]
mod tests {
    use super::*;
    use crate::atlassian::jira_types::{DevStatusAuthor, JiraIssueResponse, JiraVisibilityType};

    #[test]
    fn new_client_strips_trailing_slash() {
        let client =
            AtlassianClient::new("https://org.atlassian.net/", "user@test.com", "token").unwrap();
        assert_eq!(client.instance_url(), "https://org.atlassian.net");
    }

    #[test]
    fn new_client_preserves_clean_url() {
        let client =
            AtlassianClient::new("https://org.atlassian.net", "user@test.com", "token").unwrap();
        assert_eq!(client.instance_url(), "https://org.atlassian.net");
    }

    #[test]
    fn new_client_sets_basic_auth() {
        let client =
            AtlassianClient::new("https://org.atlassian.net", "user@test.com", "token").unwrap();
        let expected_credentials = "user@test.com:token";
        let expected_encoded =
            base64::engine::general_purpose::STANDARD.encode(expected_credentials);
        assert_eq!(client.auth_header, format!("Basic {expected_encoded}"));
    }

    #[test]
    fn from_credentials() {
        let creds = crate::atlassian::auth::AtlassianCredentials {
            instance_url: "https://org.atlassian.net".to_string(),
            email: "user@test.com".to_string(),
            api_token: "token123".into(),
        };
        let client = AtlassianClient::from_credentials(&creds).unwrap();
        assert_eq!(client.instance_url(), "https://org.atlassian.net");
    }

    #[test]
    fn jira_issue_struct_fields() {
        let issue = JiraIssue {
            key: "TEST-1".to_string(),
            summary: "Test issue".to_string(),
            description_adf: None,
            status: Some("Open".to_string()),
            issue_type: Some("Bug".to_string()),
            assignee: Some("Alice".to_string()),
            priority: Some("High".to_string()),
            labels: vec!["backend".to_string()],
            custom_fields: Vec::new(),
        };
        assert_eq!(issue.key, "TEST-1");
        assert_eq!(issue.labels.len(), 1);
    }

    #[test]
    fn jira_user_deserialization() {
        let json = r#"{
            "displayName": "Alice Smith",
            "emailAddress": "alice@example.com",
            "accountId": "abc123"
        }"#;
        let user: JiraUser = serde_json::from_str(json).unwrap();
        assert_eq!(user.display_name, "Alice Smith");
        assert_eq!(user.email_address.as_deref(), Some("alice@example.com"));
        assert_eq!(user.account_id, "abc123");
    }

    #[test]
    fn jira_user_optional_email() {
        let json = r#"{
            "displayName": "Bot",
            "accountId": "bot123"
        }"#;
        let user: JiraUser = serde_json::from_str(json).unwrap();
        assert!(user.email_address.is_none());
    }

    #[test]
    fn jira_issue_response_deserialization() {
        let json = r#"{
            "key": "PROJ-42",
            "fields": {
                "summary": "Test",
                "description": null,
                "status": {"name": "Open"},
                "issuetype": {"name": "Bug"},
                "assignee": {"displayName": "Bob"},
                "priority": {"name": "Medium"},
                "labels": ["frontend"]
            }
        }"#;
        let response: JiraIssueResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.key, "PROJ-42");
        assert_eq!(response.fields.summary.as_deref(), Some("Test"));
        assert_eq!(response.fields.labels, vec!["frontend"]);
    }

    #[test]
    fn jira_issue_response_minimal_fields() {
        let json = r#"{
            "key": "PROJ-1",
            "fields": {
                "summary": null,
                "description": null,
                "status": null,
                "issuetype": null,
                "assignee": null,
                "priority": null,
                "labels": []
            }
        }"#;
        let response: JiraIssueResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.key, "PROJ-1");
        assert!(response.fields.summary.is_none());
    }

    #[tokio::test]
    async fn get_json_retries_on_429() {
        let server = wiremock::MockServer::start().await;

        // First request returns 429 with Retry-After: 0
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/test"))
            .respond_with(wiremock::ResponseTemplate::new(429).append_header("Retry-After", "0"))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        // Second request succeeds
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/test"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let resp = client
            .get_json(&format!("{}/test", server.uri()))
            .await
            .unwrap();
        assert!(resp.status().is_success());
    }

    #[tokio::test]
    async fn get_json_returns_429_after_max_retries() {
        let server = wiremock::MockServer::start().await;

        // All requests return 429
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/test"))
            .respond_with(wiremock::ResponseTemplate::new(429).append_header("Retry-After", "0"))
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let resp = client
            .get_json(&format!("{}/test", server.uri()))
            .await
            .unwrap();
        // After max retries, returns the 429 response to the caller
        assert_eq!(resp.status().as_u16(), 429);
    }

    // ── user-get (account ID → record) ────────────────────────────

    #[test]
    fn user_lookup_error_formats() {
        assert_eq!(user_lookup_error(404, ""), "HTTP 404");
        assert_eq!(user_lookup_error(404, "   "), "HTTP 404");
        assert_eq!(
            user_lookup_error(403, "no permission"),
            "HTTP 403: no permission"
        );
    }

    #[tokio::test]
    async fn get_jira_user_success() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/user"))
            .and(wiremock::matchers::query_param("accountId", "abc123"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "accountId": "abc123",
                    "displayName": "Alice Smith",
                    "emailAddress": "alice@example.com",
                    "active": true,
                    "accountType": "atlassian"
                })),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let record = client.get_jira_user("abc123").await.unwrap();
        assert_eq!(record.account_id, "abc123");
        assert_eq!(record.display_name.as_deref(), Some("Alice Smith"));
        assert_eq!(record.email_address.as_deref(), Some("alice@example.com"));
        assert_eq!(record.active, Some(true));
        assert_eq!(record.account_type.as_deref(), Some("atlassian"));
        assert!(record.error.is_none());
    }

    #[tokio::test]
    async fn get_jira_user_deactivated_is_a_record_not_an_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/user"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "accountId": "gone1",
                    "active": false,
                    "accountType": "atlassian"
                })),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let record = client.get_jira_user("gone1").await.unwrap();
        assert_eq!(record.account_id, "gone1");
        assert_eq!(record.active, Some(false));
        assert!(record.display_name.is_none());
        assert!(record.error.is_none());
    }

    #[tokio::test]
    async fn get_jira_user_not_found_yields_stub() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/user"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not found"))
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let record = client.get_jira_user("missing").await.unwrap();
        assert_eq!(record.account_id, "missing");
        assert!(record.display_name.is_none());
        assert!(record.error.as_deref().unwrap().starts_with("HTTP 404"));
    }

    #[tokio::test]
    async fn get_jira_user_unauthorized_is_hard_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/user"))
            .respond_with(wiremock::ResponseTemplate::new(401).set_body_string("Unauthorized"))
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        assert!(client.get_jira_user("whoever").await.is_err());
    }

    #[tokio::test]
    async fn get_jira_users_batch_survives_one_bad_id() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/user"))
            .and(wiremock::matchers::query_param("accountId", "good"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "accountId": "good",
                    "displayName": "Good User",
                    "active": true
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/user"))
            .and(wiremock::matchers::query_param("accountId", "bad"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not found"))
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let ids = vec!["good".to_string(), "bad".to_string()];
        let results = client.get_jira_users(&ids).await.unwrap();
        assert_eq!(results.users.len(), 2);
        assert_eq!(results.users[0].account_id, "good");
        assert_eq!(results.users[0].display_name.as_deref(), Some("Good User"));
        assert!(results.users[0].error.is_none());
        assert_eq!(results.users[1].account_id, "bad");
        assert!(results.users[1].error.is_some());
    }

    #[tokio::test]
    async fn get_jira_users_empty_input_makes_no_requests() {
        let client = AtlassianClient::new("https://org.atlassian.net", "u@t.com", "tok").unwrap();
        let results = client.get_jira_users(&[]).await.unwrap();
        assert!(results.users.is_empty());
    }

    #[tokio::test]
    async fn get_confluence_user_success() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/rest/api/user"))
            .and(wiremock::matchers::query_param("accountId", "abc123"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "accountId": "abc123",
                    "accountType": "atlassian",
                    "displayName": "Alice Smith",
                    "email": "alice@example.com"
                })),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let record = client.get_confluence_user("abc123").await.unwrap();
        assert_eq!(record.account_id, "abc123");
        assert_eq!(record.display_name.as_deref(), Some("Alice Smith"));
        assert_eq!(record.email.as_deref(), Some("alice@example.com"));
        assert_eq!(record.account_type.as_deref(), Some("atlassian"));
        assert!(record.active.is_none());
        assert!(record.error.is_none());
    }

    #[tokio::test]
    async fn get_confluence_user_falls_back_to_public_name() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/rest/api/user"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "accountId": "app1",
                    "accountType": "app",
                    "publicName": "Automation App"
                })),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let record = client.get_confluence_user("app1").await.unwrap();
        assert_eq!(record.display_name.as_deref(), Some("Automation App"));
    }

    #[tokio::test]
    async fn get_confluence_user_not_found_yields_stub() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/rest/api/user"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not found"))
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let record = client.get_confluence_user("missing").await.unwrap();
        assert_eq!(record.account_id, "missing");
        assert!(record.error.as_deref().unwrap().starts_with("HTTP 404"));
    }

    #[tokio::test]
    async fn get_confluence_user_unauthorized_is_hard_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/rest/api/user"))
            .respond_with(wiremock::ResponseTemplate::new(401).set_body_string("Unauthorized"))
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        assert!(client.get_confluence_user("whoever").await.is_err());
    }

    #[tokio::test]
    async fn get_confluence_users_batch_survives_one_bad_id() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/rest/api/user"))
            .and(wiremock::matchers::query_param("accountId", "good"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "accountId": "good",
                    "displayName": "Good User"
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/rest/api/user"))
            .and(wiremock::matchers::query_param("accountId", "bad"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not found"))
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let ids = vec!["good".to_string(), "bad".to_string()];
        let results = client.get_confluence_users(&ids).await.unwrap();
        assert_eq!(results.users.len(), 2);
        assert_eq!(results.users[0].display_name.as_deref(), Some("Good User"));
        assert!(results.users[0].error.is_none());
        assert!(results.users[1].error.is_some());
    }

    #[tokio::test]
    async fn post_json_retries_on_429() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/test"))
            .respond_with(wiremock::ResponseTemplate::new(429).append_header("Retry-After", "0"))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/test"))
            .respond_with(wiremock::ResponseTemplate::new(201))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let body = serde_json::json!({"key": "value"});
        let resp = client
            .post_json(&format!("{}/test", server.uri()), &body)
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 201);
    }

    #[tokio::test]
    async fn delete_retries_on_429() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/test"))
            .respond_with(wiremock::ResponseTemplate::new(429).append_header("Retry-After", "0"))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/test"))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let resp = client
            .delete(&format!("{}/test", server.uri()))
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 204);
    }

    #[tokio::test]
    async fn get_json_sends_auth_header() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::header(
                "Authorization",
                "Basic dXNlckB0ZXN0LmNvbTp0b2tlbg==",
            ))
            .and(wiremock::matchers::header("Accept", "application/json"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let resp = client
            .get_json(&format!("{}/test", server.uri()))
            .await
            .unwrap();
        assert!(resp.status().is_success());
    }

    #[tokio::test]
    async fn put_json_sends_body_and_auth() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::header(
                "Authorization",
                "Basic dXNlckB0ZXN0LmNvbTp0b2tlbg==",
            ))
            .and(wiremock::matchers::header(
                "Content-Type",
                "application/json",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let body = serde_json::json!({"key": "value"});
        let resp = client
            .put_json(&format!("{}/test", server.uri()), &body)
            .await
            .unwrap();
        assert!(resp.status().is_success());
    }

    #[tokio::test]
    async fn post_json_sends_body_and_auth() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::header(
                "Authorization",
                "Basic dXNlckB0ZXN0LmNvbTp0b2tlbg==",
            ))
            .and(wiremock::matchers::header(
                "Content-Type",
                "application/json",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(201).set_body_json(serde_json::json!({"id": "1"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let body = serde_json::json!({"name": "test"});
        let resp = client
            .post_json(&format!("{}/test", server.uri()), &body)
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 201);
    }

    #[tokio::test]
    async fn post_json_error_response() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(400).set_body_string("Bad Request"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let body = serde_json::json!({});
        let resp = client
            .post_json(&format!("{}/test", server.uri()), &body)
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 400);
    }

    #[tokio::test]
    async fn delete_sends_auth_header() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::header(
                "Authorization",
                "Basic dXNlckB0ZXN0LmNvbTp0b2tlbg==",
            ))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let resp = client
            .delete(&format!("{}/test", server.uri()))
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 204);
    }

    #[tokio::test]
    async fn delete_error_response() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let resp = client
            .delete(&format!("{}/test", server.uri()))
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 404);
    }

    #[tokio::test]
    async fn get_issue_success() {
        let server = wiremock::MockServer::start().await;

        let issue_json = serde_json::json!({
            "key": "PROJ-42",
            "fields": {
                "summary": "Fix the bug",
                "description": {
                    "version": 1,
                    "type": "doc",
                    "content": [{"type": "paragraph", "content": [{"type": "text", "text": "Details"}]}]
                },
                "status": {"name": "Open"},
                "issuetype": {"name": "Bug"},
                "assignee": {"displayName": "Alice"},
                "priority": {"name": "High"},
                "labels": ["backend", "urgent"]
            }
        });

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-42"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(&issue_json))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let issue = client.get_issue("PROJ-42").await.unwrap();

        assert_eq!(issue.key, "PROJ-42");
        assert_eq!(issue.summary, "Fix the bug");
        assert_eq!(issue.status.as_deref(), Some("Open"));
        assert_eq!(issue.issue_type.as_deref(), Some("Bug"));
        assert_eq!(issue.assignee.as_deref(), Some("Alice"));
        assert_eq!(issue.priority.as_deref(), Some("High"));
        assert_eq!(issue.labels, vec!["backend", "urgent"]);
        assert!(issue.description_adf.is_some());
    }

    #[tokio::test]
    async fn get_issue_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/NOPE-1"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client.get_issue("NOPE-1").await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn get_issue_with_fields_named_populates_custom_fields() {
        let server = wiremock::MockServer::start().await;

        let issue_json = serde_json::json!({
            "key": "ACCS-1",
            "fields": {
                "summary": "S",
                "description": null,
                "status": {"name": "Open"},
                "issuetype": {"name": "Bug"},
                "assignee": null,
                "priority": null,
                "labels": [],
                "customfield_19300": {
                    "type": "doc",
                    "version": 1,
                    "content": [{"type": "paragraph", "content": [{"type": "text", "text": "AC"}]}]
                }
            },
            "names": {
                "customfield_19300": "Acceptance Criteria"
            }
        });

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/ACCS-1"))
            .and(wiremock::matchers::query_param("expand", "names,schema"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(&issue_json))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let issue = client
            .get_issue_with_fields(
                "ACCS-1",
                FieldSelection::Named(vec!["customfield_19300".to_string()]),
            )
            .await
            .unwrap();

        assert_eq!(issue.key, "ACCS-1");
        assert_eq!(issue.custom_fields.len(), 1);
        let cf = &issue.custom_fields[0];
        assert_eq!(cf.id, "customfield_19300");
        assert_eq!(cf.name, "Acceptance Criteria");
        assert_eq!(cf.value["type"], "doc");
    }

    #[tokio::test]
    async fn get_issue_with_fields_standard_omits_custom_fields() {
        let server = wiremock::MockServer::start().await;

        let issue_json = serde_json::json!({
            "key": "ACCS-1",
            "fields": {
                "summary": "S",
                "description": null,
                "status": null,
                "issuetype": null,
                "assignee": null,
                "priority": null,
                "labels": [],
                "customfield_19300": {"value": "Unplanned"}
            },
            "names": {
                "customfield_19300": "Planned / Unplanned Work"
            }
        });

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/ACCS-1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(&issue_json))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let issue = client
            .get_issue_with_fields("ACCS-1", FieldSelection::Standard)
            .await
            .unwrap();

        assert!(issue.custom_fields.is_empty());
    }

    #[tokio::test]
    async fn get_issue_with_fields_all_uses_star_param() {
        let server = wiremock::MockServer::start().await;

        let issue_json = serde_json::json!({
            "key": "ACCS-1",
            "fields": {
                "summary": "S",
                "description": null,
                "status": null,
                "issuetype": null,
                "assignee": null,
                "priority": null,
                "labels": [],
                "customfield_10001": {"value": "Unplanned"},
                "customfield_10002": 42
            },
            "names": {
                "customfield_10001": "Planned / Unplanned Work",
                "customfield_10002": "Story points"
            }
        });

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/ACCS-1"))
            .and(wiremock::matchers::query_param("fields", "*all"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(&issue_json))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let issue = client
            .get_issue_with_fields("ACCS-1", FieldSelection::All)
            .await
            .unwrap();

        assert_eq!(issue.custom_fields.len(), 2);
        let names: Vec<&str> = issue
            .custom_fields
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert!(names.contains(&"Planned / Unplanned Work"));
        assert!(names.contains(&"Story points"));
    }

    #[tokio::test]
    async fn get_editmeta_parses_field_schema() {
        let server = wiremock::MockServer::start().await;

        let editmeta_json = serde_json::json!({
            "fields": {
                "customfield_19300": {
                    "name": "Acceptance Criteria",
                    "schema": {
                        "type": "string",
                        "custom": "com.atlassian.jira.plugin.system.customfieldtypes:textarea",
                        "customId": 19300
                    }
                },
                "customfield_10001": {
                    "name": "Planned / Unplanned Work",
                    "schema": {
                        "type": "option",
                        "custom": "com.atlassian.jira.plugin.system.customfieldtypes:select",
                        "customId": 10001
                    }
                },
                "labels": {
                    "name": "Labels",
                    "schema": {
                        "type": "array",
                        "items": "string",
                        "system": "labels"
                    }
                },
                "description": {
                    "name": "Description",
                    "schema": {
                        "type": "string",
                        "system": "description"
                    }
                }
            }
        });

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/ACCS-1/editmeta",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(&editmeta_json))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let meta = client.get_editmeta("ACCS-1").await.unwrap();

        assert_eq!(meta.fields.len(), 4);
        let ac = meta.fields.get("customfield_19300").unwrap();
        assert_eq!(ac.name, "Acceptance Criteria");
        assert!(ac.is_adf_rich_text());
        let opt = meta.fields.get("customfield_10001").unwrap();
        assert_eq!(opt.schema.kind, "option");
        assert!(!opt.is_adf_rich_text());
        let labels = meta.fields.get("labels").unwrap();
        assert_eq!(labels.schema.kind, "array");
        assert_eq!(labels.schema.items.as_deref(), Some("string"));
        assert_eq!(labels.schema.system.as_deref(), Some("labels"));
        assert!(!labels.is_adf_rich_text());
        let description = meta.fields.get("description").unwrap();
        assert_eq!(description.schema.system.as_deref(), Some("description"));
        assert!(description.is_adf_rich_text());
    }

    #[tokio::test]
    async fn get_editmeta_api_error_surfaces_status() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/NOPE-1/editmeta",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client.get_editmeta("NOPE-1").await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn update_issue_with_custom_fields_merges_into_payload() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/rest/api/3/issue/ACCS-1"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "fields": {
                    "description": {"version": 1, "type": "doc", "content": []},
                    "summary": "New title",
                    "customfield_10001": {"value": "Unplanned"},
                    "customfield_19300": {
                        "type": "doc",
                        "version": 1,
                        "content": [{"type": "paragraph"}]
                    }
                }
            })))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let adf = ValidatedAdfDocument::empty();
        let mut custom = std::collections::BTreeMap::new();
        custom.insert(
            "customfield_10001".to_string(),
            serde_json::json!({"value": "Unplanned"}),
        );
        custom.insert(
            "customfield_19300".to_string(),
            serde_json::json!({"type": "doc", "version": 1, "content": [{"type": "paragraph"}]}),
        );
        let result = client
            .update_issue_with_custom_fields("ACCS-1", Some(&adf), Some("New title"), &custom)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn update_issue_with_no_fields_errors() {
        let client =
            AtlassianClient::new("https://example.atlassian.net", "user@test.com", "token")
                .unwrap();
        let err = client
            .update_issue_with_custom_fields(
                "ACCS-1",
                None,
                None,
                &std::collections::BTreeMap::new(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no fields to update"));
    }

    #[tokio::test]
    async fn update_issue_shim_sends_no_custom_fields() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/rest/api/3/issue/ACCS-1"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "fields": {
                    "description": {"version": 1, "type": "doc", "content": []}
                }
            })))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let adf = ValidatedAdfDocument::empty();
        client.update_issue("ACCS-1", &adf, None).await.unwrap();
    }

    #[tokio::test]
    async fn get_issue_with_fields_falls_back_to_id_when_names_missing() {
        let server = wiremock::MockServer::start().await;

        let issue_json = serde_json::json!({
            "key": "ACCS-1",
            "fields": {
                "summary": "S",
                "description": null,
                "status": null,
                "issuetype": null,
                "assignee": null,
                "priority": null,
                "labels": [],
                "customfield_99999": "raw"
            }
        });

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/ACCS-1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(&issue_json))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let issue = client
            .get_issue_with_fields("ACCS-1", FieldSelection::All)
            .await
            .unwrap();

        assert_eq!(issue.custom_fields.len(), 1);
        assert_eq!(issue.custom_fields[0].name, "customfield_99999");
    }

    #[tokio::test]
    async fn update_issue_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-42"))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let adf = ValidatedAdfDocument::empty();
        let result = client
            .update_issue("PROJ-42", &adf, Some("New title"))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn update_issue_without_summary() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-42"))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let adf = ValidatedAdfDocument::empty();
        let result = client.update_issue("PROJ-42", &adf, None).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn update_issue_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-42"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let adf = ValidatedAdfDocument::empty();
        let err = client
            .update_issue("PROJ-42", &adf, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn search_issues_success() {
        let server = wiremock::MockServer::start().await;

        let search_json = serde_json::json!({
            "issues": [
                {
                    "key": "PROJ-1",
                    "fields": {
                        "summary": "First issue",
                        "description": null,
                        "status": {"name": "Open"},
                        "issuetype": {"name": "Bug"},
                        "assignee": {"displayName": "Alice"},
                        "priority": {"name": "High"},
                        "labels": []
                    }
                },
                {
                    "key": "PROJ-2",
                    "fields": {
                        "summary": "Second issue",
                        "description": null,
                        "status": {"name": "Done"},
                        "issuetype": {"name": "Task"},
                        "assignee": null,
                        "priority": null,
                        "labels": ["backend"]
                    }
                }
            ],
            "total": 2
        });

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/search/jql"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(&search_json))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.search_issues("project = PROJ", 50).await.unwrap();

        assert_eq!(result.total, 2);
        assert_eq!(result.issues.len(), 2);
        assert_eq!(result.issues[0].key, "PROJ-1");
        assert_eq!(result.issues[0].summary, "First issue");
        assert_eq!(result.issues[0].status.as_deref(), Some("Open"));
        assert_eq!(result.issues[1].key, "PROJ-2");
        assert!(result.issues[1].assignee.is_none());
    }

    #[tokio::test]
    async fn search_issues_without_total() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/search/jql"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "issues": [{
                        "key": "PROJ-1",
                        "fields": {
                            "summary": "Test",
                            "description": null,
                            "status": null,
                            "issuetype": null,
                            "assignee": null,
                            "priority": null,
                            "labels": []
                        }
                    }]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.search_issues("project = PROJ", 50).await.unwrap();

        assert_eq!(result.issues.len(), 1);
        // total falls back to issues count when not in response
        assert_eq!(result.total, 1);
    }

    #[tokio::test]
    async fn search_issues_empty_results() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/search/jql"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"issues": [], "total": 0})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.search_issues("project = NOPE", 50).await.unwrap();

        assert_eq!(result.total, 0);
        assert!(result.issues.is_empty());
    }

    #[tokio::test]
    async fn search_issues_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/search/jql"))
            .respond_with(wiremock::ResponseTemplate::new(400).set_body_string("Invalid JQL query"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client
            .search_issues("invalid jql !!!", 50)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("400"));
    }

    #[tokio::test]
    async fn create_issue_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/issue"))
            .respond_with(wiremock::ResponseTemplate::new(201).set_body_json(
                serde_json::json!({"key": "PROJ-124", "id": "10042", "self": "https://org.atlassian.net/rest/api/3/issue/10042"}),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client
            .create_issue("PROJ", "Bug", "Fix login", None, &[])
            .await
            .unwrap();

        assert_eq!(result.key, "PROJ-124");
        assert_eq!(result.id, "10042");
        assert!(result.self_url.contains("10042"));
    }

    #[tokio::test]
    async fn create_issue_with_description_and_labels() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/issue"))
            .respond_with(wiremock::ResponseTemplate::new(201).set_body_json(
                serde_json::json!({"key": "PROJ-125", "id": "10043", "self": "https://org.atlassian.net/rest/api/3/issue/10043"}),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let adf = ValidatedAdfDocument::empty();
        let labels = vec!["backend".to_string(), "urgent".to_string()];
        let result = client
            .create_issue("PROJ", "Task", "Add feature", Some(&adf), &labels)
            .await
            .unwrap();

        assert_eq!(result.key, "PROJ-125");
    }

    #[tokio::test]
    async fn create_issue_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/issue"))
            .respond_with(wiremock::ResponseTemplate::new(400).set_body_string("Project not found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client
            .create_issue("NOPE", "Bug", "Test", None, &[])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("400"));
    }

    #[tokio::test]
    async fn create_issue_with_custom_fields_merges_into_payload() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/issue"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "fields": {
                    "project": {"key": "PROJ"},
                    "issuetype": {"name": "Task"},
                    "summary": "Test",
                    "customfield_10001": {"value": "Unplanned"}
                }
            })))
            .respond_with(
                wiremock::ResponseTemplate::new(201).set_body_json(serde_json::json!({
                    "id": "100",
                    "key": "PROJ-100",
                    "self": "https://org.atlassian.net/rest/api/3/issue/100"
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let mut custom = std::collections::BTreeMap::new();
        custom.insert(
            "customfield_10001".to_string(),
            serde_json::json!({"value": "Unplanned"}),
        );
        let result = client
            .create_issue_with_custom_fields("PROJ", "Task", "Test", None, &[], &custom)
            .await
            .unwrap();
        assert_eq!(result.key, "PROJ-100");
    }

    #[tokio::test]
    async fn create_issue_surfaces_adf_field_required_on_400() {
        // Issue #1047: a 400 whose error envelope reports a field needs an
        // "Atlassian document" must surface as the actionable
        // JiraAdfFieldRequired, matching the update path's behaviour.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/issue"))
            .respond_with(
                wiremock::ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "errorMessages": [],
                    "errors": {
                        "description": "Operation value must be an Atlassian document."
                    }
                })),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client
            .create_issue_with_custom_fields(
                "PROJ",
                "Task",
                "Test",
                None,
                &[],
                &std::collections::BTreeMap::new(),
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("description"), "got: {msg}");
        assert!(msg.contains("ADF"), "got: {msg}");
    }

    #[tokio::test]
    async fn create_issue_shim_sends_no_custom_fields() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/issue"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "fields": {
                    "project": {"key": "PROJ"},
                    "issuetype": {"name": "Task"},
                    "summary": "Test"
                }
            })))
            .respond_with(
                wiremock::ResponseTemplate::new(201).set_body_json(serde_json::json!({
                    "id": "100",
                    "key": "PROJ-100",
                    "self": "https://org.atlassian.net/rest/api/3/issue/100"
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        client
            .create_issue("PROJ", "Task", "Test", None, &[])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn get_createmeta_parses_nested_fields() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/createmeta"))
            .and(wiremock::matchers::query_param("projectKeys", "PROJ"))
            .and(wiremock::matchers::query_param("issuetypeNames", "Task"))
            .and(wiremock::matchers::query_param(
                "expand",
                "projects.issuetypes.fields",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "projects": [{
                        "key": "PROJ",
                        "issuetypes": [{
                            "name": "Task",
                            "fields": {
                                "customfield_10001": {
                                    "name": "Planned / Unplanned Work",
                                    "schema": {
                                        "type": "option",
                                        "custom": "com.atlassian.jira.plugin.system.customfieldtypes:select",
                                        "customId": 10001
                                    },
                                    "allowedValues": [
                                        {"value": "Planned", "id": "10100"},
                                        {"value": "Unplanned", "id": "10101"}
                                    ]
                                }
                            }
                        }]
                    }]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let meta = client.get_createmeta("PROJ", "Task").await.unwrap();
        assert_eq!(meta.fields.len(), 1);
        let field = meta.fields.get("customfield_10001").unwrap();
        assert_eq!(field.name, "Planned / Unplanned Work");
        assert_eq!(field.schema.kind, "option");
        // allowedValues flow into EditMetaField for --set-field validation.
        assert_eq!(field.allowed_values, vec!["Planned", "Unplanned"]);
    }

    #[tokio::test]
    async fn get_createmeta_empty_projects_returns_empty_meta() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/createmeta"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "projects": []
                })),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let meta = client.get_createmeta("PROJ", "Task").await.unwrap();
        assert!(meta.fields.is_empty());
    }

    #[tokio::test]
    async fn get_createmeta_api_error_surfaces_status() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/createmeta"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not found"))
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client.get_createmeta("NOPE", "Task").await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn get_project_create_meta_parses_required_allowed_and_default() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/createmeta"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "projects": [{
                        "issuetypes": [{
                            "fields": {
                                "summary": {
                                    "name": "Summary",
                                    "required": true,
                                    "schema": { "type": "string" }
                                },
                                "customfield_10001": {
                                    "name": "Work Type",
                                    "required": true,
                                    "schema": {
                                        "type": "option",
                                        "custom": "com.atlassian.jira.plugin.system.customfieldtypes:select"
                                    },
                                    "defaultValue": { "id": "10100", "value": "Planned" },
                                    "allowedValues": [
                                        { "id": "10100", "value": "Planned" },
                                        { "id": "10101", "value": "Unplanned" }
                                    ]
                                },
                                "customfield_10002": {
                                    "name": "Region",
                                    "required": false,
                                    "schema": {
                                        "type": "option-with-child",
                                        "custom": "com.atlassian.jira.plugin.system.customfieldtypes:cascadingselect"
                                    },
                                    "allowedValues": [
                                        {
                                            "id": "20000",
                                            "value": "APAC",
                                            "children": [
                                                { "id": "20001", "value": "AU" },
                                                { "id": "20002", "value": "NZ" }
                                            ]
                                        }
                                    ]
                                },
                                "labels": {
                                    "name": "Labels",
                                    "required": false,
                                    "schema": { "type": "array", "items": "string" }
                                }
                            }
                        }]
                    }]
                })),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let meta = client
            .get_project_create_meta("PROJ", "Task")
            .await
            .unwrap();

        assert_eq!(meta.project, "PROJ");
        assert_eq!(meta.issue_type, "Task");
        assert_eq!(meta.fields.len(), 4);

        // Required fields sort first (Summary before Work Type by name).
        assert_eq!(meta.fields[0].field_id, "summary");
        assert!(meta.fields[0].required);
        assert_eq!(meta.fields[1].field_id, "customfield_10001");
        assert!(meta.fields[1].required);
        // Optional fields follow, alphabetically by name (Labels, Region).
        assert!(!meta.fields[2].required);
        assert_eq!(meta.fields[2].name, "Labels");
        assert!(!meta.fields[3].required);
        assert_eq!(meta.fields[3].name, "Region");

        let work_type = &meta.fields[1];
        assert_eq!(work_type.schema_type, "option");
        assert_eq!(
            work_type.custom.as_deref(),
            Some("com.atlassian.jira.plugin.system.customfieldtypes:select")
        );
        assert_eq!(work_type.allowed_values.len(), 2);
        assert_eq!(
            work_type.allowed_values[0].value.as_deref(),
            Some("Planned")
        );
        assert!(work_type.default_value.is_some());

        // labels (array) carries its element type.
        let labels = &meta.fields[2];
        assert_eq!(labels.schema_type, "array");
        assert_eq!(labels.items.as_deref(), Some("string"));

        // Cascading select resolves nested children.
        let region = &meta.fields[3];
        assert_eq!(region.allowed_values.len(), 1);
        assert_eq!(region.allowed_values[0].value.as_deref(), Some("APAC"));
        assert_eq!(region.allowed_values[0].children.len(), 2);
        assert_eq!(
            region.allowed_values[0].children[0].value.as_deref(),
            Some("AU")
        );
    }

    #[tokio::test]
    async fn get_project_create_meta_empty_projects_returns_empty_fields() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/createmeta"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "projects": [] })),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let meta = client
            .get_project_create_meta("PROJ", "Task")
            .await
            .unwrap();
        assert!(meta.fields.is_empty());
        assert_eq!(meta.project, "PROJ");
    }

    #[tokio::test]
    async fn get_project_create_meta_api_error_surfaces_status() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/createmeta"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client
            .get_project_create_meta("NOPE", "Task")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn get_comments_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1/comment"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "startAt": 0,
                    "maxResults": 100,
                    "total": 2,
                    "comments": [
                        {
                            "id": "100",
                            "author": {"displayName": "Alice"},
                            "body": {"version": 1, "type": "doc", "content": []},
                            "created": "2026-04-01T10:00:00.000+0000"
                        },
                        {
                            "id": "101",
                            "author": {"displayName": "Bob"},
                            "body": null,
                            "created": "2026-04-02T14:00:00.000+0000"
                        }
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let comments = client.get_comments("PROJ-1", 0).await.unwrap();

        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].id, "100");
        assert_eq!(comments[0].author, "Alice");
        assert!(comments[0].body_adf.is_some());
        assert!(comments[0].created.contains("2026-04-01"));
        assert_eq!(comments[1].id, "101");
        assert_eq!(comments[1].author, "Bob");
        assert!(comments[1].body_adf.is_none());
    }

    #[tokio::test]
    async fn get_comments_empty() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1/comment"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"startAt": 0, "maxResults": 100, "total": 0, "comments": []}),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let comments = client.get_comments("PROJ-1", 0).await.unwrap();
        assert!(comments.is_empty());
    }

    #[tokio::test]
    async fn get_comments_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/NOPE-1/comment"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client.get_comments("NOPE-1", 0).await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn get_comments_paginates_with_offset() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1/comment"))
            .and(wiremock::matchers::query_param("startAt", "0"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "startAt": 0,
                    "maxResults": 2,
                    "total": 3,
                    "comments": [
                        {"id": "1", "author": {"displayName": "A"}, "body": null, "created": "2026-04-01T10:00:00.000+0000"},
                        {"id": "2", "author": {"displayName": "B"}, "body": null, "created": "2026-04-02T10:00:00.000+0000"}
                    ]
                })),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1/comment"))
            .and(wiremock::matchers::query_param("startAt", "2"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "startAt": 2,
                    "maxResults": 2,
                    "total": 3,
                    "comments": [
                        {"id": "3", "author": {"displayName": "C"}, "body": null, "created": "2026-04-03T10:00:00.000+0000"}
                    ]
                })),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let comments = client.get_comments("PROJ-1", 0).await.unwrap();

        assert_eq!(comments.len(), 3);
        assert_eq!(comments[0].id, "1");
        assert_eq!(comments[1].id, "2");
        assert_eq!(comments[2].id, "3");
    }

    #[tokio::test]
    async fn get_comments_respects_limit_single_page() {
        let server = wiremock::MockServer::start().await;

        // Only one page should be fetched because limit (2) < total (5)
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1/comment"))
            .and(wiremock::matchers::query_param("maxResults", "2"))
            .and(wiremock::matchers::query_param("startAt", "0"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "startAt": 0,
                    "maxResults": 2,
                    "total": 5,
                    "comments": [
                        {"id": "1", "author": {"displayName": "A"}, "body": null, "created": "2026-04-01T10:00:00.000+0000"},
                        {"id": "2", "author": {"displayName": "B"}, "body": null, "created": "2026-04-02T10:00:00.000+0000"}
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let comments = client.get_comments("PROJ-1", 2).await.unwrap();

        assert_eq!(comments.len(), 2);
    }

    #[tokio::test]
    async fn add_comment_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1/comment"))
            .respond_with(
                wiremock::ResponseTemplate::new(201).set_body_json(
                    serde_json::json!({"id": "200", "author": {"displayName": "Me"}}),
                ),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let adf = ValidatedAdfDocument::empty();
        let result = client.add_comment("PROJ-1", &adf).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn add_comment_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1/comment"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let adf = ValidatedAdfDocument::empty();
        let err = client.add_comment("PROJ-1", &adf).await.unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn update_comment_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/comment/100",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "100",
                    "author": {"displayName": "Me"},
                    "created": "2026-04-01T10:00:00.000+0000",
                    "updated": "2026-05-10T12:00:00.000+0000",
                    "body": {"type": "doc", "version": 1, "content": []}
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let adf = ValidatedAdfDocument::empty();
        let comment = client
            .update_comment("PROJ-1", "100", &adf, None)
            .await
            .unwrap();
        assert_eq!(comment.id, "100");
        assert_eq!(comment.author, "Me");
        assert_eq!(
            comment.updated.as_deref(),
            Some("2026-05-10T12:00:00.000+0000")
        );
    }

    #[tokio::test]
    async fn update_comment_sends_visibility() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/comment/100",
            ))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "visibility": {"type": "role", "identifier": "Administrators"}
            })))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "100",
                    "author": {"displayName": "Me"},
                    "created": "2026-04-01T10:00:00.000+0000",
                    "updated": "2026-05-10T12:00:00.000+0000",
                    "body": null
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let adf = ValidatedAdfDocument::empty();
        let visibility = JiraVisibility {
            ty: JiraVisibilityType::Role,
            value: "Administrators".to_string(),
        };
        client
            .update_comment("PROJ-1", "100", &adf, Some(&visibility))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn update_comment_forbidden_surfaces_jira_message() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/comment/100",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(403).set_body_json(serde_json::json!({
                    "errorMessages": ["You do not have permission to edit this comment"],
                    "errors": {}
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let adf = ValidatedAdfDocument::empty();
        let err = client
            .update_comment("PROJ-1", "100", &adf, None)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("403"));
        assert!(msg.contains("permission to edit"));
    }

    #[tokio::test]
    async fn update_comment_not_found() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/comment/9999",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(404).set_body_json(serde_json::json!({
                    "errorMessages": ["Comment not found"]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let adf = ValidatedAdfDocument::empty();
        let err = client
            .update_comment("PROJ-1", "9999", &adf, None)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("404"));
        assert!(msg.contains("Comment not found"));
    }

    #[tokio::test]
    async fn get_transitions_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/transitions",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "transitions": [
                        {"id": "11", "name": "In Progress"},
                        {"id": "21", "name": "Done"},
                        {"id": "31", "name": "Won't Do"}
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let transitions = client.get_transitions("PROJ-1").await.unwrap();

        assert_eq!(transitions.len(), 3);
        assert_eq!(transitions[0].id, "11");
        assert_eq!(transitions[0].name, "In Progress");
        assert_eq!(transitions[1].id, "21");
        assert_eq!(transitions[2].name, "Won't Do");
    }

    #[tokio::test]
    async fn get_transitions_empty() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/transitions",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"transitions": []})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let transitions = client.get_transitions("PROJ-1").await.unwrap();
        assert!(transitions.is_empty());
    }

    #[tokio::test]
    async fn get_transitions_rich_fields() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/transitions",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
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
                            "hasScreen": true,
                            "to": {
                                "id": "10000",
                                "name": "Done",
                                "statusCategory": {"key": "done"}
                            }
                        }
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let transitions = client.get_transitions("PROJ-1").await.unwrap();

        assert_eq!(transitions.len(), 2);
        assert_eq!(transitions[0].id, "21");
        assert_eq!(transitions[0].has_screen, Some(false));
        let to0 = transitions[0].to_status.as_ref().unwrap();
        assert_eq!(to0.id, "3");
        assert_eq!(to0.name, "In Progress");
        assert_eq!(to0.category.as_deref(), Some("indeterminate"));
        assert_eq!(transitions[1].has_screen, Some(true));
        let to1 = transitions[1].to_status.as_ref().unwrap();
        assert_eq!(to1.category.as_deref(), Some("done"));
    }

    #[tokio::test]
    async fn get_transitions_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/NOPE-1/transitions",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client.get_transitions("NOPE-1").await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn do_transition_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/transitions",
            ))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.do_transition("PROJ-1", "21").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn do_transition_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/transitions",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(400).set_body_string("Invalid transition"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client.do_transition("PROJ-1", "999").await.unwrap_err();
        assert!(err.to_string().contains("400"));
    }

    #[tokio::test]
    async fn get_transitions_with_fields_parses_screen_metadata() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/transitions",
            ))
            .and(wiremock::matchers::query_param(
                "expand",
                "transitions.fields",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "transitions": [
                        {"id": "11", "name": "In Progress"},
                        {
                            "id": "21",
                            "name": "Resolve",
                            "hasScreen": true,
                            "fields": {
                                "resolution": {
                                    "name": "Resolution",
                                    "schema": {"type": "resolution"}
                                },
                                "comment": {
                                    "name": "Comment",
                                    "schema": {"type": "comment"}
                                }
                            }
                        }
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let (transitions, metas) = client.get_transitions_with_fields("PROJ-1").await.unwrap();

        assert_eq!(transitions.len(), 2);
        // Screenless transition has no metadata entry.
        assert!(!metas.contains_key("11"));
        // The "Resolve" transition's screen fields are captured.
        let resolve_meta = metas.get("21").unwrap();
        assert!(resolve_meta.fields.contains_key("resolution"));
        assert_eq!(
            resolve_meta.fields.get("comment").map(|f| f.name.as_str()),
            Some("Comment")
        );
    }

    #[tokio::test]
    async fn do_transition_with_fields_posts_fields_and_comment() {
        let server = wiremock::MockServer::start().await;

        // Serialize the comment ADF the same way the client will, so the
        // expected body matches exactly without hardcoding the ADF shape.
        let comment = crate::atlassian::adf_validated::markdown_to_validated_adf("done").unwrap();
        let comment_json = serde_json::to_value(&comment).unwrap();
        let expected = serde_json::json!({
            "transition": {"id": "21"},
            "fields": {"resolution": {"name": "Fixed"}},
            "update": { "comment": [ { "add": { "body": comment_json } } ] }
        });

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/transitions",
            ))
            .and(wiremock::matchers::body_json(expected))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let mut fields = std::collections::BTreeMap::new();
        fields.insert(
            "resolution".to_string(),
            serde_json::json!({ "name": "Fixed" }),
        );
        let result = client
            .do_transition_with_fields("PROJ-1", "21", &fields, Some(&comment))
            .await;
        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    async fn do_transition_with_fields_bare_body_when_empty() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/transitions",
            ))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "transition": {"id": "21"}
            })))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client
            .do_transition_with_fields("PROJ-1", "21", &std::collections::BTreeMap::new(), None)
            .await;
        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    async fn search_confluence_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/rest/api/content/search"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {
                            "id": "12345",
                            "title": "Architecture Overview",
                            "_expandable": {"space": "/wiki/rest/api/space/ENG"}
                        },
                        {
                            "id": "67890",
                            "title": "Getting Started",
                            "_expandable": {"space": "/wiki/rest/api/space/DOC"}
                        }
                    ],
                    "size": 2
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.search_confluence("type = page", 25).await.unwrap();

        assert_eq!(result.total, 2);
        assert_eq!(result.results.len(), 2);
        assert_eq!(result.results[0].id, "12345");
        assert_eq!(result.results[0].title, "Architecture Overview");
        assert_eq!(result.results[0].space_key, "ENG");
        assert_eq!(result.results[1].space_key, "DOC");
    }

    #[tokio::test]
    async fn search_confluence_empty() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/rest/api/content/search"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": [], "size": 0})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client
            .search_confluence("title = \"Nonexistent\"", 25)
            .await
            .unwrap();
        assert_eq!(result.total, 0);
        assert!(result.results.is_empty());
    }

    #[tokio::test]
    async fn search_confluence_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/rest/api/content/search"))
            .respond_with(wiremock::ResponseTemplate::new(400).set_body_string("Invalid CQL"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client
            .search_confluence("bad cql !!!", 25)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("400"));
    }

    #[tokio::test]
    async fn search_confluence_missing_space() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/rest/api/content/search"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{"id": "111", "title": "No Space"}],
                    "size": 1
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.search_confluence("type = page", 10).await.unwrap();
        assert_eq!(result.results[0].space_key, "");
    }

    // ── search_jira_users ───────────────────────────────────────

    #[tokio::test]
    async fn search_jira_users_returns_decoded_results() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/user/search"))
            .and(wiremock::matchers::query_param("query", "alice"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([
                    {
                        "accountId": "abc123",
                        "displayName": "Alice Smith",
                        "emailAddress": "alice@example.com",
                        "active": true,
                        "accountType": "atlassian"
                    },
                    {
                        "accountId": "def456",
                        "displayName": "Alice Jones",
                        "active": true,
                        "accountType": "atlassian"
                    }
                ])),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.search_jira_users("alice", 25).await.unwrap();
        assert_eq!(result.count, 2);
        assert_eq!(result.users[0].account_id, "abc123");
        assert_eq!(result.users[0].display_name.as_deref(), Some("Alice Smith"));
        assert_eq!(
            result.users[0].email_address.as_deref(),
            Some("alice@example.com")
        );
        assert!(result.users[0].active);
        // The second user has email redacted by GDPR.
        assert!(result.users[1].email_address.is_none());
    }

    #[tokio::test]
    async fn search_jira_users_empty_returns_empty_list() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/user/search"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .expect(1)
            .mount(&server)
            .await;
        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.search_jira_users("nobody", 25).await.unwrap();
        assert_eq!(result.count, 0);
        assert!(result.users.is_empty());
    }

    #[tokio::test]
    async fn search_jira_users_truncates_at_limit() {
        let server = wiremock::MockServer::start().await;
        let users_page_1 = serde_json::json!([
            {"accountId": "u1", "displayName": "U1", "active": true, "accountType": "atlassian"},
            {"accountId": "u2", "displayName": "U2", "active": true, "accountType": "atlassian"}
        ]);

        // limit=2 fits the first page exactly, so only one request should fire.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/user/search"))
            .and(wiremock::matchers::query_param("startAt", "0"))
            .and(wiremock::matchers::query_param("maxResults", "2"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(&users_page_1))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.search_jira_users("u", 2).await.unwrap();
        assert_eq!(result.count, 2);
    }

    #[tokio::test]
    async fn search_jira_users_unlimited_paginates_to_completion() {
        let server = wiremock::MockServer::start().await;

        // Build a full page of PAGE_SIZE (100) users, then a short page of 3.
        let full_page: Vec<serde_json::Value> = (0..100)
            .map(|i| {
                serde_json::json!({
                    "accountId": format!("u{i}"),
                    "displayName": format!("User {i}"),
                    "active": true,
                    "accountType": "atlassian"
                })
            })
            .collect();
        let short_page: Vec<serde_json::Value> = (100..103)
            .map(|i| {
                serde_json::json!({
                    "accountId": format!("u{i}"),
                    "displayName": format!("User {i}"),
                    "active": true,
                    "accountType": "atlassian"
                })
            })
            .collect();

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/user/search"))
            .and(wiremock::matchers::query_param("startAt", "0"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::Value::Array(full_page)),
            )
            .expect(1)
            .mount(&server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/user/search"))
            .and(wiremock::matchers::query_param("startAt", "100"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::Value::Array(short_page)),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.search_jira_users("u", 0).await.unwrap();
        assert_eq!(result.count, 103);
    }

    #[tokio::test]
    async fn search_jira_users_propagates_403() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/user/search"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client.search_jira_users("alice", 25).await.unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn search_jira_users_inactive_user_passes_through() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/user/search"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([
                    {
                        "accountId": "old1",
                        "displayName": "Former Employee",
                        "active": false,
                        "accountType": "atlassian"
                    }
                ])),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.search_jira_users("former", 25).await.unwrap();
        assert_eq!(result.count, 1);
        assert!(!result.users[0].active);
    }

    // ── search_confluence_users ─────────────────────────────────

    #[tokio::test]
    async fn search_confluence_users_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/rest/api/search/user"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {
                            "user": {
                                "accountId": "abc123",
                                "displayName": "Alice Smith",
                                "email": "alice@example.com"
                            },
                            "entityType": "user"
                        },
                        {
                            "user": {
                                "accountId": "def456",
                                "displayName": "Bob Jones",
                                "email": "bob@example.com"
                            },
                            "entityType": "user"
                        }
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.search_confluence_users("alice", 25).await.unwrap();

        assert_eq!(result.total, 2);
        assert_eq!(result.users.len(), 2);
        assert_eq!(result.users[0].account_id.as_deref(), Some("abc123"));
        assert_eq!(result.users[0].display_name, "Alice Smith");
        assert_eq!(result.users[0].email.as_deref(), Some("alice@example.com"));
        assert_eq!(result.users[1].account_id.as_deref(), Some("def456"));
        assert_eq!(result.users[1].display_name, "Bob Jones");
    }

    #[tokio::test]
    async fn search_confluence_users_empty() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/rest/api/search/user"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client
            .search_confluence_users("nonexistent", 25)
            .await
            .unwrap();
        assert_eq!(result.total, 0);
        assert!(result.users.is_empty());
    }

    #[tokio::test]
    async fn search_confluence_users_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/rest/api/search/user"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client
            .search_confluence_users("alice", 25)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn search_confluence_users_missing_email() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/rest/api/search/user"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {
                            "user": {
                                "accountId": "xyz789",
                                "displayName": "No Email User"
                            }
                        }
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client
            .search_confluence_users("no email", 25)
            .await
            .unwrap();
        assert_eq!(result.users.len(), 1);
        assert_eq!(result.users[0].display_name, "No Email User");
        assert!(result.users[0].email.is_none());
    }

    #[tokio::test]
    async fn search_confluence_users_missing_account_id() {
        // Regression for rust-works/omni-dev#542: some user records (e.g. app
        // users, deactivated users) return no `accountId`. Such entries must
        // not fail deserialization.
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/rest/api/search/user"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {
                            "user": {
                                "accountId": "abc123",
                                "displayName": "Alice Smith",
                                "email": "alice@example.com"
                            }
                        },
                        {
                            "user": {
                                "displayName": "App Bot",
                                "accountType": "app"
                            }
                        }
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.search_confluence_users("any", 25).await.unwrap();
        assert_eq!(result.users.len(), 2);
        assert_eq!(result.users[0].account_id.as_deref(), Some("abc123"));
        assert!(result.users[1].account_id.is_none());
        assert_eq!(result.users[1].display_name, "App Bot");
    }

    #[tokio::test]
    async fn search_confluence_users_uses_public_name_when_no_display_name() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/rest/api/search/user"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {
                            "user": {
                                "accountId": "abc123",
                                "publicName": "alice.smith"
                            }
                        }
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.search_confluence_users("alice", 25).await.unwrap();
        assert_eq!(result.users.len(), 1);
        assert_eq!(result.users[0].display_name, "alice.smith");
    }

    #[tokio::test]
    async fn search_confluence_users_skips_entries_without_user() {
        // Defensive: the search endpoint may return non-user entries if filters
        // are relaxed server-side; skip them rather than failing.
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/rest/api/search/user"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"title": "Not a user", "entityType": "content"},
                        {
                            "user": {
                                "accountId": "abc123",
                                "displayName": "Alice Smith"
                            }
                        }
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.search_confluence_users("alice", 25).await.unwrap();
        assert_eq!(result.users.len(), 1);
        assert_eq!(result.users[0].account_id.as_deref(), Some("abc123"));
    }

    #[tokio::test]
    async fn search_confluence_users_pagination() {
        let server = wiremock::MockServer::start().await;

        // First page returns one result with a next link
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/rest/api/search/user"))
            .and(wiremock::matchers::query_param("start", "0"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {
                            "user": {
                                "accountId": "page1",
                                "displayName": "User One"
                            }
                        }
                    ],
                    "_links": {"next": "/wiki/rest/api/search/user?start=1"}
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        // Second page returns one result with no next link
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/rest/api/search/user"))
            .and(wiremock::matchers::query_param("start", "1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {
                            "user": {
                                "accountId": "page2",
                                "displayName": "User Two"
                            }
                        }
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.search_confluence_users("user", 0).await.unwrap();

        assert_eq!(result.total, 2);
        assert_eq!(result.users[0].account_id.as_deref(), Some("page1"));
        assert_eq!(result.users[1].account_id.as_deref(), Some("page2"));
    }

    #[tokio::test]
    async fn get_boards_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/agile/1.0/board"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({
                    "values": [
                        {"id": 1, "name": "PROJ Board", "type": "scrum", "location": {"projectKey": "PROJ"}},
                        {"id": 2, "name": "Kanban", "type": "kanban"}
                    ],
                    "total": 2, "isLast": true
                }),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.get_boards(None, None, 50).await.unwrap();

        assert_eq!(result.total, 2);
        assert_eq!(result.boards.len(), 2);
        assert_eq!(result.boards[0].id, 1);
        assert_eq!(result.boards[0].name, "PROJ Board");
        assert_eq!(result.boards[0].board_type, "scrum");
        assert_eq!(result.boards[0].project_key.as_deref(), Some("PROJ"));
        assert!(result.boards[1].project_key.is_none());
    }

    #[tokio::test]
    async fn get_boards_with_filters() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/agile/1.0/board"))
            .and(wiremock::matchers::query_param("projectKeyOrId", "PROJ"))
            .and(wiremock::matchers::query_param("type", "scrum"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({
                    "values": [{"id": 1, "name": "PROJ Board", "type": "scrum", "location": {"projectKey": "PROJ"}}],
                    "total": 1, "isLast": true
                }),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client
            .get_boards(Some("PROJ"), Some("scrum"), 50)
            .await
            .unwrap();

        assert_eq!(result.boards.len(), 1);
        assert_eq!(result.boards[0].project_key.as_deref(), Some("PROJ"));
    }

    #[tokio::test]
    async fn search_issues_paginates_with_token() {
        let server = wiremock::MockServer::start().await;

        // First page returns a nextPageToken
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/search/jql"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({"jql": "project = PROJ"})))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({
                    "issues": [{"key": "PROJ-1", "fields": {"summary": "First", "description": null, "status": null, "issuetype": null, "assignee": null, "priority": null, "labels": []}}],
                    "nextPageToken": "token123"
                }),
            ))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        // Second page has no nextPageToken (last page)
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/search/jql"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({"nextPageToken": "token123"})))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({
                    "issues": [{"key": "PROJ-2", "fields": {"summary": "Second", "description": null, "status": null, "issuetype": null, "assignee": null, "priority": null, "labels": []}}]
                }),
            ))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.search_issues("project = PROJ", 0).await.unwrap();

        assert_eq!(result.issues.len(), 2);
        assert_eq!(result.issues[0].key, "PROJ-1");
        assert_eq!(result.issues[1].key, "PROJ-2");
    }

    #[tokio::test]
    async fn search_issues_respects_limit() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/search/jql"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({
                    "issues": [
                        {"key": "PROJ-1", "fields": {"summary": "A", "description": null, "status": null, "issuetype": null, "assignee": null, "priority": null, "labels": []}},
                        {"key": "PROJ-2", "fields": {"summary": "B", "description": null, "status": null, "issuetype": null, "assignee": null, "priority": null, "labels": []}}
                    ],
                    "nextPageToken": "more"
                }),
            ))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        // Limit to 2 — should not fetch second page
        let result = client.search_issues("project = PROJ", 2).await.unwrap();
        assert_eq!(result.issues.len(), 2);
    }

    #[tokio::test]
    async fn get_boards_paginates_with_offset() {
        let server = wiremock::MockServer::start().await;

        // First page
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/agile/1.0/board"))
            .and(wiremock::matchers::query_param("startAt", "0"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "values": [{"id": 1, "name": "Board 1", "type": "scrum"}],
                    "total": 2, "isLast": false
                })),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;

        // Second page
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/agile/1.0/board"))
            .and(wiremock::matchers::query_param("startAt", "1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "values": [{"id": 2, "name": "Board 2", "type": "kanban"}],
                    "total": 2, "isLast": true
                })),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.get_boards(None, None, 0).await.unwrap();

        assert_eq!(result.boards.len(), 2);
        assert_eq!(result.boards[0].name, "Board 1");
        assert_eq!(result.boards[1].name, "Board 2");
    }

    #[tokio::test]
    async fn get_boards_empty() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/agile/1.0/board"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"values": [], "total": 0})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.get_boards(None, None, 50).await.unwrap();
        assert!(result.boards.is_empty());
    }

    #[tokio::test]
    async fn get_boards_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/agile/1.0/board"))
            .respond_with(wiremock::ResponseTemplate::new(401).set_body_string("Unauthorized"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client.get_boards(None, None, 50).await.unwrap_err();
        assert!(err.to_string().contains("401"));
    }

    #[tokio::test]
    async fn get_board_issues_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/agile/1.0/board/1/issue"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "issues": [{
                        "key": "PROJ-1",
                        "fields": {
                            "summary": "Board issue",
                            "description": null,
                            "status": {"name": "Open"},
                            "issuetype": {"name": "Task"},
                            "assignee": null,
                            "priority": null,
                            "labels": []
                        }
                    }],
                    "total": 1, "isLast": true
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.get_board_issues(1, None, 50).await.unwrap();

        assert_eq!(result.total, 1);
        assert_eq!(result.issues[0].key, "PROJ-1");
        assert_eq!(result.issues[0].summary, "Board issue");
    }

    #[tokio::test]
    async fn get_board_issues_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/agile/1.0/board/999/issue"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client.get_board_issues(999, None, 50).await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn get_sprints_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/agile/1.0/board/1/sprint"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({
                    "values": [
                        {"id": 10, "name": "Sprint 1", "state": "closed", "startDate": "2026-03-01", "endDate": "2026-03-14", "goal": "MVP"},
                        {"id": 11, "name": "Sprint 2", "state": "active", "startDate": "2026-03-15", "endDate": "2026-03-28"}
                    ],
                    "total": 2, "isLast": true
                }),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.get_sprints(1, None, 50).await.unwrap();

        assert_eq!(result.total, 2);
        assert_eq!(result.sprints.len(), 2);
        assert_eq!(result.sprints[0].id, 10);
        assert_eq!(result.sprints[0].name, "Sprint 1");
        assert_eq!(result.sprints[0].state, "closed");
        assert_eq!(result.sprints[0].goal.as_deref(), Some("MVP"));
        assert!(result.sprints[1].goal.is_none());
    }

    #[tokio::test]
    async fn get_sprints_with_state_filter() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/agile/1.0/board/1/sprint"))
            .and(wiremock::matchers::query_param("state", "active"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "values": [{"id": 11, "name": "Sprint 2", "state": "active"}],
                    "total": 1, "isLast": true
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.get_sprints(1, Some("active"), 50).await.unwrap();
        assert_eq!(result.sprints.len(), 1);
        assert_eq!(result.sprints[0].state, "active");
    }

    #[tokio::test]
    async fn get_sprints_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/agile/1.0/board/999/sprint"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client.get_sprints(999, None, 50).await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn get_sprint_issues_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/agile/1.0/sprint/10/issue"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "issues": [{
                        "key": "PROJ-1",
                        "fields": {
                            "summary": "Sprint issue",
                            "description": null,
                            "status": {"name": "In Progress"},
                            "issuetype": {"name": "Story"},
                            "assignee": {"displayName": "Alice"},
                            "priority": null,
                            "labels": []
                        }
                    }],
                    "total": 1, "isLast": true
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.get_sprint_issues(10, None, 50).await.unwrap();

        assert_eq!(result.total, 1);
        assert_eq!(result.issues[0].key, "PROJ-1");
        assert_eq!(result.issues[0].assignee.as_deref(), Some("Alice"));
    }

    #[tokio::test]
    async fn get_sprint_issues_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/agile/1.0/sprint/999/issue"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client.get_sprint_issues(999, None, 50).await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn add_issues_to_sprint_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/agile/1.0/sprint/10/issue"))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.add_issues_to_sprint(10, &["PROJ-1", "PROJ-2"]).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn add_issues_to_sprint_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/agile/1.0/sprint/999/issue"))
            .respond_with(wiremock::ResponseTemplate::new(400).set_body_string("Bad Request"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client
            .add_issues_to_sprint(999, &["NOPE-1"])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("400"));
    }

    #[tokio::test]
    async fn create_sprint_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/agile/1.0/sprint"))
            .respond_with(
                wiremock::ResponseTemplate::new(201).set_body_json(serde_json::json!({
                    "id": 42,
                    "name": "Sprint 5",
                    "state": "future",
                    "startDate": "2026-05-01",
                    "endDate": "2026-05-14",
                    "goal": "Ship v2"
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let sprint = client
            .create_sprint(
                1,
                "Sprint 5",
                Some("2026-05-01"),
                Some("2026-05-14"),
                Some("Ship v2"),
            )
            .await
            .unwrap();

        assert_eq!(sprint.id, 42);
        assert_eq!(sprint.name, "Sprint 5");
        assert_eq!(sprint.state, "future");
        assert_eq!(sprint.goal.as_deref(), Some("Ship v2"));
    }

    #[tokio::test]
    async fn create_sprint_minimal() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/agile/1.0/sprint"))
            .respond_with(wiremock::ResponseTemplate::new(201).set_body_json(
                serde_json::json!({"id": 43, "name": "Sprint 6", "state": "future"}),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let sprint = client
            .create_sprint(1, "Sprint 6", None, None, None)
            .await
            .unwrap();

        assert_eq!(sprint.id, 43);
        assert!(sprint.start_date.is_none());
    }

    #[tokio::test]
    async fn create_sprint_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/agile/1.0/sprint"))
            .respond_with(wiremock::ResponseTemplate::new(400).set_body_string("Bad Request"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client
            .create_sprint(999, "Bad", None, None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("400"));
    }

    #[tokio::test]
    async fn update_sprint_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/rest/agile/1.0/sprint/42"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"id": 42, "name": "Sprint 5 Updated", "state": "active"}),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client
            .update_sprint(
                42,
                Some("Sprint 5 Updated"),
                Some("active"),
                None,
                None,
                None,
            )
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn update_sprint_all_fields() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/rest/agile/1.0/sprint/42"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"id": 42, "name": "Sprint 5", "state": "active"}),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client
            .update_sprint(
                42,
                Some("Sprint 5"),
                Some("active"),
                Some("2026-05-01"),
                Some("2026-05-14"),
                Some("Ship v2"),
            )
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn update_sprint_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/rest/agile/1.0/sprint/999"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client
            .update_sprint(999, Some("Nope"), None, None, None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn get_project_versions_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/project/PROJ/versions",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([
                    {
                        "id": "10000",
                        "name": "1.0.0",
                        "description": "First release",
                        "released": true,
                        "archived": false,
                        "releaseDate": "2026-04-01",
                        "startDate": "2026-03-01",
                    },
                    {
                        "id": "10001",
                        "name": "1.1.0",
                        "released": false,
                        "archived": false,
                    }
                ])),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client
            .get_project_versions("PROJ", None, None)
            .await
            .unwrap();

        assert_eq!(result.total, 2);
        assert_eq!(result.versions[0].id, "10000");
        assert_eq!(result.versions[0].name, "1.0.0");
        assert_eq!(result.versions[0].project_key, "PROJ");
        assert!(result.versions[0].released);
        assert_eq!(
            result.versions[0].release_date.as_deref(),
            Some("2026-04-01")
        );
        assert_eq!(result.versions[1].name, "1.1.0");
        assert!(!result.versions[1].released);
    }

    #[tokio::test]
    async fn get_project_versions_filters_released() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/project/PROJ/versions",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([
                    {"id": "1", "name": "1.0", "released": true, "archived": false},
                    {"id": "2", "name": "2.0", "released": false, "archived": false},
                    {"id": "3", "name": "0.9", "released": true, "archived": true},
                ])),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client
            .get_project_versions("PROJ", Some(true), Some(false))
            .await
            .unwrap();

        assert_eq!(result.total, 1);
        assert_eq!(result.versions[0].name, "1.0");
    }

    #[tokio::test]
    async fn get_project_versions_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/project/NONE/versions",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client
            .get_project_versions("NONE", None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn create_project_version_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/version"))
            .respond_with(
                wiremock::ResponseTemplate::new(201).set_body_json(serde_json::json!({
                    "id": "10010",
                    "name": "1.2.0",
                    "description": "Bugfix release",
                    "released": false,
                    "archived": false,
                    "releaseDate": "2026-06-01",
                    "startDate": "2026-05-01",
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let version = client
            .create_project_version(
                "PROJ",
                "1.2.0",
                Some("Bugfix release"),
                Some("2026-06-01"),
                Some("2026-05-01"),
                false,
                false,
            )
            .await
            .unwrap();

        assert_eq!(version.id, "10010");
        assert_eq!(version.name, "1.2.0");
        assert_eq!(version.project_key, "PROJ");
        assert_eq!(version.description.as_deref(), Some("Bugfix release"));
        assert_eq!(version.release_date.as_deref(), Some("2026-06-01"));
    }

    #[tokio::test]
    async fn create_project_version_minimal() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/version"))
            .respond_with(wiremock::ResponseTemplate::new(201).set_body_json(
                serde_json::json!({"id": "10011", "name": "2.0.0", "released": false, "archived": false}),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let version = client
            .create_project_version("PROJ", "2.0.0", None, None, None, false, false)
            .await
            .unwrap();

        assert_eq!(version.id, "10011");
        assert!(version.release_date.is_none());
    }

    #[tokio::test]
    async fn create_project_version_forbidden() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/version"))
            .respond_with(
                wiremock::ResponseTemplate::new(403)
                    .set_body_string("You do not have permission to administer this project."),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client
            .create_project_version("PROJ", "1.0", None, None, None, false, false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn create_project_version_invalid_date_short_circuits() {
        // Server should never be hit because validation fails client-side.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/version"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client
            .create_project_version("PROJ", "1.0", None, Some("06-01-2026"), None, false, false)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("release_date"));
        assert!(msg.contains("YYYY-MM-DD"));
    }

    #[tokio::test]
    async fn create_project_version_invalid_start_date_short_circuits() {
        // start_date validation runs after release_date; this test drives that
        // second branch by passing a valid release_date with a malformed
        // start_date.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/version"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client
            .create_project_version(
                "PROJ",
                "1.0",
                None,
                Some("2026-06-01"),
                Some("not-a-date"),
                false,
                false,
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("start_date"));
        assert!(msg.contains("YYYY-MM-DD"));
    }

    #[test]
    fn validate_iso_date_accepts_valid() {
        assert!(validate_iso_date(Some("2026-05-10"), "release_date").is_ok());
        assert!(validate_iso_date(None, "release_date").is_ok());
    }

    #[test]
    fn validate_iso_date_rejects_bad_shape() {
        let err = validate_iso_date(Some("2026/05/10"), "release_date").unwrap_err();
        assert!(err.to_string().contains("release_date"));
    }

    #[test]
    fn validate_iso_date_rejects_impossible() {
        let err = validate_iso_date(Some("2026-13-40"), "start_date").unwrap_err();
        assert!(err.to_string().contains("start_date"));
    }

    /// Exercises the `?` Err propagation on the `get_json` call in
    /// `get_project_versions` by pointing the client at an unreachable port.
    #[tokio::test]
    async fn get_project_versions_transport_error() {
        // Port 1 is reserved for `tcpmux` and almost never has a listener,
        // so connection attempts fail before any response.
        let client = AtlassianClient::new("http://127.0.0.1:1", "user@test.com", "token").unwrap();
        let err = client
            .get_project_versions("PROJ", None, None)
            .await
            .unwrap_err();
        // Transport failures bubble up via anyhow `Context` from `get_json`.
        assert!(err.to_string().contains("Failed to send GET request"));
    }

    /// Exercises the `?` Err propagation on the `.json().context(...)?`
    /// call in `get_project_versions` by returning a 200 with a body that
    /// can't be parsed as the expected JSON shape.
    #[tokio::test]
    async fn get_project_versions_invalid_json() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/project/PROJ/versions",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("not-json"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client
            .get_project_versions("PROJ", None, None)
            .await
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("Failed to parse project versions response"));
    }

    /// Exercises the `?` Err propagation on the `post_json` call in
    /// `create_project_version`.
    #[tokio::test]
    async fn create_project_version_transport_error() {
        let client = AtlassianClient::new("http://127.0.0.1:1", "user@test.com", "token").unwrap();
        let err = client
            .create_project_version("PROJ", "1.0", None, None, None, false, false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Failed to send POST request"));
    }

    /// Exercises the `?` Err propagation on the `.json().context(...)?`
    /// call in `create_project_version`.
    #[tokio::test]
    async fn create_project_version_invalid_json() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/version"))
            .respond_with(wiremock::ResponseTemplate::new(201).set_body_string("not-json"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client
            .create_project_version("PROJ", "1.0", None, None, None, false, false)
            .await
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("Failed to parse version create response"));
    }

    #[tokio::test]
    async fn get_issue_links_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({
                    "fields": {
                        "issuelinks": [
                            {
                                "id": "100",
                                "type": {"name": "Blocks"},
                                "outwardIssue": {"key": "PROJ-2", "fields": {"summary": "Blocked issue"}}
                            },
                            {
                                "id": "101",
                                "type": {"name": "Relates"},
                                "inwardIssue": {"key": "PROJ-3", "fields": {"summary": "Related issue"}}
                            }
                        ]
                    }
                }),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let links = client.get_issue_links("PROJ-1").await.unwrap();

        assert_eq!(links.len(), 2);
        assert_eq!(links[0].id, "100");
        assert_eq!(links[0].link_type, "Blocks");
        assert_eq!(links[0].direction, "outward");
        assert_eq!(links[0].linked_issue_key, "PROJ-2");
        assert_eq!(links[0].linked_issue_summary, "Blocked issue");
        assert_eq!(links[1].id, "101");
        assert_eq!(links[1].direction, "inward");
        assert_eq!(links[1].linked_issue_key, "PROJ-3");
    }

    #[tokio::test]
    async fn get_issue_links_empty() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"fields": {"issuelinks": []}})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let links = client.get_issue_links("PROJ-1").await.unwrap();
        assert!(links.is_empty());
    }

    #[tokio::test]
    async fn get_issue_links_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/NOPE-1"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client.get_issue_links("NOPE-1").await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn get_remote_issue_links_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/remotelink",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([
                    {
                        "id": 10001,
                        "globalId": "system=https://example.atlassian.net/wiki&id=12345",
                        "relationship": "mentioned in",
                        "object": {
                            "url": "https://example.atlassian.net/wiki/spaces/X/pages/12345",
                            "title": "Design doc",
                            "summary": "Architecture overview",
                            "icon": {
                                "url16x16": "https://example.atlassian.net/icons/page.png",
                                "title": "Confluence Page"
                            }
                        }
                    },
                    {
                        "id": "10002",
                        "object": {
                            "url": "https://bitbucket.org/acme/repo/pull-requests/42"
                        }
                    }
                ])),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let links = client.get_remote_issue_links("PROJ-1").await.unwrap();

        assert_eq!(links.len(), 2);

        // First entry: full payload, numeric id normalized to string.
        assert_eq!(links[0].id, "10001");
        assert_eq!(
            links[0].global_id.as_deref(),
            Some("system=https://example.atlassian.net/wiki&id=12345")
        );
        assert_eq!(links[0].relationship.as_deref(), Some("mentioned in"));
        assert_eq!(
            links[0].object.url,
            "https://example.atlassian.net/wiki/spaces/X/pages/12345"
        );
        assert_eq!(links[0].object.title.as_deref(), Some("Design doc"));
        assert_eq!(
            links[0].object.summary.as_deref(),
            Some("Architecture overview")
        );
        let icon = links[0].object.icon.as_ref().expect("icon present");
        assert_eq!(
            icon.url.as_deref(),
            Some("https://example.atlassian.net/icons/page.png")
        );
        assert_eq!(icon.title.as_deref(), Some("Confluence Page"));

        // Second entry: minimal payload, string id, no optional fields.
        assert_eq!(links[1].id, "10002");
        assert!(links[1].global_id.is_none());
        assert!(links[1].relationship.is_none());
        assert_eq!(
            links[1].object.url,
            "https://bitbucket.org/acme/repo/pull-requests/42"
        );
        assert!(links[1].object.title.is_none());
        assert!(links[1].object.summary.is_none());
        assert!(links[1].object.icon.is_none());
    }

    #[tokio::test]
    async fn get_remote_issue_links_empty() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/remotelink",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .expect(1)
            .mount(&server)
            .await;
        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let links = client.get_remote_issue_links("PROJ-1").await.unwrap();
        assert!(links.is_empty());
    }

    #[tokio::test]
    async fn get_remote_issue_links_rejects_unexpected_id_type() {
        // Exercise the defensive `other =>` arm of the id-normalisation
        // match. JIRA's wire contract is number-or-string; anything else
        // should be surfaced as a clear error rather than silently
        // accepted.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/remotelink",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([
                    {
                        "id": null,
                        "object": {"url": "https://example.com/x"}
                    }
                ])),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client.get_remote_issue_links("PROJ-1").await.unwrap_err();
        assert!(err.to_string().contains("unexpected remote link id type"));
    }

    #[tokio::test]
    async fn get_remote_issue_links_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/NOPE-1/remotelink",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;
        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client.get_remote_issue_links("NOPE-1").await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn get_link_types_success() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issueLinkType"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"issueLinkTypes": [{"id": "1", "name": "Blocks", "inward": "is blocked by", "outward": "blocks"}, {"id": "2", "name": "Clones", "inward": "is cloned by", "outward": "clones"}]})))
            .expect(1).mount(&server).await;
        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let types = client.get_link_types().await.unwrap();
        assert_eq!(types.len(), 2);
        assert_eq!(types[0].name, "Blocks");
        assert_eq!(types[0].inward, "is blocked by");
    }

    #[tokio::test]
    async fn get_link_types_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issueLinkType"))
            .respond_with(wiremock::ResponseTemplate::new(401).set_body_string("Unauthorized"))
            .expect(1)
            .mount(&server)
            .await;
        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client.get_link_types().await.unwrap_err();
        assert!(err.to_string().contains("401"));
    }

    #[tokio::test]
    async fn create_issue_link_success() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/issueLink"))
            .respond_with(wiremock::ResponseTemplate::new(201))
            .expect(1)
            .mount(&server)
            .await;
        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        assert!(client
            .create_issue_link("Blocks", "PROJ-1", "PROJ-2")
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn create_issue_link_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/issueLink"))
            .respond_with(wiremock::ResponseTemplate::new(400).set_body_string("Bad Request"))
            .expect(1)
            .mount(&server)
            .await;
        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client
            .create_issue_link("Invalid", "NOPE-1", "NOPE-2")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("400"));
    }

    #[tokio::test]
    async fn remove_issue_link_success() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/rest/api/3/issueLink/12345"))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        assert!(client.remove_issue_link("12345").await.is_ok());
    }

    #[tokio::test]
    async fn remove_issue_link_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/rest/api/3/issueLink/99999"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;
        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client.remove_issue_link("99999").await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn set_issue_parent_success() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-2"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "fields": {"parent": {"key": "EPIC-1"}}
            })))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        assert!(client.set_issue_parent("PROJ-2", "EPIC-1").await.is_ok());
    }

    #[tokio::test]
    async fn set_issue_parent_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-2"))
            .respond_with(wiremock::ResponseTemplate::new(400).set_body_string("Not allowed"))
            .expect(1)
            .mount(&server)
            .await;
        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client
            .set_issue_parent("PROJ-2", "NOPE-1")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("400"));
    }

    #[tokio::test]
    async fn get_bytes_success() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/file.bin"))
            .and(wiremock::matchers::header("Accept", "*/*"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"binary content"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let data = client
            .get_bytes(&format!("{}/file.bin", server.uri()))
            .await
            .unwrap();
        assert_eq!(&data[..], b"binary content");
    }

    #[tokio::test]
    async fn get_bytes_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/missing.bin"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client
            .get_bytes(&format!("{}/missing.bin", server.uri()))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn get_attachments_success() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({
                    "fields": {
                        "attachment": [
                            {"id": "1", "filename": "screenshot.png", "mimeType": "image/png", "size": 12345, "content": "https://org.atlassian.net/attachment/1"},
                            {"id": "2", "filename": "report.pdf", "mimeType": "application/pdf", "size": 99999, "content": "https://org.atlassian.net/attachment/2"}
                        ]
                    }
                }),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let attachments = client.get_attachments("PROJ-1").await.unwrap();

        assert_eq!(attachments.len(), 2);
        assert_eq!(attachments[0].filename, "screenshot.png");
        assert_eq!(attachments[0].mime_type, "image/png");
        assert_eq!(attachments[0].size, 12345);
        assert_eq!(attachments[1].filename, "report.pdf");
    }

    #[tokio::test]
    async fn get_attachments_empty() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"fields": {"attachment": []}})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let attachments = client.get_attachments("PROJ-1").await.unwrap();
        assert!(attachments.is_empty());
    }

    #[tokio::test]
    async fn get_attachments_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/NOPE-1"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client.get_attachments("NOPE-1").await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn upload_attachments_success() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/attachments",
            ))
            .and(wiremock::matchers::header("X-Atlassian-Token", "no-check"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "10001", "filename": "log.txt", "mimeType": "text/plain", "size": 5, "content": "https://org.atlassian.net/attachment/10001"},
                {"id": "10002", "filename": "shot.png", "mimeType": "image/png", "size": 4, "content": "https://org.atlassian.net/attachment/10002"}
            ])))
            .expect(1)
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("log.txt");
        let b = dir.path().join("shot.png");
        std::fs::write(&a, b"hello").unwrap();
        std::fs::write(&b, b"\x89PNG").unwrap();

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let created = client.upload_attachments("PROJ-1", &[a, b]).await.unwrap();

        assert_eq!(created.len(), 2);
        assert_eq!(created[0].id, "10001");
        assert_eq!(created[0].filename, "log.txt");
        assert_eq!(created[1].mime_type, "image/png");
    }

    #[tokio::test]
    async fn upload_attachments_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/attachments",
            ))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .expect(1)
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("log.txt");
        std::fs::write(&f, b"hello").unwrap();

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client.upload_attachments("PROJ-1", &[f]).await.unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn upload_attachments_rejects_path_without_filename() {
        // A path terminating in `..` has no `file_name()` component, yet its
        // metadata resolves and it opens as a directory — so it reaches the
        // filename guard rather than failing earlier.
        let dir = tempfile::tempdir().unwrap();
        let no_name = dir.path().join("..");
        let client =
            AtlassianClient::new("https://org.atlassian.net", "user@test.com", "token").unwrap();
        let err = client
            .upload_attachments("PROJ-1", &[no_name])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no filename component"));
    }

    #[tokio::test]
    async fn delete_attachment_success() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/rest/api/3/attachment/10042"))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        client.delete_attachment("10042").await.unwrap();
    }

    #[tokio::test]
    async fn delete_attachment_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/rest/api/3/attachment/nope"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client.delete_attachment("nope").await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn get_changelog_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/changelog",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({
                    "values": [
                        {
                            "id": "100",
                            "author": {"displayName": "Alice"},
                            "created": "2026-04-01T10:00:00.000+0000",
                            "items": [
                                {"field": "status", "fromString": "Open", "toString": "In Progress"},
                                {"field": "assignee", "fromString": null, "toString": "Bob"}
                            ]
                        },
                        {
                            "id": "101",
                            "author": null,
                            "created": "2026-04-02T14:00:00.000+0000",
                            "items": [{"field": "priority", "fromString": "Medium", "toString": "High"}]
                        }
                    ],
                    "isLast": true
                }),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let entries = client.get_changelog("PROJ-1", 50).await.unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "100");
        assert_eq!(entries[0].author, "Alice");
        assert_eq!(entries[0].items.len(), 2);
        assert_eq!(entries[0].items[0].field, "status");
        assert_eq!(entries[0].items[0].from_string.as_deref(), Some("Open"));
        assert_eq!(
            entries[0].items[0].to_string.as_deref(),
            Some("In Progress")
        );
        assert_eq!(entries[0].items[1].from_string, None);
        assert_eq!(entries[1].author, "");
    }

    #[tokio::test]
    async fn get_changelog_empty() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/changelog",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"values": []})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let entries = client.get_changelog("PROJ-1", 50).await.unwrap();
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn get_changelog_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/NOPE-1/changelog",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client.get_changelog("NOPE-1", 50).await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn get_fields_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/field"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!([
                    {"id": "summary", "name": "Summary", "custom": false, "schema": {"type": "string"}},
                    {"id": "customfield_10001", "name": "Story Points", "custom": true, "schema": {"type": "number"}},
                    {"id": "labels", "name": "Labels", "custom": false},
                    {
                        "id": "customfield_19300",
                        "name": "Acceptance Criteria",
                        "custom": true,
                        "schema": {
                            "type": "string",
                            "custom": "com.atlassian.jira.plugin.system.customfieldtypes:textarea"
                        }
                    }
                ]),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let fields = client.get_fields().await.unwrap();

        assert_eq!(fields.len(), 4);
        assert_eq!(fields[0].id, "summary");
        assert_eq!(fields[0].name, "Summary");
        assert!(!fields[0].custom);
        assert_eq!(fields[0].schema_type.as_deref(), Some("string"));
        assert!(fields[0].schema_custom.is_none());
        assert_eq!(fields[1].id, "customfield_10001");
        assert!(fields[1].custom);
        assert_eq!(fields[1].schema_type.as_deref(), Some("number"));
        assert!(fields[1].schema_custom.is_none());
        assert!(fields[2].schema_type.is_none());
        assert!(fields[2].schema_custom.is_none());
        assert_eq!(fields[3].id, "customfield_19300");
        assert!(fields[3].custom);
        assert_eq!(fields[3].schema_type.as_deref(), Some("richtext"));
        assert_eq!(
            fields[3].schema_custom.as_deref(),
            Some("com.atlassian.jira.plugin.system.customfieldtypes:textarea")
        );
    }

    #[tokio::test]
    async fn get_fields_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/field"))
            .respond_with(wiremock::ResponseTemplate::new(401).set_body_string("Unauthorized"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client.get_fields().await.unwrap_err();
        assert!(err.to_string().contains("401"));
    }

    #[tokio::test]
    async fn get_field_contexts_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/field/customfield_10001/context",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"values": [{"id": "12345"}, {"id": "67890"}]}),
                ),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let contexts = client
            .get_field_contexts("customfield_10001")
            .await
            .unwrap();

        assert_eq!(contexts.len(), 2);
        assert_eq!(contexts[0], "12345");
    }

    #[tokio::test]
    async fn get_field_contexts_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/field/nonexistent/context",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client.get_field_contexts("nonexistent").await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn get_field_contexts_empty() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/field/customfield_99999/context",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"values": []})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let contexts = client
            .get_field_contexts("customfield_99999")
            .await
            .unwrap();
        assert!(contexts.is_empty());
    }

    #[tokio::test]
    async fn get_field_options_auto_discovers_context() {
        let server = wiremock::MockServer::start().await;

        // Context discovery
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/field/customfield_10001/context",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"values": [{"id": "12345"}]})),
            )
            .expect(1)
            .mount(&server)
            .await;

        // Options for discovered context
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/field/customfield_10001/context/12345/option",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"values": [{"id": "1", "value": "High"}]})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let options = client
            .get_field_options("customfield_10001", None)
            .await
            .unwrap();

        assert_eq!(options.len(), 1);
        assert_eq!(options[0].value, "High");
    }

    #[tokio::test]
    async fn get_field_options_no_context_errors() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/field/customfield_99999/context",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"values": []})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client
            .get_field_options("customfield_99999", None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("No contexts found"));
    }

    #[tokio::test]
    async fn get_field_options_with_explicit_context() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/field/customfield_10001/context/12345/option",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"values": [
                    {"id": "1", "value": "High"},
                    {"id": "2", "value": "Medium"},
                    {"id": "3", "value": "Low"}
                ]}),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let options = client
            .get_field_options("customfield_10001", Some("12345"))
            .await
            .unwrap();

        assert_eq!(options.len(), 3);
        assert_eq!(options[0].id, "1");
        assert_eq!(options[0].value, "High");
    }

    #[tokio::test]
    async fn get_field_options_with_context() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/field/customfield_10001/context/12345/option",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"values": [{"id": "1", "value": "Option A"}]}),
                ),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let options = client
            .get_field_options("customfield_10001", Some("12345"))
            .await
            .unwrap();

        assert_eq!(options.len(), 1);
        assert_eq!(options[0].value, "Option A");
    }

    #[tokio::test]
    async fn get_field_options_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/field/nonexistent/context/99999/option",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client
            .get_field_options("nonexistent", Some("99999"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn get_projects_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/project/search"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "values": [
                        {
                            "id": "10001",
                            "key": "PROJ",
                            "name": "My Project",
                            "projectTypeKey": "software",
                            "lead": {"displayName": "Alice"}
                        },
                        {
                            "id": "10002",
                            "key": "OPS",
                            "name": "Operations",
                            "projectTypeKey": "business",
                            "lead": null
                        }
                    ],
                    "total": 2, "isLast": true
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.get_projects(50).await.unwrap();

        assert_eq!(result.total, 2);
        assert_eq!(result.projects.len(), 2);
        assert_eq!(result.projects[0].key, "PROJ");
        assert_eq!(result.projects[0].name, "My Project");
        assert_eq!(result.projects[0].project_type.as_deref(), Some("software"));
        assert_eq!(result.projects[0].lead.as_deref(), Some("Alice"));
        assert_eq!(result.projects[1].key, "OPS");
        assert!(result.projects[1].lead.is_none());
    }

    #[tokio::test]
    async fn get_projects_empty() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/project/search"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"values": [], "total": 0})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.get_projects(50).await.unwrap();
        assert_eq!(result.total, 0);
        assert!(result.projects.is_empty());
    }

    #[tokio::test]
    async fn get_projects_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/project/search"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client.get_projects(50).await.unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn delete_issue_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-42"))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.delete_issue("PROJ-42").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn delete_issue_not_found() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/rest/api/3/issue/NOPE-1"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client.delete_issue("NOPE-1").await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn delete_issue_forbidden() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client.delete_issue("PROJ-1").await.unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    // ── get_watchers ──────────────────────────────────────────────

    #[tokio::test]
    async fn get_watchers_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/watchers",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "watchCount": 2,
                    "watchers": [
                        {
                            "accountId": "abc123",
                            "displayName": "Alice",
                            "emailAddress": "alice@example.com"
                        },
                        {
                            "accountId": "def456",
                            "displayName": "Bob"
                        }
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.get_watchers("PROJ-1").await.unwrap();

        assert_eq!(result.watch_count, 2);
        assert_eq!(result.watchers.len(), 2);
        assert_eq!(result.watchers[0].display_name, "Alice");
        assert_eq!(result.watchers[0].account_id, "abc123");
        assert_eq!(
            result.watchers[0].email_address.as_deref(),
            Some("alice@example.com")
        );
        assert_eq!(result.watchers[1].display_name, "Bob");
        assert!(result.watchers[1].email_address.is_none());
    }

    #[tokio::test]
    async fn get_watchers_empty() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/watchers",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "watchCount": 0,
                    "watchers": []
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.get_watchers("PROJ-1").await.unwrap();

        assert_eq!(result.watch_count, 0);
        assert!(result.watchers.is_empty());
    }

    #[tokio::test]
    async fn get_watchers_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/NOPE-1/watchers",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client.get_watchers("NOPE-1").await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    // ── add_watcher ───────────────────────────────────────────────

    #[tokio::test]
    async fn add_watcher_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/watchers",
            ))
            .and(wiremock::matchers::body_json(serde_json::json!("abc123")))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.add_watcher("PROJ-1", "abc123").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn add_watcher_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/watchers",
            ))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client.add_watcher("PROJ-1", "abc123").await.unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    // ── remove_watcher ────────────────────────────────────────────

    #[tokio::test]
    async fn remove_watcher_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/watchers",
            ))
            .and(wiremock::matchers::query_param("accountId", "abc123"))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.remove_watcher("PROJ-1", "abc123").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn remove_watcher_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/watchers",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client.remove_watcher("PROJ-1", "abc123").await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn get_myself_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/myself"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "displayName": "Alice Smith",
                    "emailAddress": "alice@example.com",
                    "accountId": "abc123"
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let user = client.get_myself().await.unwrap();
        assert_eq!(user.display_name, "Alice Smith");
        assert_eq!(user.email_address.as_deref(), Some("alice@example.com"));
        assert_eq!(user.account_id, "abc123");
    }

    #[tokio::test]
    async fn get_myself_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/myself"))
            .respond_with(wiremock::ResponseTemplate::new(401).set_body_string("Unauthorized"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client.get_myself().await.unwrap_err();
        assert!(err.to_string().contains("401"));
    }

    // ── get_issue_id ──────────────────────────────────────────────

    #[tokio::test]
    async fn get_issue_id_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"id": "12345", "key": "PROJ-1", "fields": {}}),
                ),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let id = client.get_issue_id("PROJ-1").await.unwrap();
        assert_eq!(id, "12345");
    }

    #[tokio::test]
    async fn get_issue_id_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/NOPE-1"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client.get_issue_id("NOPE-1").await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    // ── get_dev_status_summary ────────────────────────────────────

    #[tokio::test]
    async fn get_dev_status_summary_success() {
        let server = wiremock::MockServer::start().await;

        // Mock issue ID resolution.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"id": "10001", "key": "PROJ-1", "fields": {}}),
                ),
            )
            .mount(&server)
            .await;

        // Mock summary endpoint.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/dev-status/1.0/issue/summary",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "summary": {
                        "pullrequest": {
                            "overall": {"count": 2},
                            "byInstanceType": {"GitHub": {"count": 2, "name": "GitHub"}}
                        },
                        "branch": {
                            "overall": {"count": 1},
                            "byInstanceType": {"GitHub": {"count": 1, "name": "GitHub"}}
                        },
                        "repository": {
                            "overall": {"count": 1},
                            "byInstanceType": {}
                        }
                    }
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let summary = client.get_dev_status_summary("PROJ-1").await.unwrap();
        assert_eq!(summary.pullrequest.count, 2);
        assert_eq!(
            summary.pullrequest.providers,
            vec![JiraDevProvider {
                instance_type: "GitHub".to_string(),
                name: "GitHub".to_string(),
            }]
        );
        assert_eq!(summary.branch.count, 1);
        assert_eq!(summary.repository.count, 1);
        assert!(summary.repository.providers.is_empty());
    }

    #[tokio::test]
    async fn get_dev_status_summary_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"id": "10001", "key": "PROJ-1", "fields": {}}),
                ),
            )
            .mount(&server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/dev-status/1.0/issue/summary",
            ))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client.get_dev_status_summary("PROJ-1").await.unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    // ── get_dev_status ────────────────────────────────────────────

    /// Helper: mounts a mock for issue ID resolution returning id "10001".
    async fn mount_issue_id_mock(server: &wiremock::MockServer) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"id": "10001", "key": "PROJ-1", "fields": {}}),
                ),
            )
            .mount(server)
            .await;
    }

    /// Helper: mounts a mock for the dev-status summary returning GitHub as the only provider.
    async fn mount_summary_mock(server: &wiremock::MockServer) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/dev-status/1.0/issue/summary",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "summary": {
                        "pullrequest": {
                            "overall": {"count": 1},
                            "byInstanceType": {"GitHub": {"count": 1, "name": "GitHub"}}
                        },
                        "branch": {
                            "overall": {"count": 0},
                            "byInstanceType": {}
                        },
                        "repository": {
                            "overall": {"count": 0},
                            "byInstanceType": {}
                        }
                    }
                })),
            )
            .mount(server)
            .await;
    }

    fn dev_status_detail_response() -> serde_json::Value {
        serde_json::json!({
            "detail": [{
                "pullRequests": [{
                    "id": "#42",
                    "name": "Fix login bug",
                    "status": "MERGED",
                    "url": "https://github.com/org/repo/pull/42",
                    "repositoryName": "org/repo",
                    "source": {"branch": "fix-login"},
                    "destination": {"branch": "main"},
                    "author": {"name": "Alice"},
                    "reviewers": [{"name": "Bob"}],
                    "commentCount": 3,
                    "lastUpdate": "2024-01-15T10:30:00.000+0000"
                }],
                "branches": [{
                    "name": "fix-login",
                    "url": "https://github.com/org/repo/tree/fix-login",
                    "repositoryName": "org/repo",
                    "createPullRequestUrl": "https://github.com/org/repo/compare/fix-login",
                    "lastCommit": {
                        "id": "abc123def456",
                        "displayId": "abc123d",
                        "message": "Fix the login",
                        "author": {"name": "Alice"},
                        "authorTimestamp": "2024-01-14T08:00:00.000+0000",
                        "url": "https://github.com/org/repo/commit/abc123d",
                        "fileCount": 2,
                        "merge": false
                    }
                }],
                "repositories": [{
                    "name": "org/repo",
                    "url": "https://github.com/org/repo",
                    "commits": [{
                        "id": "abc123def456",
                        "displayId": "abc123d",
                        "message": "Fix the login",
                        "author": {"name": "Alice"},
                        "authorTimestamp": "2024-01-14T08:00:00.000+0000",
                        "url": "https://github.com/org/repo/commit/abc123d",
                        "fileCount": 2,
                        "merge": false
                    }]
                }],
                "_instance": {"name": "GitHub", "type": "GitHub"}
            }]
        })
    }

    #[tokio::test]
    async fn get_dev_status_pullrequest_fields() {
        let server = wiremock::MockServer::start().await;
        mount_issue_id_mock(&server).await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/dev-status/1.0/issue/detail",
            ))
            .and(wiremock::matchers::query_param("dataType", "pullrequest"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(dev_status_detail_response()),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let status = client
            .get_dev_status("PROJ-1", Some("pullrequest"), Some("GitHub"))
            .await
            .unwrap();

        assert_eq!(status.pull_requests.len(), 1);
        let pr = &status.pull_requests[0];
        assert_eq!(pr.id, "#42");
        assert_eq!(pr.status, "MERGED");
        assert_eq!(pr.author.as_deref(), Some("Alice"));
        assert_eq!(pr.reviewers, vec!["Bob"]);
        assert_eq!(pr.comment_count, Some(3));
        assert!(pr.last_update.is_some());
        assert_eq!(pr.source_branch, "fix-login");
        assert_eq!(pr.destination_branch, "main");
    }

    #[tokio::test]
    async fn get_dev_status_branch_fields() {
        let server = wiremock::MockServer::start().await;
        mount_issue_id_mock(&server).await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/dev-status/1.0/issue/detail",
            ))
            .and(wiremock::matchers::query_param("dataType", "branch"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(dev_status_detail_response()),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let status = client
            .get_dev_status("PROJ-1", Some("branch"), Some("GitHub"))
            .await
            .unwrap();

        assert_eq!(status.branches.len(), 1);
        let branch = &status.branches[0];
        assert_eq!(branch.name, "fix-login");
        assert!(branch.create_pr_url.is_some());
        let commit = branch.last_commit.as_ref().unwrap();
        assert_eq!(commit.display_id, "abc123d");
        assert_eq!(commit.file_count, 2);
        assert!(!commit.merge);
    }

    #[tokio::test]
    async fn get_dev_status_repository_with_commits() {
        let server = wiremock::MockServer::start().await;
        mount_issue_id_mock(&server).await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/dev-status/1.0/issue/detail",
            ))
            .and(wiremock::matchers::query_param("dataType", "repository"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(dev_status_detail_response()),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let status = client
            .get_dev_status("PROJ-1", Some("repository"), Some("GitHub"))
            .await
            .unwrap();

        assert_eq!(status.repositories.len(), 1);
        assert_eq!(status.repositories[0].commits.len(), 1);
        assert_eq!(status.repositories[0].commits[0].display_id, "abc123d");
        assert_eq!(
            status.repositories[0].commits[0].author.as_deref(),
            Some("Alice")
        );
    }

    #[tokio::test]
    async fn get_dev_status_auto_discovers_providers() {
        let server = wiremock::MockServer::start().await;
        mount_issue_id_mock(&server).await;
        mount_summary_mock(&server).await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/dev-status/1.0/issue/detail",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(dev_status_detail_response()),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let status = client
            .get_dev_status("PROJ-1", Some("pullrequest"), None)
            .await
            .unwrap();

        assert_eq!(status.pull_requests.len(), 1);
        assert_eq!(status.pull_requests[0].name, "Fix login bug");
    }

    /// Regression test for #924: a Bitbucket Server PR is keyed under `stash`
    /// in the summary's `byInstanceType` map (with the display name "Bitbucket
    /// Server"). Auto-discovery must query the detail endpoint with the *key*
    /// (`applicationType=stash`), not the display name, or the PR is missed and
    /// the result is empty.
    #[tokio::test]
    async fn get_dev_status_auto_discovers_bitbucket_server() {
        let server = wiremock::MockServer::start().await;
        mount_issue_id_mock(&server).await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/dev-status/1.0/issue/summary",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "summary": {
                        "pullrequest": {
                            "overall": {"count": 1},
                            "byInstanceType": {"stash": {"count": 1, "name": "Bitbucket Server"}}
                        },
                        "branch": {"overall": {"count": 0}, "byInstanceType": {}},
                        "repository": {
                            "overall": {"count": 1},
                            "byInstanceType": {"stash": {"count": 1, "name": "Bitbucket Server"}}
                        }
                    }
                })),
            )
            .mount(&server)
            .await;

        // Only respond when the detail query carries `applicationType=stash`.
        // The buggy code queried `applicationType=Bitbucket Server`, which would
        // not match this mock and surface as an API error.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/dev-status/1.0/issue/detail",
            ))
            .and(wiremock::matchers::query_param("applicationType", "stash"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(dev_status_detail_response()),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let status = client
            .get_dev_status("PROJ-1", Some("pullrequest"), None)
            .await
            .unwrap();

        assert_eq!(status.pull_requests.len(), 1);
        assert_eq!(status.pull_requests[0].name, "Fix login bug");
    }

    /// The summary must keep *both* halves of a `byInstanceType` entry: the key
    /// (`stash`) as `instance_type` for the detail round-trip, and the value's
    /// `name` ("Bitbucket Server") for display. Earlier behaviour collapsed them
    /// onto one or the other.
    #[tokio::test]
    async fn get_dev_status_summary_keeps_key_and_name() {
        let server = wiremock::MockServer::start().await;
        mount_issue_id_mock(&server).await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/dev-status/1.0/issue/summary",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "summary": {
                        "pullrequest": {
                            "overall": {"count": 1},
                            "byInstanceType": {"stash": {"count": 1, "name": "Bitbucket Server"}}
                        },
                        "branch": {"overall": {"count": 0}, "byInstanceType": {}},
                        "repository": {"overall": {"count": 0}, "byInstanceType": {}}
                    }
                })),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let summary = client.get_dev_status_summary("PROJ-1").await.unwrap();

        assert_eq!(
            summary.pullrequest.providers,
            vec![JiraDevProvider {
                instance_type: "stash".to_string(),
                name: "Bitbucket Server".to_string(),
            }]
        );
    }

    #[tokio::test]
    async fn get_dev_status_empty_response() {
        let server = wiremock::MockServer::start().await;
        mount_issue_id_mock(&server).await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/dev-status/1.0/issue/detail",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"detail": []})),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let status = client
            .get_dev_status("PROJ-1", None, Some("GitHub"))
            .await
            .unwrap();

        assert!(status.pull_requests.is_empty());
        assert!(status.branches.is_empty());
        assert!(status.repositories.is_empty());
    }

    #[tokio::test]
    async fn get_dev_status_detail_api_error() {
        let server = wiremock::MockServer::start().await;
        mount_issue_id_mock(&server).await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/dev-status/1.0/issue/detail",
            ))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("Server Error"))
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client
            .get_dev_status("PROJ-1", Some("pullrequest"), Some("GitHub"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    #[tokio::test]
    async fn get_dev_status_with_data_type_filter() {
        let server = wiremock::MockServer::start().await;
        mount_issue_id_mock(&server).await;

        // Only return branch data.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/dev-status/1.0/issue/detail",
            ))
            .and(wiremock::matchers::query_param("dataType", "branch"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "detail": [{
                        "pullRequests": [],
                        "branches": [{
                            "name": "feature-x",
                            "url": "https://github.com/org/repo/tree/feature-x",
                            "repositoryName": "org/repo"
                        }],
                        "repositories": []
                    }]
                })),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let status = client
            .get_dev_status("PROJ-1", Some("branch"), Some("GitHub"))
            .await
            .unwrap();

        assert!(status.pull_requests.is_empty());
        assert_eq!(status.branches.len(), 1);
        assert_eq!(status.branches[0].name, "feature-x");
        assert!(status.branches[0].last_commit.is_none());
        assert!(status.branches[0].create_pr_url.is_none());
        assert!(status.repositories.is_empty());
    }

    #[tokio::test]
    async fn get_dev_status_summary_empty() {
        let server = wiremock::MockServer::start().await;
        mount_issue_id_mock(&server).await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/dev-status/1.0/issue/summary",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"summary": {}})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let summary = client.get_dev_status_summary("PROJ-1").await.unwrap();
        assert_eq!(summary.pullrequest.count, 0);
        assert_eq!(summary.branch.count, 0);
        assert_eq!(summary.repository.count, 0);
    }

    #[tokio::test]
    async fn convert_commit_maps_all_fields() {
        let internal = DevStatusCommit {
            id: "abc123".to_string(),
            display_id: "abc".to_string(),
            message: "Test commit".to_string(),
            author: Some(DevStatusAuthor {
                name: "Alice".to_string(),
            }),
            author_timestamp: Some("2024-01-01T00:00:00.000+0000".to_string()),
            url: "https://example.com/commit/abc".to_string(),
            file_count: 5,
            merge: true,
        };
        let public = AtlassianClient::convert_commit(internal);
        assert_eq!(public.id, "abc123");
        assert_eq!(public.display_id, "abc");
        assert_eq!(public.message, "Test commit");
        assert_eq!(public.author.as_deref(), Some("Alice"));
        assert!(public.timestamp.is_some());
        assert_eq!(public.file_count, 5);
        assert!(public.merge);
    }

    #[tokio::test]
    async fn convert_commit_no_author() {
        let internal = DevStatusCommit {
            id: "def456".to_string(),
            display_id: "def".to_string(),
            message: "Anonymous".to_string(),
            author: None,
            author_timestamp: None,
            url: "https://example.com/commit/def".to_string(),
            file_count: 0,
            merge: false,
        };
        let public = AtlassianClient::convert_commit(internal);
        assert!(public.author.is_none());
        assert!(public.timestamp.is_none());
    }

    // ── extract_worklog_comment ────────────────────────────────────

    #[test]
    fn extract_worklog_comment_none() {
        assert_eq!(AtlassianClient::extract_worklog_comment(None), None);
    }

    #[test]
    fn extract_worklog_comment_valid_adf() {
        let adf = serde_json::json!({
            "version": 1,
            "type": "doc",
            "content": [{
                "type": "paragraph",
                "content": [{"type": "text", "text": "Fixed the login bug"}]
            }]
        });
        let result = AtlassianClient::extract_worklog_comment(Some(&adf));
        assert_eq!(result.as_deref(), Some("Fixed the login bug"));
    }

    #[test]
    fn extract_worklog_comment_empty_adf() {
        let adf = serde_json::json!({
            "version": 1,
            "type": "doc",
            "content": []
        });
        let result = AtlassianClient::extract_worklog_comment(Some(&adf));
        assert_eq!(result, None);
    }

    #[test]
    fn extract_worklog_comment_invalid_json() {
        let invalid = serde_json::json!({"not": "adf"});
        let result = AtlassianClient::extract_worklog_comment(Some(&invalid));
        assert_eq!(result, None);
    }

    // ── worklog deserialization ────────────────────────────────────

    #[test]
    fn worklog_response_deserializes() {
        let json = r#"{
            "worklogs": [
                {
                    "id": "100",
                    "author": {"displayName": "Alice"},
                    "timeSpent": "2h",
                    "timeSpentSeconds": 7200,
                    "started": "2026-04-16T09:00:00.000+0000",
                    "comment": {
                        "version": 1,
                        "type": "doc",
                        "content": [{"type": "paragraph", "content": [{"type": "text", "text": "Debugging"}]}]
                    }
                },
                {
                    "id": "101",
                    "author": {"displayName": "Bob"},
                    "timeSpent": "1d",
                    "timeSpentSeconds": 28800,
                    "started": "2026-04-15T10:00:00.000+0000"
                }
            ],
            "total": 2
        }"#;
        let resp: JiraWorklogResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.total, 2);
        assert_eq!(resp.worklogs.len(), 2);
        assert_eq!(resp.worklogs[0].id, "100");
        assert_eq!(resp.worklogs[0].time_spent.as_deref(), Some("2h"));
        assert_eq!(resp.worklogs[0].time_spent_seconds, 7200);
        assert!(resp.worklogs[0].comment.is_some());
        assert!(resp.worklogs[1].comment.is_none());
    }

    #[test]
    fn worklog_response_empty() {
        let json = r#"{"worklogs": [], "total": 0}"#;
        let resp: JiraWorklogResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.total, 0);
        assert!(resp.worklogs.is_empty());
    }

    #[test]
    fn worklog_response_missing_optional_fields() {
        let json = r#"{
            "worklogs": [{
                "id": "200",
                "timeSpentSeconds": 3600
            }],
            "total": 1
        }"#;
        let resp: JiraWorklogResponse = serde_json::from_str(json).unwrap();
        assert!(resp.worklogs[0].author.is_none());
        assert!(resp.worklogs[0].time_spent.is_none());
        assert!(resp.worklogs[0].started.is_none());
    }

    // ── worklog wiremock tests ────────────────────────────────────

    #[tokio::test]
    async fn get_worklogs_success() {
        let server = wiremock::MockServer::start().await;

        let worklog_json = serde_json::json!({
            "worklogs": [
                {
                    "id": "100",
                    "author": {"displayName": "Alice"},
                    "timeSpent": "2h",
                    "timeSpentSeconds": 7200,
                    "started": "2026-04-16T09:00:00.000+0000",
                    "comment": {
                        "version": 1,
                        "type": "doc",
                        "content": [{"type": "paragraph", "content": [{"type": "text", "text": "Debugging login"}]}]
                    }
                },
                {
                    "id": "101",
                    "author": {"displayName": "Bob"},
                    "timeSpent": "1d",
                    "timeSpentSeconds": 28800,
                    "started": "2026-04-15T10:00:00.000+0000"
                }
            ],
            "total": 2
        });

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1/worklog"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(worklog_json))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.get_worklogs("PROJ-1", 50).await.unwrap();

        assert_eq!(result.total, 2);
        assert_eq!(result.worklogs.len(), 2);
        assert_eq!(result.worklogs[0].author, "Alice");
        assert_eq!(result.worklogs[0].time_spent, "2h");
        assert_eq!(result.worklogs[0].time_spent_seconds, 7200);
        assert_eq!(
            result.worklogs[0].comment.as_deref(),
            Some("Debugging login")
        );
        assert_eq!(result.worklogs[1].author, "Bob");
        assert_eq!(result.worklogs[1].comment, None);
    }

    #[tokio::test]
    async fn get_worklogs_empty() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1/worklog"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"worklogs": [], "total": 0})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.get_worklogs("PROJ-1", 50).await.unwrap();

        assert_eq!(result.total, 0);
        assert!(result.worklogs.is_empty());
    }

    #[tokio::test]
    async fn get_worklogs_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1/worklog"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.get_worklogs("PROJ-1", 50).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn add_worklog_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1/worklog"))
            .respond_with(wiremock::ResponseTemplate::new(201))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.add_worklog("PROJ-1", "2h", None, None).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn add_worklog_with_all_fields() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1/worklog"))
            .respond_with(wiremock::ResponseTemplate::new(201))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client
            .add_worklog(
                "PROJ-1",
                "2h 30m",
                Some("2026-04-16T09:00:00.000+0000"),
                Some("Fixed the bug"),
            )
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn add_worklog_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1/worklog"))
            .respond_with(wiremock::ResponseTemplate::new(400).set_body_string("Bad Request"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.add_worklog("PROJ-1", "2h", None, None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn get_worklogs_respects_limit() {
        let server = wiremock::MockServer::start().await;

        let worklog_json = serde_json::json!({
            "worklogs": [
                {"id": "1", "author": {"displayName": "A"}, "timeSpent": "1h", "timeSpentSeconds": 3600, "started": "2026-04-16T09:00:00.000+0000"},
                {"id": "2", "author": {"displayName": "B"}, "timeSpent": "2h", "timeSpentSeconds": 7200, "started": "2026-04-16T10:00:00.000+0000"},
                {"id": "3", "author": {"displayName": "C"}, "timeSpent": "3h", "timeSpentSeconds": 10800, "started": "2026-04-16T11:00:00.000+0000"}
            ],
            "total": 3
        });

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1/worklog"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(worklog_json))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let result = client.get_worklogs("PROJ-1", 2).await.unwrap();

        assert_eq!(result.worklogs.len(), 2);
        assert_eq!(result.total, 3);
    }
}

impl AtlassianClient {
    /// Creates a new Atlassian API client.
    ///
    /// Constructs the Basic Auth header from the email and API token.
    pub fn new(instance_url: &str, email: &str, api_token: &str) -> Result<Self> {
        let client = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("Failed to build HTTP client")?;

        let credentials = format!("{email}:{api_token}");
        let encoded = base64::engine::general_purpose::STANDARD.encode(credentials);
        let auth_header = format!("Basic {encoded}");

        Ok(Self {
            client,
            instance_url: instance_url.trim_end_matches('/').to_string(),
            auth_header,
        })
    }

    /// Creates a client from stored credentials.
    pub fn from_credentials(creds: &crate::atlassian::auth::AtlassianCredentials) -> Result<Self> {
        Self::new(
            &creds.instance_url,
            &creds.email,
            creds.api_token.expose_secret(),
        )
    }

    /// Returns the instance URL.
    #[must_use]
    pub fn instance_url(&self) -> &str {
        &self.instance_url
    }

    /// Appends a best-effort HTTP record for one request attempt. The service
    /// tag is `confluence` for `/wiki/` paths, else `jira`.
    fn log_request(
        &self,
        method: &str,
        url: &str,
        started: Instant,
        result: &reqwest::Result<reqwest::Response>,
    ) {
        let service = if url.contains("/wiki/") {
            "confluence"
        } else {
            "jira"
        };
        request_log::record_http_result(service, method, url, started, result);
    }

    /// Sends an authenticated GET request and returns the raw response.
    ///
    /// Shared transport method used by both JIRA and Confluence API
    /// implementations.
    pub async fn get_json(&self, url: &str) -> Result<reqwest::Response> {
        retry_429(
            || {
                self.client
                    .get(url)
                    .header("Authorization", &self.auth_header)
                    .header("Accept", "application/json")
            },
            |started, result| self.log_request("GET", url, started, result),
        )
        .await
        .context("Failed to send GET request to Atlassian API")
    }

    /// Sends an authenticated PUT request with a JSON body and returns the raw response.
    ///
    /// Shared transport method used by both JIRA and Confluence API
    /// implementations.
    pub async fn put_json<T: serde::Serialize + Sync + ?Sized>(
        &self,
        url: &str,
        body: &T,
    ) -> Result<reqwest::Response> {
        retry_429(
            || {
                self.client
                    .put(url)
                    .header("Authorization", &self.auth_header)
                    .header("Content-Type", "application/json")
                    .json(body)
            },
            |started, result| self.log_request("PUT", url, started, result),
        )
        .await
        .context("Failed to send PUT request to Atlassian API")
    }

    /// Sends an authenticated POST request with a JSON body and returns the raw response.
    pub async fn post_json<T: serde::Serialize + Sync + ?Sized>(
        &self,
        url: &str,
        body: &T,
    ) -> Result<reqwest::Response> {
        retry_429(
            || {
                self.client
                    .post(url)
                    .header("Authorization", &self.auth_header)
                    .header("Content-Type", "application/json")
                    .json(body)
            },
            |started, result| self.log_request("POST", url, started, result),
        )
        .await
        .context("Failed to send POST request to Atlassian API")
    }

    /// Sends an authenticated GET request and returns raw bytes.
    pub async fn get_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let response = self.get_json_raw_accept(url, "*/*").await?;

        let response = Self::ensure_success(response).await?;

        let bytes = response
            .bytes()
            .await
            .context("Failed to read response bytes")?;
        Ok(bytes.to_vec())
    }

    /// Sends an authenticated DELETE request and returns the raw response.
    pub async fn delete(&self, url: &str) -> Result<reqwest::Response> {
        retry_429(
            || {
                self.client
                    .delete(url)
                    .header("Authorization", &self.auth_header)
            },
            |started, result| self.log_request("DELETE", url, started, result),
        )
        .await
        .context("Failed to send DELETE request to Atlassian API")
    }

    /// Sends an authenticated POST request with a multipart body and returns the raw response.
    ///
    /// Does not retry on 429: a streamed multipart body cannot be replayed. Callers
    /// that need retry must rebuild the form and call again.
    pub async fn post_multipart(
        &self,
        url: &str,
        form: reqwest::multipart::Form,
        extra_headers: &[(&str, &str)],
    ) -> Result<reqwest::Response> {
        let mut req = self
            .client
            .post(url)
            .header("Authorization", &self.auth_header)
            .multipart(form);
        for (name, value) in extra_headers {
            req = req.header(*name, *value);
        }
        let started = Instant::now();
        let result = req.send().await;
        self.log_request("POST", url, started, &result);
        result.context("Failed to send multipart POST request to Atlassian API")
    }

    /// Internal: GET with custom Accept header and 429 retry.
    async fn get_json_raw_accept(&self, url: &str, accept: &str) -> Result<reqwest::Response> {
        retry_429(
            || {
                self.client
                    .get(url)
                    .header("Authorization", &self.auth_header)
                    .header("Accept", accept)
            },
            |started, result| self.log_request("GET", url, started, result),
        )
        .await
        .context("Failed to send GET request to Atlassian API")
    }

    /// Returns `response` unchanged if its status is a success, otherwise reads
    /// the body and fails with [`AtlassianError::ApiRequestFailed`].
    ///
    /// Centralises the "check status → read body → build error" block copied
    /// after nearly every request. Call sites that need bespoke diagnostics for
    /// specific status codes (e.g. `jira_write_error`, `confluence_write_error`)
    /// build their error directly instead of using this helper.
    pub(crate) async fn ensure_success(response: reqwest::Response) -> Result<reqwest::Response> {
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        Err(AtlassianError::ApiRequestFailed { status, body }.into())
    }

    /// Deserialises `response`'s JSON body into `T`, attaching `context` on
    /// failure.
    ///
    /// Pairs with [`Self::ensure_success`]; the common
    /// `Self::parse_json(Self::ensure_success(resp).await?, "…").await?`
    /// spelling replaces the hand-copied status-check + `json().context(…)`
    /// block.
    pub(crate) async fn parse_json<T: serde::de::DeserializeOwned>(
        response: reqwest::Response,
        context: &'static str,
    ) -> Result<T> {
        response.json().await.context(context)
    }

    /// Fetches a JIRA issue by key with only the standard fields.
    ///
    /// Thin shim over [`Self::get_issue_with_fields`] with
    /// [`FieldSelection::Standard`]. Preserved for callers that do not need
    /// custom field data.
    pub async fn get_issue(&self, key: &str) -> Result<JiraIssue> {
        self.get_issue_with_fields(key, FieldSelection::Standard)
            .await
    }

    /// Fetches a JIRA issue by key with the given field selection.
    ///
    /// Always requests `expand=names,schema` so human-readable field names
    /// and type metadata are available for rendering custom fields. When
    /// `selection` is [`FieldSelection::Standard`], `custom_fields` on the
    /// returned issue will be empty.
    pub async fn get_issue_with_fields(
        &self,
        key: &str,
        selection: FieldSelection,
    ) -> Result<JiraIssue> {
        const STANDARD_FIELDS: &str =
            "summary,description,status,issuetype,assignee,priority,labels";

        let fields_param = match &selection {
            FieldSelection::Standard => STANDARD_FIELDS.to_string(),
            FieldSelection::Named(names) => {
                let mut parts: Vec<&str> = STANDARD_FIELDS.split(',').collect();
                parts.extend(names.iter().map(String::as_str));
                parts.join(",")
            }
            FieldSelection::All => "*all".to_string(),
        };

        let base = format!("{}/rest/api/3/issue/{}", self.instance_url, key);
        let url = reqwest::Url::parse_with_params(
            &base,
            &[
                ("fields", fields_param.as_str()),
                ("expand", "names,schema"),
            ],
        )
        .context("Failed to build JIRA issue URL")?;

        let response = self
            .client
            .get(url)
            .header("Authorization", &self.auth_header)
            .header("Accept", "application/json")
            .send()
            .await
            .context("Failed to send request to JIRA API")?;

        let envelope: JiraIssueEnvelope = Self::parse_json(
            Self::ensure_success(response).await?,
            "Failed to parse JIRA issue response",
        )
        .await?;

        Ok(envelope.into_issue(&selection))
    }

    /// Updates a JIRA issue's description and optionally its summary.
    ///
    /// Thin shim over [`Self::update_issue_with_custom_fields`] that sends no
    /// custom field changes.
    pub async fn update_issue(
        &self,
        key: &str,
        description_adf: &ValidatedAdfDocument,
        summary: Option<&str>,
    ) -> Result<()> {
        self.update_issue_with_custom_fields(
            key,
            Some(description_adf),
            summary,
            &std::collections::BTreeMap::new(),
        )
        .await
    }

    /// Updates a JIRA issue with any subset of supported fields.
    ///
    /// `description_adf` and `summary` are each `Option`: `None` leaves the
    /// field untouched, `Some` overwrites it. `custom_fields` is merged
    /// verbatim into the `fields` payload, keyed by stable JIRA field id —
    /// both standard fields (`assignee`, `reporter`, `priority`, `labels`)
    /// and custom fields (`customfield_19300`). The system `parent` field is
    /// set via [`Self::set_issue_parent`], not here. Returns an error when
    /// nothing would be sent (avoids a no-op PUT that JIRA still validates).
    pub async fn update_issue_with_custom_fields(
        &self,
        key: &str,
        description_adf: Option<&ValidatedAdfDocument>,
        summary: Option<&str>,
        custom_fields: &std::collections::BTreeMap<String, serde_json::Value>,
    ) -> Result<()> {
        let url = format!("{}/rest/api/3/issue/{}", self.instance_url, key);

        let mut fields = serde_json::Map::new();
        if let Some(adf) = description_adf {
            fields.insert(
                "description".to_string(),
                serde_json::to_value(adf).context("Failed to serialize ADF document")?,
            );
        }
        if let Some(summary_text) = summary {
            fields.insert(
                "summary".to_string(),
                serde_json::Value::String(summary_text.to_string()),
            );
        }
        for (id, value) in custom_fields {
            fields.insert(id.clone(), value.clone());
        }

        if fields.is_empty() {
            anyhow::bail!("update_issue_with_custom_fields: no fields to update");
        }

        let body = serde_json::json!({ "fields": fields });

        let response = self
            .client
            .put(&url)
            .header("Authorization", &self.auth_header)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to send update request to JIRA API")?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(jira_write_error(status, body));
        }

        Ok(())
    }

    /// Fetches editable field metadata scoped to an issue's edit screen.
    ///
    /// `GET /rest/api/3/issue/{key}/editmeta` returns only fields on the
    /// issue's screen, so field names are unambiguous even when multiple
    /// custom fields share a display name globally.
    pub async fn get_editmeta(&self, key: &str) -> Result<EditMeta> {
        let url = format!("{}/rest/api/3/issue/{}/editmeta", self.instance_url, key);

        let response = self
            .client
            .get(&url)
            .header("Authorization", &self.auth_header)
            .header("Accept", "application/json")
            .send()
            .await
            .context("Failed to send editmeta request to JIRA API")?;

        let raw: JiraEditMetaResponse = Self::parse_json(
            Self::ensure_success(response).await?,
            "Failed to parse JIRA editmeta response",
        )
        .await?;

        Ok(edit_meta_from_raw_fields(raw.fields))
    }

    /// Creates a new JIRA issue.
    ///
    /// Thin shim over [`Self::create_issue_with_custom_fields`] that sends no
    /// custom field values.
    pub async fn create_issue(
        &self,
        project_key: &str,
        issue_type: &str,
        summary: &str,
        description_adf: Option<&ValidatedAdfDocument>,
        labels: &[String],
    ) -> Result<JiraCreatedIssue> {
        self.create_issue_with_custom_fields(
            project_key,
            issue_type,
            summary,
            description_adf,
            labels,
            &std::collections::BTreeMap::new(),
        )
        .await
    }

    /// Creates a new JIRA issue with standard fields and any custom fields
    /// keyed by stable ID (e.g., `customfield_19300`).
    pub async fn create_issue_with_custom_fields(
        &self,
        project_key: &str,
        issue_type: &str,
        summary: &str,
        description_adf: Option<&ValidatedAdfDocument>,
        labels: &[String],
        custom_fields: &std::collections::BTreeMap<String, serde_json::Value>,
    ) -> Result<JiraCreatedIssue> {
        let url = format!("{}/rest/api/3/issue", self.instance_url);

        let mut fields = serde_json::Map::new();
        fields.insert(
            "project".to_string(),
            serde_json::json!({ "key": project_key }),
        );
        fields.insert(
            "issuetype".to_string(),
            serde_json::json!({ "name": issue_type }),
        );
        fields.insert(
            "summary".to_string(),
            serde_json::Value::String(summary.to_string()),
        );
        if let Some(adf) = description_adf {
            fields.insert(
                "description".to_string(),
                serde_json::to_value(adf).context("Failed to serialize ADF document")?,
            );
        }
        if !labels.is_empty() {
            fields.insert("labels".to_string(), serde_json::to_value(labels)?);
        }
        for (id, value) in custom_fields {
            fields.insert(id.clone(), value.clone());
        }

        let body = serde_json::json!({ "fields": fields });

        let response = self
            .post_json(&url, &body)
            .await
            .context("Failed to send create request to JIRA API")?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            // Parity with update: surface JIRA's `{ "errors": {...} }` envelope
            // as the actionable `JiraAdfFieldRequired` when a field reports it
            // needs ADF, instead of an opaque `ApiRequestFailed` (issue #1047).
            return Err(jira_write_error(status, body));
        }

        let create_response: JiraCreateResponse = response
            .json()
            .await
            .context("Failed to parse JIRA create response")?;

        Ok(JiraCreatedIssue {
            key: create_response.key,
            id: create_response.id,
            self_url: create_response.self_url,
        })
    }

    /// Fetches field metadata for creating a JIRA issue of a given project
    /// and issue type.
    ///
    /// `GET /rest/api/3/issue/createmeta?projectKeys={p}&issuetypeNames={t}&expand=projects.issuetypes.fields`
    /// returns fields on the create screen, which is the write-time source
    /// of truth for custom-field resolution prior to issue creation.
    pub async fn get_createmeta(&self, project_key: &str, issue_type: &str) -> Result<EditMeta> {
        let base = format!("{}/rest/api/3/issue/createmeta", self.instance_url);
        let url = reqwest::Url::parse_with_params(
            &base,
            &[
                ("projectKeys", project_key),
                ("issuetypeNames", issue_type),
                ("expand", "projects.issuetypes.fields"),
            ],
        )
        .context("Failed to build JIRA createmeta URL")?;

        let response = self
            .client
            .get(url)
            .header("Authorization", &self.auth_header)
            .header("Accept", "application/json")
            .send()
            .await
            .context("Failed to send createmeta request to JIRA API")?;

        let raw: JiraCreateMetaResponse = Self::parse_json(
            Self::ensure_success(response).await?,
            "Failed to parse JIRA createmeta response",
        )
        .await?;

        let Some(project) = raw.projects.into_iter().next() else {
            return Ok(EditMeta::default());
        };
        let Some(issuetype) = project.issuetypes.into_iter().next() else {
            return Ok(EditMeta::default());
        };

        Ok(edit_meta_from_raw_fields(issuetype.fields))
    }

    /// Introspects the create screen for a project + issue type, returning each
    /// field with its `required` flag, schema type, allowed values, and default.
    ///
    /// `GET /rest/api/3/issue/createmeta?projectKeys={p}&issuetypeNames={t}&expand=projects.issuetypes.fields`
    /// — the same endpoint as [`get_createmeta`](Self::get_createmeta), parsed
    /// for the full field metadata an agent needs to prompt before creating.
    pub async fn get_project_create_meta(
        &self,
        project_key: &str,
        issue_type: &str,
    ) -> Result<CreateMeta> {
        let base = format!("{}/rest/api/3/issue/createmeta", self.instance_url);
        let url = reqwest::Url::parse_with_params(
            &base,
            &[
                ("projectKeys", project_key),
                ("issuetypeNames", issue_type),
                ("expand", "projects.issuetypes.fields"),
            ],
        )
        .context("Failed to build JIRA createmeta URL")?;

        let response = self
            .client
            .get(url)
            .header("Authorization", &self.auth_header)
            .header("Accept", "application/json")
            .send()
            .await
            .context("Failed to send createmeta request to JIRA API")?;

        let raw: JiraCreateMetaFullResponse = Self::parse_json(
            Self::ensure_success(response).await?,
            "Failed to parse JIRA createmeta response",
        )
        .await?;

        let mut fields: Vec<CreateMetaField> = raw
            .projects
            .into_iter()
            .next()
            .and_then(|p| p.issuetypes.into_iter().next())
            .map(|it| {
                it.fields
                    .into_iter()
                    .map(|(field_id, field)| {
                        let schema = field.schema.unwrap_or(JiraCreateMetaSchemaRaw {
                            kind: None,
                            items: None,
                            custom: None,
                        });
                        CreateMetaField {
                            field_id,
                            name: field.name.unwrap_or_default(),
                            required: field.required,
                            schema_type: schema.kind.unwrap_or_default(),
                            items: schema.items,
                            custom: schema.custom,
                            allowed_values: field
                                .allowed_values
                                .into_iter()
                                .map(JiraAllowedValueRaw::into_allowed_value)
                                .collect(),
                            default_value: field.default_value,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Required fields first, then alphabetically by name for stable output.
        fields.sort_by(|a, b| {
            b.required
                .cmp(&a.required)
                .then_with(|| a.name.cmp(&b.name))
        });

        Ok(CreateMeta {
            project: project_key.to_string(),
            issue_type: issue_type.to_string(),
            fields,
        })
    }

    /// Lists comments on a JIRA issue with auto-pagination.
    ///
    /// `limit` caps the total number of comments returned. Pass `0` for unlimited.
    pub async fn get_comments(&self, key: &str, limit: u32) -> Result<Vec<JiraComment>> {
        let effective_limit = if limit == 0 { u32::MAX } else { limit };
        let mut all_comments = Vec::new();
        let mut start_at: u32 = 0;

        loop {
            let remaining = effective_limit.saturating_sub(all_comments.len() as u32);
            if remaining == 0 {
                break;
            }
            let page_size = remaining.min(PAGE_SIZE);

            let url = format!(
                "{}/rest/api/3/issue/{}/comment?orderBy=created&maxResults={}&startAt={}",
                self.instance_url, key, page_size, start_at
            );

            let response = self.get_json(&url).await?;

            let resp: JiraCommentsResponse = Self::parse_json(
                Self::ensure_success(response).await?,
                "Failed to parse comments response",
            )
            .await?;

            let page_count = resp.comments.len() as u32;
            for c in resp.comments {
                all_comments.push(JiraComment {
                    id: c.id,
                    author: c.author.and_then(|a| a.display_name).unwrap_or_default(),
                    body_adf: c.body,
                    created: c.created.unwrap_or_default(),
                    updated: c.updated,
                });
            }

            if page_count == 0 {
                break;
            }

            let fetched = resp.start_at.saturating_add(page_count);
            if fetched >= resp.total {
                break;
            }

            start_at += page_count;
        }

        Ok(all_comments)
    }

    /// Adds a comment to a JIRA issue.
    pub async fn add_comment(&self, key: &str, body_adf: &ValidatedAdfDocument) -> Result<()> {
        let url = format!("{}/rest/api/3/issue/{}/comment", self.instance_url, key);

        let body = serde_json::json!({
            "body": body_adf
        });

        let response = self.post_json(&url, &body).await?;

        Self::ensure_success(response).await?;

        Ok(())
    }

    /// Updates an existing comment on a JIRA issue.
    ///
    /// Issues a `PUT /rest/api/3/issue/{key}/comment/{id}` with the new ADF
    /// body and an optional visibility restriction. Returns the updated
    /// comment as parsed from the JIRA response so callers can surface the
    /// `updated` timestamp and any author/body changes JIRA applied.
    pub async fn update_comment(
        &self,
        key: &str,
        comment_id: &str,
        body_adf: &ValidatedAdfDocument,
        visibility: Option<&JiraVisibility>,
    ) -> Result<JiraComment> {
        let url = format!(
            "{}/rest/api/3/issue/{}/comment/{}",
            self.instance_url, key, comment_id
        );

        let mut body = serde_json::json!({ "body": body_adf });
        if let Some(v) = visibility {
            body["visibility"] =
                serde_json::to_value(v).context("Failed to serialize comment visibility")?;
        }

        let response = self.put_json(&url, &body).await?;

        let entry: JiraCommentEntry = Self::parse_json(
            Self::ensure_success(response).await?,
            "Failed to parse updated comment response",
        )
        .await?;

        Ok(JiraComment {
            id: entry.id,
            author: entry
                .author
                .and_then(|a| a.display_name)
                .unwrap_or_default(),
            body_adf: entry.body,
            created: entry.created.unwrap_or_default(),
            updated: entry.updated,
        })
    }

    /// Lists worklogs for a JIRA issue.
    pub async fn get_worklogs(&self, key: &str, limit: u32) -> Result<JiraWorklogList> {
        let effective_limit = if limit == 0 { u32::MAX } else { limit };
        let url = format!(
            "{}/rest/api/3/issue/{}/worklog?maxResults={}",
            self.instance_url,
            key,
            effective_limit.min(5000)
        );

        let response = self.get_json(&url).await?;

        let resp: JiraWorklogResponse = Self::parse_json(
            Self::ensure_success(response).await?,
            "Failed to parse worklog response",
        )
        .await?;

        let worklogs: Vec<JiraWorklog> = resp
            .worklogs
            .into_iter()
            .take(effective_limit as usize)
            .map(|w| JiraWorklog {
                id: w.id,
                author: w.author.and_then(|a| a.display_name).unwrap_or_default(),
                time_spent: w.time_spent.unwrap_or_default(),
                time_spent_seconds: w.time_spent_seconds,
                started: w.started.unwrap_or_default(),
                comment: Self::extract_worklog_comment(w.comment.as_ref()),
            })
            .collect();

        Ok(JiraWorklogList {
            total: resp.total,
            worklogs,
        })
    }

    /// Adds a worklog entry to a JIRA issue.
    pub async fn add_worklog(
        &self,
        key: &str,
        time_spent: &str,
        started: Option<&str>,
        comment: Option<&str>,
    ) -> Result<()> {
        let url = format!("{}/rest/api/3/issue/{}/worklog", self.instance_url, key);

        let mut body = serde_json::json!({
            "timeSpent": time_spent,
        });

        if let Some(started) = started {
            body["started"] = serde_json::Value::String(started.to_string());
        }

        if let Some(comment_text) = comment {
            body["comment"] = serde_json::json!({
                "type": "doc",
                "version": 1,
                "content": [{
                    "type": "paragraph",
                    "content": [{
                        "type": "text",
                        "text": comment_text
                    }]
                }]
            });
        }

        let response = self.post_json(&url, &body).await?;

        Self::ensure_success(response).await?;

        Ok(())
    }

    /// Extracts plain text from a worklog comment ADF value.
    fn extract_worklog_comment(adf_value: Option<&serde_json::Value>) -> Option<String> {
        let adf_value = adf_value?;
        let adf: AdfDocument = serde_json::from_value(adf_value.clone()).ok()?;
        let md = adf_to_markdown(&adf).ok()?;
        let trimmed = md.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    /// Lists available transitions for a JIRA issue.
    pub async fn get_transitions(&self, key: &str) -> Result<Vec<JiraTransition>> {
        let url = format!("{}/rest/api/3/issue/{}/transitions", self.instance_url, key);

        let response = self.get_json(&url).await?;

        let resp: JiraTransitionsResponse = Self::parse_json(
            Self::ensure_success(response).await?,
            "Failed to parse transitions response",
        )
        .await?;

        Ok(resp
            .transitions
            .into_iter()
            .map(transition_from_entry)
            .collect())
    }

    /// Lists available transitions for a JIRA issue together with each
    /// transition's screen-field metadata.
    ///
    /// Requests `expand=transitions.fields` so screen fields can be resolved
    /// for `execute --set-field`/`--resolution` and so a mandatory-comment
    /// screen can be detected. Returns the same [`JiraTransition`] list as
    /// [`Self::get_transitions`] plus a map keyed by transition id holding the
    /// screen [`EditMeta`]; screenless transitions have no entry.
    pub async fn get_transitions_with_fields(
        &self,
        key: &str,
    ) -> Result<(
        Vec<JiraTransition>,
        std::collections::BTreeMap<String, EditMeta>,
    )> {
        let url = format!(
            "{}/rest/api/3/issue/{}/transitions?expand=transitions.fields",
            self.instance_url, key
        );

        let response = self.get_json(&url).await?;

        let resp: JiraTransitionsResponse = Self::parse_json(
            Self::ensure_success(response).await?,
            "Failed to parse transitions response",
        )
        .await?;

        let mut metas: std::collections::BTreeMap<String, EditMeta> =
            std::collections::BTreeMap::new();
        let mut transitions = Vec::with_capacity(resp.transitions.len());
        for mut t in resp.transitions {
            if !t.fields.is_empty() {
                let fields = std::mem::take(&mut t.fields);
                metas.insert(t.id.clone(), edit_meta_from_raw_fields(fields));
            }
            transitions.push(transition_from_entry(t));
        }

        Ok((transitions, metas))
    }

    /// Executes a transition on a JIRA issue.
    ///
    /// Thin shim over [`Self::do_transition_with_fields`] that sends no screen
    /// fields and no transition comment.
    pub async fn do_transition(&self, key: &str, transition_id: &str) -> Result<()> {
        self.do_transition_with_fields(key, transition_id, &std::collections::BTreeMap::new(), None)
            .await
    }

    /// Executes a transition, optionally setting transition-screen `fields` and
    /// adding a comment in the same request.
    ///
    /// `fields` is a map of stable JIRA field id → API-shaped value (e.g.
    /// `resolution` → `{"name": "Fixed"}`); it is omitted from the body when
    /// empty. When `comment` is `Some`, it is added via the transition
    /// `update.comment` operation so it lands atomically with the transition —
    /// the only way to satisfy a transition screen that mandates a comment.
    pub async fn do_transition_with_fields(
        &self,
        key: &str,
        transition_id: &str,
        fields: &std::collections::BTreeMap<String, serde_json::Value>,
        comment: Option<&ValidatedAdfDocument>,
    ) -> Result<()> {
        let url = format!("{}/rest/api/3/issue/{}/transitions", self.instance_url, key);

        let mut body = serde_json::json!({
            "transition": { "id": transition_id }
        });
        if !fields.is_empty() {
            body["fields"] = serde_json::to_value(fields)
                .context("Failed to serialize transition fields payload")?;
        }
        if let Some(adf) = comment {
            body["update"] = serde_json::json!({
                "comment": [ { "add": { "body": adf } } ]
            });
        }

        let response = self.post_json(&url, &body).await?;

        Self::ensure_success(response).await?;

        Ok(())
    }

    /// Searches JIRA issues using JQL with auto-pagination.
    ///
    /// `limit` controls total results: 0 means unlimited.
    pub async fn search_issues(&self, jql: &str, limit: u32) -> Result<JiraSearchResult> {
        let url = format!("{}/rest/api/3/search/jql", self.instance_url);
        let effective_limit = if limit == 0 { u32::MAX } else { limit };
        let mut all_issues = Vec::new();
        let mut next_token: Option<String> = None;

        loop {
            let remaining = effective_limit.saturating_sub(all_issues.len() as u32);
            if remaining == 0 {
                break;
            }
            let page_size = remaining.min(PAGE_SIZE);

            let mut body = serde_json::json!({
                "jql": jql,
                "maxResults": page_size,
                "fields": ["summary", "status", "issuetype", "assignee", "priority"]
            });
            if let Some(ref token) = next_token {
                body["nextPageToken"] = serde_json::Value::String(token.clone());
            }

            let response = self
                .post_json(&url, &body)
                .await
                .context("Failed to send search request to JIRA API")?;

            let page: JiraSearchResponse = Self::parse_json(
                Self::ensure_success(response).await?,
                "Failed to parse JIRA search response",
            )
            .await?;

            let page_count = page.issues.len();
            for r in page.issues {
                all_issues.push(JiraIssue {
                    key: r.key,
                    summary: r.fields.summary.unwrap_or_default(),
                    description_adf: r.fields.description,
                    status: r.fields.status.and_then(|s| s.name),
                    issue_type: r.fields.issuetype.and_then(|t| t.name),
                    assignee: r.fields.assignee.and_then(|a| a.display_name),
                    priority: r.fields.priority.and_then(|p| p.name),
                    labels: r.fields.labels,
                    custom_fields: Vec::new(),
                });
            }

            match page.next_page_token {
                Some(token) if page_count > 0 => next_token = Some(token),
                _ => break,
            }
        }

        let total = all_issues.len() as u32;
        Ok(JiraSearchResult {
            issues: all_issues,
            total,
        })
    }

    /// Searches Confluence pages using CQL with auto-pagination.
    pub async fn search_confluence(
        &self,
        cql: &str,
        limit: u32,
    ) -> Result<ConfluenceSearchResults> {
        let effective_limit = if limit == 0 { u32::MAX } else { limit };
        let mut all_results = Vec::new();
        let mut start: u32 = 0;

        loop {
            let remaining = effective_limit.saturating_sub(all_results.len() as u32);
            if remaining == 0 {
                break;
            }
            let page_size = remaining.min(PAGE_SIZE);

            let base = format!("{}/wiki/rest/api/content/search", self.instance_url);
            let url = reqwest::Url::parse_with_params(
                &base,
                &[
                    ("cql", cql),
                    ("limit", &page_size.to_string()),
                    ("start", &start.to_string()),
                    ("expand", "space"),
                ],
            )
            .context("Failed to build Confluence search URL")?;

            let response = self.get_json(url.as_str()).await?;

            let resp: ConfluenceContentSearchResponse = Self::parse_json(
                Self::ensure_success(response).await?,
                "Failed to parse Confluence search response",
            )
            .await?;

            let page_count = resp.results.len() as u32;
            for r in resp.results {
                let space_key = r
                    .expandable
                    .and_then(|e| e.space)
                    .and_then(|s| s.rsplit('/').next().map(String::from))
                    .unwrap_or_default();
                all_results.push(ConfluenceSearchResult {
                    id: r.id,
                    title: r.title,
                    space_key,
                });
            }

            let has_next = resp.links.and_then(|l| l.next).is_some();
            if !has_next || page_count == 0 {
                break;
            }
            start += page_count;
        }

        let total = all_results.len() as u32;
        Ok(ConfluenceSearchResults {
            results: all_results,
            total,
        })
    }

    /// Searches JIRA users by display name or email substring.
    ///
    /// `query` is matched against `displayName` and `emailAddress` server-
    /// side; matching is substring and case-insensitive. `limit` of `0`
    /// returns every match (paginating internally), otherwise the result
    /// is truncated. Inactive users and app/customer account types are
    /// included — callers that need only assignable atlassian-account
    /// users should filter on `active` and `account_type`.
    ///
    /// Note: many tenants strip `emailAddress` from search results due to
    /// GDPR / privacy settings, even when the user has an email on file.
    pub async fn search_jira_users(
        &self,
        query: &str,
        limit: u32,
    ) -> Result<JiraUserSearchResults> {
        let effective_limit = if limit == 0 { u32::MAX } else { limit };
        let mut all_results: Vec<JiraUserSearchResult> = Vec::new();
        let mut start_at: u32 = 0;

        loop {
            let remaining = effective_limit.saturating_sub(all_results.len() as u32);
            if remaining == 0 {
                break;
            }
            let page_size = remaining.min(PAGE_SIZE);

            let base = format!("{}/rest/api/3/user/search", self.instance_url);
            let url = reqwest::Url::parse_with_params(
                &base,
                &[
                    ("query", query),
                    ("maxResults", &page_size.to_string()),
                    ("startAt", &start_at.to_string()),
                ],
            )
            .context("Failed to build JIRA user search URL")?;

            let response = self.get_json(url.as_str()).await?;

            let page: Vec<JiraUserSearchEntry> = Self::parse_json(
                Self::ensure_success(response).await?,
                "Failed to parse JIRA user search response",
            )
            .await?;

            let page_count = page.len() as u32;
            for entry in page {
                all_results.push(JiraUserSearchResult {
                    account_id: entry.account_id,
                    display_name: entry.display_name,
                    email_address: entry.email_address,
                    active: entry.active,
                    account_type: entry.account_type,
                });
            }

            // The API has no `isLast` / `next` envelope; when the page comes
            // back shorter than the page size, we've reached the end.
            if page_count < page_size {
                break;
            }
            start_at += page_count;
        }

        let count = all_results.len() as u32;
        Ok(JiraUserSearchResults {
            users: all_results,
            count,
        })
    }

    /// Searches Confluence users by display name or email.
    pub async fn search_confluence_users(
        &self,
        query: &str,
        limit: u32,
    ) -> Result<ConfluenceUserSearchResults> {
        let effective_limit = if limit == 0 { u32::MAX } else { limit };
        let mut all_results = Vec::new();
        let mut start: u32 = 0;

        let cql = format!("user.fullname~\"{query}\"");

        loop {
            let remaining = effective_limit.saturating_sub(all_results.len() as u32);
            if remaining == 0 {
                break;
            }
            let page_size = remaining.min(PAGE_SIZE);

            let base = format!("{}/wiki/rest/api/search/user", self.instance_url);
            let url = reqwest::Url::parse_with_params(
                &base,
                &[
                    ("cql", cql.as_str()),
                    ("limit", &page_size.to_string()),
                    ("start", &start.to_string()),
                ],
            )
            .context("Failed to build Confluence user search URL")?;

            let response = self.get_json(url.as_str()).await?;

            let resp: ConfluenceUserSearchResponse = Self::parse_json(
                Self::ensure_success(response).await?,
                "Failed to parse Confluence user search response",
            )
            .await?;

            let page_count = resp.results.len() as u32;
            for r in resp.results {
                let Some(user) = r.user else {
                    continue;
                };
                let display_name = user.display_name.or(user.public_name).unwrap_or_default();
                all_results.push(ConfluenceUserSearchResult {
                    account_id: user.account_id,
                    display_name,
                    email: user.email,
                });
            }

            let has_next = resp.links.and_then(|l| l.next).is_some();
            if !has_next || page_count == 0 {
                break;
            }
            start += page_count;
        }

        let total = all_results.len() as u32;
        Ok(ConfluenceUserSearchResults {
            users: all_results,
            total,
        })
    }

    /// Resolves a single JIRA user by account ID
    /// (`GET /rest/api/3/user?accountId=`).
    ///
    /// Failure-tolerant: an unknown / anonymised account (HTTP 404) or any
    /// other non-auth failure resolves to a stub record with `error` set rather
    /// than an `Err`, so a batch lookup never aborts for one bad ID. A `401`
    /// (bad credentials) is a hard error worth surfacing. Deactivated accounts
    /// come back from Atlassian as a real `200` record with `active: false`.
    pub async fn get_jira_user(&self, account_id: &str) -> Result<JiraUserRecord> {
        let base = format!("{}/rest/api/3/user", self.instance_url);
        let url = reqwest::Url::parse_with_params(&base, &[("accountId", account_id)])
            .context("Failed to build JIRA user get URL")?;

        let response = self.get_json(url.as_str()).await?;
        let status = response.status();

        if status.is_success() {
            let entry: JiraUserSearchEntry = response
                .json()
                .await
                .context("Failed to parse JIRA user get response")?;
            return Ok(JiraUserRecord {
                account_id: entry.account_id,
                display_name: entry.display_name,
                email_address: entry.email_address,
                active: Some(entry.active),
                account_type: entry.account_type,
                error: None,
            });
        }

        if status.as_u16() == 401 {
            let body = response.text().await.unwrap_or_default();
            return Err(AtlassianError::ApiRequestFailed { status: 401, body }.into());
        }

        let code = status.as_u16();
        let body = response.text().await.unwrap_or_default();
        Ok(JiraUserRecord {
            account_id: account_id.to_string(),
            display_name: None,
            email_address: None,
            active: None,
            account_type: None,
            error: Some(user_lookup_error(code, &body)),
        })
    }

    /// Resolves multiple JIRA users by account ID, concurrently.
    ///
    /// Each ID is fetched independently via [`Self::get_jira_user`]; per-ID
    /// failures become stub records, so the batch only errors on a genuine auth
    /// failure (or transport error). Results preserve request order.
    pub async fn get_jira_users(&self, account_ids: &[String]) -> Result<JiraUserGetResults> {
        let lookups = account_ids.iter().map(|id| self.get_jira_user(id));
        let users = futures::future::join_all(lookups)
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()?;
        Ok(JiraUserGetResults { users })
    }

    /// Resolves a single Confluence user by account ID
    /// (`GET /wiki/rest/api/user?accountId=`).
    ///
    /// Failure-tolerant in the same way as [`Self::get_jira_user`]. The v1 user
    /// object has no `active` flag, so [`ConfluenceUserRecord::active`] is
    /// always `None`; `displayName` falls back to `publicName`.
    pub async fn get_confluence_user(&self, account_id: &str) -> Result<ConfluenceUserRecord> {
        let base = format!("{}/wiki/rest/api/user", self.instance_url);
        let url = reqwest::Url::parse_with_params(&base, &[("accountId", account_id)])
            .context("Failed to build Confluence user get URL")?;

        let response = self.get_json(url.as_str()).await?;
        let status = response.status();

        if status.is_success() {
            let entry: ConfluenceUserGetEntry = response
                .json()
                .await
                .context("Failed to parse Confluence user get response")?;
            return Ok(ConfluenceUserRecord {
                account_id: entry.account_id.unwrap_or_else(|| account_id.to_string()),
                display_name: entry.display_name.or(entry.public_name),
                email: entry.email,
                account_type: entry.account_type,
                active: None,
                error: None,
            });
        }

        if status.as_u16() == 401 {
            let body = response.text().await.unwrap_or_default();
            return Err(AtlassianError::ApiRequestFailed { status: 401, body }.into());
        }

        let code = status.as_u16();
        let body = response.text().await.unwrap_or_default();
        Ok(ConfluenceUserRecord {
            account_id: account_id.to_string(),
            display_name: None,
            email: None,
            account_type: None,
            active: None,
            error: Some(user_lookup_error(code, &body)),
        })
    }

    /// Resolves multiple Confluence users by account ID, concurrently.
    ///
    /// Behaves like [`Self::get_jira_users`]: per-ID failures become stub
    /// records; the batch only errors on a genuine auth / transport failure.
    pub async fn get_confluence_users(
        &self,
        account_ids: &[String],
    ) -> Result<ConfluenceUserGetResults> {
        let lookups = account_ids.iter().map(|id| self.get_confluence_user(id));
        let users = futures::future::join_all(lookups)
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()?;
        Ok(ConfluenceUserGetResults { users })
    }

    /// Lists agile boards with auto-pagination.
    pub async fn get_boards(
        &self,
        project: Option<&str>,
        board_type: Option<&str>,
        limit: u32,
    ) -> Result<AgileBoardList> {
        let effective_limit = if limit == 0 { u32::MAX } else { limit };
        let mut all_boards = Vec::new();
        let mut start_at: u32 = 0;

        loop {
            let remaining = effective_limit.saturating_sub(all_boards.len() as u32);
            if remaining == 0 {
                break;
            }
            let page_size = remaining.min(PAGE_SIZE);

            let mut url = format!(
                "{}/rest/agile/1.0/board?maxResults={}&startAt={}",
                self.instance_url, page_size, start_at
            );
            if let Some(proj) = project {
                url.push_str(&format!("&projectKeyOrId={proj}"));
            }
            if let Some(bt) = board_type {
                url.push_str(&format!("&type={bt}"));
            }

            let response = self.get_json(&url).await?;

            let resp: AgileBoardListResponse = Self::parse_json(
                Self::ensure_success(response).await?,
                "Failed to parse board list response",
            )
            .await?;

            let page_count = resp.values.len() as u32;
            for b in resp.values {
                all_boards.push(AgileBoard {
                    id: b.id,
                    name: b.name,
                    board_type: b.board_type,
                    project_key: b.location.and_then(|l| l.project_key),
                });
            }

            if resp.is_last || page_count == 0 {
                break;
            }
            start_at += page_count;
        }

        let total = all_boards.len() as u32;
        Ok(AgileBoardList {
            boards: all_boards,
            total,
        })
    }

    /// Lists issues on an agile board with auto-pagination.
    pub async fn get_board_issues(
        &self,
        board_id: u64,
        jql: Option<&str>,
        limit: u32,
    ) -> Result<JiraSearchResult> {
        let effective_limit = if limit == 0 { u32::MAX } else { limit };
        let mut all_issues = Vec::new();
        let mut start_at: u32 = 0;

        loop {
            let remaining = effective_limit.saturating_sub(all_issues.len() as u32);
            if remaining == 0 {
                break;
            }
            let page_size = remaining.min(PAGE_SIZE);

            let base = format!(
                "{}/rest/agile/1.0/board/{}/issue",
                self.instance_url, board_id
            );
            let mut params: Vec<(&str, String)> = vec![
                ("maxResults", page_size.to_string()),
                ("startAt", start_at.to_string()),
            ];
            if let Some(jql_str) = jql {
                params.push(("jql", jql_str.to_string()));
            }
            let url = reqwest::Url::parse_with_params(
                &base,
                params.iter().map(|(k, v)| (*k, v.as_str())),
            )
            .context("Failed to build board issues URL")?;

            let response = self.get_json(url.as_str()).await?;

            let resp: AgileIssueListResponse = Self::parse_json(
                Self::ensure_success(response).await?,
                "Failed to parse board issues response",
            )
            .await?;

            let page_count = resp.issues.len() as u32;
            for r in resp.issues {
                all_issues.push(JiraIssue {
                    key: r.key,
                    summary: r.fields.summary.unwrap_or_default(),
                    description_adf: r.fields.description,
                    status: r.fields.status.and_then(|s| s.name),
                    issue_type: r.fields.issuetype.and_then(|t| t.name),
                    assignee: r.fields.assignee.and_then(|a| a.display_name),
                    priority: r.fields.priority.and_then(|p| p.name),
                    labels: r.fields.labels,
                    custom_fields: Vec::new(),
                });
            }

            if resp.is_last || page_count == 0 {
                break;
            }
            start_at += page_count;
        }

        let total = all_issues.len() as u32;
        Ok(JiraSearchResult {
            issues: all_issues,
            total,
        })
    }

    /// Lists sprints for an agile board with auto-pagination.
    pub async fn get_sprints(
        &self,
        board_id: u64,
        state: Option<&str>,
        limit: u32,
    ) -> Result<AgileSprintList> {
        let effective_limit = if limit == 0 { u32::MAX } else { limit };
        let mut all_sprints = Vec::new();
        let mut start_at: u32 = 0;

        loop {
            let remaining = effective_limit.saturating_sub(all_sprints.len() as u32);
            if remaining == 0 {
                break;
            }
            let page_size = remaining.min(PAGE_SIZE);

            let mut url = format!(
                "{}/rest/agile/1.0/board/{}/sprint?maxResults={}&startAt={}",
                self.instance_url, board_id, page_size, start_at
            );
            if let Some(s) = state {
                url.push_str(&format!("&state={s}"));
            }

            let response = self.get_json(&url).await?;

            let resp: AgileSprintListResponse = Self::parse_json(
                Self::ensure_success(response).await?,
                "Failed to parse sprint list response",
            )
            .await?;

            let page_count = resp.values.len() as u32;
            for s in resp.values {
                all_sprints.push(AgileSprint {
                    id: s.id,
                    name: s.name,
                    state: s.state,
                    start_date: s.start_date,
                    end_date: s.end_date,
                    goal: s.goal,
                });
            }

            if resp.is_last || page_count == 0 {
                break;
            }
            start_at += page_count;
        }

        let total = all_sprints.len() as u32;
        Ok(AgileSprintList {
            sprints: all_sprints,
            total,
        })
    }

    /// Lists issues in an agile sprint with auto-pagination.
    pub async fn get_sprint_issues(
        &self,
        sprint_id: u64,
        jql: Option<&str>,
        limit: u32,
    ) -> Result<JiraSearchResult> {
        let effective_limit = if limit == 0 { u32::MAX } else { limit };
        let mut all_issues = Vec::new();
        let mut start_at: u32 = 0;

        loop {
            let remaining = effective_limit.saturating_sub(all_issues.len() as u32);
            if remaining == 0 {
                break;
            }
            let page_size = remaining.min(PAGE_SIZE);

            let base = format!(
                "{}/rest/agile/1.0/sprint/{}/issue",
                self.instance_url, sprint_id
            );
            let mut params: Vec<(&str, String)> = vec![
                ("maxResults", page_size.to_string()),
                ("startAt", start_at.to_string()),
            ];
            if let Some(jql_str) = jql {
                params.push(("jql", jql_str.to_string()));
            }
            let url = reqwest::Url::parse_with_params(
                &base,
                params.iter().map(|(k, v)| (*k, v.as_str())),
            )
            .context("Failed to build sprint issues URL")?;

            let response = self.get_json(url.as_str()).await?;

            let resp: AgileIssueListResponse = Self::parse_json(
                Self::ensure_success(response).await?,
                "Failed to parse sprint issues response",
            )
            .await?;

            let page_count = resp.issues.len() as u32;
            for r in resp.issues {
                all_issues.push(JiraIssue {
                    key: r.key,
                    summary: r.fields.summary.unwrap_or_default(),
                    description_adf: r.fields.description,
                    status: r.fields.status.and_then(|s| s.name),
                    issue_type: r.fields.issuetype.and_then(|t| t.name),
                    assignee: r.fields.assignee.and_then(|a| a.display_name),
                    priority: r.fields.priority.and_then(|p| p.name),
                    labels: r.fields.labels,
                    custom_fields: Vec::new(),
                });
            }

            if resp.is_last || page_count == 0 {
                break;
            }
            start_at += page_count;
        }

        let total = all_issues.len() as u32;
        Ok(JiraSearchResult {
            issues: all_issues,
            total,
        })
    }

    /// Adds issues to an agile sprint.
    pub async fn add_issues_to_sprint(&self, sprint_id: u64, issue_keys: &[&str]) -> Result<()> {
        let url = format!(
            "{}/rest/agile/1.0/sprint/{}/issue",
            self.instance_url, sprint_id
        );

        let body = serde_json::json!({ "issues": issue_keys });

        let response = self.post_json(&url, &body).await?;

        Self::ensure_success(response).await?;

        Ok(())
    }

    /// Creates a new sprint on an agile board.
    pub async fn create_sprint(
        &self,
        board_id: u64,
        name: &str,
        start_date: Option<&str>,
        end_date: Option<&str>,
        goal: Option<&str>,
    ) -> Result<AgileSprint> {
        let url = format!("{}/rest/agile/1.0/sprint", self.instance_url);

        let mut body = serde_json::json!({
            "originBoardId": board_id,
            "name": name
        });
        if let Some(sd) = start_date {
            body["startDate"] = serde_json::Value::String(sd.to_string());
        }
        if let Some(ed) = end_date {
            body["endDate"] = serde_json::Value::String(ed.to_string());
        }
        if let Some(g) = goal {
            body["goal"] = serde_json::Value::String(g.to_string());
        }

        let response = self.post_json(&url, &body).await?;

        let entry: AgileSprintEntry = Self::parse_json(
            Self::ensure_success(response).await?,
            "Failed to parse sprint create response",
        )
        .await?;

        Ok(AgileSprint {
            id: entry.id,
            name: entry.name,
            state: entry.state,
            start_date: entry.start_date,
            end_date: entry.end_date,
            goal: entry.goal,
        })
    }

    /// Updates an existing sprint.
    pub async fn update_sprint(
        &self,
        sprint_id: u64,
        name: Option<&str>,
        state: Option<&str>,
        start_date: Option<&str>,
        end_date: Option<&str>,
        goal: Option<&str>,
    ) -> Result<()> {
        let url = format!("{}/rest/agile/1.0/sprint/{}", self.instance_url, sprint_id);

        let mut body = serde_json::Map::new();
        if let Some(n) = name {
            body.insert("name".to_string(), serde_json::Value::String(n.to_string()));
        }
        if let Some(s) = state {
            body.insert(
                "state".to_string(),
                serde_json::Value::String(s.to_string()),
            );
        }
        if let Some(sd) = start_date {
            body.insert(
                "startDate".to_string(),
                serde_json::Value::String(sd.to_string()),
            );
        }
        if let Some(ed) = end_date {
            body.insert(
                "endDate".to_string(),
                serde_json::Value::String(ed.to_string()),
            );
        }
        if let Some(g) = goal {
            body.insert("goal".to_string(), serde_json::Value::String(g.to_string()));
        }

        let response = self
            .put_json(&url, &serde_json::Value::Object(body))
            .await?;

        Self::ensure_success(response).await?;

        Ok(())
    }

    /// Lists versions for a JIRA project.
    ///
    /// Uses the lightweight `GET /rest/api/3/project/{key}/versions` endpoint,
    /// which returns all versions in a single response without pagination.
    /// `released` and `archived` filters are applied client-side.
    pub async fn get_project_versions(
        &self,
        project_key: &str,
        released: Option<bool>,
        archived: Option<bool>,
    ) -> Result<JiraProjectVersionList> {
        let url = format!(
            "{}/rest/api/3/project/{}/versions",
            self.instance_url, project_key
        );

        let response = self.get_json(&url).await?;

        let entries: Vec<JiraProjectVersionEntry> = Self::parse_json(
            Self::ensure_success(response).await?,
            "Failed to parse project versions response",
        )
        .await?;

        let versions: Vec<JiraProjectVersion> = entries
            .into_iter()
            .filter(|e| released.map_or(true, |r| e.released == r))
            .filter(|e| archived.map_or(true, |a| e.archived == a))
            .map(|e| JiraProjectVersion {
                id: e.id,
                name: e.name,
                description: e.description,
                project_key: project_key.to_string(),
                released: e.released,
                archived: e.archived,
                release_date: e.release_date,
                start_date: e.start_date,
            })
            .collect();

        let total = versions.len() as u32;
        Ok(JiraProjectVersionList { versions, total })
    }

    /// Creates a new version in a JIRA project.
    ///
    /// Validates `release_date` and `start_date` as `YYYY-MM-DD` client-side
    /// to surface clear errors before JIRA rejects the request with an
    /// opaque 400.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_project_version(
        &self,
        project_key: &str,
        name: &str,
        description: Option<&str>,
        release_date: Option<&str>,
        start_date: Option<&str>,
        released: bool,
        archived: bool,
    ) -> Result<JiraProjectVersion> {
        validate_iso_date(release_date, "release_date")?;
        validate_iso_date(start_date, "start_date")?;

        let url = format!("{}/rest/api/3/version", self.instance_url);

        let mut body = serde_json::json!({
            "project": project_key,
            "name": name,
            "released": released,
            "archived": archived,
        });
        if let Some(d) = description {
            body["description"] = serde_json::Value::String(d.to_string());
        }
        if let Some(rd) = release_date {
            body["releaseDate"] = serde_json::Value::String(rd.to_string());
        }
        if let Some(sd) = start_date {
            body["startDate"] = serde_json::Value::String(sd.to_string());
        }

        let response = self.post_json(&url, &body).await?;

        let entry: JiraProjectVersionEntry = Self::parse_json(
            Self::ensure_success(response).await?,
            "Failed to parse version create response",
        )
        .await?;

        Ok(JiraProjectVersion {
            id: entry.id,
            name: entry.name,
            description: entry.description,
            project_key: project_key.to_string(),
            released: entry.released,
            archived: entry.archived,
            release_date: entry.release_date,
            start_date: entry.start_date,
        })
    }

    /// Lists links on a JIRA issue.
    pub async fn get_issue_links(&self, key: &str) -> Result<Vec<JiraIssueLink>> {
        let url = format!(
            "{}/rest/api/3/issue/{}?fields=issuelinks",
            self.instance_url, key
        );

        let response = self.get_json(&url).await?;

        let resp: JiraIssueLinksResponse = Self::parse_json(
            Self::ensure_success(response).await?,
            "Failed to parse issue links response",
        )
        .await?;

        let mut links = Vec::new();
        for entry in resp.fields.issuelinks {
            if let Some(inward) = entry.inward_issue {
                links.push(JiraIssueLink {
                    id: entry.id.clone(),
                    link_type: entry.link_type.name.clone(),
                    direction: "inward".to_string(),
                    linked_issue_key: inward.key,
                    linked_issue_summary: inward.fields.and_then(|f| f.summary).unwrap_or_default(),
                });
            }
            if let Some(outward) = entry.outward_issue {
                links.push(JiraIssueLink {
                    id: entry.id,
                    link_type: entry.link_type.name,
                    direction: "outward".to_string(),
                    linked_issue_key: outward.key,
                    linked_issue_summary: outward
                        .fields
                        .and_then(|f| f.summary)
                        .unwrap_or_default(),
                });
            }
        }

        Ok(links)
    }

    /// Lists remote (external URL) issue links on a JIRA issue.
    ///
    /// Endpoint: `GET /rest/api/3/issue/{key}/remotelink` — returns a bare
    /// JSON array (not a wrapped `{ links: [...] }` envelope).
    pub async fn get_remote_issue_links(&self, key: &str) -> Result<Vec<JiraRemoteIssueLink>> {
        let url = format!("{}/rest/api/3/issue/{}/remotelink", self.instance_url, key);

        let response = self.get_json(&url).await?;

        let entries: Vec<JiraRemoteIssueLinkEntry> = Self::parse_json(
            Self::ensure_success(response).await?,
            "Failed to parse remote issue links response",
        )
        .await?;

        let mut links = Vec::with_capacity(entries.len());
        for entry in entries {
            // JIRA returns the remote link id as a number; normalize to String
            // so callers don't have to care about the wire shape.
            let id = match entry.id {
                serde_json::Value::String(s) => s,
                serde_json::Value::Number(n) => n.to_string(),
                other => {
                    return Err(anyhow::anyhow!(
                        "unexpected remote link id type in response: {other:?}"
                    ));
                }
            };
            links.push(JiraRemoteIssueLink {
                id,
                global_id: entry.global_id,
                relationship: entry.relationship,
                object: JiraRemoteIssueLinkObject {
                    url: entry.object.url,
                    title: entry.object.title,
                    summary: entry.object.summary,
                    icon: entry.object.icon.map(|i| JiraRemoteIssueLinkIcon {
                        url: i.url,
                        title: i.title,
                    }),
                },
            });
        }
        Ok(links)
    }

    /// Lists available issue link types.
    pub async fn get_link_types(&self) -> Result<Vec<JiraLinkType>> {
        let url = format!("{}/rest/api/3/issueLinkType", self.instance_url);
        let response = self.get_json(&url).await?;
        let resp: JiraLinkTypesResponse = Self::parse_json(
            Self::ensure_success(response).await?,
            "Failed to parse link types response",
        )
        .await?;
        Ok(resp
            .issue_link_types
            .into_iter()
            .map(|t| JiraLinkType {
                id: t.id,
                name: t.name,
                inward: t.inward,
                outward: t.outward,
            })
            .collect())
    }

    /// Creates a link between two JIRA issues.
    pub async fn create_issue_link(
        &self,
        type_name: &str,
        inward_key: &str,
        outward_key: &str,
    ) -> Result<()> {
        let url = format!("{}/rest/api/3/issueLink", self.instance_url);
        let body = serde_json::json!({"type": {"name": type_name}, "inwardIssue": {"key": inward_key}, "outwardIssue": {"key": outward_key}});
        let response = self.post_json(&url, &body).await?;
        Self::ensure_success(response).await?;
        Ok(())
    }

    /// Removes an issue link by ID.
    pub async fn remove_issue_link(&self, link_id: &str) -> Result<()> {
        let url = format!("{}/rest/api/3/issueLink/{}", self.instance_url, link_id);
        let response = self.delete(&url).await?;
        Self::ensure_success(response).await?;
        Ok(())
    }

    /// Sets the parent of a JIRA issue (e.g., links a Story to its Epic, a
    /// Sub-task to its Story, or any issue to a parent of a hierarchy-allowed
    /// type).
    pub async fn set_issue_parent(&self, issue_key: &str, parent_key: &str) -> Result<()> {
        let url = format!("{}/rest/api/3/issue/{}", self.instance_url, issue_key);
        let body = serde_json::json!({"fields": {"parent": {"key": parent_key}}});
        let response = self.put_json(&url, &body).await?;
        Self::ensure_success(response).await?;
        Ok(())
    }

    /// Resolves a JIRA issue key to its numeric ID.
    pub async fn get_issue_id(&self, key: &str) -> Result<String> {
        let url = format!("{}/rest/api/3/issue/{}?fields=", self.instance_url, key);
        let response = self.get_json(&url).await?;
        let resp: JiraIssueIdResponse = Self::parse_json(
            Self::ensure_success(response).await?,
            "Failed to parse issue ID response",
        )
        .await?;
        Ok(resp.id)
    }

    /// Fetches a development status summary (counts per category) for a JIRA issue.
    ///
    /// Uses the DevStatus summary endpoint. Returns counts and providers (each
    /// carrying both the `applicationType` instance-type key and its display
    /// name) for each category (pull requests, branches, repositories).
    pub async fn get_dev_status_summary(&self, key: &str) -> Result<JiraDevStatusSummary> {
        let issue_id = self.get_issue_id(key).await?;
        let url = format!(
            "{}/rest/dev-status/1.0/issue/summary?issueId={}",
            self.instance_url, issue_id
        );
        let response = self.get_json(&url).await?;
        let resp: DevStatusSummaryResponse = Self::parse_json(
            Self::ensure_success(response).await?,
            "Failed to parse DevStatus summary response",
        )
        .await?;

        fn extract_count(cat: Option<DevStatusSummaryCategory>) -> JiraDevStatusCount {
            match cat {
                Some(c) => JiraDevStatusCount {
                    count: c.overall.map_or(0, |o| o.count),
                    // The `byInstanceType` map is keyed by the instance-type
                    // identifier (e.g. "github", "stash", "bitbucket") — this
                    // key, not the human-readable `name` ("Bitbucket Server"),
                    // is what the detail endpoint expects as `applicationType`.
                    // Keep the key as `instance_type` for provider auto-discovery
                    // in `get_dev_status`, and the value's `name` for display,
                    // falling back to the key when the API omits a name.
                    providers: c
                        .by_instance_type
                        .into_iter()
                        .filter(|(k, _)| !k.is_empty())
                        .map(|(k, v)| JiraDevProvider {
                            name: v
                                .get("name")
                                .and_then(|n| n.as_str())
                                .filter(|s| !s.is_empty())
                                .unwrap_or(&k)
                                .to_string(),
                            instance_type: k,
                        })
                        .collect(),
                },
                None => JiraDevStatusCount {
                    count: 0,
                    providers: Vec::new(),
                },
            }
        }

        Ok(JiraDevStatusSummary {
            pullrequest: extract_count(resp.summary.pullrequest),
            branch: extract_count(resp.summary.branch),
            repository: extract_count(resp.summary.repository),
        })
    }

    /// Fetches development status (PRs, branches, repositories) for a JIRA issue.
    ///
    /// Uses the DevStatus API which requires the numeric issue ID. The key is
    /// resolved automatically via [`get_issue_id`](Self::get_issue_id).
    ///
    /// If `application_type` is `None`, discovers available providers via the
    /// summary endpoint and queries each one. If `Some`, queries only that
    /// provider (e.g., "GitHub", "bitbucket", "stash").
    pub async fn get_dev_status(
        &self,
        key: &str,
        data_type: Option<&str>,
        application_type: Option<&str>,
    ) -> Result<JiraDevStatus> {
        let issue_id = self.get_issue_id(key).await?;

        let app_types: Vec<String> = if let Some(app) = application_type {
            vec![app.to_string()]
        } else {
            // Discover available providers via the summary endpoint. The
            // `instance_type` key — not the display name — is what the detail
            // endpoint expects as `applicationType`.
            let summary = self.get_dev_status_summary(key).await?;
            let mut providers: Vec<String> = Vec::new();
            for p in summary
                .pullrequest
                .providers
                .into_iter()
                .chain(summary.branch.providers)
                .chain(summary.repository.providers)
            {
                if !providers.contains(&p.instance_type) {
                    providers.push(p.instance_type);
                }
            }
            if providers.is_empty() {
                providers.push("GitHub".to_string());
            }
            providers
        };

        let data_types: Vec<&str> = match data_type {
            Some(dt) => vec![dt],
            None => vec!["pullrequest", "branch", "repository"],
        };

        let mut status = JiraDevStatus {
            pull_requests: Vec::new(),
            branches: Vec::new(),
            repositories: Vec::new(),
        };

        for app in &app_types {
            for dt in &data_types {
                let url = format!(
                    "{}/rest/dev-status/1.0/issue/detail?issueId={}&applicationType={}&dataType={}",
                    self.instance_url, issue_id, app, dt
                );
                let response = self.get_json(&url).await?;
                let resp: DevStatusResponse = Self::parse_json(
                    Self::ensure_success(response).await?,
                    "Failed to parse DevStatus response",
                )
                .await?;

                for detail in resp.detail {
                    for pr in detail.pull_requests {
                        status.pull_requests.push(JiraDevPullRequest {
                            id: pr.id,
                            name: pr.name,
                            status: pr.status,
                            url: pr.url,
                            repository_name: pr.repository_name,
                            source_branch: pr.source.map(|s| s.branch).unwrap_or_default(),
                            destination_branch: pr
                                .destination
                                .map(|d| d.branch)
                                .unwrap_or_default(),
                            author: pr.author.map(|a| a.name),
                            reviewers: pr.reviewers.into_iter().map(|r| r.name).collect(),
                            comment_count: pr.comment_count,
                            last_update: pr.last_update,
                        });
                    }
                    for branch in detail.branches {
                        status.branches.push(JiraDevBranch {
                            name: branch.name,
                            url: branch.url,
                            repository_name: branch.repository_name,
                            create_pr_url: branch.create_pr_url,
                            last_commit: branch.last_commit.map(Self::convert_commit),
                        });
                    }
                    for repo in detail.repositories {
                        status.repositories.push(JiraDevRepository {
                            name: repo.name,
                            url: repo.url,
                            commits: repo.commits.into_iter().map(Self::convert_commit).collect(),
                        });
                    }
                }
            }
        }

        Ok(status)
    }

    /// Converts an internal `DevStatusCommit` to a public `JiraDevCommit`.
    fn convert_commit(c: DevStatusCommit) -> JiraDevCommit {
        JiraDevCommit {
            id: c.id,
            display_id: c.display_id,
            message: c.message,
            author: c.author.map(|a| a.name),
            timestamp: c.author_timestamp,
            url: c.url,
            file_count: c.file_count,
            merge: c.merge,
        }
    }

    /// Gets attachment metadata for a JIRA issue.
    pub async fn get_attachments(&self, key: &str) -> Result<Vec<JiraAttachment>> {
        let url = format!(
            "{}/rest/api/3/issue/{}?fields=attachment",
            self.instance_url, key
        );

        let response = self.get_json(&url).await?;

        let resp: JiraAttachmentIssueResponse = Self::parse_json(
            Self::ensure_success(response).await?,
            "Failed to parse attachment response",
        )
        .await?;

        Ok(resp
            .fields
            .attachment
            .into_iter()
            .map(JiraAttachment::from)
            .collect())
    }

    /// Uploads one or more files as attachments to a JIRA issue.
    ///
    /// Streams each file body — files are never fully buffered in memory. All
    /// files ride a single multipart POST (JIRA accepts repeated `file` parts),
    /// and the endpoint returns metadata for every created attachment.
    ///
    /// Sends `X-Atlassian-Token: no-check` (Atlassian's XSRF opt-out required
    /// on this endpoint). Does not retry on 429: see
    /// [`AtlassianClient::post_multipart`].
    pub async fn upload_attachments(
        &self,
        key: &str,
        files: &[PathBuf],
    ) -> Result<Vec<JiraAttachment>> {
        let mut form = reqwest::multipart::Form::new();
        for file in files {
            let metadata = tokio::fs::metadata(file)
                .await
                .with_context(|| format!("Failed to read file metadata for {}", file.display()))?;
            let size = metadata.len();
            let handle = tokio::fs::File::open(file)
                .await
                .with_context(|| format!("Failed to open {}", file.display()))?;

            let filename = file
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .ok_or_else(|| {
                    anyhow::anyhow!("File path has no filename component: {}", file.display())
                })?;

            let mime = mime_guess::from_path(file).first_or_octet_stream();
            let body = reqwest::Body::wrap_stream(ReaderStream::new(handle));
            let part = reqwest::multipart::Part::stream_with_length(body, size)
                .file_name(filename)
                .mime_str(mime.essence_str())
                .with_context(|| format!("Invalid MIME type for {}", file.display()))?;
            form = form.part("file", part);
        }

        let url = format!("{}/rest/api/3/issue/{}/attachments", self.instance_url, key);

        let response = self
            .post_multipart(&url, form, &[("X-Atlassian-Token", "no-check")])
            .await?;

        let entries: Vec<JiraAttachmentEntry> = Self::parse_json(
            Self::ensure_success(response).await?,
            "Failed to parse attachment upload response",
        )
        .await?;

        Ok(entries.into_iter().map(JiraAttachment::from).collect())
    }

    /// Deletes a JIRA attachment by ID.
    ///
    /// `DELETE /rest/api/3/attachment/{id}` — permanent (JIRA has no trash).
    pub async fn delete_attachment(&self, attachment_id: &str) -> Result<()> {
        let url = format!(
            "{}/rest/api/3/attachment/{}",
            self.instance_url, attachment_id
        );
        let response = self.delete(&url).await?;
        Self::ensure_success(response).await?;
        Ok(())
    }

    /// Gets the changelog for a JIRA issue with auto-pagination.
    pub async fn get_changelog(&self, key: &str, limit: u32) -> Result<Vec<JiraChangelogEntry>> {
        let effective_limit = if limit == 0 { u32::MAX } else { limit };
        let mut all_entries = Vec::new();
        let mut start_at: u32 = 0;

        loop {
            let remaining = effective_limit.saturating_sub(all_entries.len() as u32);
            if remaining == 0 {
                break;
            }
            let page_size = remaining.min(PAGE_SIZE);

            let url = format!(
                "{}/rest/api/3/issue/{}/changelog?maxResults={}&startAt={}",
                self.instance_url, key, page_size, start_at
            );

            let response = self.get_json(&url).await?;

            let resp: JiraChangelogResponse = Self::parse_json(
                Self::ensure_success(response).await?,
                "Failed to parse changelog response",
            )
            .await?;

            let page_count = resp.values.len() as u32;
            for e in resp.values {
                all_entries.push(JiraChangelogEntry {
                    id: e.id,
                    author: e.author.and_then(|a| a.display_name).unwrap_or_default(),
                    created: e.created.unwrap_or_default(),
                    items: e
                        .items
                        .into_iter()
                        .map(|i| JiraChangelogItem {
                            field: i.field,
                            from_string: i.from_string,
                            to_string: i.to_string,
                        })
                        .collect(),
                });
            }

            if resp.is_last || page_count == 0 {
                break;
            }
            start_at += page_count;
        }

        Ok(all_entries)
    }

    /// Lists all JIRA field definitions.
    pub async fn get_fields(&self) -> Result<Vec<JiraField>> {
        let url = format!("{}/rest/api/3/field", self.instance_url);

        let response = self.get_json(&url).await?;

        let entries: Vec<JiraFieldEntry> = Self::parse_json(
            Self::ensure_success(response).await?,
            "Failed to parse field list response",
        )
        .await?;

        Ok(entries
            .into_iter()
            .map(|f| {
                let (raw_type, raw_custom) = match f.schema {
                    Some(s) => (s.schema_type, s.custom),
                    None => (None, None),
                };
                JiraField {
                    id: f.id,
                    name: f.name,
                    custom: f.custom,
                    schema_type: map_schema_type(raw_type, raw_custom.as_deref()),
                    schema_custom: raw_custom,
                }
            })
            .collect())
    }

    /// Lists options for a JIRA custom field.
    /// Lists contexts for a JIRA custom field.
    pub async fn get_field_contexts(&self, field_id: &str) -> Result<Vec<String>> {
        let url = format!(
            "{}/rest/api/3/field/{}/context",
            self.instance_url, field_id
        );

        let response = self.get_json(&url).await?;

        let resp: JiraFieldContextsResponse = Self::parse_json(
            Self::ensure_success(response).await?,
            "Failed to parse field contexts response",
        )
        .await?;

        Ok(resp.values.into_iter().map(|c| c.id).collect())
    }

    /// Lists options for a JIRA custom field.
    ///
    /// When `context_id` is `None`, auto-discovers the first context for the field.
    pub async fn get_field_options(
        &self,
        field_id: &str,
        context_id: Option<&str>,
    ) -> Result<Vec<JiraFieldOption>> {
        let ctx = if let Some(id) = context_id {
            id.to_string()
        } else {
            let contexts = self.get_field_contexts(field_id).await?;
            contexts.into_iter().next().ok_or_else(|| {
                anyhow::anyhow!(
                    "No contexts found for field \"{field_id}\". \
                     Use --context-id to specify one explicitly."
                )
            })?
        };

        let url = format!(
            "{}/rest/api/3/field/{}/context/{}/option",
            self.instance_url, field_id, ctx
        );

        let response = self.get_json(&url).await?;

        let resp: JiraFieldOptionsResponse = Self::parse_json(
            Self::ensure_success(response).await?,
            "Failed to parse field options response",
        )
        .await?;

        Ok(resp
            .values
            .into_iter()
            .map(|o| JiraFieldOption {
                id: o.id,
                value: o.value,
            })
            .collect())
    }

    /// Lists JIRA projects.
    pub async fn get_projects(&self, limit: u32) -> Result<JiraProjectList> {
        let effective_limit = if limit == 0 { u32::MAX } else { limit };
        let mut all_projects = Vec::new();
        let mut start_at: u32 = 0;

        loop {
            let remaining = effective_limit.saturating_sub(all_projects.len() as u32);
            if remaining == 0 {
                break;
            }
            let page_size = remaining.min(PAGE_SIZE);

            let url = format!(
                "{}/rest/api/3/project/search?maxResults={}&startAt={}",
                self.instance_url, page_size, start_at
            );

            let response = self.get_json(&url).await?;

            let resp: JiraProjectSearchResponse = Self::parse_json(
                Self::ensure_success(response).await?,
                "Failed to parse project search response",
            )
            .await?;

            let page_count = resp.values.len() as u32;
            for p in resp.values {
                all_projects.push(JiraProject {
                    id: p.id,
                    key: p.key,
                    name: p.name,
                    project_type: p.project_type_key,
                    lead: p.lead.and_then(|l| l.display_name),
                });
            }

            if resp.is_last || page_count == 0 {
                break;
            }
            start_at += page_count;
        }

        let total = all_projects.len() as u32;
        Ok(JiraProjectList {
            projects: all_projects,
            total,
        })
    }

    /// Deletes a JIRA issue.
    pub async fn delete_issue(&self, key: &str) -> Result<()> {
        let url = format!("{}/rest/api/3/issue/{}", self.instance_url, key);

        let response = self.delete(&url).await?;

        Self::ensure_success(response).await?;

        Ok(())
    }

    /// Lists watchers on a JIRA issue.
    pub async fn get_watchers(&self, key: &str) -> Result<JiraWatcherList> {
        let url = format!("{}/rest/api/3/issue/{}/watchers", self.instance_url, key);

        let response = self.get_json(&url).await?;

        let json: serde_json::Value = Self::parse_json(
            Self::ensure_success(response).await?,
            "Failed to parse watchers response",
        )
        .await?;

        let watch_count = json["watchCount"].as_u64().unwrap_or(0) as u32;

        let watchers = json["watchers"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| serde_json::from_value::<JiraUser>(v.clone()).ok())
                    .collect()
            })
            .unwrap_or_default();

        Ok(JiraWatcherList {
            watchers,
            watch_count,
        })
    }

    /// Adds a user as a watcher on a JIRA issue.
    pub async fn add_watcher(&self, key: &str, account_id: &str) -> Result<()> {
        let url = format!("{}/rest/api/3/issue/{}/watchers", self.instance_url, key);

        let body = serde_json::json!(account_id);

        let response = self.post_json(&url, &body).await?;

        Self::ensure_success(response).await?;

        Ok(())
    }

    /// Removes a user from watchers on a JIRA issue.
    pub async fn remove_watcher(&self, key: &str, account_id: &str) -> Result<()> {
        let url = format!(
            "{}/rest/api/3/issue/{}/watchers?accountId={}",
            self.instance_url, key, account_id
        );

        let response = self.delete(&url).await?;

        Self::ensure_success(response).await?;

        Ok(())
    }

    /// Verifies authentication by fetching the current user.
    pub async fn get_myself(&self) -> Result<JiraUser> {
        let url = format!("{}/rest/api/3/myself", self.instance_url);

        let response = self
            .client
            .get(&url)
            .header("Authorization", &self.auth_header)
            .header("Accept", "application/json")
            .send()
            .await
            .context("Failed to send request to JIRA API")?;

        let response = Self::ensure_success(response).await?;

        response
            .json()
            .await
            .context("Failed to parse user response")
    }
}
