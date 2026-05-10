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
    #[serde(default)]
    ancestors: Vec<ConfluenceAncestorEntry>,
}

#[derive(Deserialize)]
struct ConfluenceAncestorEntry {
    id: String,
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
    #[serde(default)]
    status: Option<String>,
}

#[derive(Deserialize)]
struct ConfluenceChildrenLinks {
    next: Option<String>,
}

// V2 space-pages response (for `depth=root`).
#[derive(Deserialize)]
struct ConfluenceSpacePagesResponse {
    results: Vec<ConfluenceSpacePageEntry>,
    #[serde(rename = "_links", default)]
    links: Option<ConfluenceChildrenLinks>,
}

#[derive(Deserialize)]
struct ConfluenceSpacePageEntry {
    id: String,
    title: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(rename = "parentId", default)]
    parent_id: Option<String>,
}

/// A child page returned from the children API.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChildPage {
    /// Page ID.
    pub id: String,
    /// Page title.
    pub title: String,
    /// Page status (e.g. "current", "draft"). Empty if not provided by the API.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status: String,
    /// Parent page ID, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Space key, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_key: Option<String>,
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
    #[serde(rename = "_links", default)]
    links: Option<ConfluenceCommentsLinks>,
}

#[derive(Deserialize)]
struct ConfluenceCommentsLinks {
    next: Option<String>,
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

// ── Labels ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ConfluenceLabelsResponse {
    results: Vec<ConfluenceLabelEntry>,
    #[serde(rename = "_links", default)]
    links: Option<ConfluenceLabelsLinks>,
}

#[derive(Deserialize)]
struct ConfluenceLabelEntry {
    id: String,
    name: String,
    prefix: String,
}

#[derive(Deserialize)]
struct ConfluenceLabelsLinks {
    next: Option<String>,
}

/// A label on a Confluence page.
#[derive(Debug, Clone, Serialize)]
pub struct ConfluenceLabel {
    /// Label ID.
    pub id: String,
    /// Label name.
    pub name: String,
    /// Label prefix (e.g. "global").
    pub prefix: String,
}

#[derive(Serialize)]
struct ConfluenceAddLabelEntry {
    prefix: String,
    name: String,
}

// ── Versions ───────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ConfluenceVersionsResponse {
    results: Vec<ConfluenceVersionEntry>,
    #[serde(rename = "_links", default)]
    links: Option<ConfluenceVersionsLinks>,
}

#[derive(Deserialize)]
struct ConfluenceVersionEntry {
    number: u32,
    #[serde(rename = "createdAt", default)]
    created_at: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(rename = "minorEdit", default)]
    minor_edit: Option<bool>,
    #[serde(rename = "authorId", default)]
    author_id: Option<String>,
}

#[derive(Deserialize)]
struct ConfluenceVersionsLinks {
    next: Option<String>,
}

/// A single version entry from a Confluence page's history.
///
/// Optional fields (`created_at`, `author_id`, `message`) are returned as
/// empty strings when the API omits them — older pages can have null author
/// or timestamp data, see issue #708.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageVersion {
    /// Version number (1-based; current version at the head of the list).
    pub number: u32,
    /// ISO 8601 creation timestamp; empty if the API returned null.
    #[serde(default)]
    pub created_at: String,
    /// Account ID of the author; empty if the API returned null.
    #[serde(default)]
    pub author_id: String,
    /// Version comment / edit message; empty if the API returned null.
    #[serde(default)]
    pub message: String,
    /// Whether the edit was marked as minor.
    #[serde(default)]
    pub minor_edit: bool,
}

/// Filter applied to a version listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinceFilter {
    /// Keep versions whose `number >= n`.
    Version(u32),
    /// Keep versions whose `created_at >= iso` (lexicographic compare on
    /// ISO 8601 strings — ordering is correct as long as the timestamps
    /// are fully qualified with offsets, which Confluence's API guarantees).
    CreatedAt(String),
}

impl SinceFilter {
    /// Parses a `since` parameter. A purely numeric input is interpreted as
    /// a version number; anything containing `-` or `T` (the typical ISO 8601
    /// markers) is treated as a date.
    pub fn parse(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            anyhow::bail!("`since` must be a version number or ISO 8601 date");
        }
        if trimmed.chars().all(|c| c.is_ascii_digit()) {
            let n: u32 = trimmed
                .parse()
                .with_context(|| format!("Invalid version number \"{trimmed}\""))?;
            return Ok(Self::Version(n));
        }
        if trimmed.contains('-') || trimmed.contains('T') {
            return Ok(Self::CreatedAt(trimmed.to_string()));
        }
        anyhow::bail!(
            "`since` must be a numeric version (e.g. \"5\") or ISO 8601 date \
             (e.g. \"2026-01-01T00:00:00Z\"); got \"{trimmed}\""
        );
    }

    /// Whether `version` satisfies this filter (i.e. should be kept).
    fn matches(&self, version: &PageVersion) -> bool {
        match self {
            Self::Version(min) => version.number >= *min,
            Self::CreatedAt(min) => {
                if version.created_at.is_empty() {
                    // Tolerate missing timestamps: treat as too-old.
                    false
                } else {
                    version.created_at.as_str() >= min.as_str()
                }
            }
        }
    }
}

