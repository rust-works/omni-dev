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
