//! Drive Files API wrapper.
//!
//! `files.list` returns full metadata per hit in one call via the `fields`
//! parameter — unlike Gmail's `messages.list` (which needs a follow-up
//! `messages.get` per hit), so there is no ids-then-enrich split and no
//! `--enrich`/`search_summaries` analog (`src/gmail/messages_api.rs`'s
//! sibling concept has nothing to mirror here).

use anyhow::{Context, Result};
use base64::Engine;
use rand::Rng;
use url::Url;

use crate::drive::client::DriveClient;
use crate::drive::error::DriveError;
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

/// Refuses to buffer a [`FilesApi::download`] response body larger than
/// this into memory. `files.export` already has Drive's own 10 MB cap, but
/// `files.get?alt=media` has none — an unbounded read of a very large file
/// risks exhausting process memory.
const MAX_DOWNLOAD_BYTES: u64 = 500 * 1024 * 1024;

/// Refuses to buffer local content larger than this for
/// [`FilesApi::upload`]/[`FilesApi::edit_content`] — Google's documented
/// cap on `uploadType=multipart`/`uploadType=media` simple-upload request
/// bodies. Content above this needs a resumable upload session (chunked,
/// with its own restart/retry bookkeeping), explicitly out of scope for
/// v1 — refused outright rather than silently degrading, the same
/// documented-boundary posture ADR-0070 §8 took for shallow folder moves.
pub(crate) const MAX_UPLOAD_BYTES: u64 = 5 * 1024 * 1024;

/// `fields` value for `files.list` — enough for `drive search`'s
/// table/JSON output with zero follow-up calls per hit.
const LIST_FIELDS: &str = "nextPageToken,incompleteSearch,files(id,name,mimeType,size,\
    md5Checksum,sha1Checksum,sha256Checksum,modifiedTime,parents,webViewLink,\
    owners(displayName,emailAddress),driveId)";