// ── Page metadata ──────────────────────────────────────────────────

/// Lightweight metadata about a Confluence page, returned by
/// [`ConfluenceApi::get_page_metadata`].
#[derive(Debug, Clone, Serialize)]
pub struct PageMetadata {
    /// Page ID.
    pub id: String,
    /// Page title.
    pub title: String,
    /// Current version number, if known.
    pub current_version: Option<u32>,
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

// ── Move types ─────────────────────────────────────────────────────

/// Position for [`ConfluenceApi::move_page`]. Same-space only —
/// cross-space moves are not supported by the v2 API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MovePosition {
    /// Place the page as the last child of the target (target becomes the new parent).
    Append,
    /// Place the page as a sibling immediately before the target.
    Before,
    /// Place the page as a sibling immediately after the target.
    After,
}

impl MovePosition {
    /// Returns the URL-path segment used by the Confluence move endpoint.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Append => "append",
            Self::Before => "before",
            Self::After => "after",
        }
    }
}

/// Updated page metadata returned by [`ConfluenceApi::move_page`].
#[derive(Debug, Clone, Serialize)]
pub struct MovedPage {
    /// Page ID.
    pub id: String,
    /// Page title.
    pub title: String,
    /// New parent page ID, if the page now has a parent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Ancestor page IDs from root toward the immediate parent.
    pub ancestors: Vec<String>,
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

    /// Moves or reparents a Confluence page within its current space.
    ///
    /// Same-space only — cross-space moves are not supported by the v2 API.
    /// Uses the v1 move endpoint (`PUT /wiki/rest/api/content/{id}/move/{position}/{target}`),
    /// then re-fetches the page with `?include-ancestors=true` to populate
    /// the returned [`MovedPage`].
    pub async fn move_page(
        &self,
        page_id: &str,
        target_id: &str,
        position: MovePosition,
    ) -> Result<MovedPage> {
        let url = format!(
            "{}/wiki/rest/api/content/{}/move/{}/{}",
            self.client.instance_url(),
            page_id,
            position.as_str(),
            target_id
        );

        let response = self
            .client
            .put_json(&url, &serde_json::json!({}))
            .await
            .context("Failed to send Confluence move request")?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            if status == 403 {
                anyhow::bail!(
                    "Move failed: insufficient permissions to move page {page_id} \
                     relative to target {target_id}. Confluence response: {body}"
                );
            }
            if status == 404 {
                anyhow::bail!(
                    "Move failed: page {page_id} or target {target_id} not found, \
                     or insufficient permissions. Confluence response: {body}"
                );
            }
            return Err(AtlassianError::ApiRequestFailed { status, body }.into());
        }

