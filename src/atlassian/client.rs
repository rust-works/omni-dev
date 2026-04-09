//! Atlassian Cloud REST API client.
//!
//! Provides HTTP access to JIRA Cloud REST API v3 for reading and
//! writing issues. Uses Basic Auth (email + API token).

use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine;
use reqwest::Client;
use serde::Deserialize;

use crate::atlassian::adf::AdfDocument;
use crate::atlassian::error::AtlassianError;

/// HTTP request timeout for Atlassian API calls.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// HTTP client for Atlassian Cloud REST APIs.
pub struct AtlassianClient {
    client: Client,
    instance_url: String,
    auth_header: String,
}

/// JIRA issue data returned from the REST API.
#[derive(Debug, Clone)]
pub struct JiraIssue {
    /// Issue key (e.g., "PROJ-123").
    pub key: String,

    /// Issue summary (title).
    pub summary: String,

    /// Issue description as raw ADF JSON (may be null).
    pub description_adf: Option<serde_json::Value>,

    /// Issue status name.
    pub status: Option<String>,

    /// Issue type name.
    pub issue_type: Option<String>,

    /// Assignee display name.
    pub assignee: Option<String>,

    /// Priority name.
    pub priority: Option<String>,

    /// Labels.
    pub labels: Vec<String>,
}

/// Response from the JIRA `/myself` endpoint.
#[derive(Debug, Deserialize)]
pub struct JiraUser {
    /// User display name.
    #[serde(rename = "displayName")]
    pub display_name: String,

    /// User email address.
    #[serde(rename = "emailAddress")]
    pub email_address: Option<String>,

    /// Account ID.
    #[serde(rename = "accountId")]
    pub account_id: String,
}

/// Result from creating a JIRA issue via the REST API.
#[derive(Debug, Clone)]
pub struct JiraCreatedIssue {
    /// Issue key (e.g., "PROJ-124").
    pub key: String,
    /// Issue numeric ID.
    pub id: String,
    /// API self URL.
    pub self_url: String,
}

/// Result from a JIRA JQL search.
#[derive(Debug, Clone)]
pub struct JiraSearchResult {
    /// Matching issues.
    pub issues: Vec<JiraIssue>,

    /// Total number of matching issues (may exceed `issues.len()` if paginated).
    pub total: u32,
}

/// A Confluence search result.
#[derive(Debug, Clone)]
pub struct ConfluenceSearchResult {
    /// Page ID.
    pub id: String,
    /// Page title.
    pub title: String,
    /// Space key (e.g., "ENG").
    pub space_key: String,
}

/// Result from a Confluence CQL search.
#[derive(Debug, Clone)]
pub struct ConfluenceSearchResults {
    /// Matching pages.
    pub results: Vec<ConfluenceSearchResult>,
    /// Total number of matching results.
    pub total: u32,
}

/// A JIRA issue comment.
#[derive(Debug, Clone)]
pub struct JiraComment {
    /// Comment ID.
    pub id: String,
    /// Author display name.
    pub author: String,
    /// Comment body as raw ADF JSON.
    pub body_adf: Option<serde_json::Value>,
    /// ISO 8601 creation timestamp.
    pub created: String,
}

/// A JIRA project.
#[derive(Debug, Clone)]
pub struct JiraProject {
    /// Project ID.
    pub id: String,
    /// Project key (e.g., "PROJ").
    pub key: String,
    /// Project name.
    pub name: String,
    /// Project type key (e.g., "software", "business").
    pub project_type: Option<String>,
    /// Project lead display name.
    pub lead: Option<String>,
}

/// Result from listing JIRA projects.
#[derive(Debug, Clone)]
pub struct JiraProjectList {
    /// Projects returned.
    pub projects: Vec<JiraProject>,
    /// Total number of projects.
    pub total: u32,
}

/// A JIRA field definition.
#[derive(Debug, Clone)]
pub struct JiraField {
    /// Field ID (e.g., "summary", "customfield_10001").
    pub id: String,
    /// Human-readable field name.
    pub name: String,
    /// Whether this is a custom field.
    pub custom: bool,
    /// Schema type (e.g., "string", "array", "option").
    pub schema_type: Option<String>,
}