/// `fields` value for `files.get` — additionally includes `exportLinks` so
/// `drive read`'s content-export error path can list which MIME types a
/// Google-native file actually supports exporting to.
const GET_FIELDS: &str = "id,name,mimeType,size,md5Checksum,sha1Checksum,sha256Checksum,\
    modifiedTime,parents,webViewLink,owners(displayName,emailAddress),driveId,exportLinks";

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
    /// `incomplete_search` (Drive has no result-count estimate field),
    /// with one deliberate divergence: when the accumulated total exceeds
    /// `cap`, this method clears `next_page_token`/`incomplete_search`
    /// before truncating `files`, so a caller can never resume pagination
    /// past files that were fetched but then discarded (#1536). Gmail's
    /// `paginate` still has the matching bug — issue #1536 was scoped to
    /// Drive only.
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
        if result.files.len() > cap {
            result.files.truncate(cap);
            result.next_page_token = None;
            result.incomplete_search = None;
        }
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

    /// Renames a file (`files.update` with a `name` body, no `parents`
    /// change — renaming never affects visibility, see ADR-0070). Requires
    /// the `drive.metadata` scope (`drive auth login --write`).
    pub async fn rename(&self, file_id: &str, new_name: &str) -> Result<DriveFile> {
        let url = build_file_update_url(self.client.base_url(), file_id, None, None)?;
        let response = self
            .client
            .patch_json(url.as_str(), &serde_json::json!({ "name": new_name }))
            .await?;
        self.client
            .parse_response(response, "Failed to parse files.update response")
            .await
            .map_err(|err| append_write_scope_hint(err, WriteCapability::Metadata))
    }

    /// Moves a file between folders (`files.update` with `addParents`/
    /// `removeParents` query params, comma-separated file ids — Drive v3 has
    /// no separate move endpoint). Requires the `drive.metadata` scope
    /// (`drive auth login --write`).
    pub async fn move_to(
        &self,
        file_id: &str,
        add_parents: &str,
        remove_parents: &str,
    ) -> Result<DriveFile> {
        let url = build_file_update_url(
            self.client.base_url(),
            file_id,
            Some(add_parents),
            Some(remove_parents),
        )?;
        let response = self
            .client
            .patch_json(url.as_str(), &serde_json::json!({}))
            .await?;
        self.client
            .parse_response(response, "Failed to parse files.update response")
            .await
            .map_err(|err| append_write_scope_hint(err, WriteCapability::Metadata))
    }

    /// Creates a new file or folder (`files.create`, metadata-only — no
    /// content). Requires the `drive.file` or `drive` scope (`drive auth
    /// login --write-file`/`--write-full`).
    ///
    /// Restricted to `crate::drive`: every mutating call here must run
    /// through `write_gate::resolve` first (issue #1574's folder-permission
    /// gate), and that gate is only ever invoked by the engine modules
    /// (`crate::drive::{create,upload,content_edit}`), never this façade
    /// itself. This visibility is the actual enforcement of "no bypass by
    /// construction" — a caller outside `crate::drive` (a new CLI command, a
    /// future MCP tool) cannot even compile a direct call to this method; it
    /// has to go through the gated engine function instead.
    pub(in crate::drive) async fn create(
        &self,
        name: &str,
        parent_folder_id: &str,
        mime_type: &str,
    ) -> Result<DriveFile> {
        let url = build_file_create_url(self.client.base_url())?;
        let response = self
            .client
            .post_json(
                url.as_str(),
                &serde_json::json!({
                    "name": name,
                    "mimeType": mime_type,
                    "parents": [parent_folder_id],
                }),
            )
            .await?;
        self.client
            .parse_response(response, "Failed to parse files.create response")
            .await
            .map_err(|err| append_write_scope_hint(err, WriteCapability::CreateOrUpload))
    }

    /// Uploads `content` as a new file (`files.create` with
    /// `uploadType=multipart`, Drive's simple upload endpoint — no
    /// resumable-session support). Requires the `drive.file` or `drive`
    /// scope (`drive auth login --write-file`/`--write-full`).
    ///
    /// Refuses content over [`MAX_UPLOAD_BYTES`] before ever building the
    /// request body.
    ///
    /// Restricted to `crate::drive` — see [`Self::create`]'s doc comment
    /// for why.
    pub(in crate::drive) async fn upload(
        &self,
        name: &str,
        parent_folder_id: &str,
        content: &[u8],
        content_type: &str,
    ) -> Result<DriveFile> {
        check_upload_size(content.len() as u64)?;
        check_content_type(content_type)?;
        let boundary = generate_multipart_boundary();
        let metadata = serde_json::json!({
            "name": name,
            "parents": [parent_folder_id],
        });
        let body = build_multipart_related_body(&metadata, content, content_type, &boundary);
        let url = build_file_upload_url(self.client.base_url())?;
        let response = self
            .client
            .post_bytes(
                url.as_str(),
                &body,
                &format!("multipart/related; boundary={boundary}"),
            )
            .await?;
        self.client
            .parse_response(
                response,
                "Failed to parse files.create (multipart) response",
            )
            .await
            .map_err(|err| append_write_scope_hint(err, WriteCapability::CreateOrUpload))
    }

    /// Replaces an existing file's content (`files.update` with
    /// `uploadType=media` — content-only, no multipart envelope since
    /// there's no accompanying metadata change). Requires the `drive.file`
    /// scope if `omni-dev` created `file_id`, or the unrestricted `drive`
    /// scope for any pre-existing file.
    ///
    /// Refuses content over [`MAX_UPLOAD_BYTES`] before ever sending it.
    ///
    /// Restricted to `crate::drive` — see [`Self::create`]'s doc comment
    /// for why.
    pub(in crate::drive) async fn edit_content(
        &self,
        file_id: &str,
        content: &[u8],
        content_type: &str,
    ) -> Result<DriveFile> {
        check_upload_size(content.len() as u64)?;
        check_content_type(content_type)?;
        let url = build_file_edit_content_url(self.client.base_url(), file_id)?;
        let response = self
            .client
            .patch_bytes(url.as_str(), content, content_type)
            .await?;
        self.client
            .parse_response(response, "Failed to parse files.update (media) response")
            .await
            .map_err(|err| append_write_scope_hint(err, WriteCapability::EditContent))
    }

    /// Shared GET-then-check-status-then-collect-bytes body for
    /// [`Self::export`]/[`Self::download`] — both go through
    /// [`DriveClient::get_bytes`], not `get_json`/`get_parsed`.
    async fn fetch_bytes(&self, url: &Url) -> Result<Vec<u8>> {
        let response = self.client.get_bytes(url.as_str()).await?;
        if !response.status().is_success() {
            return Err(DriveClient::response_to_error(response).await.into());
        }
        check_download_size(response.content_length())?;
        let bytes = response
            .bytes()
            .await
            .context("Failed to read response body")?;
        Ok(bytes.to_vec())
    }
}

/// Refuses a download whose declared `Content-Length` exceeds
/// [`MAX_DOWNLOAD_BYTES`]. A missing length (e.g. chunked transfer
/// encoding) is allowed through — there's nothing to check up front in
/// that case.
fn check_download_size(content_length: Option<u64>) -> Result<()> {
    if let Some(len) = content_length {
        anyhow::ensure!(
            len <= MAX_DOWNLOAD_BYTES,
            "refusing to load {len} bytes into memory (limit: {MAX_DOWNLOAD_BYTES} bytes); \
             this file is too large for `drive read --content`"
        );
    }
    Ok(())
}