        let page = self.fetch_page_with_ancestors(page_id).await?;
        Ok(MovedPage {
            id: page.id,
            title: page.title,
            parent_id: page.parent_id,
            ancestors: page.ancestors.into_iter().map(|a| a.id).collect(),
        })
    }

    /// Fetches a Confluence page with its ancestors populated.
    async fn fetch_page_with_ancestors(&self, id: &str) -> Result<ConfluencePageResponse> {
        let url = format!(
            "{}/wiki/api/v2/pages/{}?include-ancestors=true",
            self.client.instance_url(),
            id
        );

        let response = self
            .client
            .get_json(&url)
            .await
            .context("Failed to fetch Confluence page with ancestors")?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(AtlassianError::ApiRequestFailed { status, body }.into());
        }

        response
            .json()
            .await
            .context("Failed to parse Confluence page response")
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
                    status: child.status.unwrap_or_default(),
                    parent_id: Some(page_id.to_string()),
                    space_key: None,
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

    /// Fetches top-level pages in a space (pages with no parent), handling pagination.
    ///
    /// Uses the v2 API endpoint `/wiki/api/v2/spaces/{space-id}/pages?depth=root`.
    pub async fn get_space_root_pages(&self, space_id: &str) -> Result<Vec<ChildPage>> {
        let mut all_pages = Vec::new();
        let mut url = format!(
            "{}/wiki/api/v2/spaces/{}/pages?depth=root&limit=50",
            self.client.instance_url(),
            space_id
        );

        loop {
            let response = self
                .client
                .get_json(&url)
                .await
                .context("Failed to fetch space root pages")?;

            if !response.status().is_success() {
                let status = response.status().as_u16();
                let body = response.text().await.unwrap_or_default();
                return Err(AtlassianError::ApiRequestFailed { status, body }.into());
            }

            let resp: ConfluenceSpacePagesResponse = response
                .json()
                .await
                .context("Failed to parse space pages response")?;

            let page_count = resp.results.len();
            for entry in resp.results {
                all_pages.push(ChildPage {
                    id: entry.id,
                    title: entry.title,
                    status: entry.status.unwrap_or_default(),
                    parent_id: entry.parent_id,
                    space_key: None,
                });
            }

            match resp.links.and_then(|l| l.next) {
                Some(next_path) if page_count > 0 => {
                    url = format!("{}{}", self.client.instance_url(), next_path);
                }
                _ => break,
            }
        }

        Ok(all_pages)
    }

    /// Lists footer comments on a Confluence page, handling pagination.
    pub async fn get_page_comments(&self, page_id: &str) -> Result<Vec<ConfluenceComment>> {
        let mut all_comments = Vec::new();
        let mut url = format!(
            "{}/wiki/api/v2/pages/{}/footer-comments?body-format=atlas_doc_format",
            self.client.instance_url(),
            page_id
        );

        loop {
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

            let page_count = resp.results.len();
            for c in resp.results {
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
                all_comments.push(ConfluenceComment {
                    id: c.id,
                    author,
                    body_adf,
                    created,
                });
            }

            match resp.links.and_then(|l| l.next) {
                Some(next_path) if page_count > 0 => {
                    url = format!("{}{}", self.client.instance_url(), next_path);
                }
                _ => break,
            }
        }

        Ok(all_comments)
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

    /// Fetches all labels on a Confluence page, handling pagination.
    pub async fn get_labels(&self, page_id: &str) -> Result<Vec<ConfluenceLabel>> {
        let mut all_labels = Vec::new();
        let mut url = format!(
            "{}/wiki/api/v2/pages/{}/labels",
            self.client.instance_url(),
            page_id
        );

        loop {
            let response = self
                .client
                .get_json(&url)
                .await
                .context("Failed to fetch page labels")?;

            if !response.status().is_success() {
                let status = response.status().as_u16();
                let body = response.text().await.unwrap_or_default();
                return Err(AtlassianError::ApiRequestFailed { status, body }.into());
            }

            let resp: ConfluenceLabelsResponse = response
                .json()
                .await
                .context("Failed to parse labels response")?;

            let page_count = resp.results.len();
            for entry in resp.results {
                all_labels.push(ConfluenceLabel {
                    id: entry.id,
                    name: entry.name,
                    prefix: entry.prefix,
                });
            }

            match resp.links.and_then(|l| l.next) {
                Some(next_path) if page_count > 0 => {
                    url = format!("{}{}", self.client.instance_url(), next_path);
                }
                _ => break,
            }
        }

        Ok(all_labels)
    }

    /// Adds one or more labels to a Confluence page.
    pub async fn add_labels(&self, page_id: &str, labels: &[String]) -> Result<()> {
        let url = format!(
            "{}/wiki/rest/api/content/{}/label",
            self.client.instance_url(),
            page_id
        );

        let body: Vec<ConfluenceAddLabelEntry> = labels
            .iter()
            .map(|name| ConfluenceAddLabelEntry {
                prefix: "global".to_string(),
                name: name.clone(),
            })
            .collect();

        let response = self
            .client
            .post_json(&url, &body)
            .await
            .context("Failed to add labels")?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(AtlassianError::ApiRequestFailed { status, body }.into());
        }

        Ok(())
    }

    /// Removes a label from a Confluence page.
    pub async fn remove_label(&self, page_id: &str, label_name: &str) -> Result<()> {
        let url = format!(
            "{}/wiki/rest/api/content/{}/label/{}",
            self.client.instance_url(),
            page_id,
            label_name
        );

        let response = self.client.delete(&url).await?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(AtlassianError::ApiRequestFailed { status, body }.into());
        }

        Ok(())
    }

    /// Fetches lightweight metadata (id, title, current version) for a page.
    ///
    /// Cheaper than [`AtlassianApi::get_content`] because it skips the body
    /// and the space-key lookup.
    pub async fn get_page_metadata(&self, page_id: &str) -> Result<PageMetadata> {
        let url = format!(
            "{}/wiki/api/v2/pages/{}",
            self.client.instance_url(),
            page_id
        );

        let response = self
            .client
            .get_json(&url)
            .await
            .context("Failed to fetch Confluence page metadata")?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(AtlassianError::ApiRequestFailed { status, body }.into());
        }

        let page: ConfluencePageResponse = response
            .json()
            .await
            .context("Failed to parse Confluence page response")?;

        Ok(PageMetadata {
            id: page.id,
            title: page.title,
            current_version: page.version.map(|v| v.number),
        })
    }

    /// Lists version history for a Confluence page, auto-paginated.
    ///
    /// Returns up to `limit` versions matching the optional `since` filter.
    /// `limit = 0` means unlimited. The Confluence v2 API returns versions
    /// newest-first, so encountering a version older than `since` ends
    /// pagination early.
    ///
    /// The boolean in the return tuple is `truncated`: `true` when `limit`
    /// was hit before the API was exhausted (more newer-than-`since`
    /// versions exist upstream).
    pub async fn list_page_versions(
        &self,
        page_id: &str,
        since: Option<&SinceFilter>,
        limit: u32,
    ) -> Result<(Vec<PageVersion>, bool)> {
        // Page size: cap at 100 per the v2 API; otherwise size to `limit`.
        let page_size = if limit == 0 { 100 } else { limit.min(100) };
        let mut url = format!(
            "{}/wiki/api/v2/pages/{}/versions?limit={}",
            self.client.instance_url(),
            page_id,
            page_size
        );

        let mut collected: Vec<PageVersion> = Vec::new();

        loop {
            let response = self
                .client
                .get_json(&url)
                .await
                .context("Failed to fetch Confluence page versions")?;

            if !response.status().is_success() {
                let status = response.status().as_u16();
                let body = response.text().await.unwrap_or_default();
                return Err(AtlassianError::ApiRequestFailed { status, body }.into());
            }

            let resp: ConfluenceVersionsResponse = response
                .json()
                .await
                .context("Failed to parse Confluence versions response")?;

            let page_count = resp.results.len();
            let next_link = resp.links.and_then(|l| l.next);

            for (idx, entry) in resp.results.into_iter().enumerate() {
                let version = PageVersion {
                    number: entry.number,
                    created_at: entry.created_at.unwrap_or_default(),
                    author_id: entry.author_id.unwrap_or_default(),
                    message: entry.message.unwrap_or_default(),
                    minor_edit: entry.minor_edit.unwrap_or(false),
                };

                if let Some(filter) = since {
                    if !filter.matches(&version) {
                        // Versions are newest-first; nothing further can match.
                        return Ok((collected, false));
                    }
                }

                collected.push(version);
                if limit > 0 && collected.len() as u32 >= limit {
                    // Truncated if more results exist on this page or in
                    // subsequent pages.
                    let more_on_page = idx + 1 < page_count;
                    let has_next = next_link.is_some();
                    return Ok((collected, more_on_page || has_next));
                }
            }

            match next_link {
                Some(next_path) if page_count > 0 => {
                    url = format!("{}{}", self.client.instance_url(), next_path);
                }
                _ => return Ok((collected, false)),
            }
        }
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

    // ── move_page ──────────────────────────────────────────────────

    #[test]
    fn move_position_as_str() {
        assert_eq!(MovePosition::Append.as_str(), "append");
        assert_eq!(MovePosition::Before.as_str(), "before");
        assert_eq!(MovePosition::After.as_str(), "after");
    }

    /// Mounts the post-move ancestor fetch (`GET /wiki/api/v2/pages/{id}?include-ancestors=true`).
    async fn mount_ancestor_fetch(server: &wiremock::MockServer, id: &str, parent_id: &str) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!("/wiki/api/v2/pages/{id}")))
            .and(wiremock::matchers::query_param("include-ancestors", "true"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": id,
                    "title": "Moved Page",
                    "status": "current",
                    "spaceId": "98765",
                    "parentId": parent_id,
                    "ancestors": [
                        {"id": "10"},
                        {"id": parent_id}
                    ]
                })),
            )
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn move_page_append_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/move/append/456",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"pageId": "12345"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        mount_ancestor_fetch(&server, "12345", "456").await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let moved = api
            .move_page("12345", "456", MovePosition::Append)
            .await
            .unwrap();
        assert_eq!(moved.id, "12345");
        assert_eq!(moved.title, "Moved Page");
        assert_eq!(moved.parent_id.as_deref(), Some("456"));
        assert_eq!(moved.ancestors, vec!["10".to_string(), "456".to_string()]);
    }

    #[tokio::test]
    async fn move_page_before_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/move/before/456",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"pageId": "12345"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        mount_ancestor_fetch(&server, "12345", "789").await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let moved = api
            .move_page("12345", "456", MovePosition::Before)
            .await
            .unwrap();
        assert_eq!(moved.parent_id.as_deref(), Some("789"));
    }

    #[tokio::test]
    async fn move_page_after_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/move/after/456",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"pageId": "12345"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        mount_ancestor_fetch(&server, "12345", "789").await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let moved = api
            .move_page("12345", "456", MovePosition::After)
            .await
            .unwrap();
        assert_eq!(moved.id, "12345");
    }

    #[tokio::test]
    async fn move_page_forbidden_surfaces_reason() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/move/append/456",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(403).set_body_json(serde_json::json!({
                    "errors": [{"detail": "User cannot move page"}]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let err = api
            .move_page("12345", "456", MovePosition::Append)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("insufficient permissions"));
        assert!(msg.contains("User cannot move page"));
    }

    #[tokio::test]
    async fn move_page_not_found() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/99999/move/append/456",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Page not found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let err = api
            .move_page("99999", "456", MovePosition::Append)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not found"));
        assert!(msg.contains("Page not found"));
    }

    #[tokio::test]
    async fn move_page_other_error_falls_through_to_generic() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/move/append/456",
            ))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let err = api
            .move_page("12345", "456", MovePosition::Append)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    #[tokio::test]
    async fn move_page_ancestor_fetch_failure_is_propagated() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/move/append/456",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"pageId": "12345"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345"))
            .and(wiremock::matchers::query_param("include-ancestors", "true"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("ancestor boom"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let err = api
            .move_page("12345", "456", MovePosition::Append)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("500"));
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
    async fn get_children_pagination() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/child/page",
            ))
            .and(wiremock::matchers::query_param_is_missing("start"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{"id": "111", "title": "First", "status": "current"}],
                    "_links": {
                        "next": "/wiki/rest/api/content/12345/child/page?limit=50&start=50"
                    }
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/child/page",
            ))
            .and(wiremock::matchers::query_param("start", "50"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{"id": "222", "title": "Second", "status": "current"}]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let children = api.get_children("12345").await.unwrap();
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].status, "current");
        assert_eq!(children[0].parent_id.as_deref(), Some("12345"));
    }

    #[tokio::test]
    async fn get_space_root_pages_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98765/pages"))
            .and(wiremock::matchers::query_param("depth", "root"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"id": "111", "title": "Top One", "status": "current"},
                        {"id": "222", "title": "Top Two", "status": "draft", "parentId": null}
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let pages = api.get_space_root_pages("98765").await.unwrap();
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].id, "111");
        assert_eq!(pages[0].status, "current");
        assert_eq!(pages[1].status, "draft");
    }

    #[tokio::test]
    async fn get_space_root_pages_empty() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98765/pages"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let pages = api.get_space_root_pages("98765").await.unwrap();
        assert!(pages.is_empty());
    }

    #[tokio::test]
    async fn get_space_root_pages_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/99999/pages"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let err = api.get_space_root_pages("99999").await.unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn get_space_root_pages_pagination() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98765/pages"))
            .and(wiremock::matchers::query_param_is_missing("cursor"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{"id": "111", "title": "A", "status": "current"}],
                    "_links": {
                        "next": "/wiki/api/v2/spaces/98765/pages?depth=root&cursor=page2"
                    }
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98765/pages"))
            .and(wiremock::matchers::query_param("cursor", "page2"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{"id": "222", "title": "B", "status": "current"}]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let pages = api.get_space_root_pages("98765").await.unwrap();
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].id, "111");
        assert_eq!(pages[1].id, "222");
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

    #[tokio::test]
    async fn get_page_comments_with_pagination() {
        let server = wiremock::MockServer::start().await;

        // First page returns one comment with a next link.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/pages/12345/footer-comments",
            ))
            .and(wiremock::matchers::query_param_is_missing("cursor"))
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
                        }
                    ],
                    "_links": {
                        "next": "/wiki/api/v2/pages/12345/footer-comments?body-format=atlas_doc_format&cursor=page2"
                    }
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        // Second page returns another comment with no next link.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/pages/12345/footer-comments",
            ))
            .and(wiremock::matchers::query_param("cursor", "page2"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
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
        assert_eq!(comments[1].author, "user-def");
    }

    #[tokio::test]
    async fn get_page_comments_pagination_stops_on_empty_page() {
        let server = wiremock::MockServer::start().await;

        // Response advertises a next link but returns no results; loop must stop
        // to avoid infinite pagination.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/pages/12345/footer-comments",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [],
                    "_links": {
                        "next": "/wiki/api/v2/pages/12345/footer-comments?cursor=loop"
                    }
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let comments = api.get_page_comments("12345").await.unwrap();
        assert!(comments.is_empty());
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

    // ── get_labels ────────────────────────────────────────────────

    #[tokio::test]
    async fn get_labels_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345/labels"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"id": "1", "name": "architecture", "prefix": "global"},
                        {"id": "2", "name": "draft", "prefix": "global"}
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let labels = api.get_labels("12345").await.unwrap();

        assert_eq!(labels.len(), 2);
        assert_eq!(labels[0].name, "architecture");
        assert_eq!(labels[0].prefix, "global");
        assert_eq!(labels[1].name, "draft");
    }

    #[tokio::test]
    async fn get_labels_with_pagination() {
        let server = wiremock::MockServer::start().await;

        // First page returns one label with a next link.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345/labels"))
            .and(wiremock::matchers::query_param_is_missing("cursor"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"id": "1", "name": "architecture", "prefix": "global"}
                    ],
                    "_links": {
                        "next": "/wiki/api/v2/pages/12345/labels?cursor=page2"
                    }
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        // Second page returns another label with no next link.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345/labels"))
            .and(wiremock::matchers::query_param("cursor", "page2"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"id": "2", "name": "draft", "prefix": "global"}
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let labels = api.get_labels("12345").await.unwrap();

        assert_eq!(labels.len(), 2);
        assert_eq!(labels[0].name, "architecture");
        assert_eq!(labels[1].name, "draft");
    }

    #[tokio::test]
    async fn get_labels_empty() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345/labels"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let labels = api.get_labels("12345").await.unwrap();
        assert!(labels.is_empty());
    }

    #[tokio::test]
    async fn get_labels_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/99999/labels"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let err = api.get_labels("99999").await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    // ── add_labels ────────────────────────────────────────────────

    #[tokio::test]
    async fn add_labels_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/label",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"prefix": "global", "name": "architecture", "id": "1"},
                        {"prefix": "global", "name": "draft", "id": "2"}
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let result = api
            .add_labels("12345", &["architecture".to_string(), "draft".to_string()])
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn add_labels_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/99999/label",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let err = api
            .add_labels("99999", &["test".to_string()])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    // ── remove_label ──────────────────────────────────────────────

    #[tokio::test]
    async fn remove_label_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/label/architecture",
            ))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let result = api.remove_label("12345", "architecture").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn remove_label_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/99999/label/missing",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let err = api.remove_label("99999", "missing").await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    // ── label struct serialization ────────────────────────────────

    #[test]
    fn confluence_label_entry_deserialization() {
        let json = r#"{"id": "1", "name": "architecture", "prefix": "global"}"#;
        let entry: ConfluenceLabelEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.id, "1");
        assert_eq!(entry.name, "architecture");
        assert_eq!(entry.prefix, "global");
    }

    #[test]
    fn confluence_add_label_entry_serialization() {
        let entry = ConfluenceAddLabelEntry {
            prefix: "global".to_string(),
            name: "test".to_string(),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["prefix"], "global");
        assert_eq!(json["name"], "test");
    }

    // ── SinceFilter::parse ────────────────────────────────────────

    #[test]
    fn since_filter_parse_numeric() {
        assert_eq!(SinceFilter::parse("5").unwrap(), SinceFilter::Version(5));
        assert_eq!(SinceFilter::parse("0").unwrap(), SinceFilter::Version(0));
    }

    #[test]
    fn since_filter_parse_iso_date() {
        let f = SinceFilter::parse("2026-01-01T00:00:00Z").unwrap();
        assert_eq!(
            f,
            SinceFilter::CreatedAt("2026-01-01T00:00:00Z".to_string())
        );
    }

    #[test]
    fn since_filter_parse_iso_date_no_time() {
        let f = SinceFilter::parse("2026-01-01").unwrap();
        assert_eq!(f, SinceFilter::CreatedAt("2026-01-01".to_string()));
    }

    #[test]
    fn since_filter_parse_trims_whitespace() {
        assert_eq!(
            SinceFilter::parse("  7  ").unwrap(),
            SinceFilter::Version(7)
        );
    }

    #[test]
    fn since_filter_parse_empty_rejected() {
        assert!(SinceFilter::parse("").is_err());
        assert!(SinceFilter::parse("   ").is_err());
    }

    #[test]
    fn since_filter_parse_garbage_rejected() {
        assert!(SinceFilter::parse("nope").is_err());
    }

    #[test]
    fn since_filter_matches_version() {
        let v = PageVersion {
            number: 5,
            created_at: String::new(),
            author_id: String::new(),
            message: String::new(),
            minor_edit: false,
        };
        assert!(SinceFilter::Version(5).matches(&v));
        assert!(SinceFilter::Version(4).matches(&v));
        assert!(!SinceFilter::Version(6).matches(&v));
    }

    #[test]
    fn since_filter_matches_created_at() {
        let v = PageVersion {
            number: 1,
            created_at: "2026-05-01T00:00:00Z".to_string(),
            author_id: String::new(),
            message: String::new(),
            minor_edit: false,
        };
        assert!(SinceFilter::CreatedAt("2026-04-01T00:00:00Z".to_string()).matches(&v));
        assert!(SinceFilter::CreatedAt("2026-05-01T00:00:00Z".to_string()).matches(&v));
        assert!(!SinceFilter::CreatedAt("2026-06-01T00:00:00Z".to_string()).matches(&v));
    }

    #[test]
    fn since_filter_created_at_treats_empty_as_too_old() {
        let v = PageVersion {
            number: 1,
            created_at: String::new(),
            author_id: String::new(),
            message: String::new(),
            minor_edit: false,
        };
        assert!(!SinceFilter::CreatedAt("2026-01-01".to_string()).matches(&v));
    }

    // ── ConfluenceVersionEntry deserialization ────────────────────

    #[test]
    fn version_entry_deserialization_full() {
        let json = r#"{
            "number": 9,
            "createdAt": "2026-05-08T10:23:11Z",
            "message": "Updated DB version",
            "minorEdit": false,
            "authorId": "abc-123"
        }"#;
        let entry: ConfluenceVersionEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.number, 9);
        assert_eq!(entry.created_at.as_deref(), Some("2026-05-08T10:23:11Z"));
        assert_eq!(entry.message.as_deref(), Some("Updated DB version"));
        assert_eq!(entry.minor_edit, Some(false));
        assert_eq!(entry.author_id.as_deref(), Some("abc-123"));
    }

    #[test]
    fn version_entry_deserialization_sparse() {
        let json = r#"{"number": 1}"#;
        let entry: ConfluenceVersionEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.number, 1);
        assert!(entry.created_at.is_none());
        assert!(entry.message.is_none());
        assert!(entry.minor_edit.is_none());
        assert!(entry.author_id.is_none());
    }

    // ── get_page_metadata ─────────────────────────────────────────

    #[tokio::test]
    async fn get_page_metadata_success() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "12345",
                    "title": "Hello",
                    "status": "current",
                    "spaceId": "1",
                    "version": {"number": 7}
                })),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "u@t.com", "tok").unwrap();
        let api = ConfluenceApi::new(client);
        let meta = api.get_page_metadata("12345").await.unwrap();
        assert_eq!(meta.id, "12345");
        assert_eq!(meta.title, "Hello");
        assert_eq!(meta.current_version, Some(7));
    }

    #[tokio::test]
    async fn get_page_metadata_no_version() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "12345",
                    "title": "Hello",
                    "status": "current",
                    "spaceId": "1"
                })),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "u@t.com", "tok").unwrap();
        let api = ConfluenceApi::new(client);
        let meta = api.get_page_metadata("12345").await.unwrap();
        assert_eq!(meta.current_version, None);
    }

    #[tokio::test]
    async fn get_page_metadata_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/99999"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "u@t.com", "tok").unwrap();
        let api = ConfluenceApi::new(client);
        let err = api.get_page_metadata("99999").await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    // ── list_page_versions ────────────────────────────────────────

    fn version_json(
        number: u32,
        created: &str,
        author: &str,
        msg: &str,
        minor: bool,
    ) -> serde_json::Value {
        serde_json::json!({
            "number": number,
            "createdAt": created,
            "message": msg,
            "minorEdit": minor,
            "authorId": author
        })
    }

    #[tokio::test]
    async fn list_page_versions_single_page() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12/versions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        version_json(3, "2026-05-08T10:00:00Z", "a", "third", false),
                        version_json(2, "2026-05-07T10:00:00Z", "b", "second", true),
                        version_json(1, "2026-05-06T10:00:00Z", "c", "first", false),
                    ]
                })),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "u@t.com", "tok").unwrap();
        let api = ConfluenceApi::new(client);
        let (versions, truncated) = api.list_page_versions("12", None, 0).await.unwrap();
        assert_eq!(versions.len(), 3);
        assert!(!truncated);
        assert_eq!(versions[0].number, 3);
        assert_eq!(versions[0].author_id, "a");
        assert_eq!(versions[0].message, "third");
        assert!(versions[1].minor_edit);
    }

    #[tokio::test]
    async fn list_page_versions_paginates_until_exhausted() {
        let server = wiremock::MockServer::start().await;
        // First page advertises a `next` link; second page is the last.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12/versions"))
            .and(wiremock::matchers::query_param("limit", "100"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        version_json(4, "2026-05-08T10:00:00Z", "a", "four", false),
                        version_json(3, "2026-05-07T10:00:00Z", "b", "three", false),
                    ],
                    "_links": {"next": "/wiki/api/v2/pages/12/versions?cursor=abc"}
                })),
            )
            .mount(&server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12/versions"))
            .and(wiremock::matchers::query_param("cursor", "abc"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        version_json(2, "2026-05-06T10:00:00Z", "c", "two", false),
                        version_json(1, "2026-05-05T10:00:00Z", "d", "one", false),
                    ]
                })),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "u@t.com", "tok").unwrap();
        let api = ConfluenceApi::new(client);
        let (versions, truncated) = api.list_page_versions("12", None, 0).await.unwrap();
        assert_eq!(
            versions.iter().map(|v| v.number).collect::<Vec<_>>(),
            vec![4, 3, 2, 1]
        );
        assert!(!truncated);
    }

    #[tokio::test]
    async fn list_page_versions_limit_truncates() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12/versions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        version_json(5, "2026-05-09T10:00:00Z", "a", "five", false),
                        version_json(4, "2026-05-08T10:00:00Z", "b", "four", false),
                        version_json(3, "2026-05-07T10:00:00Z", "c", "three", false),
                    ]
                })),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "u@t.com", "tok").unwrap();
        let api = ConfluenceApi::new(client);
        let (versions, truncated) = api.list_page_versions("12", None, 2).await.unwrap();
        assert_eq!(versions.len(), 2);
        assert!(truncated, "limit reached mid-page should mark truncated");
        assert_eq!(versions[0].number, 5);
        assert_eq!(versions[1].number, 4);
    }

    #[tokio::test]
    async fn list_page_versions_limit_exact_page_with_next_truncated() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12/versions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        version_json(5, "2026-05-09T10:00:00Z", "a", "five", false),
                        version_json(4, "2026-05-08T10:00:00Z", "b", "four", false),
                    ],
                    "_links": {"next": "/wiki/api/v2/pages/12/versions?cursor=z"}
                })),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "u@t.com", "tok").unwrap();
        let api = ConfluenceApi::new(client);
        let (versions, truncated) = api.list_page_versions("12", None, 2).await.unwrap();
        assert_eq!(versions.len(), 2);
        assert!(truncated);
    }

    #[tokio::test]
    async fn list_page_versions_since_numeric_filter() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12/versions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        version_json(5, "2026-05-09T10:00:00Z", "a", "5", false),
                        version_json(4, "2026-05-08T10:00:00Z", "b", "4", false),
                        version_json(3, "2026-05-07T10:00:00Z", "c", "3", false),
                        version_json(2, "2026-05-06T10:00:00Z", "d", "2", false),
                    ]
                })),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "u@t.com", "tok").unwrap();
        let api = ConfluenceApi::new(client);
        let filter = SinceFilter::parse("4").unwrap();
        let (versions, truncated) = api
            .list_page_versions("12", Some(&filter), 0)
            .await
            .unwrap();
        assert_eq!(
            versions.iter().map(|v| v.number).collect::<Vec<_>>(),
            vec![5, 4]
        );
        assert!(!truncated);
    }

    #[tokio::test]
    async fn list_page_versions_since_iso_filter() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12/versions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        version_json(3, "2026-05-08T10:00:00Z", "a", "", false),
                        version_json(2, "2026-04-01T10:00:00Z", "b", "", false),
                        version_json(1, "2026-03-01T10:00:00Z", "c", "", false),
                    ]
                })),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "u@t.com", "tok").unwrap();
        let api = ConfluenceApi::new(client);
        let filter = SinceFilter::parse("2026-05-01").unwrap();
        let (versions, truncated) = api
            .list_page_versions("12", Some(&filter), 0)
            .await
            .unwrap();
        assert_eq!(
            versions.iter().map(|v| v.number).collect::<Vec<_>>(),
            vec![3]
        );
        assert!(!truncated);
    }

    #[tokio::test]
    async fn list_page_versions_since_stops_pagination_early() {
        let server = wiremock::MockServer::start().await;
        // Page 1 has results that all match; page 2 includes the cutoff version.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12/versions"))
            .and(wiremock::matchers::query_param("limit", "100"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        version_json(5, "2026-05-09T10:00:00Z", "a", "5", false),
                        version_json(4, "2026-05-08T10:00:00Z", "b", "4", false),
                    ],
                    "_links": {"next": "/wiki/api/v2/pages/12/versions?cursor=p2"}
                })),
            )
            .mount(&server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12/versions"))
            .and(wiremock::matchers::query_param("cursor", "p2"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        version_json(3, "2026-05-07T10:00:00Z", "c", "3", false),
                        version_json(2, "2026-04-30T10:00:00Z", "d", "2", false),
                    ]
                })),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "u@t.com", "tok").unwrap();
        let api = ConfluenceApi::new(client);
        let filter = SinceFilter::parse("2026-05-01").unwrap();
        let (versions, truncated) = api
            .list_page_versions("12", Some(&filter), 0)
            .await
            .unwrap();
        assert_eq!(
            versions.iter().map(|v| v.number).collect::<Vec<_>>(),
            vec![5, 4, 3]
        );
        assert!(!truncated, "since cutoff is a stop, not a truncation");
    }

    #[tokio::test]
    async fn list_page_versions_tolerates_missing_optional_fields() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12/versions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"number": 1}
                    ]
                })),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "u@t.com", "tok").unwrap();
        let api = ConfluenceApi::new(client);
        let (versions, truncated) = api.list_page_versions("12", None, 0).await.unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].number, 1);
        assert_eq!(versions[0].created_at, "");
        assert_eq!(versions[0].author_id, "");
        assert_eq!(versions[0].message, "");
        assert!(!versions[0].minor_edit);
        assert!(!truncated);
    }

    #[tokio::test]
    async fn list_page_versions_empty_result() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12/versions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "u@t.com", "tok").unwrap();
        let api = ConfluenceApi::new(client);
        let (versions, truncated) = api.list_page_versions("12", None, 0).await.unwrap();
        assert!(versions.is_empty());
        assert!(!truncated);
    }

    #[tokio::test]
    async fn list_page_versions_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/pages/99999/versions",
            ))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "u@t.com", "tok").unwrap();
        let api = ConfluenceApi::new(client);
        let err = api.list_page_versions("99999", None, 0).await.unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn list_page_versions_uses_limit_as_page_size_when_small() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12/versions"))
            .and(wiremock::matchers::query_param("limit", "5"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        version_json(1, "2026-05-01T00:00:00Z", "a", "", false),
                    ]
                })),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "u@t.com", "tok").unwrap();
        let api = ConfluenceApi::new(client);
        let (versions, _) = api.list_page_versions("12", None, 5).await.unwrap();
        assert_eq!(versions.len(), 1);
    }
}
