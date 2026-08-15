//! Drive Files API wrapper.
//!
//! `files.list` returns full metadata per hit in one call via the `fields`
//! parameter — unlike Gmail's `messages.list` (which needs a follow-up
//! `messages.get` per hit), so there is no ids-then-enrich split and no
//! `--enrich`/`search_summaries` analog (`src/gmail/messages_api.rs`'s
//! sibling concept has nothing to mirror here).

use anyhow::{Context, Result};
use url::Url;

use crate::drive::client::DriveClient;
use crate::drive::types::{DriveFile, FileListResponse};

/// Maximum `pageSize` accepted by `GET /drive/v3/files`.
///
/// Drive REST API reference: acceptable values are 1 to 1000, inclusive.
/// Unlike Gmail's 500 (`crate::gmail::messages_api::MAX_PAGE_LIMIT`),
/// Drive's cap is 1000.
pub const MAX_PAGE_LIMIT: usize = 1000;

/// Per-call upper bound on [`FilesApi::search_all`], even when the caller
/// passes `limit = 0`.
///
/// Mirrors `crate::gmail::messages_api::HARD_CAP` verbatim — same safety
/// rationale, no Drive-specific derivation.
pub const HARD_CAP: usize = 10_000;

/// Default `limit` for `drive search` when `--limit` is omitted. Mirrors
/// `crate::gmail::messages_api::DEFAULT_SEARCH_LIMIT`.
pub const DEFAULT_SEARCH_LIMIT: usize = 50;

/// `fields` value for `files.list` — enough for `drive search`'s
/// table/JSON output with zero follow-up calls per hit.
const LIST_FIELDS: &str = "nextPageToken,incompleteSearch,files(id,name,mimeType,size,\
    modifiedTime,parents,webViewLink,owners(displayName,emailAddress),driveId)";

/// `fields` value for `files.get` — additionally includes `exportLinks` so
/// `drive read`'s content-export error path can list which MIME types a
/// Google-native file actually supports exporting to.
const GET_FIELDS: &str = "id,name,mimeType,size,modifiedTime,parents,webViewLink,\
    owners(displayName,emailAddress),driveId,exportLinks";

/// Files API façade.
#[derive(Debug)]
pub struct FilesApi<'a> {
    client: &'a DriveClient,
}

impl<'a> FilesApi<'a> {
    /// Wraps an existing [`DriveClient`] for file operations.
    #[must_use]
    pub fn new(client: &'a DriveClient) -> Self {
        Self { client }
    }

    /// Searches files matching `query`, returning a single page.
    ///
    /// `limit` is rejected client-side when it exceeds [`MAX_PAGE_LIMIT`];
    /// use [`Self::search_all`] to auto-paginate across pages.
    pub async fn search(
        &self,
        query: Option<&str>,
        limit: usize,
        page_token: Option<&str>,
    ) -> Result<FileListResponse> {
        if limit > MAX_PAGE_LIMIT {
            return Err(anyhow::anyhow!(
                "`limit` must be <= {MAX_PAGE_LIMIT} (Drive files.list per-page cap; use \
                 `search_all` to auto-paginate)"
            ));
        }
        let url = build_files_list_url(self.client.base_url(), query, limit, page_token)?;
        self.client
            .get_parsed(url.as_str(), "Failed to parse files.list response")
            .await
    }

    /// Searches files, auto-paginating via cursor as needed.
    ///
    /// `limit == 0` means "fetch every match up to [`HARD_CAP`]" — a
    /// deliberate safety limit for this interactive surface.
    pub async fn search_all(&self, query: Option<&str>, limit: usize) -> Result<FileListResponse> {
        self.paginate(query, effective_cap(limit)).await
    }

