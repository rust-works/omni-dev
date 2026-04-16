//! Confluence Cloud REST API v2 implementation of [`AtlassianApi`].
//!
//! Uses the Confluence REST API v2 to read and write pages.
//! Pages are fetched with ADF body format and updated with version
//! number increments for optimistic locking.

use std::future::Future;
use std::pin::Pin;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::atlassian::adf::AdfDocument;
use crate::atlassian::api::{AtlassianApi, ContentItem, ContentMetadata};
use crate::atlassian::client::AtlassianClient;
use crate::atlassian::error::AtlassianError;

/// Confluence Cloud REST API v2 backend.
pub struct ConfluenceApi {
    client: AtlassianClient,
}

impl ConfluenceApi {
    /// Creates a new Confluence API backend.
    pub fn new(client: AtlassianClient) -> Self {
        Self { client }
    }
}

// ── Internal API response structs ───────────────────────────────────

#[derive(Deserialize)]
struct ConfluencePageResponse {
    id: String,
    title: String,
    status: String,
    #[serde(rename = "spaceId")]
    space_id: String,
    version: Option<ConfluenceVersion>,
    body: Option<ConfluenceBody>,
    #[serde(rename = "parentId")]
    parent_id: Option<String>,
}

#[derive(Deserialize)]
struct ConfluenceVersion {
    number: u32,
}

#[derive(Deserialize)]
struct ConfluenceBody {
    atlas_doc_format: Option<ConfluenceAtlasDoc>,
}

#[derive(Deserialize)]
struct ConfluenceAtlasDoc {
    value: String,
}

// ── Space lookup ────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ConfluenceSpaceResponse {
    key: String,
}

#[derive(Deserialize)]
struct ConfluenceSpacesSearchResponse {
    results: Vec<ConfluenceSpaceSearchEntry>,
}

#[derive(Deserialize)]
struct ConfluenceSpaceSearchEntry {
    id: String,
}

// ── Children response ──────────────────────────────────────────────

#[derive(Deserialize)]
struct ConfluenceChildrenResponse {
    results: Vec<ConfluenceChildEntry>,
    #[serde(rename = "_links", default)]
    links: Option<ConfluenceChildrenLinks>,
}

#[derive(Deserialize)]
struct ConfluenceChildEntry {
    id: String,
    title: String,
}

#[derive(Deserialize)]
struct ConfluenceChildrenLinks {
    next: Option<String>,
}

/// A child page returned from the children API.
#[derive(Debug, Clone)]
pub struct ChildPage {
    /// Page ID.
    pub id: String,
    /// Page title.
    pub title: String,
}

// ── Comment types ─────────────────────────────────────────────────

/// A comment on a Confluence page.
#[derive(Debug, Clone, Serialize)]
pub struct ConfluenceComment {
    /// Comment ID.
    pub id: String,
    /// Author display name.
    pub author: String,
    /// Comment body as raw ADF JSON.
    pub body_adf: Option<serde_json::Value>,
    /// ISO 8601 creation timestamp.
    pub created: String,
}

#[derive(Deserialize)]
struct ConfluenceCommentsResponse {
    results: Vec<ConfluenceCommentEntry>,
}

#[derive(Deserialize)]
struct ConfluenceCommentEntry {
    id: String,
    #[serde(default)]
    version: Option<ConfluenceCommentVersion>,
    #[serde(default)]
    body: Option<ConfluenceCommentBody>,
}

#[derive(Deserialize)]
struct ConfluenceCommentVersion {
    #[serde(rename = "authorId", default)]
    author_id: Option<String>,
    #[serde(rename = "createdAt", default)]
    created_at: Option<String>,
}

#[derive(Deserialize)]
struct ConfluenceCommentBody {
    atlas_doc_format: Option<ConfluenceAtlasDoc>,
}