/// Refuses to upload content larger than [`MAX_UPLOAD_BYTES`]. Unlike
/// [`check_download_size`] (which checks a caller-*reported*
/// `Content-Length` that might be absent), a caller-supplied local
/// buffer's length is always known up front, so this takes a plain `u64`,
/// no `Option`.
pub(crate) fn check_upload_size(len: u64) -> Result<()> {
    anyhow::ensure!(
        len <= MAX_UPLOAD_BYTES,
        "refusing to upload {len} bytes (limit: {MAX_UPLOAD_BYTES} bytes); Drive's simple \
         upload endpoint caps requests at 5 MB — larger content needs resumable upload, not \
         supported by `drive upload`/`drive edit` yet"
    );
    Ok(())
}

/// Refuses a `Content-Type` value containing a CR or LF byte.
///
/// [`FilesApi::upload`] splices `content_type` directly into a
/// hand-assembled `multipart/related` body header line
/// ([`build_multipart_related_body`]), which bypasses the CRLF rejection
/// `reqwest`'s own `header()` already applies to a real HTTP header value
/// (the mechanism protecting [`FilesApi::edit_content`]'s plain
/// `Content-Type` header) — an unchecked value here could inject an extra
/// multipart boundary/part into the request Google receives. Applied to
/// both mutating call sites for a consistent, clearly-worded refusal
/// rather than relying on two different enforcement mechanisms.
fn check_content_type(content_type: &str) -> Result<()> {
    anyhow::ensure!(
        !content_type.contains(['\r', '\n']),
        "refusing content type {content_type:?}: must not contain a CR or LF byte"
    );
    Ok(())
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

/// `files.create` URL for [`FilesApi::create`] — metadata-only, `fields`
/// selects the same response shape `files.get` returns.
fn build_file_create_url(base_url: &str) -> Result<Url> {
    let mut url = DriveClient::api_url(base_url, "/drive/v3/files")?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("fields", GET_FIELDS);
        pairs.append_pair("supportsAllDrives", "true");
    }
    Ok(url)
}

/// `files.create` URL for [`FilesApi::upload`], on Drive's separate
/// `/upload/` path prefix (`uploadType=multipart`, Google's simple-upload
/// endpoint — no resumable-session support here).
fn build_file_upload_url(base_url: &str) -> Result<Url> {
    let mut url = DriveClient::api_url(base_url, "/upload/drive/v3/files")?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("uploadType", "multipart");
        pairs.append_pair("fields", GET_FIELDS);
        pairs.append_pair("supportsAllDrives", "true");
    }
    Ok(url)
}

/// `files.update` URL for [`FilesApi::edit_content`], on Drive's `/upload/`
/// path prefix (`uploadType=media` — content-only, no multipart envelope).
fn build_file_edit_content_url(base_url: &str, file_id: &str) -> Result<Url> {
    let mut url = DriveClient::api_url(base_url, &format!("/upload/drive/v3/files/{file_id}"))?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("uploadType", "media");
        pairs.append_pair("fields", GET_FIELDS);
        pairs.append_pair("supportsAllDrives", "true");
    }
    Ok(url)
}

/// A fresh, random `multipart/related` boundary — unlikely to collide with
/// arbitrary binary content, unlike a fixed string would risk.
fn generate_multipart_boundary() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    format!(
        "omnidev-{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    )
}

/// Hand-assembles a `multipart/related` (RFC 2387) body for Drive's simple
/// multipart upload endpoint.
///
/// Drive's upload endpoint requires exactly this format — two parts, a
/// JSON metadata part followed by the raw content part — and rejects the
/// `multipart/form-data` `reqwest::multipart::Form` would produce, so this
/// can't just call into `reqwest`'s own multipart support. Pure and
/// unit-tested at the byte level, since Drive is strict about this shape
/// (exact `\r\n` placement, no trailing content after the closing
/// boundary).
fn build_multipart_related_body(
    metadata: &serde_json::Value,
    content: &[u8],
    content_type: &str,
    boundary: &str,
) -> Vec<u8> {
    let mut body = Vec::with_capacity(content.len() + 256);
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Type: application/json; charset=UTF-8\r\n\r\n");
    body.extend_from_slice(metadata.to_string().as_bytes());
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(format!("Content-Type: {content_type}\r\n\r\n").as_bytes());
    body.extend_from_slice(content);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}--").as_bytes());
    body
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

