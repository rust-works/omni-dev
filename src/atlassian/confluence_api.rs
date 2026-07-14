//! Confluence Cloud REST API v2 implementation of [`AtlassianApi`].
//!
//! Uses the Confluence REST API v2 to read and write pages.
//! Pages are fetched with ADF body format and updated with version
//! number increments for optimistic locking.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use anyhow::{Context, Result};
use tokio_util::io::ReaderStream;
use tracing::debug;

use crate::atlassian::adf::AdfDocument;
use crate::atlassian::adf_hints;
use crate::atlassian::adf_schema;
use crate::atlassian::adf_validated::ValidatedAdfDocument;
use crate::atlassian::api::{AtlassianApi, ContentItem, ContentMetadata};
use crate::atlassian::client::AtlassianClient;
use crate::atlassian::confluence_types::{
    ChildPage,
    CommentKind,
    ConfluenceAddCommentRequest,
    ConfluenceAddInlineCommentRequest,
    ConfluenceAddLabelEntry,
    ConfluenceAttachment,
    ConfluenceAttachmentEntry,
    ConfluenceAttachmentPage,
    ConfluenceAttachmentsResponse,
    ConfluenceChildrenResponse,
    ConfluenceComment,
    ConfluenceCommentDetail,
    ConfluenceCommentsResponse,
    ConfluenceCreateRequest,
    ConfluenceCreateResponse,
    ConfluenceLabel,
    ConfluenceLabelsResponse,
    // (ConfluenceCommentDetail reuses ConfluenceCommentVersion/ConfluenceCommentBody)
    ConfluencePageResponse,
    ConfluenceSpacePage,
    ConfluenceSpacePagesResponse,
    ConfluenceSpacePagesSummaryResponse,
    ConfluenceSpaceResponse,
    ConfluenceSpacesResponse,
    ConfluenceUpdateBody,
    ConfluenceUpdateCommentRequest,
    ConfluenceUpdateRequest,
    ConfluenceUpdateVersion,
    ConfluenceV1AttachmentEntry,
    ConfluenceV1AttachmentResponse,
    ConfluenceVersionsResponse,
    ConfluenceWatchStatus,
    InlineAnchor,
    InlineCommentProperties,
    MovePosition,
    MovedPage,
    PageMetadata,
    PageSummary,
    PageSummaryPage,
    PageVersion,
    SinceFilter,
};
use crate::atlassian::error::AtlassianError;