#[derive(Serialize)]
struct ConfluenceAddCommentRequest {
    #[serde(rename = "pageId")]
    page_id: String,
    body: ConfluenceUpdateBody,
}

// ── Create request ─────────────────────────────────────────────────

#[derive(Serialize)]
struct ConfluenceCreateRequest {
    #[serde(rename = "spaceId")]
    space_id: String,
    title: String,
    body: ConfluenceUpdateBody,
    #[serde(rename = "parentId", skip_serializing_if = "Option::is_none")]
    parent_id: Option<String>,
    status: String,
}

#[derive(Deserialize)]
struct ConfluenceCreateResponse {
    id: String,
}

// ── Update request ──────────────────────────────────────────────────

#[derive(Serialize)]
struct ConfluenceUpdateRequest {
    id: String,
    status: String,
    title: String,
    body: ConfluenceUpdateBody,
    version: ConfluenceUpdateVersion,
}

#[derive(Serialize)]
struct ConfluenceUpdateBody {
    representation: String,
    value: String,
}

#[derive(Serialize)]
struct ConfluenceUpdateVersion {
    number: u32,
    message: Option<String>,
}

impl AtlassianApi for ConfluenceApi {
    fn get_content<'a>(
        &'a self,
        id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<ContentItem>> + Send + 'a>> {
        Box::pin(async move {
            let url = format!(
                "{}/wiki/api/v2/pages/{}?body-format=atlas_doc_format",
                self.client.instance_url(),
                id
            );

            let response = self
                .client
                .get_json(&url)
                .await
                .context("Failed to fetch Confluence page")?;

            if !response.status().is_success() {
                let status = response.status().as_u16();
                let body = response.text().await.unwrap_or_default();
                return Err(AtlassianError::ApiRequestFailed { status, body }.into());
            }

            let page: ConfluencePageResponse = response
                .json()
                .await
                .context("Failed to parse Confluence page response")?;

            debug!(
                page_id = page.id,
                title = page.title,
                "Fetched Confluence page"
            );

            // Confluence returns ADF as a JSON string — parse it to a Value.
            let body_adf = if let Some(body) = &page.body {
                if let Some(atlas_doc) = &body.atlas_doc_format {
                    if tracing::enabled!(tracing::Level::TRACE) {
                        if let Ok(pretty) =
                            serde_json::from_str::<serde_json::Value>(&atlas_doc.value)
                                .and_then(|v| serde_json::to_string_pretty(&v))
                        {
                            tracing::trace!("Original ADF from Confluence:\n{pretty}");
                        }
                    }
                    Some(
                        serde_json::from_str(&atlas_doc.value)
                            .context("Failed to parse ADF from Confluence body")?,
                    )
                } else {
                    None
                }
            } else {
                None
            };

            // Resolve space key from space ID.
            let space_key = self.resolve_space_key(&page.space_id).await?;

            Ok(ContentItem {
                id: page.id,
                title: page.title,
                body_adf,
                metadata: ContentMetadata::Confluence {
                    space_key,
                    status: Some(page.status),
                    version: page.version.map(|v| v.number),
                    parent_id: page.parent_id,
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
            // Fetch current page to get version number and title.
            let current = self.get_content(id).await?;
            let current_version = match &current.metadata {
                ContentMetadata::Confluence { version, .. } => version.unwrap_or(1),
                ContentMetadata::Jira { .. } => 1,
            };
            let current_title = current.title;

            let adf_json =
                serde_json::to_string(body_adf).context("Failed to serialize ADF document")?;

            debug!(
                page_id = id,
                version = current_version + 1,
                adf_bytes = adf_json.len(),
                "Updating Confluence page"
            );
            if tracing::enabled!(tracing::Level::TRACE) {
                let pretty = serde_json::to_string_pretty(body_adf)
                    .unwrap_or_else(|e| format!("<serialization error: {e}>"));
                tracing::trace!("ADF body for update:\n{pretty}");
            }

            let update = ConfluenceUpdateRequest {
                id: id.to_string(),
                status: "current".to_string(),
                title: title.unwrap_or(&current_title).to_string(),
                body: ConfluenceUpdateBody {
                    representation: "atlas_doc_format".to_string(),
                    value: adf_json,
                },
                version: ConfluenceUpdateVersion {
                    number: current_version + 1,
                    message: None,
                },
            };

            let url = format!("{}/wiki/api/v2/pages/{}", self.client.instance_url(), id);

            let response = self
                .client
                .put_json(&url, &update)
                .await
                .context("Failed to update Confluence page")?;

            if !response.status().is_success() {
                let status = response.status().as_u16();
                let body = response.text().await.unwrap_or_default();
                return Err(AtlassianError::ApiRequestFailed { status, body }.into());
            }

            Ok(())
        })
    }

    fn verify_auth<'a>(&'a self) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
        // Reuse the JIRA /myself endpoint — same Atlassian Cloud instance.
        Box::pin(async move {
            let user = self.client.get_myself().await?;
            Ok(user.display_name)
        })
    }