/// `files.update` URL for [`FilesApi::rename`]/[`FilesApi::move_to`].
/// `add_parents`/`remove_parents` are Drive's `addParents`/`removeParents`
/// query params (comma-separated file ids) — omitted entirely for a plain
/// rename, present for a move.
fn build_file_update_url(
    base_url: &str,
    file_id: &str,
    add_parents: Option<&str>,
    remove_parents: Option<&str>,
) -> Result<Url> {
    let mut url = DriveClient::api_url(base_url, &format!("/drive/v3/files/{file_id}"))?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("fields", GET_FIELDS);
        pairs.append_pair("supportsAllDrives", "true");
        if let Some(add) = add_parents {
            pairs.append_pair("addParents", add);
        }
        if let Some(remove) = remove_parents {
            pairs.append_pair("removeParents", remove);
        }
    }
    Ok(url)
}

/// Which write capability a mutating call needed — parameterizes
/// [`append_write_scope_hint`] so one function serves every mutating verb's
/// 403 instead of hardcoding a single hint (issue #1574 generalization of
/// [ADR-0070](../../docs/adrs/adr-0070.md) §2's rename/move-only hint).
pub(crate) enum WriteCapability {
    /// `files.update` on `name`/`parents` (rename/move) — `drive.metadata`.
    Metadata,
    /// Creating a new file/folder, or uploading new content — `drive.file`
    /// or `drive`.
    CreateOrUpload,
    /// Editing an existing file's content — `drive.file` if `omni-dev`
    /// created it, `drive` (unrestricted) for any pre-existing file. The
    /// client has no cheap way to know which a given file id is, so the
    /// hint names both.
    EditContent,
}