    /// Pagination loop backing [`Self::search_all`] — structurally
    /// identical to `crate::gmail::messages_api::MessagesApi::paginate`,
    /// `messages` renamed to `files`, `result_size_estimate` renamed to
    /// `incomplete_search` (Drive has no result-count estimate field).
    async fn paginate(&self, query: Option<&str>, cap: usize) -> Result<FileListResponse> {
        let mut acc: Option<FileListResponse> = None;
        let mut page_token: Option<String> = None;
        loop {
            let collected = acc.as_ref().map_or(0, |r| r.files.len());
            let page_size = (cap - collected).min(MAX_PAGE_LIMIT);
            let page = self.search(query, page_size, page_token.as_deref()).await?;
            let next_token = page.next_page_token.clone();
            match acc.as_mut() {
                Some(existing) => {
                    existing.files.extend(page.files);
                    existing.next_page_token = page.next_page_token;
                    existing.incomplete_search = page.incomplete_search;
                }
                None => acc = Some(page),
            }
            let collected = acc.as_ref().map_or(0, |r| r.files.len());
            if collected >= cap || next_token.is_none() {
                break;
            }
            page_token = next_token;
        }
        let mut result = acc.unwrap_or_default();
        result.files.truncate(cap);
        Ok(result)
    }

    /// Fetches a single file's metadata (`files.get`), including
    /// `exportLinks`.
    pub async fn get_metadata(&self, file_id: &str) -> Result<DriveFile> {
        let url = build_file_get_url(self.client.base_url(), file_id)?;
        self.client
            .get_parsed(url.as_str(), "Failed to parse files.get response")
            .await
    }

    /// Exports a Google-native file's content as `export_mime_type`
    /// (`files.export`). Drive limits export responses to 10 MB — a larger
    /// document surfaces as an `ApiRequestFailed` from
    /// [`DriveClient::response_to_error`].
    pub async fn export(&self, file_id: &str, export_mime_type: &str) -> Result<Vec<u8>> {
        let url = build_export_url(self.client.base_url(), file_id, export_mime_type)?;
        self.fetch_bytes(&url).await
    }

    /// Downloads a non-Google-native file's raw bytes
    /// (`files.get?alt=media`).
    pub async fn download(&self, file_id: &str) -> Result<Vec<u8>> {
        let url = build_download_url(self.client.base_url(), file_id)?;
        self.fetch_bytes(&url).await
    }

    /// Shared GET-then-check-status-then-collect-bytes body for
    /// [`Self::export`]/[`Self::download`] — both go through
    /// [`DriveClient::get_bytes`], not `get_json`/`get_parsed`.
    async fn fetch_bytes(&self, url: &Url) -> Result<Vec<u8>> {
        let response = self.client.get_bytes(url.as_str()).await?;
        if !response.status().is_success() {
            return Err(DriveClient::response_to_error(response).await.into());
        }
        let bytes = response
            .bytes()
            .await
            .context("Failed to read response body")?;
        Ok(bytes.to_vec())
    }
}

fn build_files_list_url(
    base_url: &str,
    query: Option<&str>,
    limit: usize,
    page_token: Option<&str>,
) -> Result<Url> {
    let mut url = DriveClient::api_url(base_url, "/drive/v3/files")?;
    {
        let mut pairs = url.query_pairs_mut();
        // Always sent — issue requirement, not opt-in — so shared-drive
        // files are visible/searchable by default.
        pairs.append_pair("supportsAllDrives", "true");
        pairs.append_pair("includeItemsFromAllDrives", "true");
        pairs.append_pair("fields", LIST_FIELDS);
        if let Some(q) = query.filter(|q| !q.is_empty()) {
            pairs.append_pair("q", q);
        }
        if limit > 0 {
            pairs.append_pair("pageSize", &limit.to_string());
        }
        if let Some(token) = page_token {
            pairs.append_pair("pageToken", token);
        }
    }
    Ok(url)
}

fn build_file_get_url(base_url: &str, file_id: &str) -> Result<Url> {
    let mut url = DriveClient::api_url(base_url, &format!("/drive/v3/files/{file_id}"))?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("fields", GET_FIELDS);
        pairs.append_pair("supportsAllDrives", "true");
    }
    Ok(url)
}

fn build_export_url(base_url: &str, file_id: &str, mime_type: &str) -> Result<Url> {
    // `supportsAllDrives` is not a documented `files.export` parameter
    // (unlike `files.get`/`files.list`) — deliberately omitted rather than
    // sent speculatively.
    let mut url = DriveClient::api_url(base_url, &format!("/drive/v3/files/{file_id}/export"))?;
    url.query_pairs_mut().append_pair("mimeType", mime_type);
    Ok(url)
}

fn build_download_url(base_url: &str, file_id: &str) -> Result<Url> {
    let mut url = DriveClient::api_url(base_url, &format!("/drive/v3/files/{file_id}"))?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("alt", "media");
        pairs.append_pair("supportsAllDrives", "true");
    }
    Ok(url)
}