    fn backend_name(&self) -> &'static str {
        "confluence"
    }
}

impl ConfluenceApi {
    /// Resolves a space key to a space ID via the Confluence API.
    pub async fn resolve_space_id(&self, space_key: &str) -> Result<String> {
        let url = format!(
            "{}/wiki/api/v2/spaces?keys={}",
            self.client.instance_url(),
            space_key
        );

        let response = self
            .client
            .get_json(&url)
            .await
            .context("Failed to search Confluence spaces")?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(AtlassianError::ApiRequestFailed { status, body }.into());
        }

        let resp: ConfluenceSpacesSearchResponse = response
            .json()
            .await
            .context("Failed to parse Confluence spaces response")?;

        resp.results
            .first()
            .map(|s| s.id.clone())
            .ok_or_else(|| anyhow::anyhow!("Space with key \"{space_key}\" not found"))
    }

    /// Creates a new Confluence page.
    pub async fn create_page(
        &self,
        space_key: &str,
        title: &str,
        body_adf: &AdfDocument,
        parent_id: Option<&str>,
    ) -> Result<String> {
        let space_id = self.resolve_space_id(space_key).await?;

        let adf_json =
            serde_json::to_string(body_adf).context("Failed to serialize ADF document")?;

        let request = ConfluenceCreateRequest {
            space_id,
            title: title.to_string(),
            body: ConfluenceUpdateBody {
                representation: "atlas_doc_format".to_string(),
                value: adf_json,
            },
            parent_id: parent_id.map(String::from),
            status: "current".to_string(),
        };

        let url = format!("{}/wiki/api/v2/pages", self.client.instance_url());

        let response = self
            .client
            .post_json(&url, &request)
            .await
            .context("Failed to create Confluence page")?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(AtlassianError::ApiRequestFailed { status, body }.into());
        }

        let resp: ConfluenceCreateResponse = response
            .json()
            .await
            .context("Failed to parse Confluence create response")?;

        Ok(resp.id)
    }

    /// Deletes a Confluence page.
    pub async fn delete_page(&self, id: &str, purge: bool) -> Result<()> {
        let mut url = format!("{}/wiki/api/v2/pages/{}", self.client.instance_url(), id);
        if purge {
            url.push_str("?purge=true");
        }

        let response = self.client.delete(&url).await?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            if status == 404 {
                anyhow::bail!(
                    "Page {id} not found or insufficient permissions. \
                     Confluence returns 404 when the API user lacks space-level delete permission. \
                     Check Space Settings > Permissions."
                );
            }
            return Err(AtlassianError::ApiRequestFailed { status, body }.into());
        }

        Ok(())
    }

    /// Fetches all child pages of a given page, handling pagination.
    ///
    /// Uses the v1 content API (`/wiki/rest/api/content/{id}/child/page`)
    /// which is more widely supported than the v2 children endpoint.
    pub async fn get_children(&self, page_id: &str) -> Result<Vec<ChildPage>> {
        let mut all_children = Vec::new();
        let mut url = format!(
            "{}/wiki/rest/api/content/{}/child/page?limit=50",
            self.client.instance_url(),
            page_id
        );

        loop {
            let response = self
                .client
                .get_json(&url)
                .await
                .context("Failed to fetch child pages")?;

            if !response.status().is_success() {
                let status = response.status().as_u16();
                let body = response.text().await.unwrap_or_default();
                return Err(AtlassianError::ApiRequestFailed { status, body }.into());
            }

            let resp: ConfluenceChildrenResponse = response
                .json()
                .await
                .context("Failed to parse children response")?;

            let page_count = resp.results.len();
            for child in resp.results {
                all_children.push(ChildPage {
                    id: child.id,
                    title: child.title,
                });
            }

            match resp.links.and_then(|l| l.next) {
                Some(next_path) if page_count > 0 => {
                    url = format!("{}{}", self.client.instance_url(), next_path);
                }
                _ => break,
            }
        }

        Ok(all_children)
    }

    /// Lists footer comments on a Confluence page.
    pub async fn get_page_comments(&self, page_id: &str) -> Result<Vec<ConfluenceComment>> {
        let url = format!(
            "{}/wiki/api/v2/pages/{}/footer-comments?body-format=atlas_doc_format",
            self.client.instance_url(),
            page_id
        );

        let response = self
            .client
            .get_json(&url)
            .await
            .context("Failed to fetch Confluence page comments")?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(AtlassianError::ApiRequestFailed { status, body }.into());
        }

        let resp: ConfluenceCommentsResponse = response
            .json()
            .await
            .context("Failed to parse Confluence comments response")?;

        Ok(resp
            .results
            .into_iter()
            .map(|c| {
                let body_adf = c.body.and_then(|b| {
                    b.atlas_doc_format
                        .and_then(|a| serde_json::from_str(&a.value).ok())
                });
                let author = c
                    .version
                    .as_ref()
                    .and_then(|v| v.author_id.clone())
                    .unwrap_or_default();
                let created = c.version.and_then(|v| v.created_at).unwrap_or_default();
                ConfluenceComment {
                    id: c.id,
                    author,
                    body_adf,
                    created,
                }
            })
            .collect())
    }

    /// Adds a footer comment to a Confluence page.
    pub async fn add_page_comment(&self, page_id: &str, body_adf: &AdfDocument) -> Result<()> {
        let adf_json =
            serde_json::to_string(body_adf).context("Failed to serialize ADF document")?;

        let request = ConfluenceAddCommentRequest {
            page_id: page_id.to_string(),
            body: ConfluenceUpdateBody {
                representation: "atlas_doc_format".to_string(),
                value: adf_json,
            },
        };

        let url = format!("{}/wiki/api/v2/footer-comments", self.client.instance_url());

        let response = self
            .client
            .post_json(&url, &request)
            .await
            .context("Failed to add Confluence page comment")?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(AtlassianError::ApiRequestFailed { status, body }.into());
        }

        Ok(())
    }

    /// Resolves a space ID to a space key via the Confluence API.
    async fn resolve_space_key(&self, space_id: &str) -> Result<String> {
        let url = format!(
            "{}/wiki/api/v2/spaces/{}",
            self.client.instance_url(),
            space_id
        );

        let response = self
            .client
            .get_json(&url)
            .await
            .context("Failed to fetch Confluence space")?;

        if !response.status().is_success() {
            // Fall back to using the space ID as key if lookup fails.
            return Ok(space_id.to_string());
        }

        let space: ConfluenceSpaceResponse = response
            .json()
            .await
            .context("Failed to parse Confluence space response")?;

        Ok(space.key)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn confluence_api_backend_name() {
        let client =
            AtlassianClient::new("https://org.atlassian.net", "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        assert_eq!(api.backend_name(), "confluence");
    }

    #[test]
    fn confluence_page_response_deserialization() {
        let json = r#"{
            "id": "12345",
            "title": "Test Page",
            "status": "current",
            "spaceId": "98765",
            "version": {"number": 3},
            "body": {
                "atlas_doc_format": {
                    "value": "{\"version\":1,\"type\":\"doc\",\"content\":[]}"
                }
            },
            "parentId": "11111"
        }"#;
        let page: ConfluencePageResponse = serde_json::from_str(json).unwrap();
        assert_eq!(page.id, "12345");
        assert_eq!(page.title, "Test Page");
        assert_eq!(page.status, "current");
        assert_eq!(page.space_id, "98765");
        assert_eq!(page.version.unwrap().number, 3);
        assert_eq!(page.parent_id.as_deref(), Some("11111"));

        let body = page.body.unwrap();
        let atlas_doc = body.atlas_doc_format.unwrap();
        let adf: serde_json::Value = serde_json::from_str(&atlas_doc.value).unwrap();
        assert_eq!(adf["version"], 1);
        assert_eq!(adf["type"], "doc");
    }

    #[test]
    fn confluence_page_response_minimal() {
        let json = r#"{
            "id": "99",
            "title": "Minimal",
            "status": "draft",
            "spaceId": "1"
        }"#;
        let page: ConfluencePageResponse = serde_json::from_str(json).unwrap();
        assert_eq!(page.id, "99");
        assert!(page.version.is_none());
        assert!(page.body.is_none());
        assert!(page.parent_id.is_none());
    }

    #[test]
    fn confluence_update_request_serialization() {
        let req = ConfluenceUpdateRequest {
            id: "12345".to_string(),
            status: "current".to_string(),
            title: "Updated Title".to_string(),
            body: ConfluenceUpdateBody {
                representation: "atlas_doc_format".to_string(),
                value: r#"{"version":1,"type":"doc","content":[]}"#.to_string(),
            },
            version: ConfluenceUpdateVersion {
                number: 4,
                message: None,
            },
        };

        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["id"], "12345");
        assert_eq!(json["status"], "current");
        assert_eq!(json["title"], "Updated Title");
        assert_eq!(json["body"]["representation"], "atlas_doc_format");
        assert_eq!(json["version"]["number"], 4);
    }

    #[test]
    fn confluence_update_version_with_message() {
        let req = ConfluenceUpdateRequest {
            id: "1".to_string(),
            status: "current".to_string(),
            title: "T".to_string(),
            body: ConfluenceUpdateBody {
                representation: "atlas_doc_format".to_string(),
                value: "{}".to_string(),
            },
            version: ConfluenceUpdateVersion {
                number: 2,
                message: Some("Updated via API".to_string()),
            },
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["version"]["message"], "Updated via API");
    }

    #[test]
    fn confluence_space_response_deserialization() {
        let json = r#"{"key": "ENG"}"#;
        let space: ConfluenceSpaceResponse = serde_json::from_str(json).unwrap();
        assert_eq!(space.key, "ENG");
    }

    /// Helper to set up a wiremock server with the Confluence page and space endpoints.
    async fn setup_confluence_mock() -> (wiremock::MockServer, ConfluenceApi) {
        let server = wiremock::MockServer::start().await;

        let page_json = serde_json::json!({
            "id": "12345",
            "title": "Test Page",
            "status": "current",
            "spaceId": "98765",
            "version": {"number": 3},
            "body": {
                "atlas_doc_format": {
                    "value": "{\"version\":1,\"type\":\"doc\",\"content\":[{\"type\":\"paragraph\",\"content\":[{\"type\":\"text\",\"text\":\"Hello\"}]}]}"
                }
            },
            "parentId": "11111"
        });

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(&page_json))
            .mount(&server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98765"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"key": "ENG"})),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);

        (server, api)
    }

    #[tokio::test]
    async fn get_content_success() {
        use crate::atlassian::api::{AtlassianApi, ContentMetadata};

        let (_server, api) = setup_confluence_mock().await;
        let item = api.get_content("12345").await.unwrap();

        assert_eq!(item.id, "12345");
        assert_eq!(item.title, "Test Page");
        assert!(item.body_adf.is_some());
        match &item.metadata {
            ContentMetadata::Confluence {
                space_key,
                status,
                version,
                parent_id,
            } => {
                assert_eq!(space_key, "ENG");
                assert_eq!(status.as_deref(), Some("current"));
                assert_eq!(*version, Some(3));
                assert_eq!(parent_id.as_deref(), Some("11111"));
            }
            ContentMetadata::Jira { .. } => panic!("Expected Confluence metadata"),
        }
    }

    #[tokio::test]
    async fn get_content_api_error() {
        use crate::atlassian::api::AtlassianApi;

        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/99999"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let err = api.get_content("99999").await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn get_content_no_body() {
        use crate::atlassian::api::AtlassianApi;

        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/55555"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "55555",
                    "title": "No Body",
                    "status": "draft",
                    "spaceId": "11111"
                })),
            )
            .mount(&server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/11111"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"key": "DEV"})),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let item = api.get_content("55555").await.unwrap();
        assert!(item.body_adf.is_none());
    }

    #[tokio::test]
    async fn update_content_success() {
        use crate::atlassian::api::AtlassianApi;

        let (server, api) = setup_confluence_mock().await;

        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let adf = AdfDocument::new();
        let result = api.update_content("12345", &adf, Some("New Title")).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn update_content_api_error() {
        use crate::atlassian::api::AtlassianApi;

        let (server, api) = setup_confluence_mock().await;

        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;

        let adf = AdfDocument::new();
        let err = api.update_content("12345", &adf, None).await.unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn verify_auth_success() {
        use crate::atlassian::api::AtlassianApi;

        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/myself"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "displayName": "Alice",
                    "accountId": "abc123"
                })),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let name = api.verify_auth().await.unwrap();
        assert_eq!(name, "Alice");
    }

    #[tokio::test]
    async fn resolve_space_id_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": [{"id": "98765"}]})),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let id = api.resolve_space_id("ENG").await.unwrap();
        assert_eq!(id, "98765");
    }

    #[tokio::test]
    async fn resolve_space_id_not_found() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let err = api.resolve_space_id("NOPE").await.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn resolve_space_id_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let err = api.resolve_space_id("ENG").await.unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn create_page_success() {
        let server = wiremock::MockServer::start().await;

        // Space lookup
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": [{"id": "98765"}]})),
            )
            .mount(&server)
            .await;

        // Create page
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "54321"})),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let adf = AdfDocument::new();
        let id = api
            .create_page("ENG", "New Page", &adf, None)
            .await
            .unwrap();
        assert_eq!(id, "54321");
    }

    #[tokio::test]
    async fn create_page_with_parent() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": [{"id": "98765"}]})),
            )
            .mount(&server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "54322"})),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let adf = AdfDocument::new();
        let id = api
            .create_page("ENG", "Child Page", &adf, Some("11111"))
            .await
            .unwrap();
        assert_eq!(id, "54322");
    }

    #[tokio::test]
    async fn create_page_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": [{"id": "98765"}]})),
            )
            .mount(&server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages"))
            .respond_with(wiremock::ResponseTemplate::new(400).set_body_string("Bad Request"))
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let adf = AdfDocument::new();
        let err = api
            .create_page("ENG", "Fail", &adf, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("400"));
    }

    #[tokio::test]
    async fn delete_page_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345"))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let result = api.delete_page("12345", false).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn delete_page_with_purge() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345"))
            .and(wiremock::matchers::query_param("purge", "true"))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let result = api.delete_page("12345", true).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn delete_page_not_found_hints_permissions() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/99999"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let err = api.delete_page("99999", false).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not found or insufficient permissions"));
        assert!(msg.contains("Space Settings"));
    }

    #[tokio::test]
    async fn delete_page_forbidden() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let err = api.delete_page("12345", false).await.unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn get_children_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/child/page",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"id": "111", "title": "Child One"},
                        {"id": "222", "title": "Child Two"}
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let children = api.get_children("12345").await.unwrap();

        assert_eq!(children.len(), 2);
        assert_eq!(children[0].id, "111");
        assert_eq!(children[0].title, "Child One");
        assert_eq!(children[1].id, "222");
    }

    #[tokio::test]
    async fn get_children_empty() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/child/page",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let children = api.get_children("12345").await.unwrap();
        assert!(children.is_empty());
    }

    #[tokio::test]
    async fn get_children_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/99999/child/page",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let err = api.get_children("99999").await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn resolve_space_key_fallback_on_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/unknown"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let key = api.resolve_space_key("unknown").await.unwrap();
        // Falls back to the space ID when lookup fails
        assert_eq!(key, "unknown");
    }

    // ── get_page_comments ─────────────────────────────────────────

    #[tokio::test]
    async fn get_page_comments_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/pages/12345/footer-comments",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {
                            "id": "100",
                            "version": {
                                "authorId": "user-abc",
                                "createdAt": "2026-04-01T10:00:00.000Z"
                            },
                            "body": {
                                "atlas_doc_format": {
                                    "value": "{\"version\":1,\"type\":\"doc\",\"content\":[]}"
                                }
                            }
                        },
                        {
                            "id": "101",
                            "version": {
                                "authorId": "user-def",
                                "createdAt": "2026-04-02T14:00:00.000Z"
                            }
                        }
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let comments = api.get_page_comments("12345").await.unwrap();

        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].id, "100");
        assert_eq!(comments[0].author, "user-abc");
        assert!(comments[0].body_adf.is_some());
        assert_eq!(comments[1].id, "101");
        assert!(comments[1].body_adf.is_none());
    }

    #[tokio::test]
    async fn get_page_comments_malformed_adf_body() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/pages/12345/footer-comments",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {
                            "id": "100",
                            "version": {
                                "authorId": "user-abc",
                                "createdAt": "2026-04-01T10:00:00.000Z"
                            },
                            "body": {
                                "atlas_doc_format": {
                                    "value": "{ invalid json }"
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
        let api = ConfluenceApi::new(client);
        let comments = api.get_page_comments("12345").await.unwrap();

        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].id, "100");
        // Malformed ADF silently becomes None
        assert!(comments[0].body_adf.is_none());
    }

    #[tokio::test]
    async fn get_page_comments_missing_version() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/pages/12345/footer-comments",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {
                            "id": "100"
                        }
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let comments = api.get_page_comments("12345").await.unwrap();

        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].author, "");
        assert_eq!(comments[0].created, "");
        assert!(comments[0].body_adf.is_none());
    }

    #[tokio::test]
    async fn get_page_comments_empty() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/pages/12345/footer-comments",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let comments = api.get_page_comments("12345").await.unwrap();
        assert!(comments.is_empty());
    }

    #[tokio::test]
    async fn get_page_comments_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/pages/99999/footer-comments",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let err = api.get_page_comments("99999").await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    // ── add_page_comment ──────────────────────────────────────────

    #[tokio::test]
    async fn add_page_comment_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/wiki/api/v2/footer-comments"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "200"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let adf = AdfDocument::new();
        let result = api.add_page_comment("12345", &adf).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn add_page_comment_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/wiki/api/v2/footer-comments"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let adf = AdfDocument::new();
        let err = api.add_page_comment("12345", &adf).await.unwrap_err();
        assert!(err.to_string().contains("403"));
    }
}