/// An option value for a JIRA custom field.
#[derive(Debug, Clone)]
pub struct JiraFieldOption {
    /// Option ID.
    pub id: String,
    /// Option display value.
    pub value: String,
}

/// A JIRA workflow transition.
#[derive(Debug, Clone)]
pub struct JiraTransition {
    /// Transition ID.
    pub id: String,
    /// Transition name (e.g., "In Progress", "Done").
    pub name: String,
}

// ── Internal API response structs ───────────────────────────────────

#[derive(Deserialize)]
struct JiraIssueResponse {
    key: String,
    fields: JiraIssueFields,
}

#[derive(Deserialize)]
struct JiraIssueFields {
    summary: Option<String>,
    description: Option<serde_json::Value>,
    status: Option<JiraNameField>,
    issuetype: Option<JiraNameField>,
    assignee: Option<JiraAssigneeField>,
    priority: Option<JiraNameField>,
    #[serde(default)]
    labels: Vec<String>,
}

#[derive(Deserialize)]
struct JiraNameField {
    name: Option<String>,
}

#[derive(Deserialize)]
struct JiraAssigneeField {
    #[serde(rename = "displayName")]
    display_name: Option<String>,
}

#[derive(Deserialize)]
struct JiraSearchResponse {
    issues: Vec<JiraIssueResponse>,
    total: u32,
}

#[derive(Deserialize)]
struct JiraTransitionsResponse {
    transitions: Vec<JiraTransitionEntry>,
}

#[derive(Deserialize)]
struct JiraTransitionEntry {
    id: String,
    name: String,
}

#[derive(Deserialize)]
struct JiraCommentsResponse {
    comments: Vec<JiraCommentEntry>,
}

#[derive(Deserialize)]
struct JiraCommentEntry {
    id: String,
    author: Option<JiraCommentAuthor>,
    body: Option<serde_json::Value>,
    created: Option<String>,
}

#[derive(Deserialize)]
struct JiraCommentAuthor {
    #[serde(rename = "displayName")]
    display_name: Option<String>,
}

#[derive(Deserialize)]
struct ConfluenceContentSearchResponse {
    results: Vec<ConfluenceContentSearchEntry>,
    #[serde(default)]
    size: u32,
}

#[derive(Deserialize)]
struct ConfluenceContentSearchEntry {
    id: String,
    title: String,
    #[serde(rename = "_expandable")]
    expandable: Option<ConfluenceExpandable>,
}

#[derive(Deserialize)]
struct ConfluenceExpandable {
    space: Option<String>,
}

#[derive(Deserialize)]
struct JiraFieldEntry {
    id: String,
    name: String,
    #[serde(default)]
    custom: bool,
    schema: Option<JiraFieldSchema>,
}

#[derive(Deserialize)]
struct JiraFieldSchema {
    #[serde(rename = "type")]
    schema_type: Option<String>,
}

#[derive(Deserialize)]
struct JiraFieldOptionsResponse {
    values: Vec<JiraFieldOptionEntry>,
}

#[derive(Deserialize)]
struct JiraFieldOptionEntry {
    id: String,
    value: String,
}

#[derive(Deserialize)]
struct JiraProjectSearchResponse {
    values: Vec<JiraProjectEntry>,
    total: u32,
}

#[derive(Deserialize)]
struct JiraProjectEntry {
    id: String,
    key: String,
    name: String,
    #[serde(rename = "projectTypeKey")]
    project_type_key: Option<String>,
    lead: Option<JiraProjectLead>,
}

#[derive(Deserialize)]
struct JiraProjectLead {
    #[serde(rename = "displayName")]
    display_name: Option<String>,
}