/// Appends an actionable hint to a mutating-call failure caused by an
/// insufficient OAuth scope. No client-side scope pre-check exists (mirrors
/// Gmail's label-mutation commands): the mutating call is always attempted,
/// and Google's 403 is made actionable here instead.
///
/// Matches **both** spellings Google ships for the same condition:
/// `insufficientPermissions` in Drive v3's legacy `error.errors[].reason`
/// envelope, and `PERMISSION_DENIED` in the `google.rpc` `error.status`
/// envelope newer services use — Sheets v4 among them (issue #1589). Matching
/// only the legacy string would leave every Sheets 403 hint-less, and would do
/// so silently: the call still fails, just without telling the operator which
/// login flag fixes it. See `crate::drive::api_client::error_reason`.
pub(in crate::drive) fn append_write_scope_hint(
    err: anyhow::Error,
    capability: WriteCapability,
) -> anyhow::Error {
    let is_insufficient_permissions = matches!(
        err.downcast_ref::<DriveError>(),
        Some(DriveError::ApiRequestFailed {
            reason: Some(reason),
            ..
        }) if reason == "insufficientPermissions" || reason == "PERMISSION_DENIED"
    );
    if !is_insufficient_permissions {
        return err;
    }
    let hint = match capability {
        WriteCapability::Metadata => {
            "Run `omni-dev drive auth login --write` to grant the drive.metadata scope needed \
             for rename/move"
        }
        WriteCapability::CreateOrUpload => {
            "Run `omni-dev drive auth login --write-file` (or `--write-full`) to grant the \
             scope needed to create files/folders and upload content"
        }
        WriteCapability::EditContent => {
            "Run `omni-dev drive auth login --write-file` if this file was created by \
             omni-dev, or `--write-full` to edit any pre-existing file's content, then retry"
        }
    };
    err.context(hint)
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
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::types::Owner;
    use crate::utils::secret::Secret;

    fn test_credentials() -> DriveCredentials {
        DriveCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: DriveGrantedScopes::READONLY,
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

    // ── fields= selector coverage ────────────────────────────────────
    //
    // LIST_FIELDS/GET_FIELDS are hand-maintained `fields=` selector
    // strings with no compiler-enforced link to DriveFile/Owner. These
    // tests construct every field explicitly (no `..Default::default()`),
    // so adding a struct field forces a compile error here until the test
    // — and, by extension, the selector strings — are updated to match.

    fn fully_populated_drive_file() -> DriveFile {
        DriveFile {
            id: "f1".to_string(),
            name: "n".to_string(),
            mime_type: "application/pdf".to_string(),
            size: Some("1".to_string()),
            md5_checksum: Some("5d41402abc4b2a76b9719d911017c592".to_string()),
            sha1_checksum: Some("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d".to_string()),
            sha256_checksum: Some(
                "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08".to_string(),
            ),
            modified_time: Some("2026-01-01T00:00:00Z".to_string()),
            parents: vec!["p1".to_string()],
            web_view_link: Some("https://example.com/view".to_string()),
            owners: vec![Owner {
                display_name: Some("Alice".to_string()),
                email_address: Some("alice@example.com".to_string()),
            }],
            drive_id: Some("d1".to_string()),
            export_links: Some(std::collections::HashMap::from([(
                "text/markdown".to_string(),
                "https://export.example.com/md".to_string(),
            )])),
        }
    }

    #[test]
    fn get_fields_requests_every_drive_file_field() {
        let json = serde_json::to_value(fully_populated_drive_file()).unwrap();
        for key in json.as_object().unwrap().keys() {
            assert!(
                GET_FIELDS.contains(key.as_str()),
                "GET_FIELDS is missing `{key}` — add it, or update this test if that's \
                 deliberate"
            );
        }
    }

    #[test]
    fn list_fields_requests_every_drive_file_field_except_export_links() {
        let json = serde_json::to_value(fully_populated_drive_file()).unwrap();
        for key in json.as_object().unwrap().keys() {
            if key == "exportLinks" {
                // Deliberately excluded — see DriveFile::export_links' doc comment.
                continue;
            }
            assert!(
                LIST_FIELDS.contains(key.as_str()),
                "LIST_FIELDS is missing `{key}` — add it, or update this test if that's \
                 deliberate"
            );
        }
    }

    #[test]
    fn fields_selectors_request_every_owner_field() {
        let owner = Owner {
            display_name: Some("Alice".to_string()),
            email_address: Some("alice@example.com".to_string()),
        };
        let json = serde_json::to_value(owner).unwrap();
        for key in json.as_object().unwrap().keys() {
            assert!(
                GET_FIELDS.contains(key.as_str()),
                "GET_FIELDS' owners() selector is missing `{key}`"
            );
            assert!(
                LIST_FIELDS.contains(key.as_str()),
                "LIST_FIELDS' owners() selector is missing `{key}`"
            );
        }
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
    fn build_file_create_url_includes_fields_and_supports_all_drives() {
        let url = build_file_create_url("https://www.googleapis.com").unwrap();
        assert!(url.path().ends_with("/drive/v3/files"));
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
                    "incompleteSearch": true,
                })),
            )
            .mount(&server)
            .await;

        let result = FilesApi::new(&client).search_all(None, 2).await.unwrap();
        assert_eq!(result.files.len(), 2);
        // Truncation discarded fetched-but-unreturned files, so the cursor
        // fields must be cleared rather than pointing past them (#1536).
        assert_eq!(result.next_page_token, None);
        assert_eq!(result.incomplete_search, None);
    }

    #[tokio::test]
    async fn search_all_preserves_next_page_token_at_exact_cap() {
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
                    "incompleteSearch": true,
                })),
            )
            .mount(&server)
            .await;

        let result = FilesApi::new(&client).search_all(None, 3).await.unwrap();
        assert_eq!(result.files.len(), 3);
        // No files were discarded, so the cursor is still accurate and
        // must be preserved, not cleared.
        assert_eq!(result.next_page_token.as_deref(), Some("page2"));
        assert_eq!(result.incomplete_search, Some(true));
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

    // ── rename / move_to ─────────────────────────────────────────────

    #[tokio::test]
    async fn rename_sends_name_body_and_no_parents_params() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param("fields", GET_FIELDS))
            .and(wiremock::matchers::body_json(
                serde_json::json!({"name": "New Name"}),
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "New Name",
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let file = FilesApi::new(&client)
            .rename("f1", "New Name")
            .await
            .unwrap();
        assert_eq!(file.name, "New Name");

        let requests = server.received_requests().await.unwrap();
        let req = requests
            .iter()
            .find(|r| r.method.as_str() == "PATCH")
            .unwrap();
        assert!(req.url.query_pairs().all(|(k, _)| k != "addParents"));
        assert!(req.url.query_pairs().all(|(k, _)| k != "removeParents"));
    }

    #[tokio::test]
    async fn rename_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;

        let err = FilesApi::new(&client)
            .rename("f1", "New Name")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn rename_appends_write_scope_hint_on_insufficient_permissions() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(
                wiremock::ResponseTemplate::new(403).set_body_json(serde_json::json!({
                    "error": {
                        "message": "Insufficient Permission",
                        "errors": [{"reason": "insufficientPermissions"}],
                    }
                })),
            )
            .mount(&server)
            .await;

        let err = FilesApi::new(&client)
            .rename("f1", "New Name")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("drive auth login --write"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn move_to_sends_add_and_remove_parents_query_params() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param("addParents", "dest"))
            .and(wiremock::matchers::query_param("removeParents", "src"))
            .and(wiremock::matchers::body_json(serde_json::json!({})))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "a", "parents": ["dest"],
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let file = FilesApi::new(&client)
            .move_to("f1", "dest", "src")
            .await
            .unwrap();
        assert_eq!(file.parents, vec!["dest".to_string()]);
    }

    #[tokio::test]
    async fn move_to_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let err = FilesApi::new(&client)
            .move_to("f1", "dest", "src")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    #[tokio::test]
    async fn move_to_appends_write_scope_hint_on_insufficient_permissions() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(
                wiremock::ResponseTemplate::new(403).set_body_json(serde_json::json!({
                    "error": {
                        "message": "Insufficient Permission",
                        "errors": [{"reason": "insufficientPermissions"}],
                    }
                })),
            )
            .mount(&server)
            .await;

        let err = FilesApi::new(&client)
            .move_to("f1", "dest", "src")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("drive auth login --write"),
            "{err}"
        );
    }

    // ── create ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn create_sends_name_mime_type_and_parents() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/drive/v3/files"))
            .and(wiremock::matchers::query_param("fields", GET_FIELDS))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "name": "New File",
                "mimeType": "text/plain",
                "parents": ["parent-1"],
            })))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "New File", "mimeType": "text/plain",
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let file = FilesApi::new(&client)
            .create("New File", "parent-1", "text/plain")
            .await
            .unwrap();
        assert_eq!(file.id, "f1");
        assert_eq!(file.name, "New File");
    }

    #[tokio::test]
    async fn create_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/drive/v3/files"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;

        let err = FilesApi::new(&client)
            .create("New File", "parent-1", "text/plain")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn create_appends_write_scope_hint_on_insufficient_permissions() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/drive/v3/files"))
            .respond_with(
                wiremock::ResponseTemplate::new(403).set_body_json(serde_json::json!({
                    "error": {
                        "message": "Insufficient Permission",
                        "errors": [{"reason": "insufficientPermissions"}],
                    }
                })),
            )
            .mount(&server)
            .await;

        let err = FilesApi::new(&client)
            .create("New File", "parent-1", "text/plain")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("--write-file"), "{err}");
        assert!(err.to_string().contains("--write-full"), "{err}");
    }

    // ── upload ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn upload_sends_multipart_related_content_type_and_returns_file() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/upload/drive/v3/files"))
            .and(wiremock::matchers::query_param("uploadType", "multipart"))
            .and(wiremock::matchers::header_regex(
                "content-type",
                "^multipart/related; boundary=omnidev-",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "new-upload-1", "name": "photo.jpg",
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let file = FilesApi::new(&client)
            .upload("photo.jpg", "parent-1", b"JPEGDATA", "image/jpeg")
            .await
            .unwrap();
        assert_eq!(file.id, "new-upload-1");
    }

    #[tokio::test]
    async fn upload_refuses_oversized_content_before_any_network_call() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        // Deliberately no mock mounted — an oversized upload must be
        // refused before ever building/sending the request.
        let oversized = vec![0u8; (MAX_UPLOAD_BYTES + 1) as usize];

        let err = FilesApi::new(&client)
            .upload(
                "big.bin",
                "parent-1",
                &oversized,
                "application/octet-stream",
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("refusing to upload"), "{err}");
    }

    #[tokio::test]
    async fn upload_refuses_content_type_containing_crlf_before_any_network_call() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        // Deliberately no mock mounted — a CRLF in content_type must be
        // refused before ever building the multipart body, where it would
        // otherwise splice raw bytes into Drive's request.
        let err = FilesApi::new(&client)
            .upload(
                "f.txt",
                "parent-1",
                b"content",
                "text/plain\r\n--boundary\r\nX-Injected: yes",
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("refusing content type"), "{err}");
    }

    #[tokio::test]
    async fn upload_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/upload/drive/v3/files"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;

        let err = FilesApi::new(&client)
            .upload("f.txt", "parent-1", b"content", "text/plain")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn upload_appends_write_scope_hint_on_insufficient_permissions() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/upload/drive/v3/files"))
            .respond_with(
                wiremock::ResponseTemplate::new(403).set_body_json(serde_json::json!({
                    "error": {
                        "message": "Insufficient Permission",
                        "errors": [{"reason": "insufficientPermissions"}],
                    }
                })),
            )
            .mount(&server)
            .await;

        let err = FilesApi::new(&client)
            .upload("f.txt", "parent-1", b"content", "text/plain")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("--write-file"), "{err}");
        assert!(err.to_string().contains("--write-full"), "{err}");
    }

    // ── edit_content ────────────────────────────────────────────────

    #[tokio::test]
    async fn edit_content_sends_media_upload_type_and_returns_file() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/upload/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param("uploadType", "media"))
            .and(wiremock::matchers::header("content-type", "text/plain"))
            .and(wiremock::matchers::body_bytes(b"new content".to_vec()))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "existing.txt",
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let file = FilesApi::new(&client)
            .edit_content("f1", b"new content", "text/plain")
            .await
            .unwrap();
        assert_eq!(file.id, "f1");
    }

    #[tokio::test]
    async fn edit_content_refuses_oversized_content_before_any_network_call() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let oversized = vec![0u8; (MAX_UPLOAD_BYTES + 1) as usize];

        let err = FilesApi::new(&client)
            .edit_content("f1", &oversized, "application/octet-stream")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("refusing to upload"), "{err}");
    }

    #[tokio::test]
    async fn edit_content_refuses_content_type_containing_crlf_before_any_network_call() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        // Deliberately no mock mounted.

        let err = FilesApi::new(&client)
            .edit_content("f1", b"content", "text/plain\r\nX-Injected: yes")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("refusing content type"), "{err}");
    }

    #[tokio::test]
    async fn edit_content_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/upload/drive/v3/files/f1"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;

        let err = FilesApi::new(&client)
            .edit_content("f1", b"content", "text/plain")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn edit_content_appends_write_scope_hint_on_insufficient_permissions() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/upload/drive/v3/files/f1"))
            .respond_with(
                wiremock::ResponseTemplate::new(403).set_body_json(serde_json::json!({
                    "error": {
                        "message": "Insufficient Permission",
                        "errors": [{"reason": "insufficientPermissions"}],
                    }
                })),
            )
            .mount(&server)
            .await;

        let err = FilesApi::new(&client)
            .edit_content("f1", b"content", "text/plain")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("--write-file"), "{err}");
        assert!(err.to_string().contains("--write-full"), "{err}");
    }

    #[test]
    fn build_file_edit_content_url_includes_upload_type_fields_and_supports_all_drives() {
        let url = build_file_edit_content_url("https://www.googleapis.com", "f1").unwrap();
        assert!(url.path().ends_with("/upload/drive/v3/files/f1"));
        assert!(url.as_str().contains("uploadType=media"));
        assert!(url.as_str().contains("fields="));
        assert!(url.as_str().contains("supportsAllDrives=true"));
    }

    #[test]
    fn append_write_scope_hint_leaves_other_errors_unchanged() {
        let err = anyhow::anyhow!("some other failure");
        let msg = append_write_scope_hint(err, WriteCapability::Metadata).to_string();
        assert_eq!(msg, "some other failure");
    }

    fn insufficient_permissions_error() -> anyhow::Error {
        DriveError::ApiRequestFailed {
            api: "Drive",
            status: 403,
            body: String::new(),
            reason: Some("insufficientPermissions".to_string()),
        }
        .into()
    }

    /// The same condition as [`insufficient_permissions_error`], spelled the
    /// way the `google.rpc` envelope spells it — what Sheets v4 returns.
    fn permission_denied_error() -> anyhow::Error {
        DriveError::ApiRequestFailed {
            api: "Drive",
            status: 403,
            body: String::new(),
            reason: Some("PERMISSION_DENIED".to_string()),
        }
        .into()
    }

    #[test]
    fn append_write_scope_hint_also_matches_the_google_rpc_permission_denied_spelling() {
        // Regression guard for issue #1589: Sheets v4 has no
        // `error.errors[].reason`, so a hint keyed only on
        // "insufficientPermissions" would never fire for it — and would fail
        // silently, leaving the operator a bare 403 with no flag to run.
        let msg = append_write_scope_hint(permission_denied_error(), WriteCapability::EditContent)
            .to_string();
        assert!(msg.contains("--write-file"), "{msg}");
        assert!(msg.contains("--write-full"), "{msg}");
    }

    #[test]
    fn append_write_scope_hint_ignores_an_unrelated_reason_code() {
        let err: anyhow::Error = DriveError::ApiRequestFailed {
            api: "Drive",
            status: 404,
            body: "gone".to_string(),
            reason: Some("notFound".to_string()),
        }
        .into();
        let msg = append_write_scope_hint(err, WriteCapability::EditContent).to_string();
        assert!(!msg.contains("--write-file"), "{msg}");
    }

    #[test]
    fn append_write_scope_hint_metadata_names_write_flag() {
        let msg =
            append_write_scope_hint(insufficient_permissions_error(), WriteCapability::Metadata)
                .to_string();
        assert!(msg.contains("--write"), "{msg}");
        assert!(msg.contains("rename/move"), "{msg}");
    }

    #[test]
    fn append_write_scope_hint_create_or_upload_names_write_file_flag() {
        let msg = append_write_scope_hint(
            insufficient_permissions_error(),
            WriteCapability::CreateOrUpload,
        )
        .to_string();
        assert!(msg.contains("--write-file"), "{msg}");
        assert!(msg.contains("--write-full"), "{msg}");
    }

    #[test]
    fn append_write_scope_hint_edit_content_names_both_flags() {
        let msg = append_write_scope_hint(
            insufficient_permissions_error(),
            WriteCapability::EditContent,
        )
        .to_string();
        assert!(msg.contains("--write-file"), "{msg}");
        assert!(msg.contains("--write-full"), "{msg}");
    }

    // ── check_download_size ─────────────────────────────────────────

    #[test]
    fn check_download_size_rejects_a_length_over_the_cap() {
        let err = check_download_size(Some(MAX_DOWNLOAD_BYTES + 1)).unwrap_err();
        assert!(err.to_string().contains("refusing to load"), "{err}");
    }

    #[test]
    fn check_download_size_allows_a_length_at_the_cap() {
        assert!(check_download_size(Some(MAX_DOWNLOAD_BYTES)).is_ok());
    }

    #[test]
    fn check_download_size_allows_a_missing_length() {
        assert!(check_download_size(None).is_ok());
    }

    // ── check_upload_size ───────────────────────────────────────────

    #[test]
    fn check_upload_size_rejects_a_length_over_the_cap() {
        let err = check_upload_size(MAX_UPLOAD_BYTES + 1).unwrap_err();
        assert!(err.to_string().contains("refusing to upload"), "{err}");
    }

    #[test]
    fn check_upload_size_allows_a_length_at_the_cap() {
        assert!(check_upload_size(MAX_UPLOAD_BYTES).is_ok());
    }

    // ── build_multipart_related_body ────────────────────────────────

    #[test]
    fn multipart_body_has_two_parts_separated_by_the_boundary() {
        let metadata = serde_json::json!({"name": "photo.jpg", "parents": ["p1"]});
        let body = build_multipart_related_body(&metadata, b"JPEGDATA", "image/jpeg", "BOUNDARY");
        let body_str = String::from_utf8(body).unwrap();
        assert_eq!(
            body_str,
            "--BOUNDARY\r\n\
             Content-Type: application/json; charset=UTF-8\r\n\r\n\
             {\"name\":\"photo.jpg\",\"parents\":[\"p1\"]}\r\n\
             --BOUNDARY\r\n\
             Content-Type: image/jpeg\r\n\r\n\
             JPEGDATA\r\n\
             --BOUNDARY--"
        );
    }

    #[test]
    fn multipart_body_preserves_binary_content_byte_for_byte() {
        let metadata = serde_json::json!({"name": "bin"});
        let binary_content: Vec<u8> = vec![0x00, 0xFF, 0x0D, 0x0A, 0x2D, 0x2D, 0x01];
        let body = build_multipart_related_body(
            &metadata,
            &binary_content,
            "application/octet-stream",
            "B",
        );
        // The exact byte sequence must appear intact, unmangled by any
        // text-mode transformation.
        let needle_pos = body
            .windows(binary_content.len())
            .position(|w| w == binary_content.as_slice());
        assert!(
            needle_pos.is_some(),
            "binary content not found intact in body"
        );
    }

    #[test]
    fn multipart_body_ends_with_the_closing_boundary_no_trailing_bytes() {
        let metadata = serde_json::json!({});
        let body = build_multipart_related_body(&metadata, b"x", "text/plain", "B");
        assert!(body.ends_with(b"--B--"));
    }

    #[test]
    fn generate_multipart_boundary_produces_distinct_values() {
        let a = generate_multipart_boundary();
        let b = generate_multipart_boundary();
        assert_ne!(a, b);
        assert!(a.starts_with("omnidev-"));
    }

    // ── build_file_upload_url ───────────────────────────────────────

    #[test]
    fn build_file_upload_url_includes_upload_type_fields_and_supports_all_drives() {
        let url = build_file_upload_url("https://www.googleapis.com").unwrap();
        assert!(url.path().ends_with("/upload/drive/v3/files"));
        assert!(url.as_str().contains("uploadType=multipart"));
        assert!(url.as_str().contains("fields="));
        assert!(url.as_str().contains("supportsAllDrives=true"));
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
