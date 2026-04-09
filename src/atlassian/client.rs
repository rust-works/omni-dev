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