/// Builds an `anyhow::Error` for a non-success Confluence write/update/create
/// response.
///
/// On HTTP 500, runs [`adf_schema::validate_document`] against the submitted
/// ADF payload and, if a violation is found, returns
/// [`AtlassianError::ApiRequestFailedWithDiagnosis`] with the first violation
/// and a matching hint from [`adf_hints::hint_for`]. All other status codes
/// (and 500 responses with no detected violation) fall back to the existing
/// [`AtlassianError::ApiRequestFailed`] format.
fn confluence_write_error(status: u16, body: String, body_adf: &AdfDocument) -> anyhow::Error {
    if status == 500 {
        if let Some(violation) = adf_schema::validate_document(body_adf).into_iter().next() {
            let hint = adf_hints::hint_for(&violation).map(str::to_string);
            return AtlassianError::ApiRequestFailedWithDiagnosis {
                body,
                diagnosis: violation,
                hint,
            }
            .into();
        }
    }
    AtlassianError::ApiRequestFailed { status, body }.into()
}

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

            let page: ConfluencePageResponse = AtlassianClient::parse_json(
                AtlassianClient::ensure_success(response).await?,
                "Failed to parse Confluence page response",
            )
            .await?;

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
        body_adf: &'a ValidatedAdfDocument,
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
                debug!(status, body = %body, "Confluence update_content non-success");
                return Err(confluence_write_error(status, body, body_adf));
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
        let page = self.list_spaces(&[space_key], None, None, None, 1).await?;

        page.results
            .into_iter()
            .next()
            .map(|s| s.id)
            .ok_or_else(|| anyhow::anyhow!("Space with key \"{space_key}\" not found"))
    }

    /// Lists Confluence spaces (one page at a time).
    ///
    /// Optional filters: `keys` (matches any of the given space keys; joined as
    /// a single comma-separated query parameter), `type` (one of `"global"`,
    /// `"personal"`, `"collaboration"`, `"knowledge_base"`), `status` (one of
    /// `"current"`, `"archived"`). Pagination is *not* auto-drained: pass
    /// [`ConfluenceSpacePage::next_cursor`] back as `cursor` to fetch the next
    /// page.
    pub async fn list_spaces(
        &self,
        keys: &[&str],
        type_: Option<&str>,
        status: Option<&str>,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<ConfluenceSpacePage> {
        let mut url = format!(
            "{}/wiki/api/v2/spaces?limit={}",
            self.client.instance_url(),
            limit
        );
        if !keys.is_empty() {
            let joined = keys.join(",");
            url.push_str("&keys=");
            url.push_str(&urlencoding(&joined));
        }
        if let Some(t) = type_ {
            url.push_str("&type=");
            url.push_str(&urlencoding(t));
        }
        if let Some(s) = status {
            url.push_str("&status=");
            url.push_str(&urlencoding(s));
        }
        if let Some(c) = cursor {
            url.push_str("&cursor=");
            url.push_str(&urlencoding(c));
        }

        let response = self
            .client
            .get_json(&url)
            .await
            .context("Failed to list Confluence spaces")?;

        let resp: ConfluenceSpacesResponse = AtlassianClient::parse_json(
            AtlassianClient::ensure_success(response).await?,
            "Failed to parse Confluence spaces response",
        )
        .await?;

        let next_cursor = resp
            .links
            .and_then(|l| l.next)
            .and_then(|next_path| extract_cursor_from_next(&next_path));

        let results = resp.results.into_iter().map(Into::into).collect();

        Ok(ConfluenceSpacePage {
            results,
            next_cursor,
        })
    }

    /// Enumerates pages within a Confluence space (one response at a time).
    ///
    /// Optional filters are passed through to the Confluence v2 API verbatim:
    /// `status` (e.g. `current`, `archived`, `draft`, `trashed`) and `sort`
    /// (e.g. `id`, `-id`, `title`, `-title`, `created-date`, `-created-date`,
    /// `modified-date`, `-modified-date`). Pagination is *not* auto-drained:
    /// pass [`PageSummaryPage::next_cursor`] back as `cursor` to fetch the
    /// next page.
    pub async fn list_space_pages(
        &self,
        space_id: &str,
        status: Option<&str>,
        sort: Option<&str>,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<PageSummaryPage> {
        let mut url = format!(
            "{}/wiki/api/v2/spaces/{}/pages?limit={}",
            self.client.instance_url(),
            space_id,
            limit
        );
        if let Some(s) = status {
            url.push_str("&status=");
            url.push_str(&urlencoding(s));
        }
        if let Some(s) = sort {
            url.push_str("&sort=");
            url.push_str(&urlencoding(s));
        }
        if let Some(c) = cursor {
            url.push_str("&cursor=");
            url.push_str(&urlencoding(c));
        }

        let response = self
            .client
            .get_json(&url)
            .await
            .context("Failed to list Confluence space pages")?;

        let resp: ConfluenceSpacePagesSummaryResponse = AtlassianClient::parse_json(
            AtlassianClient::ensure_success(response).await?,
            "Failed to parse Confluence space pages response",
        )
        .await?;

        let next_cursor = resp
            .links
            .and_then(|l| l.next)
            .and_then(|next_path| extract_cursor_from_next(&next_path));

        let results = resp
            .results
            .into_iter()
            .map(|e| PageSummary {
                id: e.id,
                title: e.title,
                status: e.status.unwrap_or_default(),
                parent_id: e.parent_id,
                author_id: e.author_id,
                created_at: e.created_at,
            })
            .collect();

        Ok(PageSummaryPage {
            results,
            next_cursor,
        })
    }

    /// Creates a new Confluence page.
    pub async fn create_page(
        &self,
        space_key: &str,
        title: &str,
        body_adf: &ValidatedAdfDocument,
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
            debug!(status, body = %body, "Confluence create_page non-success");
            return Err(confluence_write_error(status, body, body_adf));
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

    /// Copies a single Confluence page under a destination parent page.
    ///
    /// Uses the v1 copy endpoint (`POST /wiki/rest/api/content/{id}/copy`),
    /// carrying the page's attachments, labels, and properties (but not its
    /// restrictions). Returns the new page's id. Single-page copy only — the
    /// hierarchy-copy endpoint is a separate async task not handled here.
    pub async fn copy_page(
        &self,
        page_id: &str,
        dest_parent_id: &str,
        new_title: &str,
    ) -> Result<String> {
        let url = format!(
            "{}/wiki/rest/api/content/{}/copy",
            self.client.instance_url(),
            page_id
        );

        let body = serde_json::json!({
            "destination": { "type": "parent_page", "value": dest_parent_id },
            "pageTitle": new_title,
            "copyAttachments": true,
            "copyPermissions": false,
            "copyProperties": true,
            "copyLabels": true,
            "copyCustomContents": false,
        });

        let response = self
            .client
            .post_json(&url, &body)
            .await
            .context("Failed to send Confluence copy request")?;

        let created: ConfluenceCreateResponse = AtlassianClient::parse_json(
            AtlassianClient::ensure_success(response).await?,
            "Failed to parse Confluence copy response",
        )
        .await?;

        Ok(created.id)
    }

    /// Builds the content-watch URL, optionally scoped to a specific user's
    /// `accountId` (defaults to the authenticated user when `None`).
    fn content_watch_url(&self, content_id: &str, account_id: Option<&str>) -> String {
        let mut url = format!(
            "{}/wiki/rest/api/user/watch/content/{}",
            self.client.instance_url(),
            content_id
        );
        if let Some(id) = account_id {
            url.push_str("?accountId=");
            url.push_str(&urlencoding(id));
        }
        url
    }

    /// Reports whether a user is watching a Confluence page (the authenticated
    /// user when `account_id` is `None`).
    ///
    /// `GET /wiki/rest/api/user/watch/content/{id}`.
    pub async fn is_watching_content(
        &self,
        content_id: &str,
        account_id: Option<&str>,
    ) -> Result<ConfluenceWatchStatus> {
        let url = self.content_watch_url(content_id, account_id);
        let response = self
            .client
            .get_json(&url)
            .await
            .context("Failed to fetch Confluence watch status")?;
        AtlassianClient::parse_json(
            AtlassianClient::ensure_success(response).await?,
            "Failed to parse Confluence watch status",
        )
        .await
    }

    /// Adds a watcher to a Confluence page (the authenticated user when
    /// `account_id` is `None`).
    ///
    /// `POST /wiki/rest/api/user/watch/content/{id}`.
    pub async fn add_content_watcher(
        &self,
        content_id: &str,
        account_id: Option<&str>,
    ) -> Result<()> {
        let url = self.content_watch_url(content_id, account_id);
        let response = self.client.post_json(&url, &serde_json::json!({})).await?;
        AtlassianClient::ensure_success(response).await?;
        Ok(())
    }

    /// Removes a watcher from a Confluence page (the authenticated user when
    /// `account_id` is `None`).
    ///
    /// `DELETE /wiki/rest/api/user/watch/content/{id}`.
    pub async fn remove_content_watcher(
        &self,
        content_id: &str,
        account_id: Option<&str>,
    ) -> Result<()> {
        let url = self.content_watch_url(content_id, account_id);
        let response = self.client.delete(&url).await?;
        AtlassianClient::ensure_success(response).await?;
        Ok(())
    }

    /// Reads the read/update restrictions on a Confluence page.
    ///
    /// `GET /wiki/rest/api/content/{id}/restriction`. Returns the raw response
    /// JSON (the restriction model is deeply nested — user/group arrays per
    /// operation — so it is surfaced verbatim rather than reshaped).
    pub async fn get_content_restrictions(&self, content_id: &str) -> Result<serde_json::Value> {
        let url = format!(
            "{}/wiki/rest/api/content/{}/restriction",
            self.client.instance_url(),
            content_id
        );
        let response = self
            .client
            .get_json(&url)
            .await
            .context("Failed to fetch Confluence content restrictions")?;
        AtlassianClient::parse_json(
            AtlassianClient::ensure_success(response).await?,
            "Failed to parse Confluence content restrictions",
        )
        .await
    }

    /// Builds the `byOperation` restriction URL for a user or group subject.
    ///
    /// Exactly one of `account_id` / `group` must be `Some`.
    fn restriction_subject_url(
        &self,
        content_id: &str,
        operation: &str,
        account_id: Option<&str>,
        group: Option<&str>,
    ) -> Result<String> {
        let base = format!(
            "{}/wiki/rest/api/content/{}/restriction/byOperation/{}",
            self.client.instance_url(),
            content_id,
            operation
        );
        match (account_id, group) {
            (Some(acc), None) => Ok(format!("{base}/user?accountId={}", urlencoding(acc))),
            (None, Some(g)) => Ok(format!("{base}/group/{}", urlencoding(g))),
            (Some(_), Some(_)) => {
                anyhow::bail!("Specify a user (--account-id) or a group (--group), not both")
            }
            (None, None) => {
                anyhow::bail!("Specify a user (--account-id) or a group (--group)")
            }
        }
    }

    /// Grants a user or group a restriction for an operation (`read`/`update`)
    /// on a Confluence page.
    ///
    /// `PUT /wiki/rest/api/content/{id}/restriction/byOperation/{op}/{user|group}/…`.
    pub async fn grant_content_restriction(
        &self,
        content_id: &str,
        operation: &str,
        account_id: Option<&str>,
        group: Option<&str>,
    ) -> Result<()> {
        let url = self.restriction_subject_url(content_id, operation, account_id, group)?;
        let response = self.client.put_json(&url, &serde_json::json!({})).await?;
        AtlassianClient::ensure_success(response).await?;
        Ok(())
    }

    /// Revokes a user's or group's restriction for an operation on a page.
    ///
    /// `DELETE /wiki/rest/api/content/{id}/restriction/byOperation/{op}/{user|group}/…`.
    pub async fn revoke_content_restriction(
        &self,
        content_id: &str,
        operation: &str,
        account_id: Option<&str>,
        group: Option<&str>,
    ) -> Result<()> {
        let url = self.restriction_subject_url(content_id, operation, account_id, group)?;
        let response = self.client.delete(&url).await?;
        AtlassianClient::ensure_success(response).await?;
        Ok(())
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

        let response = AtlassianClient::ensure_success(response).await?;

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

            let resp: ConfluenceChildrenResponse = AtlassianClient::parse_json(
                AtlassianClient::ensure_success(response).await?,
                "Failed to parse children response",
            )
            .await?;

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

            let resp: ConfluenceSpacePagesResponse = AtlassianClient::parse_json(
                AtlassianClient::ensure_success(response).await?,
                "Failed to parse space pages response",
            )
            .await?;

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
        let url = format!(
            "{}/wiki/api/v2/pages/{}/footer-comments?body-format=atlas_doc_format",
            self.client.instance_url(),
            page_id
        );
        self.fetch_comments_paginated(url, CommentKind::Footer)
            .await
    }

    /// Lists inline comments on a Confluence page, handling pagination.
    pub async fn get_page_inline_comments(&self, page_id: &str) -> Result<Vec<ConfluenceComment>> {
        let url = format!(
            "{}/wiki/api/v2/pages/{}/inline-comments?body-format=atlas_doc_format",
            self.client.instance_url(),
            page_id
        );
        self.fetch_comments_paginated(url, CommentKind::Inline)
            .await
    }

    /// Lists the replies (child comments) of a comment.
    ///
    /// `kind` selects which Confluence v2 endpoint to hit: footer replies and
    /// inline replies live on separate URLs. The returned comments are stamped
    /// with the same `kind` as the parent — Confluence treats reply chains as
    /// homogenous.
    pub async fn get_comment_replies(
        &self,
        comment_id: &str,
        kind: CommentKind,
    ) -> Result<Vec<ConfluenceComment>> {
        let url = format!(
            "{}/wiki/api/v2/{}/{}/children?body-format=atlas_doc_format",
            self.client.instance_url(),
            kind.endpoint_segment(),
            comment_id
        );
        self.fetch_comments_paginated(url, kind).await
    }

    /// Shared paginated GET for the comments and replies endpoints.
    async fn fetch_comments_paginated(
        &self,
        mut url: String,
        kind: CommentKind,
    ) -> Result<Vec<ConfluenceComment>> {
        let mut all_comments = Vec::new();

        loop {
            let response = self
                .client
                .get_json(&url)
                .await
                .context("Failed to fetch Confluence comments")?;

            let resp: ConfluenceCommentsResponse = AtlassianClient::parse_json(
                AtlassianClient::ensure_success(response).await?,
                "Failed to parse Confluence comments response",
            )
            .await?;

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
                let (inline_marker_ref, inline_original_selection) = match c.properties {
                    Some(p) => (p.inline_marker_ref, p.inline_original_selection),
                    None => (None, None),
                };
                all_comments.push(ConfluenceComment {
                    id: c.id,
                    author,
                    kind,
                    body_adf,
                    created,
                    inline_marker_ref,
                    inline_original_selection,
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
    pub async fn add_page_comment(
        &self,
        page_id: &str,
        body_adf: &ValidatedAdfDocument,
    ) -> Result<()> {
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
            debug!(status, body = %body, "Confluence add_page_comment non-success");
            return Err(confluence_write_error(status, body, body_adf));
        }

        Ok(())
    }

    /// Adds an inline comment anchored to a text selection on a Confluence page.
    ///
    /// `anchor` is typically produced by [`Self::resolve_anchor`], which counts
    /// occurrences on the live page and validates that a 1-based `match_index`
    /// the user supplied is in range.
    pub async fn add_inline_page_comment(
        &self,
        page_id: &str,
        body_adf: &ValidatedAdfDocument,
        anchor: &InlineAnchor,
    ) -> Result<()> {
        let adf_json =
            serde_json::to_string(body_adf).context("Failed to serialize ADF document")?;

        let request = ConfluenceAddInlineCommentRequest {
            page_id: page_id.to_string(),
            body: ConfluenceUpdateBody {
                representation: "atlas_doc_format".to_string(),
                value: adf_json,
            },
            inline_comment_properties: InlineCommentProperties {
                text_selection: anchor.text.clone(),
                text_selection_match_count: anchor.match_count,
                text_selection_match_index: anchor.match_index,
            },
        };

        let url = format!("{}/wiki/api/v2/inline-comments", self.client.instance_url());

        let response = self
            .client
            .post_json(&url, &request)
            .await
            .context("Failed to add Confluence inline comment")?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            debug!(status, body = %body, "Confluence add_inline_page_comment non-success");
            return Err(confluence_write_error(status, body, body_adf));
        }

        Ok(())
    }

    /// Reads a single comment's current `version.number` and ADF body value.
    ///
    /// `GET /wiki/api/v2/{segment}/{id}?body-format=atlas_doc_format`. Used by
    /// [`Self::update_page_comment`] (needs the version to bump) and
    /// [`Self::set_inline_comment_resolved`] (also needs the current body to
    /// re-send unchanged).
    async fn get_comment_version_and_body(
        &self,
        comment_id: &str,
        kind: CommentKind,
    ) -> Result<(u32, String)> {
        let url = format!(
            "{}/wiki/api/v2/{}/{}?body-format=atlas_doc_format",
            self.client.instance_url(),
            kind.endpoint_segment(),
            comment_id
        );

        let response = self
            .client
            .get_json(&url)
            .await
            .context("Failed to fetch Confluence comment")?;

        let detail: ConfluenceCommentDetail = AtlassianClient::parse_json(
            AtlassianClient::ensure_success(response).await?,
            "Failed to parse Confluence comment detail",
        )
        .await?;

        let version = detail.version.and_then(|v| v.number).ok_or_else(|| {
            anyhow::anyhow!("Confluence comment {comment_id} response is missing version.number")
        })?;

        let body = detail
            .body
            .and_then(|b| b.atlas_doc_format)
            .map(|v| v.value)
            .unwrap_or_default();

        Ok((version, body))
    }

    /// Edits an existing footer or inline comment's body.
    ///
    /// `PUT /wiki/api/v2/{segment}/{id}`. Confluence versions comments, so the
    /// current version is fetched first and the update sends `version.number +
    /// 1` alongside the new ADF body.
    pub async fn update_page_comment(
        &self,
        comment_id: &str,
        kind: CommentKind,
        body_adf: &ValidatedAdfDocument,
    ) -> Result<()> {
        let (version, _current_body) = self.get_comment_version_and_body(comment_id, kind).await?;

        let adf_json =
            serde_json::to_string(body_adf).context("Failed to serialize ADF document")?;

        let request = ConfluenceUpdateCommentRequest {
            version: ConfluenceUpdateVersion {
                number: version + 1,
                message: None,
            },
            body: ConfluenceUpdateBody {
                representation: "atlas_doc_format".to_string(),
                value: adf_json,
            },
            resolved: None,
        };

        let url = format!(
            "{}/wiki/api/v2/{}/{}",
            self.client.instance_url(),
            kind.endpoint_segment(),
            comment_id
        );

        let response = self
            .client
            .put_json(&url, &request)
            .await
            .context("Failed to update Confluence comment")?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            debug!(status, body = %body, "Confluence update_page_comment non-success");
            return Err(confluence_write_error(status, body, body_adf));
        }

        Ok(())
    }

    /// Deletes a footer or inline comment from a Confluence page.
    ///
    /// `DELETE /wiki/api/v2/{segment}/{id}`.
    pub async fn delete_page_comment(&self, comment_id: &str, kind: CommentKind) -> Result<()> {
        let url = format!(
            "{}/wiki/api/v2/{}/{}",
            self.client.instance_url(),
            kind.endpoint_segment(),
            comment_id
        );

        let response = self.client.delete(&url).await?;

        AtlassianClient::ensure_success(response).await?;

        Ok(())
    }

    /// Resolves (`resolved = true`) or reopens (`resolved = false`) an inline
    /// comment.
    ///
    /// `PUT /wiki/api/v2/inline-comments/{id}` carrying the `resolved` flag. The
    /// v2 update contract requires `version` and `body`, so the current version
    /// and body are fetched first and re-sent unchanged with the toggled flag.
    /// Footer comments have no resolution state, so this is inline-only.
    pub async fn set_inline_comment_resolved(
        &self,
        comment_id: &str,
        resolved: bool,
    ) -> Result<()> {
        let (version, current_body) = self
            .get_comment_version_and_body(comment_id, CommentKind::Inline)
            .await?;

        let request = ConfluenceUpdateCommentRequest {
            version: ConfluenceUpdateVersion {
                number: version + 1,
                message: None,
            },
            body: ConfluenceUpdateBody {
                representation: "atlas_doc_format".to_string(),
                value: current_body,
            },
            resolved: Some(resolved),
        };

        let url = format!(
            "{}/wiki/api/v2/inline-comments/{}",
            self.client.instance_url(),
            comment_id
        );

        let response = self
            .client
            .put_json(&url, &request)
            .await
            .context("Failed to update Confluence inline comment resolution")?;

        AtlassianClient::ensure_success(response).await?;

        Ok(())
    }

    /// Resolves an inline-comment anchor by counting `anchor_text` occurrences
    /// in the live page body.
    ///
    /// `match_index_1based` is what the user typed (1-based) and is `None` if
    /// they omitted the flag. The returned [`InlineAnchor`] is ready to hand to
    /// [`Self::add_inline_page_comment`].
    ///
    /// # Errors
    ///
    /// - The anchor text does not appear on the page.
    /// - The text appears more than once and no `--match-index` was supplied.
    /// - The supplied `--match-index` is outside `1..=match_count`.
    pub async fn resolve_anchor(
        &self,
        page_id: &str,
        anchor_text: &str,
        match_index_1based: Option<usize>,
    ) -> Result<InlineAnchor> {
        let page = self.get_content(page_id).await?;

        let plain = match &page.body_adf {
            Some(adf_value) => {
                let adf: AdfDocument = serde_json::from_value(adf_value.clone())
                    .context("Failed to parse page ADF for anchor resolution")?;
                crate::atlassian::convert::adf_to_plain_text(&adf)
            }
            None => String::new(),
        };

        let match_count = count_non_overlapping(&plain, anchor_text);
        resolve_anchor_indices(anchor_text, match_count, match_index_1based, page_id)
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

            let resp: ConfluenceLabelsResponse = AtlassianClient::parse_json(
                AtlassianClient::ensure_success(response).await?,
                "Failed to parse labels response",
            )
            .await?;

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

        AtlassianClient::ensure_success(response).await?;

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

        AtlassianClient::ensure_success(response).await?;

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

        let page: ConfluencePageResponse = AtlassianClient::parse_json(
            AtlassianClient::ensure_success(response).await?,
            "Failed to parse Confluence page response",
        )
        .await?;

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

            let resp: ConfluenceVersionsResponse = AtlassianClient::parse_json(
                AtlassianClient::ensure_success(response).await?,
                "Failed to parse Confluence versions response",
            )
            .await?;

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

    /// Uploads an attachment to a Confluence page from a local file path.
    ///
    /// Streams the file body — the file is never fully buffered in memory.
    /// Sends `X-Atlassian-Token: no-check` (Atlassian convention for
    /// state-changing multipart endpoints).
    ///
    /// Does not retry on 429: see [`AtlassianClient::post_multipart`].
    pub async fn upload_attachment(
        &self,
        page_id: &str,
        file_path: &Path,
        filename: Option<&str>,
        comment: Option<&str>,
        minor_edit: bool,
    ) -> Result<ConfluenceAttachment> {
        let metadata = tokio::fs::metadata(file_path)
            .await
            .with_context(|| format!("Failed to read file metadata for {}", file_path.display()))?;
        let size = metadata.len();
        let file = tokio::fs::File::open(file_path)
            .await
            .with_context(|| format!("Failed to open {}", file_path.display()))?;

        let resolved_name = filename
            .map(str::to_string)
            .or_else(|| {
                file_path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
            })
            .ok_or_else(|| anyhow::anyhow!("File path has no filename component"))?;

        let mime = mime_guess::from_path(file_path).first_or_octet_stream();

        let stream = ReaderStream::new(file);
        let body = reqwest::Body::wrap_stream(stream);

        let part = reqwest::multipart::Part::stream_with_length(body, size)
            .file_name(resolved_name.clone())
            .mime_str(mime.essence_str())
            .with_context(|| format!("Invalid MIME type for {}", file_path.display()))?;

        let mut form = reqwest::multipart::Form::new().part("file", part);
        if let Some(c) = comment {
            form = form.text("comment", c.to_string());
        }
        form = form.text("minorEdit", if minor_edit { "true" } else { "false" });

        // Attachment creation is v1-only: the v2 API has no
        // attachment-creation endpoint, and POSTing to
        // `/wiki/api/v2/pages/{id}/attachments` returns HTTP 405.
        let url = format!(
            "{}/wiki/rest/api/content/{}/child/attachment",
            self.client.instance_url(),
            page_id
        );

        let response = self
            .client
            .post_multipart(&url, form, &[("X-Atlassian-Token", "no-check")])
            .await?;

        let resp: ConfluenceV1AttachmentResponse = AtlassianClient::parse_json(
            AtlassianClient::ensure_success(response).await?,
            "Failed to parse upload attachment response",
        )
        .await?;

        let entry = resp
            .results
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("Upload response contained no attachment"))?;
        Ok(entry.into())
    }

    /// Uploads a **new binary version** of an existing attachment, bumping its
    /// version rather than creating a second attachment.
    ///
    /// Mirrors [`Self::upload_attachment`]'s multipart machinery but targets the
    /// per-attachment `.../child/attachment/{attachmentId}/data` endpoint (v1
    /// only, like attachment creation — the v2 API has no attachment-mutation
    /// endpoint). Returns the updated attachment metadata.
    pub async fn update_attachment(
        &self,
        page_id: &str,
        attachment_id: &str,
        file_path: &Path,
        filename: Option<&str>,
        comment: Option<&str>,
        minor_edit: bool,
    ) -> Result<ConfluenceAttachment> {
        let metadata = tokio::fs::metadata(file_path)
            .await
            .with_context(|| format!("Failed to read file metadata for {}", file_path.display()))?;
        let size = metadata.len();
        let file = tokio::fs::File::open(file_path)
            .await
            .with_context(|| format!("Failed to open {}", file_path.display()))?;

        let resolved_name = filename
            .map(str::to_string)
            .or_else(|| {
                file_path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
            })
            .ok_or_else(|| anyhow::anyhow!("File path has no filename component"))?;

        let mime = mime_guess::from_path(file_path).first_or_octet_stream();

        let stream = ReaderStream::new(file);
        let body = reqwest::Body::wrap_stream(stream);

        let part = reqwest::multipart::Part::stream_with_length(body, size)
            .file_name(resolved_name)
            .mime_str(mime.essence_str())
            .with_context(|| format!("Invalid MIME type for {}", file_path.display()))?;

        let mut form = reqwest::multipart::Form::new().part("file", part);
        if let Some(c) = comment {
            form = form.text("comment", c.to_string());
        }
        form = form.text("minorEdit", if minor_edit { "true" } else { "false" });

        let url = format!(
            "{}/wiki/rest/api/content/{}/child/attachment/{}/data",
            self.client.instance_url(),
            page_id,
            attachment_id
        );

        let response = self
            .client
            .post_multipart(&url, form, &[("X-Atlassian-Token", "no-check")])
            .await?;

        // The `.../data` endpoint returns the updated attachment as a bare
        // Content object (not the `{results: [...]}` envelope the create
        // endpoint returns).
        let entry: ConfluenceV1AttachmentEntry = AtlassianClient::parse_json(
            AtlassianClient::ensure_success(response).await?,
            "Failed to parse update attachment response",
        )
        .await?;
        Ok(entry.into())
    }

    /// Lists attachments on a Confluence page (one page at a time).
    ///
    /// Unlike other v2 list helpers in this module, this does *not*
    /// auto-drain pagination: pass [`ConfluenceAttachmentPage::next_cursor`]
    /// back as `cursor` to fetch the next page.
    pub async fn list_attachments(
        &self,
        page_id: &str,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<ConfluenceAttachmentPage> {
        let mut url = format!(
            "{}/wiki/api/v2/pages/{}/attachments?limit={}",
            self.client.instance_url(),
            page_id,
            limit,
        );
        if let Some(c) = cursor {
            url.push_str("&cursor=");
            url.push_str(&urlencoding(c));
        }

        let response = self
            .client
            .get_json(&url)
            .await
            .context("Failed to fetch page attachments")?;

        let resp: ConfluenceAttachmentsResponse = AtlassianClient::parse_json(
            AtlassianClient::ensure_success(response).await?,
            "Failed to parse attachments response",
        )
        .await?;

        let next_cursor = resp
            .links
            .and_then(|l| l.next)
            .and_then(|next_path| extract_cursor_from_next(&next_path));

        let results = resp.results.into_iter().map(Into::into).collect();

        Ok(ConfluenceAttachmentPage {
            results,
            next_cursor,
        })
    }

    /// Deletes an attachment by ID.
    ///
    /// When `purge` is true, permanently purges (requires space admin);
    /// otherwise the attachment is moved to trash.
    pub async fn delete_attachment(&self, attachment_id: &str, purge: bool) -> Result<()> {
        let mut url = format!(
            "{}/wiki/api/v2/attachments/{}",
            self.client.instance_url(),
            attachment_id
        );
        if purge {
            url.push_str("?purge=true");
        }

        let response = self.client.delete(&url).await?;

        AtlassianClient::ensure_success(response).await?;

        Ok(())
    }

    /// Fetches metadata for a single attachment by ID (v2 API).
    ///
    /// Returns the same [`ConfluenceAttachment`] shape as
    /// [`ConfluenceApi::list_attachments`], including the `download_url`
    /// needed by [`ConfluenceApi::download_attachment_bytes`].
    pub async fn get_attachment(&self, attachment_id: &str) -> Result<ConfluenceAttachment> {
        let url = format!(
            "{}/wiki/api/v2/attachments/{}",
            self.client.instance_url(),
            attachment_id
        );

        let response = self
            .client
            .get_json(&url)
            .await
            .context("Failed to fetch attachment metadata")?;

        let entry: ConfluenceAttachmentEntry = AtlassianClient::parse_json(
            AtlassianClient::ensure_success(response).await?,
            "Failed to parse attachment response",
        )
        .await?;
        Ok(entry.into())
    }

    /// Downloads the binary content of an attachment whose metadata is
    /// already in hand (e.g. from [`ConfluenceApi::list_attachments`]).
    ///
    /// The v1/v2 APIs report `download_url` as a path relative to the
    /// Confluence context root; it is resolved against the instance URL and
    /// fetched via the shared client, which follows the media-CDN redirect.
    pub async fn download_attachment_bytes(
        &self,
        attachment: &ConfluenceAttachment,
    ) -> Result<Vec<u8>> {
        let link = attachment.download_url.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "Attachment {} ({}) has no download URL",
                attachment.id,
                attachment.title
            )
        })?;
        let url = resolve_attachment_download_url(self.client.instance_url(), link);
        self.client.get_bytes(&url).await
    }

    /// Fetches an attachment's metadata by ID, then downloads its binary.
    ///
    /// Convenience for the single-attachment download path (CLI/MCP) where
    /// only the ID is known; the fan-out path uses
    /// [`ConfluenceApi::download_attachment_bytes`] directly to avoid an
    /// extra metadata round-trip per attachment.
    pub async fn download_attachment(
        &self,
        attachment_id: &str,
    ) -> Result<(ConfluenceAttachment, Vec<u8>)> {
        let attachment = self.get_attachment(attachment_id).await?;
        let bytes = self.download_attachment_bytes(&attachment).await?;
        Ok((attachment, bytes))
    }

    /// Fetches a Confluence page pinned to a specific version number.
    ///
    /// Like [`AtlassianApi::get_content`] but returns the historical
    /// snapshot at `version` rather than the current head. Used by the
    /// version-comparison tooling to fetch each side of the diff
    /// independently — Confluence stores versions as immutable snapshots.
    pub async fn get_page_at_version(&self, id: &str, version: u32) -> Result<ContentItem> {
        let url = format!(
            "{}/wiki/api/v2/pages/{}?body-format=atlas_doc_format&version={}",
            self.client.instance_url(),
            id,
            version
        );

        let response = self
            .client
            .get_json(&url)
            .await
            .context("Failed to fetch Confluence page version")?;

        let page: ConfluencePageResponse = AtlassianClient::parse_json(
            AtlassianClient::ensure_success(response).await?,
            "Failed to parse Confluence page response",
        )
        .await?;

        debug!(
            page_id = page.id,
            version,
            title = page.title,
            "Fetched Confluence page at specific version"
        );

        let body_adf = if let Some(body) = &page.body {
            if let Some(atlas_doc) = &body.atlas_doc_format {
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
    }
}

/// Minimal application/x-www-form-urlencoded encoder for query-param values.
///
/// Only escapes the small set of characters that would otherwise corrupt the
/// query string (`& = + % # space`). Cursor values returned by Confluence are
/// opaque base64-ish blobs so this is sufficient.
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("%26"),
            '=' => out.push_str("%3D"),
            '+' => out.push_str("%2B"),
            '%' => out.push_str("%25"),
            '#' => out.push_str("%23"),
            ' ' => out.push_str("%20"),
            _ => out.push(c),
        }
    }
    out
}

