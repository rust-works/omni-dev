//! JIRA Cloud REST API v3 implementation of [`AtlassianApi`].

use std::future::Future;
use std::pin::Pin;

use anyhow::{Context, Result};

use crate::atlassian::adf::AdfDocument;
use crate::atlassian::api::{AtlassianApi, ContentItem, ContentMetadata};
use crate::atlassian::client::AtlassianClient;

/// JIRA Cloud REST API v3 backend.
pub struct JiraApi {
    client: AtlassianClient,
}

impl JiraApi {
    /// Creates a new JIRA API backend.
    pub fn new(client: AtlassianClient) -> Self {
        Self { client }
    }

    /// Returns the underlying HTTP client's instance URL.
    pub fn instance_url(&self) -> &str {
        self.client.instance_url()
    }
}

impl AtlassianApi for JiraApi {
    fn get_content<'a>(
        &'a self,
        id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<ContentItem>> + Send + 'a>> {
        Box::pin(async move {
            let issue = self.client.get_issue(id).await?;
            Ok(ContentItem {
                id: issue.key,
                title: issue.summary,
                body_adf: issue.description_adf,
                metadata: ContentMetadata::Jira {
                    status: issue.status,
                    issue_type: issue.issue_type,
                    assignee: issue.assignee,
                    priority: issue.priority,
                    labels: issue.labels,
                },
            })
        })
    }

    fn update_content<'a>(
        &'a self,
        id: &'a str,
        body_adf: &'a AdfDocument,
        title: Option<&'a str>,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            self.client
                .update_issue(id, body_adf, title)
                .await
                .context("Failed to update JIRA issue")
        })
    }

    fn verify_auth<'a>(&'a self) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
        Box::pin(async move {
            let user = self.client.get_myself().await?;
            Ok(user.display_name)
        })
    }

    fn backend_name(&self) -> &'static str {
        "jira"
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn jira_api_backend_name() {
        let client =
            AtlassianClient::new("https://org.atlassian.net", "user@test.com", "token").unwrap();
        let api = JiraApi::new(client);
        assert_eq!(api.backend_name(), "jira");
    }

    #[test]
    fn jira_api_instance_url() {
        let client =
            AtlassianClient::new("https://org.atlassian.net", "user@test.com", "token").unwrap();
        let api = JiraApi::new(client);
        assert_eq!(api.instance_url(), "https://org.atlassian.net");
    }

    /// Helper: stand up a wiremock server with a JIRA issue endpoint.
    async fn setup_jira_mock() -> (wiremock::MockServer, JiraApi) {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-42"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
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
                    "labels": ["backend"]
                }
            })))
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = JiraApi::new(client);

        (server, api)
    }

    #[tokio::test]
    async fn get_content_success() {
        use crate::atlassian::api::{AtlassianApi, ContentMetadata};

        let (_server, api) = setup_jira_mock().await;
        let item = api.get_content("PROJ-42").await.unwrap();

        assert_eq!(item.id, "PROJ-42");
        assert_eq!(item.title, "Fix the bug");
        assert!(item.body_adf.is_some());
        match &item.metadata {
            ContentMetadata::Jira {
                status,
                issue_type,
                assignee,
                priority,
                labels,
            } => {
                assert_eq!(status.as_deref(), Some("Open"));
                assert_eq!(issue_type.as_deref(), Some("Bug"));
                assert_eq!(assignee.as_deref(), Some("Alice"));
                assert_eq!(priority.as_deref(), Some("High"));
                assert_eq!(labels, &["backend"]);
            }
            ContentMetadata::Confluence { .. } => panic!("Expected Jira metadata"),
        }
    }

    #[tokio::test]
    async fn update_content_success() {
        use crate::atlassian::api::AtlassianApi;

        let (server, api) = setup_jira_mock().await;

        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-42"))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let adf = crate::atlassian::adf::AdfDocument::new();
        let result = api.update_content("PROJ-42", &adf, Some("New Title")).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn verify_auth_success() {
        use crate::atlassian::api::AtlassianApi;

        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/myself"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "displayName": "Bob",
                    "accountId": "xyz789"
                })),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = JiraApi::new(client);
        let name = api.verify_auth().await.unwrap();
        assert_eq!(name, "Bob");
    }
}