#[derive(Deserialize)]
struct JiraCreateResponse {
    key: String,
    id: String,
    #[serde(rename = "self")]
    self_url: String,
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

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
            api_token: "token123".to_string(),
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
    async fn update_issue_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-42"))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let adf = AdfDocument::new();
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
        let adf = AdfDocument::new();
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
        let adf = AdfDocument::new();
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
            .and(wiremock::matchers::path("/rest/api/3/search"))
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
    async fn search_issues_empty_results() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/search"))
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
            .and(wiremock::matchers::path("/rest/api/3/search"))
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
        let adf = AdfDocument::new();
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
    async fn get_comments_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1/comment"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
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
        let comments = client.get_comments("PROJ-1").await.unwrap();

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
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"comments": []})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let comments = client.get_comments("PROJ-1").await.unwrap();
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
        let err = client.get_comments("NOPE-1").await.unwrap_err();
        assert!(err.to_string().contains("404"));
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
        let adf = AdfDocument::new();
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
        let adf = AdfDocument::new();
        let err = client.add_comment("PROJ-1", &adf).await.unwrap_err();
        assert!(err.to_string().contains("403"));
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

    #[tokio::test]
    async fn get_fields_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/field"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!([
                    {"id": "summary", "name": "Summary", "custom": false, "schema": {"type": "string"}},
                    {"id": "customfield_10001", "name": "Story Points", "custom": true, "schema": {"type": "number"}},
                    {"id": "labels", "name": "Labels", "custom": false}
                ]),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let fields = client.get_fields().await.unwrap();

        assert_eq!(fields.len(), 3);
        assert_eq!(fields[0].id, "summary");
        assert_eq!(fields[0].name, "Summary");
        assert!(!fields[0].custom);
        assert_eq!(fields[0].schema_type.as_deref(), Some("string"));
        assert_eq!(fields[1].id, "customfield_10001");
        assert!(fields[1].custom);
        assert!(fields[2].schema_type.is_none());
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
    async fn get_field_options_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/field/customfield_10001/context/default/option",
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
            .get_field_options("customfield_10001", None)
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
                "/rest/api/3/field/nonexistent/context/default/option",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let err = client
            .get_field_options("nonexistent", None)
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
                    "total": 2
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
        Self::new(&creds.instance_url, &creds.email, &creds.api_token)
    }

    /// Returns the instance URL.
    #[must_use]
    pub fn instance_url(&self) -> &str {
        &self.instance_url
    }

    /// Sends an authenticated GET request and returns the raw response.
    ///
    /// Shared transport method used by both JIRA and Confluence API
    /// implementations.
    pub async fn get_json(&self, url: &str) -> Result<reqwest::Response> {
        self.client
            .get(url)
            .header("Authorization", &self.auth_header)
            .header("Accept", "application/json")
            .send()
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
        self.client
            .put(url)
            .header("Authorization", &self.auth_header)
            .header("Content-Type", "application/json")
            .json(body)
            .send()
            .await
            .context("Failed to send PUT request to Atlassian API")
    }

    /// Sends an authenticated POST request with a JSON body and returns the raw response.
    pub async fn post_json<T: serde::Serialize + Sync + ?Sized>(
        &self,
        url: &str,
        body: &T,
    ) -> Result<reqwest::Response> {
        self.client
            .post(url)
            .header("Authorization", &self.auth_header)
            .header("Content-Type", "application/json")
            .json(body)
            .send()
            .await
            .context("Failed to send POST request to Atlassian API")
    }

    /// Sends an authenticated DELETE request and returns the raw response.
    pub async fn delete(&self, url: &str) -> Result<reqwest::Response> {
        self.client
            .delete(url)
            .header("Authorization", &self.auth_header)
            .send()
            .await
            .context("Failed to send DELETE request to Atlassian API")
    }

    /// Fetches a JIRA issue by key.
    pub async fn get_issue(&self, key: &str) -> Result<JiraIssue> {
        let url = format!(
            "{}/rest/api/3/issue/{}?fields=summary,description,status,issuetype,assignee,priority,labels",
            self.instance_url, key
        );

        let response = self
            .client
            .get(&url)
            .header("Authorization", &self.auth_header)
            .header("Accept", "application/json")
            .send()
            .await
            .context("Failed to send request to JIRA API")?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(AtlassianError::ApiRequestFailed { status, body }.into());
        }

        let issue_response: JiraIssueResponse = response
            .json()
            .await
            .context("Failed to parse JIRA issue response")?;

        Ok(JiraIssue {
            key: issue_response.key,
            summary: issue_response.fields.summary.unwrap_or_default(),
            description_adf: issue_response.fields.description,
            status: issue_response.fields.status.and_then(|s| s.name),
            issue_type: issue_response.fields.issuetype.and_then(|t| t.name),
            assignee: issue_response.fields.assignee.and_then(|a| a.display_name),
            priority: issue_response.fields.priority.and_then(|p| p.name),
            labels: issue_response.fields.labels,
        })
    }

    /// Updates a JIRA issue's description and optionally its summary.
    pub async fn update_issue(
        &self,
        key: &str,
        description_adf: &AdfDocument,
        summary: Option<&str>,
    ) -> Result<()> {
        let url = format!("{}/rest/api/3/issue/{}", self.instance_url, key);

        let mut fields = serde_json::Map::new();
        fields.insert(
            "description".to_string(),
            serde_json::to_value(description_adf).context("Failed to serialize ADF document")?,
        );
        if let Some(summary_text) = summary {
            fields.insert(
                "summary".to_string(),
                serde_json::Value::String(summary_text.to_string()),
            );
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
            return Err(AtlassianError::ApiRequestFailed { status, body }.into());
        }

        Ok(())
    }

    /// Creates a new JIRA issue.
    pub async fn create_issue(
        &self,
        project_key: &str,
        issue_type: &str,
        summary: &str,
        description_adf: Option<&AdfDocument>,
        labels: &[String],
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

        let body = serde_json::json!({ "fields": fields });

        let response = self
            .post_json(&url, &body)
            .await
            .context("Failed to send create request to JIRA API")?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(AtlassianError::ApiRequestFailed { status, body }.into());
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

    /// Lists comments on a JIRA issue.
    pub async fn get_comments(&self, key: &str) -> Result<Vec<JiraComment>> {
        let url = format!(
            "{}/rest/api/3/issue/{}/comment?orderBy=created",
            self.instance_url, key
        );

        let response = self.get_json(&url).await?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(AtlassianError::ApiRequestFailed { status, body }.into());
        }

        let resp: JiraCommentsResponse = response
            .json()
            .await
            .context("Failed to parse comments response")?;

        Ok(resp
            .comments
            .into_iter()
            .map(|c| JiraComment {
                id: c.id,
                author: c.author.and_then(|a| a.display_name).unwrap_or_default(),
                body_adf: c.body,
                created: c.created.unwrap_or_default(),
            })
            .collect())
    }

    /// Adds a comment to a JIRA issue.
    pub async fn add_comment(&self, key: &str, body_adf: &AdfDocument) -> Result<()> {
        let url = format!("{}/rest/api/3/issue/{}/comment", self.instance_url, key);

        let body = serde_json::json!({
            "body": body_adf
        });

        let response = self.post_json(&url, &body).await?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(AtlassianError::ApiRequestFailed { status, body }.into());
        }

        Ok(())
    }

    /// Lists available transitions for a JIRA issue.
    pub async fn get_transitions(&self, key: &str) -> Result<Vec<JiraTransition>> {
        let url = format!("{}/rest/api/3/issue/{}/transitions", self.instance_url, key);

        let response = self.get_json(&url).await?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(AtlassianError::ApiRequestFailed { status, body }.into());
        }

        let resp: JiraTransitionsResponse = response
            .json()
            .await
            .context("Failed to parse transitions response")?;

        Ok(resp
            .transitions
            .into_iter()
            .map(|t| JiraTransition {
                id: t.id,
                name: t.name,
            })
            .collect())
    }

    /// Executes a transition on a JIRA issue.
    pub async fn do_transition(&self, key: &str, transition_id: &str) -> Result<()> {
        let url = format!("{}/rest/api/3/issue/{}/transitions", self.instance_url, key);

        let body = serde_json::json!({
            "transition": { "id": transition_id }
        });

        let response = self.post_json(&url, &body).await?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(AtlassianError::ApiRequestFailed { status, body }.into());
        }

        Ok(())
    }

    /// Searches JIRA issues using JQL.
    pub async fn search_issues(&self, jql: &str, max_results: u32) -> Result<JiraSearchResult> {
        let url = format!("{}/rest/api/3/search", self.instance_url);

        let body = serde_json::json!({
            "jql": jql,
            "maxResults": max_results,
            "fields": ["summary", "status", "issuetype", "assignee", "priority"]
        });

        let response = self
            .post_json(&url, &body)
            .await
            .context("Failed to send search request to JIRA API")?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(AtlassianError::ApiRequestFailed { status, body }.into());
        }

        let search_response: JiraSearchResponse = response
            .json()
            .await
            .context("Failed to parse JIRA search response")?;

        let issues = search_response
            .issues
            .into_iter()
            .map(|r| JiraIssue {
                key: r.key,
                summary: r.fields.summary.unwrap_or_default(),
                description_adf: r.fields.description,
                status: r.fields.status.and_then(|s| s.name),
                issue_type: r.fields.issuetype.and_then(|t| t.name),
                assignee: r.fields.assignee.and_then(|a| a.display_name),
                priority: r.fields.priority.and_then(|p| p.name),
                labels: r.fields.labels,
            })
            .collect();

        Ok(JiraSearchResult {
            issues,
            total: search_response.total,
        })
    }

    /// Searches Confluence pages using CQL.
    pub async fn search_confluence(
        &self,
        cql: &str,
        limit: u32,
    ) -> Result<ConfluenceSearchResults> {
        let base = format!("{}/wiki/rest/api/content/search", self.instance_url);
        let url = reqwest::Url::parse_with_params(
            &base,
            &[
                ("cql", cql),
                ("limit", &limit.to_string()),
                ("expand", "space"),
            ],
        )
        .context("Failed to build Confluence search URL")?;

        let response = self.get_json(url.as_str()).await?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(AtlassianError::ApiRequestFailed { status, body }.into());
        }

        let resp: ConfluenceContentSearchResponse = response
            .json()
            .await
            .context("Failed to parse Confluence search response")?;

        let results = resp
            .results
            .into_iter()
            .map(|r| {
                let space_key = r
                    .expandable
                    .and_then(|e| e.space)
                    .and_then(|s| s.rsplit('/').next().map(String::from))
                    .unwrap_or_default();
                ConfluenceSearchResult {
                    id: r.id,
                    title: r.title,
                    space_key,
                }
            })
            .collect();

        Ok(ConfluenceSearchResults {
            results,
            total: resp.size,
        })
    }

    /// Lists all JIRA field definitions.
    pub async fn get_fields(&self) -> Result<Vec<JiraField>> {
        let url = format!("{}/rest/api/3/field", self.instance_url);

        let response = self.get_json(&url).await?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(AtlassianError::ApiRequestFailed { status, body }.into());
        }

        let entries: Vec<JiraFieldEntry> = response
            .json()
            .await
            .context("Failed to parse field list response")?;

        Ok(entries
            .into_iter()
            .map(|f| JiraField {
                id: f.id,
                name: f.name,
                custom: f.custom,
                schema_type: f.schema.and_then(|s| s.schema_type),
            })
            .collect())
    }

    /// Lists options for a JIRA custom field.
    pub async fn get_field_options(
        &self,
        field_id: &str,
        context_id: Option<&str>,
    ) -> Result<Vec<JiraFieldOption>> {
        let ctx = context_id.unwrap_or("default");
        let url = format!(
            "{}/rest/api/3/field/{}/context/{}/option",
            self.instance_url, field_id, ctx
        );

        let response = self.get_json(&url).await?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(AtlassianError::ApiRequestFailed { status, body }.into());
        }

        let resp: JiraFieldOptionsResponse = response
            .json()
            .await
            .context("Failed to parse field options response")?;

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
    pub async fn get_projects(&self, max_results: u32) -> Result<JiraProjectList> {
        let url = format!(
            "{}/rest/api/3/project/search?maxResults={}",
            self.instance_url, max_results
        );

        let response = self.get_json(&url).await?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(AtlassianError::ApiRequestFailed { status, body }.into());
        }

        let resp: JiraProjectSearchResponse = response
            .json()
            .await
            .context("Failed to parse project search response")?;

        let projects = resp
            .values
            .into_iter()
            .map(|p| JiraProject {
                id: p.id,
                key: p.key,
                name: p.name,
                project_type: p.project_type_key,
                lead: p.lead.and_then(|l| l.display_name),
            })
            .collect();

        Ok(JiraProjectList {
            projects,
            total: resp.total,
        })
    }

    /// Deletes a JIRA issue.
    pub async fn delete_issue(&self, key: &str) -> Result<()> {
        let url = format!("{}/rest/api/3/issue/{}", self.instance_url, key);

        let response = self.delete(&url).await?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(AtlassianError::ApiRequestFailed { status, body }.into());
        }

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

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(AtlassianError::ApiRequestFailed { status, body }.into());
        }

        response
            .json()
            .await
            .context("Failed to parse user response")
    }
}