/// Resolves a Confluence attachment download link to an absolute URL.
///
/// The v1/v2 APIs report `downloadLink` as a path relative to the Confluence
/// context root (e.g. `/download/attachments/123/foo.png?...`). Absolute URLs
/// pass through unchanged; root-relative paths that already carry the `/wiki`
/// context prefix are joined to the bare instance origin; all other
/// root-relative (and bare-relative) paths get the `/wiki` prefix added.
fn resolve_attachment_download_url(instance_url: &str, link: &str) -> String {
    if link.starts_with("http://") || link.starts_with("https://") {
        return link.to_string();
    }
    let base = instance_url.trim_end_matches('/');
    if link == "/wiki" || link.starts_with("/wiki/") {
        format!("{base}{link}")
    } else if let Some(rest) = link.strip_prefix('/') {
        format!("{base}/wiki/{rest}")
    } else {
        format!("{base}/wiki/{link}")
    }
}

/// Extracts the `cursor` query parameter value from a `_links.next` URL or path.
fn extract_cursor_from_next(next: &str) -> Option<String> {
    let query_start = next.find('?')?;
    let query = &next[query_start + 1..];
    for pair in query.split('&') {
        let mut it = pair.splitn(2, '=');
        let key = it.next()?;
        let value = it.next().unwrap_or("");
        if key == "cursor" {
            return Some(percent_decode(value));
        }
    }
    None
}