/// Clamps a caller-supplied limit to [`HARD_CAP`], treating `0` as "fetch
/// as many as the cap allows".
fn effective_cap(limit: usize) -> usize {
    if limit == 0 {
        HARD_CAP
    } else {
        limit.min(HARD_CAP)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, SCOPE_READONLY};
    use crate::utils::secret::Secret;

    fn test_credentials() -> DriveCredentials {
        DriveCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: SCOPE_READONLY.to_string(),
        }
    }

    async fn client_with_bootstrapped_token(server: &wiremock::MockServer) -> DriveClient {
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

        let mut client = DriveClient::new(&server.uri(), &test_credentials()).unwrap();
        crate::drive::client::test_support::replace_session(
            &mut client,
            &test_credentials(),
            &format!("{}/token", server.uri()),
        );
        client
    }

    // ── URL builders ─────────────────────────────────────────────────

    #[test]
    fn build_files_list_url_with_only_provided_filters() {
        let url = build_files_list_url("https://www.googleapis.com", None, 0, None).unwrap();
        assert!(url.as_str().contains("supportsAllDrives=true"));
        assert!(url.as_str().contains("includeItemsFromAllDrives=true"));
        assert!(url.as_str().contains("fields="));
        assert!(!url.as_str().contains("q="));
        assert!(!url.as_str().contains("pageSize="));
        assert!(!url.as_str().contains("pageToken="));
    }

    #[test]
    fn build_files_list_url_with_full_filter_set() {
        let url = build_files_list_url(
            "https://www.googleapis.com",
            Some("name contains 'x'"),
            10,
            Some("token1"),
        )
        .unwrap();
        assert!(url.as_str().contains("supportsAllDrives=true"));
        assert!(url.as_str().contains("includeItemsFromAllDrives=true"));
        assert!(url.as_str().contains("q=name"));
        assert!(url.as_str().contains("pageSize=10"));
        assert!(url.as_str().contains("pageToken=token1"));
    }

    #[test]
    fn build_files_list_url_rejects_invalid_base_url() {
        let err = build_files_list_url("not a url", None, 0, None).unwrap_err();
        assert!(err.to_string().contains("Invalid Drive base URL"));
    }

    #[test]
    fn build_file_get_url_includes_fields_and_supports_all_drives() {
        let url = build_file_get_url("https://www.googleapis.com", "f1").unwrap();
        assert!(url.as_str().contains("/drive/v3/files/f1"));
        assert!(url.as_str().contains("fields="));
        assert!(url.as_str().contains("supportsAllDrives=true"));
    }

    #[test]
    fn build_export_url_includes_mime_type_and_omits_supports_all_drives() {
        let url = build_export_url("https://www.googleapis.com", "f1", "text/markdown").unwrap();
        assert!(url.as_str().contains("/drive/v3/files/f1/export"));
        assert!(url.as_str().contains("mimeType=text%2Fmarkdown"));
        assert!(!url.as_str().contains("supportsAllDrives"));
    }

    #[test]
    fn build_download_url_includes_alt_media_and_supports_all_drives() {
        let url = build_download_url("https://www.googleapis.com", "f1").unwrap();
        assert!(url.as_str().contains("/drive/v3/files/f1"));
        assert!(url.as_str().contains("alt=media"));
        assert!(url.as_str().contains("supportsAllDrives=true"));
    }

    // ── search ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn search_rejects_limit_above_max_page_limit_client_side() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let err = FilesApi::new(&client)
            .search(None, MAX_PAGE_LIMIT + 1, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("must be <="));
    }

    #[tokio::test]
    async fn search_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let err = FilesApi::new(&client)
            .search(None, 10, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    #[tokio::test]
    async fn search_errors_on_malformed_response() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let err = FilesApi::new(&client)
            .search(None, 10, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Failed to parse"));
    }

    // ── search_all ───────────────────────────────────────────────────

    #[tokio::test]
    async fn search_all_single_page_when_no_next_token() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "files": [{"id": "f1", "name": "a"}],
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let result = FilesApi::new(&client).search_all(None, 10).await.unwrap();
        assert_eq!(result.files.len(), 1);
    }

    #[tokio::test]
    async fn search_all_follows_next_page_token_to_exhaustion() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files"))
            .and(wiremock::matchers::query_param_is_missing("pageToken"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "files": [{"id": "f1", "name": "a"}],
                    "nextPageToken": "page2",
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files"))
            .and(wiremock::matchers::query_param("pageToken", "page2"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "files": [{"id": "f2", "name": "b"}],
                })),
            )
            .mount(&server)
            .await;

        let result = FilesApi::new(&client).search_all(None, 0).await.unwrap();
        assert_eq!(result.files.len(), 2);
        assert_eq!(result.files[0].id, "f1");
        assert_eq!(result.files[1].id, "f2");
    }

    #[tokio::test]
    async fn search_all_stops_at_explicit_limit() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "files": [
                        {"id": "f1", "name": "a"},
                        {"id": "f2", "name": "b"},
                        {"id": "f3", "name": "c"},
                    ],
                    "nextPageToken": "page2",
                })),
            )
            .mount(&server)
            .await;

        let result = FilesApi::new(&client).search_all(None, 2).await.unwrap();
        assert_eq!(result.files.len(), 2);
    }

    #[tokio::test]
    async fn search_all_continues_past_empty_page_with_a_valid_next_page_token() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files"))
            .and(wiremock::matchers::query_param_is_missing("pageToken"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "files": [],
                    "nextPageToken": "page2",
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files"))
            .and(wiremock::matchers::query_param("pageToken", "page2"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "files": [{"id": "f1", "name": "a"}],
                })),
            )
            .mount(&server)
            .await;

        let result = FilesApi::new(&client).search_all(None, 0).await.unwrap();
        assert_eq!(result.files.len(), 1);
    }

    #[tokio::test]
    async fn search_all_truncates_to_hard_cap() {
        assert_eq!(effective_cap(0), HARD_CAP);
    }

    // ── get_metadata ─────────────────────────────────────────────────

    #[tokio::test]
    async fn get_metadata_sends_fields_query_param() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param("fields", GET_FIELDS))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "a",
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let file = FilesApi::new(&client).get_metadata("f1").await.unwrap();
        assert_eq!(file.id, "f1");
    }

    #[tokio::test]
    async fn get_metadata_parses_export_links() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1",
                    "name": "doc",
                    "mimeType": "application/vnd.google-apps.document",
                    "exportLinks": {"text/markdown": "https://export.example/md"},
                })),
            )
            .mount(&server)
            .await;

        let file = FilesApi::new(&client).get_metadata("f1").await.unwrap();
        assert!(file.export_links.unwrap().contains_key("text/markdown"));
    }

    #[tokio::test]
    async fn get_metadata_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/missing"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;

        let err = FilesApi::new(&client)
            .get_metadata("missing")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    // ── export / download ────────────────────────────────────────────

    #[tokio::test]
    async fn export_sends_mime_type_query_param_and_returns_bytes() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1/export"))
            .and(wiremock::matchers::query_param("mimeType", "text/markdown"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"# Title".to_vec()))
            .expect(1)
            .mount(&server)
            .await;

        let bytes = FilesApi::new(&client)
            .export("f1", "text/markdown")
            .await
            .unwrap();
        assert_eq!(bytes, b"# Title");
    }

    #[tokio::test]
    async fn export_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1/export"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;

        let err = FilesApi::new(&client)
            .export("f1", "text/markdown")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn download_sends_alt_media_and_returns_bytes() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param("alt", "media"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"binary".to_vec()))
            .expect(1)
            .mount(&server)
            .await;

        let bytes = FilesApi::new(&client).download("f1").await.unwrap();
        assert_eq!(bytes, b"binary");
    }

    #[tokio::test]
    async fn download_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let err = FilesApi::new(&client).download("f1").await.unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    // ── effective_cap ────────────────────────────────────────────────

    #[test]
    fn effective_cap_zero_is_hard_cap() {
        assert_eq!(effective_cap(0), HARD_CAP);
    }

    #[test]
    fn effective_cap_clamps_above_hard_cap() {
        assert_eq!(effective_cap(HARD_CAP + 1000), HARD_CAP);
    }

    #[test]
    fn effective_cap_passes_through_small_limits() {
        assert_eq!(effective_cap(5), 5);
    }
}