/// Decodes a single `%xx`-style percent-encoded string back to UTF-8.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push(((hi << 4) | lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Resolves a user-supplied version reference against a list of
/// [`PageVersion`] records returned by [`ConfluenceApi::list_page_versions`].
///
/// Accepts:
/// - `"latest"` — the newest known version (`versions[0].number`).
/// - `"previous"` — the version immediately before `relative_to`.
/// - `"v-N"` (e.g. `"v-2"`) — the version `relative_to - N`.
/// - Numeric (`"5"`) — that exact version; must be present in `versions`.
/// - ISO 8601 date — the most recent version whose `created_at <=` the
///   given date. Detected when the input contains `-` or `T`.
///
/// `relative_to` anchors `"previous"` and `"v-N"`. Pass the resolved `to`
/// version when resolving `from`, so `previous` always means "one before
/// `to`" regardless of what `to` itself is.
///
/// `versions` must be ordered newest-first (the natural shape returned by
/// `list_page_versions`).
pub fn resolve_version(raw: &str, versions: &[PageVersion], relative_to: u32) -> Result<u32> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        anyhow::bail!("version reference must not be empty");
    }
    if versions.is_empty() {
        anyhow::bail!("page has no versions");
    }

    if trimmed.eq_ignore_ascii_case("latest") {
        return Ok(versions[0].number);
    }
    if trimmed.eq_ignore_ascii_case("previous") {
        return offset_from(relative_to, 1, versions);
    }
    if let Some(rest) = trimmed
        .strip_prefix("v-")
        .or_else(|| trimmed.strip_prefix("V-"))
    {
        let offset: u32 = rest.parse().with_context(|| {
            format!("Invalid relative version offset \"{trimmed}\"; expected v-N with N > 0")
        })?;
        if offset == 0 {
            anyhow::bail!("Relative version offset must be > 0; got \"{trimmed}\"");
        }
        return offset_from(relative_to, offset, versions);
    }
    if trimmed.chars().all(|c| c.is_ascii_digit()) {
        let n: u32 = trimmed
            .parse()
            .with_context(|| format!("Invalid version number \"{trimmed}\""))?;
        if !versions.iter().any(|v| v.number == n) {
            anyhow::bail!("Version {n} not found in page history");
        }
        return Ok(n);
    }
    if trimmed.contains('-') || trimmed.contains('T') {
        // ISO 8601 date: pick the latest version with created_at <= date.
        // `versions` is newest-first, so the first match wins.
        for v in versions {
            if !v.created_at.is_empty() && v.created_at.as_str() <= trimmed {
                return Ok(v.number);
            }
        }
        anyhow::bail!("No version found at or before \"{trimmed}\"");
    }

    anyhow::bail!(
        "Could not parse \"{trimmed}\" as a version reference; expected \
         \"latest\", \"previous\", \"v-N\", a numeric version (e.g. \"5\"), \
         or an ISO 8601 date (e.g. \"2026-01-01T00:00:00Z\")"
    )
}

fn offset_from(anchor: u32, offset: u32, versions: &[PageVersion]) -> Result<u32> {
    if anchor <= offset {
        anyhow::bail!(
            "Cannot resolve v-{offset} relative to version {anchor}: out of range \
             (would be {} or lower)",
            i64::from(anchor) - i64::from(offset)
        );
    }
    let target = anchor - offset;
    if !versions.iter().any(|v| v.number == target) {
        anyhow::bail!(
            "Version {target} not found in page history \
             (resolved from anchor {anchor} - {offset})"
        );
    }
    Ok(target)
}

/// Counts non-overlapping occurrences of `needle` in `haystack`.
///
/// An empty `needle` returns 0 so anchor resolution rejects it as "not found".
fn count_non_overlapping(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let mut count = 0;
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        count += 1;
        start += pos + needle.len();
    }
    count
}

/// Picks an [`InlineAnchor`] match index given the live page's match count
/// and the user's 1-based `--match-index` (if any).
///
/// See [`ConfluenceApi::resolve_anchor`] for the full anchor-resolution
/// contract; this is the pure-logic half, factored out for direct unit
/// testing without an HTTP fixture.
fn resolve_anchor_indices(
    anchor_text: &str,
    match_count: usize,
    match_index_1based: Option<usize>,
    page_id: &str,
) -> Result<InlineAnchor> {
    if match_count == 0 {
        anyhow::bail!(
            "anchor text {anchor_text:?} not found on page {page_id}; \
             cannot create inline comment"
        );
    }
    let index = if let Some(i) = match_index_1based {
        if i == 0 || i > match_count {
            anyhow::bail!(
                "--match-index {i} out of range; anchor text {anchor_text:?} appears \
                 {match_count} time(s) on page {page_id} (valid range: 1..={match_count})"
            );
        }
        i - 1
    } else {
        if match_count > 1 {
            anyhow::bail!(
                "anchor text {anchor_text:?} appears {match_count} times on page {page_id}; \
                 specify --match-index <1..={match_count}> to choose which occurrence"
            );
        }
        0
    };
    Ok(InlineAnchor {
        text: anchor_text.to_string(),
        match_index: index,
        match_count,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::atlassian::confluence_types::{ConfluenceLabelEntry, ConfluenceVersionEntry};

    #[test]
    fn confluence_api_backend_name() {
        let client =
            AtlassianClient::new("https://org.atlassian.net", "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        assert_eq!(api.backend_name(), "confluence");
    }

    // ── CommentKind ────────────────────────────────────────────────

    #[test]
    fn comment_kind_endpoint_segment() {
        assert_eq!(CommentKind::Footer.endpoint_segment(), "footer-comments");
        assert_eq!(CommentKind::Inline.endpoint_segment(), "inline-comments");
    }

    #[test]
    fn comment_kind_serializes_lowercase() {
        let footer = serde_json::to_string(&CommentKind::Footer).unwrap();
        let inline = serde_json::to_string(&CommentKind::Inline).unwrap();
        assert_eq!(footer, "\"footer\"");
        assert_eq!(inline, "\"inline\"");
    }

    #[test]
    fn comment_kind_display() {
        assert_eq!(CommentKind::Footer.to_string(), "footer");
        assert_eq!(CommentKind::Inline.to_string(), "inline");
    }

    // ── count_non_overlapping ──────────────────────────────────────

    #[test]
    fn count_non_overlapping_no_matches() {
        assert_eq!(count_non_overlapping("hello world", "foo"), 0);
    }

    #[test]
    fn count_non_overlapping_single_match() {
        assert_eq!(count_non_overlapping("hello world", "world"), 1);
    }

    #[test]
    fn count_non_overlapping_multiple_matches() {
        assert_eq!(count_non_overlapping("foo bar foo baz foo", "foo"), 3);
    }

    #[test]
    fn count_non_overlapping_is_non_overlapping() {
        // "aa" in "aaaa" should be 2, not 3.
        assert_eq!(count_non_overlapping("aaaa", "aa"), 2);
    }

    #[test]
    fn count_non_overlapping_empty_needle_is_zero() {
        // Anchor resolution must treat an empty anchor as "not found".
        assert_eq!(count_non_overlapping("anything", ""), 0);
    }

    // ── resolve_anchor_indices ─────────────────────────────────────

    #[test]
    fn resolve_anchor_indices_not_found_errors() {
        let err = resolve_anchor_indices("missing", 0, None, "PAGE").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not found"), "got: {msg}");
        assert!(msg.contains("PAGE"), "got: {msg}");
    }

    #[test]
    fn resolve_anchor_indices_unique_match_uses_zero() {
        let a = resolve_anchor_indices("phrase", 1, None, "PAGE").unwrap();
        assert_eq!(a.match_index, 0);
        assert_eq!(a.match_count, 1);
        assert_eq!(a.text, "phrase");
    }

    #[test]
    fn resolve_anchor_indices_ambiguous_without_match_index_errors() {
        let err = resolve_anchor_indices("phrase", 3, None, "PAGE").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("appears 3 times"), "got: {msg}");
        assert!(msg.contains("--match-index"), "got: {msg}");
    }

    #[test]
    fn resolve_anchor_indices_ambiguous_with_valid_match_index() {
        let a = resolve_anchor_indices("phrase", 3, Some(2), "PAGE").unwrap();
        assert_eq!(a.match_index, 1); // 2-based -> 1-based zero-indexed
        assert_eq!(a.match_count, 3);
    }

    #[test]
    fn resolve_anchor_indices_match_index_zero_rejected() {
        let err = resolve_anchor_indices("phrase", 3, Some(0), "PAGE").unwrap_err();
        assert!(err.to_string().contains("out of range"));
    }

    #[test]
    fn resolve_anchor_indices_match_index_too_large_rejected() {
        let err = resolve_anchor_indices("phrase", 3, Some(4), "PAGE").unwrap_err();
        assert!(err.to_string().contains("out of range"));
    }

    // ── resolve_anchor (HTTP) ──────────────────────────────────────

    async fn mock_page_with_text(server: &wiremock::MockServer, id: &str, text: &str) {
        let adf_value = format!(
            "{{\"version\":1,\"type\":\"doc\",\"content\":[{{\"type\":\"paragraph\",\"content\":[{{\"type\":\"text\",\"text\":{}}}]}}]}}",
            serde_json::Value::String(text.to_string())
        );
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!("/wiki/api/v2/pages/{id}")))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": id,
                    "title": "Mock",
                    "status": "current",
                    "spaceId": "98",
                    "version": {"number": 1},
                    "body": {"atlas_doc_format": {"value": adf_value}}
                })),
            )
            .mount(server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"key": "ENG"})),
            )
            .mount(server)
            .await;
    }

    fn mock_confluence_api(server: &wiremock::MockServer) -> ConfluenceApi {
        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        ConfluenceApi::new(client)
    }

    #[tokio::test]
    async fn resolve_anchor_unique_match_succeeds() {
        let server = wiremock::MockServer::start().await;
        mock_page_with_text(&server, "12345", "the unique anchor phrase appears here").await;
        let api = mock_confluence_api(&server);
        let anchor = api
            .resolve_anchor("12345", "the unique anchor phrase", None)
            .await
            .unwrap();
        assert_eq!(anchor.match_count, 1);
        assert_eq!(anchor.match_index, 0);
    }

    #[tokio::test]
    async fn resolve_anchor_not_found_errors() {
        let server = wiremock::MockServer::start().await;
        mock_page_with_text(&server, "12345", "nothing relevant").await;
        let api = mock_confluence_api(&server);
        let err = api
            .resolve_anchor("12345", "missing", None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn resolve_anchor_on_body_less_page_errors_with_not_found() {
        // A Confluence page can come back with a null body; the resolver
        // must treat it as plain-text "" rather than panicking.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "12345",
                    "title": "Empty",
                    "status": "current",
                    "spaceId": "98",
                    "version": {"number": 1},
                    "body": null
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"key": "ENG"})),
            )
            .mount(&server)
            .await;

        let api = mock_confluence_api(&server);
        let err = api
            .resolve_anchor("12345", "anything", None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    // ── add_inline_page_comment ────────────────────────────────────

    #[tokio::test]
    async fn add_inline_page_comment_posts_anchor_payload() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/wiki/api/v2/inline-comments"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "pageId": "12345",
                "inlineCommentProperties": {
                    "textSelection": "phrase",
                    "textSelectionMatchCount": 2,
                    "textSelectionMatchIndex": 1
                }
            })))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "ic1"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let api = mock_confluence_api(&server);
        let adf = ValidatedAdfDocument::empty();
        let anchor = InlineAnchor {
            text: "phrase".to_string(),
            match_index: 1,
            match_count: 2,
        };
        api.add_inline_page_comment("12345", &adf, &anchor)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn add_inline_page_comment_propagates_http_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/wiki/api/v2/inline-comments"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("upstream"))
            .mount(&server)
            .await;

        let api = mock_confluence_api(&server);
        let adf = ValidatedAdfDocument::empty();
        let anchor = InlineAnchor {
            text: "phrase".to_string(),
            match_index: 0,
            match_count: 1,
        };
        let err = api
            .add_inline_page_comment("12345", &adf, &anchor)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    // ── get_page_inline_comments (properties) ──────────────────────

    #[tokio::test]
    async fn get_page_inline_comments_captures_marker_and_original_selection() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/pages/12345/inline-comments",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{
                        "id": "ic1",
                        "version": {"authorId": "alice", "createdAt": "2026-01-01T00:00:00Z"},
                        "properties": {
                            "inlineMarkerRef": "marker-abc",
                            "inlineOriginalSelection": "the original highlight"
                        }
                    }]
                })),
            )
            .mount(&server)
            .await;

        let api = mock_confluence_api(&server);
        let comments = api.get_page_inline_comments("12345").await.unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].inline_marker_ref.as_deref(), Some("marker-abc"));
        assert_eq!(
            comments[0].inline_original_selection.as_deref(),
            Some("the original highlight")
        );
    }

    #[tokio::test]
    async fn get_page_comments_footer_has_no_inline_properties() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/pages/12345/footer-comments",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{
                        "id": "fc1",
                        "version": {"authorId": "bob", "createdAt": "2026-01-01T00:00:00Z"}
                    }]
                })),
            )
            .mount(&server)
            .await;

        let api = mock_confluence_api(&server);
        let comments = api.get_page_comments("12345").await.unwrap();
        assert_eq!(comments.len(), 1);
        assert!(comments[0].inline_marker_ref.is_none());
        assert!(comments[0].inline_original_selection.is_none());
    }

    // ── get_comment_replies ────────────────────────────────────────

    #[tokio::test]
    async fn get_comment_replies_inline_kind() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/inline-comments/abc/children",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"id": "r1", "version": {"authorId": "alice", "createdAt": "2026-01-01T00:00:00Z"}}
                    ]
                })),
            )
            .mount(&server)
            .await;

        let api = mock_confluence_api(&server);
        let replies = api
            .get_comment_replies("abc", CommentKind::Inline)
            .await
            .unwrap();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].kind, CommentKind::Inline);
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

    /// Builds a small ADF document containing `expand` nested inside `panel`,
    /// which violates Confluence's content model and should trigger the
    /// HTTP-500 diagnosis path. The validator emits a single violation at
    /// path `/0/0` (`expand` is the first child of the first top-level node).
    fn adf_with_panel_expand() -> AdfDocument {
        use crate::atlassian::adf::AdfNode;
        AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode {
                node_type: "panel".to_string(),
                attrs: Some(serde_json::json!({"panelType": "info"})),
                content: Some(vec![AdfNode {
                    node_type: "expand".to_string(),
                    attrs: Some(serde_json::json!({"title": "details"})),
                    content: Some(vec![AdfNode::paragraph(vec![AdfNode::text("x")])]),
                    text: None,
                    marks: None,
                    local_id: None,
                    parameters: None,
                }]),
                text: None,
                marks: None,
                local_id: None,
                parameters: None,
            }],
        }
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

        let adf = ValidatedAdfDocument::empty();
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

        let adf = ValidatedAdfDocument::empty();
        let err = api.update_content("12345", &adf, None).await.unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn update_content_500_with_panel_expand_diagnoses() {
        use crate::atlassian::api::AtlassianApi;

        let (server, api) = setup_confluence_mock().await;

        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string(
                "{\"errors\":[{\"status\":500,\"code\":\"INTERNAL_SERVER_ERROR\"}]}",
            ))
            .mount(&server)
            .await;

        let adf = ValidatedAdfDocument::trust(adf_with_panel_expand());
        let err = api.update_content("12345", &adf, None).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Confluence API returned HTTP 500 (Internal Server Error)"),
            "missing 500 header in: {msg}"
        );
        assert!(
            msg.contains("Diagnosis:"),
            "missing Diagnosis line in: {msg}"
        );
        assert!(msg.contains("`expand`"), "missing child name in: {msg}");
        assert!(msg.contains("`panel`"), "missing parent name in: {msg}");
        assert!(msg.contains("Hint:"), "missing Hint line in: {msg}");
        assert!(
            !msg.contains("INTERNAL_SERVER_ERROR"),
            "raw response body should not be in user-facing message: {msg}"
        );
    }

    #[tokio::test]
    async fn update_content_500_without_violation_falls_back() {
        use crate::atlassian::api::AtlassianApi;

        let (server, api) = setup_confluence_mock().await;

        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345"))
            .respond_with(
                wiremock::ResponseTemplate::new(500).set_body_string("Internal Server Error"),
            )
            .mount(&server)
            .await;

        // Empty (well-formed) document — no schema violations to surface.
        let adf = ValidatedAdfDocument::empty();
        let err = api.update_content("12345", &adf, None).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("HTTP 500"), "missing status in: {msg}");
        assert!(
            msg.contains("Internal Server Error"),
            "fallback should include raw body: {msg}"
        );
        assert!(
            !msg.contains("Diagnosis:"),
            "fallback should not include diagnosis: {msg}"
        );
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
    async fn list_spaces_no_filters_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .and(wiremock::matchers::query_param("limit", "25"))
            .and(wiremock::matchers::query_param_is_missing("keys"))
            .and(wiremock::matchers::query_param_is_missing("type"))
            .and(wiremock::matchers::query_param_is_missing("status"))
            .and(wiremock::matchers::query_param_is_missing("cursor"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {
                            "id": "100",
                            "key": "ENG",
                            "name": "Engineering",
                            "type": "global",
                            "status": "current",
                            "homepageId": "200"
                        },
                        {
                            "id": "101",
                            "key": "OPS",
                            "name": "Operations",
                            "type": "global",
                            "status": "archived"
                        }
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let page = api.list_spaces(&[], None, None, None, 25).await.unwrap();
        assert_eq!(page.results.len(), 2);
        assert_eq!(page.results[0].id, "100");
        assert_eq!(page.results[0].key, "ENG");
        assert_eq!(page.results[0].name, "Engineering");
        assert_eq!(page.results[0].type_, "global");
        assert_eq!(page.results[0].status, "current");
        assert_eq!(page.results[0].homepage_id.as_deref(), Some("200"));
        assert_eq!(page.results[1].status, "archived");
        assert!(page.results[1].homepage_id.is_none());
        assert!(page.next_cursor.is_none());
    }

    #[tokio::test]
    async fn list_spaces_with_keys_filter() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .and(wiremock::matchers::query_param("keys", "ENG,DEV"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let page = api
            .list_spaces(&["ENG", "DEV"], None, None, None, 25)
            .await
            .unwrap();
        assert!(page.results.is_empty());
    }

    #[tokio::test]
    async fn list_spaces_with_type_and_status() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .and(wiremock::matchers::query_param("type", "global"))
            .and(wiremock::matchers::query_param("status", "archived"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let page = api
            .list_spaces(&[], Some("global"), Some("archived"), None, 25)
            .await
            .unwrap();
        assert!(page.results.is_empty());
    }

    #[tokio::test]
    async fn list_spaces_keys_combined_with_type() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .and(wiremock::matchers::query_param("keys", "ENG"))
            .and(wiremock::matchers::query_param("type", "knowledge_base"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let page = api
            .list_spaces(&["ENG"], Some("knowledge_base"), None, None, 25)
            .await
            .unwrap();
        assert!(page.results.is_empty());
    }

    #[tokio::test]
    async fn list_spaces_pagination_round_trip() {
        let server = wiremock::MockServer::start().await;

        // First page returns a _links.next pointing to cursor=PAGE2.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .and(wiremock::matchers::query_param_is_missing("cursor"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{
                        "id": "1", "key": "A", "name": "A",
                        "type": "global", "status": "current"
                    }],
                    "_links": {"next": "/wiki/api/v2/spaces?cursor=PAGE2&limit=25"}
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        // Second page (cursor=PAGE2) returns the next batch with no further next.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .and(wiremock::matchers::query_param("cursor", "PAGE2"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{
                        "id": "2", "key": "B", "name": "B",
                        "type": "global", "status": "current"
                    }]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);

        let first = api.list_spaces(&[], None, None, None, 25).await.unwrap();
        assert_eq!(first.next_cursor.as_deref(), Some("PAGE2"));

        let second = api
            .list_spaces(&[], None, None, Some("PAGE2"), 25)
            .await
            .unwrap();
        assert_eq!(second.results.len(), 1);
        assert_eq!(second.results[0].id, "2");
        assert!(second.next_cursor.is_none());
    }

    #[tokio::test]
    async fn list_spaces_homepage_id_absent() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{
                        "id": "1", "key": "A", "name": "A",
                        "type": "global", "status": "current"
                    }]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let page = api.list_spaces(&[], None, None, None, 25).await.unwrap();
        assert!(page.results[0].homepage_id.is_none());
    }

    #[tokio::test]
    async fn list_spaces_api_error_403() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let err = api
            .list_spaces(&[], None, None, None, 25)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn list_space_pages_no_filters_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98765/pages"))
            .and(wiremock::matchers::query_param("limit", "25"))
            .and(wiremock::matchers::query_param_is_missing("status"))
            .and(wiremock::matchers::query_param_is_missing("sort"))
            .and(wiremock::matchers::query_param_is_missing("cursor"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {
                            "id": "777",
                            "title": "Home",
                            "status": "current",
                            "parentId": null,
                            "authorId": "u1",
                            "createdAt": "2024-01-02T03:04:05Z"
                        },
                        {
                            "id": "888",
                            "title": "Other",
                            "status": "current",
                            "parentId": "777",
                            "authorId": "u2",
                            "createdAt": "2024-02-02T03:04:05Z"
                        }
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let page = api
            .list_space_pages("98765", None, None, None, 25)
            .await
            .unwrap();
        assert_eq!(page.results.len(), 2);
        assert_eq!(page.results[0].id, "777");
        assert_eq!(page.results[0].title, "Home");
        assert_eq!(page.results[0].status, "current");
        assert!(page.results[0].parent_id.is_none());
        assert_eq!(page.results[0].author_id.as_deref(), Some("u1"));
        assert_eq!(
            page.results[0].created_at.as_deref(),
            Some("2024-01-02T03:04:05Z")
        );
        assert_eq!(page.results[1].parent_id.as_deref(), Some("777"));
        assert!(page.next_cursor.is_none());
    }

    #[tokio::test]
    async fn list_space_pages_with_status_and_sort() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98765/pages"))
            .and(wiremock::matchers::query_param("status", "archived"))
            .and(wiremock::matchers::query_param("sort", "-created-date"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let page = api
            .list_space_pages("98765", Some("archived"), Some("-created-date"), None, 25)
            .await
            .unwrap();
        assert!(page.results.is_empty());
    }

    #[tokio::test]
    async fn list_space_pages_pagination_round_trip() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98765/pages"))
            .and(wiremock::matchers::query_param_is_missing("cursor"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{
                        "id": "1", "title": "A", "status": "current"
                    }],
                    "_links": {
                        "next": "/wiki/api/v2/spaces/98765/pages?cursor=PAGE2&limit=25"
                    }
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98765/pages"))
            .and(wiremock::matchers::query_param("cursor", "PAGE2"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{
                        "id": "2", "title": "B", "status": "current"
                    }]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);

        let first = api
            .list_space_pages("98765", None, None, None, 25)
            .await
            .unwrap();
        assert_eq!(first.next_cursor.as_deref(), Some("PAGE2"));

        let second = api
            .list_space_pages("98765", None, None, Some("PAGE2"), 25)
            .await
            .unwrap();
        assert_eq!(second.results.len(), 1);
        assert_eq!(second.results[0].id, "2");
        assert!(second.next_cursor.is_none());
    }

    #[tokio::test]
    async fn list_space_pages_missing_optional_fields() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98765/pages"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{"id": "1", "title": "Bare"}]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let page = api
            .list_space_pages("98765", None, None, None, 25)
            .await
            .unwrap();
        assert_eq!(page.results.len(), 1);
        assert_eq!(page.results[0].id, "1");
        assert_eq!(page.results[0].status, "");
        assert!(page.results[0].parent_id.is_none());
        assert!(page.results[0].author_id.is_none());
        assert!(page.results[0].created_at.is_none());
    }

    #[tokio::test]
    async fn list_space_pages_api_error_404() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/99999/pages"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let err = api
            .list_space_pages("99999", None, None, None, 25)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn list_space_pages_parse_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98765/pages"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("not json"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let err = api
            .list_space_pages("98765", None, None, None, 25)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Failed to parse"));
    }

    /// Exercises the transport-failure Err branch of `get_json().await?` —
    /// covers the `.context("Failed to list Confluence space pages")?`
    /// propagation when the HTTP request itself can't reach the server.
    /// Uses the reserved `127.0.0.1:1` address (the same trick the dispatch
    /// tests use to provoke connection refused).
    #[tokio::test]
    async fn list_space_pages_transport_error() {
        let client = AtlassianClient::new("http://127.0.0.1:1", "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let err = api
            .list_space_pages("98765", None, None, None, 25)
            .await
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("Failed to list Confluence space pages"));
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
        let adf = ValidatedAdfDocument::empty();
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
        let adf = ValidatedAdfDocument::empty();
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
        let adf = ValidatedAdfDocument::empty();
        let err = api
            .create_page("ENG", "Fail", &adf, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("400"));
    }

    #[tokio::test]
    async fn create_page_500_with_panel_expand_diagnoses() {
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
                wiremock::ResponseTemplate::new(500).set_body_string("Internal Server Error"),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let adf = ValidatedAdfDocument::trust(adf_with_panel_expand());
        let err = api.create_page("ENG", "Bad", &adf, None).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("HTTP 500"), "missing status in: {msg}");
        assert!(
            msg.contains("Diagnosis:"),
            "missing Diagnosis line in: {msg}"
        );
        assert!(msg.contains("`expand`"), "missing child name in: {msg}");
        assert!(msg.contains("`panel`"), "missing parent name in: {msg}");
        assert!(msg.contains("Hint:"), "missing Hint line in: {msg}");
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
    async fn copy_page_returns_new_id() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/copy",
            ))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "destination": {"type": "parent_page", "value": "456"},
                "pageTitle": "Copy of Page",
                "copyAttachments": true,
                "copyPermissions": false,
                "copyProperties": true,
                "copyLabels": true,
                "copyCustomContents": false
            })))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "99999"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let new_id = api.copy_page("12345", "456", "Copy of Page").await.unwrap();
        assert_eq!(new_id, "99999");
    }

    #[tokio::test]
    async fn copy_page_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/copy",
            ))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let err = api.copy_page("12345", "456", "Copy").await.unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn is_watching_content_reports_status() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/user/watch/content/12345",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"watching": true})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let status = api.is_watching_content("12345", None).await.unwrap();
        assert!(status.watching);
    }

    #[tokio::test]
    async fn add_content_watcher_scopes_account_id() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/user/watch/content/12345",
            ))
            .and(wiremock::matchers::query_param("accountId", "acc-1"))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        assert!(api
            .add_content_watcher("12345", Some("acc-1"))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn remove_content_watcher_success() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/user/watch/content/12345",
            ))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        assert!(api.remove_content_watcher("12345", None).await.is_ok());
    }

    #[tokio::test]
    async fn content_watcher_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/user/watch/content/12345",
            ))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let err = api.add_content_watcher("12345", None).await.unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn get_content_restrictions_returns_raw_json() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/restriction",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{"operation": "read"}]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let value = api.get_content_restrictions("12345").await.unwrap();
        assert_eq!(value["results"][0]["operation"], "read");
    }

    #[tokio::test]
    async fn grant_content_restriction_user_puts_by_operation() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/restriction/byOperation/update/user",
            ))
            .and(wiremock::matchers::query_param("accountId", "acc-1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        assert!(api
            .grant_content_restriction("12345", "update", Some("acc-1"), None)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn revoke_content_restriction_group_deletes_by_operation() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/restriction/byOperation/read/group/devs",
            ))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        assert!(api
            .revoke_content_restriction("12345", "read", None, Some("devs"))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn restriction_requires_exactly_one_subject() {
        let client = AtlassianClient::new("http://127.0.0.1:1", "u@t.com", "tok").unwrap();
        let api = ConfluenceApi::new(client);
        assert!(api
            .grant_content_restriction("12345", "read", None, None)
            .await
            .is_err());
        assert!(api
            .grant_content_restriction("12345", "read", Some("a"), Some("g"))
            .await
            .is_err());
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
        let adf = ValidatedAdfDocument::empty();
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
        let adf = ValidatedAdfDocument::empty();
        let err = api.add_page_comment("12345", &adf).await.unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn add_page_comment_500_with_panel_expand_diagnoses() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/wiki/api/v2/footer-comments"))
            .respond_with(
                wiremock::ResponseTemplate::new(500).set_body_string("Internal Server Error"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let adf = ValidatedAdfDocument::trust(adf_with_panel_expand());
        let err = api.add_page_comment("12345", &adf).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("HTTP 500"), "missing status in: {msg}");
        assert!(
            msg.contains("Diagnosis:"),
            "missing Diagnosis line in: {msg}"
        );
        assert!(msg.contains("`expand`"), "missing child name in: {msg}");
        assert!(msg.contains("`panel`"), "missing parent name in: {msg}");
        assert!(msg.contains("Hint:"), "missing Hint line in: {msg}");
    }

    // ── update / delete / resolve comment ─────────────────────────

    #[tokio::test]
    async fn update_page_comment_fetches_version_then_puts() {
        let server = wiremock::MockServer::start().await;
        // GET current version.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/footer-comments/555"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "555",
                    "version": {"number": 3},
                    "body": {"atlas_doc_format": {"value": "{}"}}
                })),
            )
            .expect(1)
            .mount(&server)
            .await;
        // PUT the new body with version 4.
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/wiki/api/v2/footer-comments/555"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "version": {"number": 4, "message": null},
                "body": {"representation": "atlas_doc_format", "value": "{\"version\":1,\"type\":\"doc\",\"content\":[]}"}
            })))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "555"})))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let adf = ValidatedAdfDocument::empty();
        api.update_page_comment("555", CommentKind::Footer, &adf)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn update_page_comment_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/footer-comments/555"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "version": {"number": 1}
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/wiki/api/v2/footer-comments/555"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let adf = ValidatedAdfDocument::empty();
        let err = api
            .update_page_comment("555", CommentKind::Footer, &adf)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn delete_page_comment_success() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/wiki/api/v2/inline-comments/555"))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        assert!(api
            .delete_page_comment("555", CommentKind::Inline)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn delete_page_comment_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/wiki/api/v2/footer-comments/999"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let err = api
            .delete_page_comment("999", CommentKind::Footer)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn set_inline_comment_resolved_sends_flag_with_current_body() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/inline-comments/555"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "version": {"number": 2},
                    "body": {"atlas_doc_format": {"value": "{\"orig\":true}"}}
                })),
            )
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/wiki/api/v2/inline-comments/555"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "version": {"number": 3, "message": null},
                "body": {"representation": "atlas_doc_format", "value": "{\"orig\":true}"},
                "resolved": true
            })))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "555"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        api.set_inline_comment_resolved("555", true).await.unwrap();
    }

    #[tokio::test]
    async fn set_inline_comment_resolved_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/inline-comments/555"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let err = api
            .set_inline_comment_resolved("555", false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
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

    // ── attachments ───────────────────────────────────────────────

    #[test]
    fn extract_cursor_extracts_value() {
        let next = "/wiki/api/v2/pages/12345/attachments?cursor=abc123&limit=25";
        assert_eq!(extract_cursor_from_next(next), Some("abc123".to_string()));
    }

    #[test]
    fn extract_cursor_returns_none_when_absent() {
        assert_eq!(
            extract_cursor_from_next("/wiki/api/v2/pages/12345/attachments?limit=25"),
            None
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
    fn extract_cursor_decodes_percent_encoded() {
        assert_eq!(
            extract_cursor_from_next("/wiki/api/v2/pages/1/attachments?cursor=foo%3Dbar"),
            Some("foo=bar".to_string())
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

    #[test]
    fn urlencoding_escapes_reserved_chars() {
        assert_eq!(urlencoding("a=b&c+d %e#"), "a%3Db%26c%2Bd%20%25e%23");
    }

    #[tokio::test]
    async fn upload_attachment_success() {
        use tempfile::NamedTempFile;
        use tokio::io::AsyncWriteExt;

        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/child/attachment",
            ))
            .and(wiremock::matchers::header("X-Atlassian-Token", "no-check"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{
                        "id": "att-1",
                        "title": "hello.txt",
                        "extensions": {
                            "mediaType": "text/plain",
                            "fileSize": 13,
                            "fileId": "f-1"
                        },
                        "version": {"number": 1},
                        "container": {"id": "12345"},
                        "_links": {"download": "/download/att-1"}
                    }]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let mut tmp = tokio::fs::File::from_std(NamedTempFile::new().unwrap().into_file());
        tmp.write_all(b"hello, world!").await.unwrap();
        tmp.flush().await.unwrap();

        // Re-create a path-backed temp file (NamedTempFile after into_file would be unlinked).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hello.txt");
        tokio::fs::write(&path, b"hello, world!").await.unwrap();

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let attachment = api
            .upload_attachment("12345", &path, None, Some("v1"), false)
            .await
            .unwrap();

        assert_eq!(attachment.id, "att-1");
        assert_eq!(attachment.title, "hello.txt");
        assert_eq!(attachment.media_type.as_deref(), Some("text/plain"));
        assert_eq!(attachment.file_size, Some(13));
        assert_eq!(attachment.version, Some(1));
        // The v1 response nests these under extensions/_links/container;
        // assert they are mapped, not silently dropped to None.
        assert_eq!(attachment.download_url.as_deref(), Some("/download/att-1"));
        assert_eq!(attachment.page_id.as_deref(), Some("12345"));
        assert_eq!(attachment.file_id.as_deref(), Some("f-1"));
    }

    #[tokio::test]
    async fn update_attachment_posts_new_version_to_data_endpoint() {
        let server = wiremock::MockServer::start().await;
        // The `.../data` endpoint returns a bare Content object (no `results`).
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/child/attachment/att-1/data",
            ))
            .and(wiremock::matchers::header("X-Atlassian-Token", "no-check"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "att-1",
                    "title": "hello.txt",
                    "extensions": {"mediaType": "text/plain", "fileSize": 20, "fileId": "f-1"},
                    "version": {"number": 2},
                    "container": {"id": "12345"},
                    "_links": {"download": "/download/att-1"}
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hello.txt");
        tokio::fs::write(&path, b"hello, world! again")
            .await
            .unwrap();

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let attachment = api
            .update_attachment("12345", "att-1", &path, None, Some("v2"), true)
            .await
            .unwrap();
        assert_eq!(attachment.id, "att-1");
        assert_eq!(attachment.version, Some(2));
    }

    #[tokio::test]
    async fn update_attachment_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/child/attachment/att-1/data",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hello.txt");
        tokio::fs::write(&path, b"data").await.unwrap();

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let err = api
            .update_attachment("12345", "att-1", &path, None, None, false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn upload_attachment_page_not_found() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/99999/child/attachment",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.bin");
        tokio::fs::write(&path, b"x").await.unwrap();

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let err = api
            .upload_attachment("99999", &path, None, None, false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn upload_attachment_too_large() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/child/attachment",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(413).set_body_string("Request entity too large"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        tokio::fs::write(&path, b"x").await.unwrap();

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let err = api
            .upload_attachment("12345", &path, None, None, false)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("413"));
        assert!(msg.contains("Request entity too large"));
    }

    #[tokio::test]
    async fn upload_attachment_overrides_filename() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/child/attachment",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{"id": "a", "title": "renamed.png"}]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.bin");
        tokio::fs::write(&path, b"data").await.unwrap();

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let attachment = api
            .upload_attachment("12345", &path, Some("renamed.png"), None, true)
            .await
            .unwrap();
        assert_eq!(attachment.title, "renamed.png");
    }

    // Regression for #1005: the v2 attachments path has no POST handler and
    // returns HTTP 405. Mount it returning 405 *and* the correct v1 path
    // returning success; the upload must succeed, proving it targets v1.
    #[tokio::test]
    async fn upload_attachment_uses_v1_endpoint_not_v2() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/pages/12345/attachments",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(405)
                    .set_body_string(r#"{"errors":[{"status":405,"code":"METHOD_NOT_ALLOWED"}]}"#),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/child/attachment",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{"id": "att-9", "title": "ok.png"}]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ok.png");
        tokio::fs::write(&path, b"data").await.unwrap();

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let attachment = api
            .upload_attachment("12345", &path, None, None, false)
            .await
            .unwrap();
        assert_eq!(attachment.id, "att-9");
    }

    #[tokio::test]
    async fn list_attachments_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/pages/12345/attachments",
            ))
            .and(wiremock::matchers::query_param("limit", "25"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"id": "a1", "title": "one.png", "mediaType": "image/png", "fileSize": 100, "version": {"number": 1}},
                        {"id": "a2", "title": "two.pdf", "mediaType": "application/pdf"}
                    ],
                    "_links": {"next": "/wiki/api/v2/pages/12345/attachments?cursor=NEXT&limit=25"}
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let page = api.list_attachments("12345", None, 25).await.unwrap();
        assert_eq!(page.results.len(), 2);
        assert_eq!(page.results[0].id, "a1");
        assert_eq!(page.results[0].file_size, Some(100));
        assert_eq!(page.next_cursor.as_deref(), Some("NEXT"));
    }

    #[tokio::test]
    async fn list_attachments_no_more_pages() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/pages/12345/attachments",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": []
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let page = api.list_attachments("12345", None, 25).await.unwrap();
        assert!(page.results.is_empty());
        assert!(page.next_cursor.is_none());
    }

    #[tokio::test]
    async fn list_attachments_page_not_found() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/pages/99999/attachments",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let err = api.list_attachments("99999", None, 25).await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn list_attachments_pagination_round_trip() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/pages/12345/attachments",
            ))
            .and(wiremock::matchers::query_param("cursor", "PAGE2"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{"id": "a3", "title": "three.bin"}]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let page = api
            .list_attachments("12345", Some("PAGE2"), 25)
            .await
            .unwrap();
        assert_eq!(page.results.len(), 1);
        assert_eq!(page.results[0].id, "a3");
    }

    #[tokio::test]
    async fn delete_attachment_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/wiki/api/v2/attachments/att-1"))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        assert!(api.delete_attachment("att-1", false).await.is_ok());
    }

    #[tokio::test]
    async fn delete_attachment_with_purge() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/wiki/api/v2/attachments/att-1"))
            .and(wiremock::matchers::query_param("purge", "true"))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        assert!(api.delete_attachment("att-1", true).await.is_ok());
    }

    #[tokio::test]
    async fn delete_attachment_not_found() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/wiki/api/v2/attachments/missing"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let err = api.delete_attachment("missing", false).await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    // ── resolve_attachment_download_url ────────────────────────────────

    #[test]
    fn resolve_download_url_root_relative_gets_wiki_prefix() {
        assert_eq!(
            resolve_attachment_download_url(
                "https://org.atlassian.net",
                "/download/attachments/123/foo.png?version=1"
            ),
            "https://org.atlassian.net/wiki/download/attachments/123/foo.png?version=1"
        );
    }

    #[test]
    fn resolve_download_url_already_has_wiki_prefix() {
        assert_eq!(
            resolve_attachment_download_url(
                "https://org.atlassian.net/",
                "/wiki/download/attachments/123/foo.png"
            ),
            "https://org.atlassian.net/wiki/download/attachments/123/foo.png"
        );
    }

    #[test]
    fn resolve_download_url_absolute_passes_through() {
        let abs = "https://api.media.atlassian.com/file/abc/binary?token=xyz";
        assert_eq!(
            resolve_attachment_download_url("https://org.atlassian.net", abs),
            abs
        );
    }

    #[test]
    fn resolve_download_url_bare_relative_gets_wiki_prefix() {
        assert_eq!(
            resolve_attachment_download_url("https://org.atlassian.net", "download/x"),
            "https://org.atlassian.net/wiki/download/x"
        );
    }

    // ── get_attachment / download_attachment ───────────────────────────

    #[tokio::test]
    async fn get_attachment_maps_v2_fields() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/attachments/att-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "att-1",
                    "title": "diagram.png",
                    "mediaType": "image/png",
                    "fileSize": 2048,
                    "downloadLink": "/download/attachments/12345/diagram.png?version=2",
                    "version": {"number": 2},
                    "pageId": "12345",
                    "fileId": "file-1"
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let attachment = api.get_attachment("att-1").await.unwrap();

        assert_eq!(attachment.id, "att-1");
        assert_eq!(attachment.title, "diagram.png");
        assert_eq!(attachment.media_type.as_deref(), Some("image/png"));
        assert_eq!(attachment.file_size, Some(2048));
        assert_eq!(
            attachment.download_url.as_deref(),
            Some("/download/attachments/12345/diagram.png?version=2")
        );
        assert_eq!(attachment.version, Some(2));
        assert_eq!(attachment.page_id.as_deref(), Some("12345"));
        assert_eq!(attachment.file_id.as_deref(), Some("file-1"));
    }

    #[tokio::test]
    async fn get_attachment_not_found() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/attachments/missing"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let err = api.get_attachment("missing").await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn download_attachment_fetches_metadata_then_bytes() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/attachments/att-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "att-1",
                    "title": "notes.txt",
                    "mediaType": "text/plain",
                    "fileSize": 5,
                    "downloadLink": "/download/attachments/12345/notes.txt"
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/download/attachments/12345/notes.txt",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let (attachment, bytes) = api.download_attachment("att-1").await.unwrap();

        assert_eq!(attachment.title, "notes.txt");
        assert_eq!(bytes, b"hello");
    }

    #[tokio::test]
    async fn download_attachment_bytes_missing_url_errors() {
        let server = wiremock::MockServer::start().await;
        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let attachment = ConfluenceAttachment {
            id: "att-1".to_string(),
            title: "x.txt".to_string(),
            media_type: None,
            file_size: None,
            download_url: None,
            version: None,
            page_id: None,
            file_id: None,
        };
        let err = api
            .download_attachment_bytes(&attachment)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no download URL"));
    }

    #[test]
    fn confluence_attachment_serialize_skips_none_fields() {
        let attachment = ConfluenceAttachment {
            id: "att-1".to_string(),
            title: "x.txt".to_string(),
            media_type: None,
            file_size: None,
            download_url: None,
            version: None,
            page_id: None,
            file_id: None,
        };
        let json = serde_json::to_value(&attachment).unwrap();
        assert_eq!(json["id"], "att-1");
        assert_eq!(json["title"], "x.txt");
        // Optional fields should be entirely absent (skip_serializing_if).
        assert!(json.get("media_type").is_none());
        assert!(json.get("file_size").is_none());
        assert!(json.get("download_url").is_none());
        assert!(json.get("version").is_none());
        assert!(json.get("page_id").is_none());
        assert!(json.get("file_id").is_none());
    }

    #[test]
    fn confluence_attachment_serialize_includes_some_fields() {
        let attachment = ConfluenceAttachment {
            id: "att-1".to_string(),
            title: "x.txt".to_string(),
            media_type: Some("text/plain".to_string()),
            file_size: Some(42),
            download_url: Some("/dl".to_string()),
            version: Some(3),
            page_id: Some("12345".to_string()),
            file_id: Some("f-1".to_string()),
        };
        let json = serde_json::to_value(&attachment).unwrap();
        assert_eq!(json["media_type"], "text/plain");
        assert_eq!(json["file_size"], 42);
        assert_eq!(json["version"], 3);
    }

    #[test]
    fn confluence_attachment_page_serialize_skips_none_cursor() {
        let page = ConfluenceAttachmentPage {
            results: vec![],
            next_cursor: None,
        };
        let json = serde_json::to_value(&page).unwrap();
        assert!(json.get("next_cursor").is_none());
    }

    // ── get_page_at_version ───────────────────────────────────────

    async fn mount_page_version(
        server: &wiremock::MockServer,
        page_id: &str,
        version: u32,
        adf_value: &str,
    ) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!(
                "/wiki/api/v2/pages/{page_id}"
            )))
            .and(wiremock::matchers::query_param(
                "version",
                version.to_string(),
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": page_id,
                    "title": format!("Page {page_id} v{version}"),
                    "status": "current",
                    "spaceId": "98",
                    "version": {"number": version},
                    "body": {
                        "atlas_doc_format": {"value": adf_value}
                    }
                })),
            )
            .mount(server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"key": "ENG"})),
            )
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn get_page_at_version_success() {
        use crate::atlassian::api::ContentMetadata;

        let server = wiremock::MockServer::start().await;
        mount_page_version(
            &server,
            "12",
            3,
            r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"v3"}]}]}"#,
        )
        .await;

        let client = AtlassianClient::new(&server.uri(), "u@t.com", "tok").unwrap();
        let api = ConfluenceApi::new(client);
        let item = api.get_page_at_version("12", 3).await.unwrap();
        assert_eq!(item.id, "12");
        assert_eq!(item.title, "Page 12 v3");
        assert!(item.body_adf.is_some());
        match item.metadata {
            ContentMetadata::Confluence {
                space_key, version, ..
            } => {
                assert_eq!(space_key, "ENG");
                assert_eq!(version, Some(3));
            }
            ContentMetadata::Jira { .. } => panic!("expected Confluence metadata"),
        }
    }

    #[tokio::test]
    async fn get_page_at_version_404() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12"))
            .and(wiremock::matchers::query_param("version", "99"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "u@t.com", "tok").unwrap();
        let api = ConfluenceApi::new(client);
        let err = api.get_page_at_version("12", 99).await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn get_page_at_version_no_body() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12"))
            .and(wiremock::matchers::query_param("version", "1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "12",
                    "title": "Empty",
                    "status": "current",
                    "spaceId": "1",
                    "version": {"number": 1}
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"key": "S"})),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "u@t.com", "tok").unwrap();
        let api = ConfluenceApi::new(client);
        let item = api.get_page_at_version("12", 1).await.unwrap();
        assert!(item.body_adf.is_none());
    }

    // ── resolve_version ───────────────────────────────────────────

    fn version_at(number: u32, created: &str) -> PageVersion {
        PageVersion {
            number,
            created_at: created.to_string(),
            author_id: String::new(),
            message: String::new(),
            minor_edit: false,
        }
    }

    fn fixture_versions() -> Vec<PageVersion> {
        // Newest-first.
        vec![
            version_at(5, "2026-05-09T10:00:00Z"),
            version_at(4, "2026-05-08T10:00:00Z"),
            version_at(3, "2026-05-07T10:00:00Z"),
            version_at(2, "2026-05-06T10:00:00Z"),
            version_at(1, "2026-05-05T10:00:00Z"),
        ]
    }

    #[test]
    fn resolve_version_latest() {
        let v = fixture_versions();
        assert_eq!(resolve_version("latest", &v, 5).unwrap(), 5);
        assert_eq!(resolve_version("LATEST", &v, 5).unwrap(), 5);
    }

    #[test]
    fn resolve_version_previous_relative_to_anchor() {
        let v = fixture_versions();
        // `previous` is relative to `relative_to`, not to head.
        assert_eq!(resolve_version("previous", &v, 5).unwrap(), 4);
        assert_eq!(resolve_version("previous", &v, 3).unwrap(), 2);
    }

    #[test]
    fn resolve_version_previous_at_first_version_errors() {
        let v = fixture_versions();
        let err = resolve_version("previous", &v, 1).unwrap_err();
        assert!(err.to_string().contains("out of range"));
    }

    #[test]
    fn resolve_version_v_minus_offset() {
        let v = fixture_versions();
        assert_eq!(resolve_version("v-2", &v, 5).unwrap(), 3);
        assert_eq!(resolve_version("V-1", &v, 5).unwrap(), 4);
    }

    #[test]
    fn resolve_version_v_minus_zero_rejected() {
        let v = fixture_versions();
        let err = resolve_version("v-0", &v, 5).unwrap_err();
        assert!(err.to_string().contains("> 0"));
    }

    #[test]
    fn resolve_version_v_minus_too_deep() {
        let v = fixture_versions();
        let err = resolve_version("v-10", &v, 5).unwrap_err();
        assert!(err.to_string().contains("out of range"));
    }

    #[test]
    fn resolve_version_numeric_in_range() {
        let v = fixture_versions();
        assert_eq!(resolve_version("3", &v, 5).unwrap(), 3);
    }

    #[test]
    fn resolve_version_numeric_not_present() {
        let v = fixture_versions();
        let err = resolve_version("99", &v, 5).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn resolve_version_iso_picks_latest_at_or_before() {
        let v = fixture_versions();
        // 2026-05-08T11:00:00Z is after v4 (10:00) but before v5 (next day),
        // so the latest version at-or-before is v4.
        assert_eq!(resolve_version("2026-05-08T11:00:00Z", &v, 5).unwrap(), 4);
        assert_eq!(resolve_version("2026-05-09T10:00:00Z", &v, 5).unwrap(), 5);
        // Date-only: Confluence returns full timestamps; lexicographic
        // compare against `2026-05-07` matches versions with empty
        // created_at NOT, but matches v with `2026-05-07T...` only when
        // the timestamp is `<= "2026-05-07"`. Use a clearly past value.
        assert_eq!(resolve_version("2026-05-06", &v, 5).unwrap(), 1);
    }

    #[test]
    fn resolve_version_iso_no_match_errors() {
        let v = fixture_versions();
        let err = resolve_version("2020-01-01", &v, 5).unwrap_err();
        assert!(err.to_string().contains("at or before"));
    }

    #[test]
    fn resolve_version_empty_versions_errors() {
        let err = resolve_version("latest", &[], 0).unwrap_err();
        assert!(err.to_string().contains("no versions"));
    }

    #[test]
    fn resolve_version_empty_string_rejected() {
        let v = fixture_versions();
        let err = resolve_version("   ", &v, 5).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn resolve_version_unparseable() {
        let v = fixture_versions();
        let err = resolve_version("garbage", &v, 5).unwrap_err();
        assert!(err.to_string().contains("Could not parse"));
    }

    #[test]
    fn resolve_version_v_minus_with_non_numeric_suffix_rejected() {
        // Exercises the `with_context` error path when v-N's suffix fails
        // to parse as a u32.
        let v = fixture_versions();
        let err = resolve_version("v-abc", &v, 5).unwrap_err();
        assert!(
            err.to_string().contains("Invalid relative version offset"),
            "got: {err}"
        );
    }

    #[test]
    fn resolve_version_v_minus_resolves_to_missing_version_errors() {
        // Anchor (4) > offset (2), so the offset doesn't go negative — but
        // version 2 isn't in the truncated history. Exercises the
        // "Version N not found" path inside `offset_from`.
        let versions = vec![
            version_at(5, "2026-05-09T10:00:00Z"),
            version_at(4, "2026-05-08T10:00:00Z"),
            // v3 and v2 are absent (e.g., the cap dropped them).
        ];
        let err = resolve_version("v-2", &versions, 4).unwrap_err();
        assert!(err.to_string().contains("not found"), "got: {err}");
    }

    #[tokio::test]
    async fn get_page_at_version_with_body_but_no_atlas_doc_format() {
        // Exercises the `else { None }` arm where body is present but
        // `atlas_doc_format` is missing.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12"))
            .and(wiremock::matchers::query_param("version", "1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "12",
                    "title": "T",
                    "status": "current",
                    "spaceId": "1",
                    "version": {"number": 1},
                    "body": { /* atlas_doc_format absent */ }
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"key": "S"})),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "u@t.com", "tok").unwrap();
        let api = ConfluenceApi::new(client);
        let item = api.get_page_at_version("12", 1).await.unwrap();
        assert!(item.body_adf.is_none());
    }
}
