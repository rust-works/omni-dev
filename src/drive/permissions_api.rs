//! Drive Permissions API wrapper.
//!
//! Fetches the raw permission snapshots `crate::drive::visibility` diffs to
//! detect a move's effect on a file's visibility — this module does no
//! diffing itself, just auto-paginated fetching, mirroring `FilesApi`'s
//! shape.

use anyhow::Result;
use serde::Deserialize;
use url::Url;

use crate::drive::client::DriveClient;
use crate::drive::types::DrivePermission;

/// Safety cap on total permissions accumulated by [`PermissionsApi::list_all`].
///
/// A real file's permission list is normally tiny (a handful of grants);
/// this exists only to bound a runaway loop against a misbehaving or
/// malicious `nextPageToken` response, mirroring
/// `crate::drive::files_api::HARD_CAP`'s rationale. Unlike `files.list`,
/// `permissions.list` gives callers no `limit` to pass through, so there's
/// no caller-facing truncation-visibility concern to signal back (compare
/// `FilesApi::paginate`'s `next_page_token`/`incomplete_search` clearing) —
/// this is purely a defensive backstop.
const MAX_PERMISSIONS: usize = 10_000;

/// `fields` value for `permissions.list` — everything
/// `crate::drive::visibility::Principal`/`principal_set` needs, plus `role`
/// for informational logging (see `DrivePermission::role`'s doc).
const LIST_FIELDS: &str = "nextPageToken,permissions(id,type,role,emailAddress,domain)";

/// Permissions API façade.
#[derive(Debug)]
pub struct PermissionsApi<'a> {
    client: &'a DriveClient,
}

impl<'a> PermissionsApi<'a> {
    /// Wraps an existing [`DriveClient`] for permission operations.
    #[must_use]
    pub fn new(client: &'a DriveClient) -> Self {
        Self { client }
    }

    /// Fetches every permission on `file_or_folder_id`, auto-paginating —
    /// `permissions.list` doesn't distinguish a file from a folder, so this
    /// works for both (the `move` engine calls it on the file being moved,
    /// its current parent(s), and the destination folder alike).
    pub async fn list_all(&self, file_or_folder_id: &str) -> Result<Vec<DrivePermission>> {
        let mut acc: Vec<DrivePermission> = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let page = self
                .list_page(file_or_folder_id, page_token.as_deref())
                .await?;
            acc.extend(page.permissions);
            if acc.len() >= MAX_PERMISSIONS || page.next_page_token.is_none() {
                break;
            }
            page_token = page.next_page_token;
        }
        acc.truncate(MAX_PERMISSIONS);
        Ok(acc)
    }

    async fn list_page(
        &self,
        file_or_folder_id: &str,
        page_token: Option<&str>,
    ) -> Result<PermissionListResponse> {
        let url =
            build_permissions_list_url(self.client.base_url(), file_or_folder_id, page_token)?;
        self.client
            .get_parsed(url.as_str(), "Failed to parse permissions.list response")
            .await
    }
}

/// Response envelope for `GET /drive/v3/files/{fileId}/permissions`.
#[derive(Debug, Clone, Default, Deserialize)]
struct PermissionListResponse {
    #[serde(default)]
    permissions: Vec<DrivePermission>,
    #[serde(default, rename = "nextPageToken")]
    next_page_token: Option<String>,
}

/// No explicit `pageSize` is sent — unlike `files.list`'s `search`,
/// `list_all` takes no caller-supplied limit to translate into a per-page
/// size, so pagination is driven purely by `nextPageToken` against
/// whatever page size Drive chooses by default.
fn build_permissions_list_url(
    base_url: &str,
    file_or_folder_id: &str,
    page_token: Option<&str>,
) -> Result<Url> {
    let mut url = DriveClient::api_url(
        base_url,
        &format!("/drive/v3/files/{file_or_folder_id}/permissions"),
    )?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("fields", LIST_FIELDS);
        pairs.append_pair("supportsAllDrives", "true");
        if let Some(token) = page_token {
            pairs.append_pair("pageToken", token);
        }
    }
    Ok(url)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::utils::secret::Secret;

    fn test_credentials() -> DriveCredentials {
        DriveCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: DriveGrantedScopes::METADATA,
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

    #[tokio::test]
    async fn list_all_returns_a_single_page_verbatim() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1/permissions"))
            .and(wiremock::matchers::query_param("fields", LIST_FIELDS))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "permissions": [
                        {"id": "p1", "type": "user", "role": "reader", "emailAddress": "alice@example.com"},
                    ],
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let perms = PermissionsApi::new(&client).list_all("f1").await.unwrap();
        assert_eq!(perms.len(), 1);
        assert_eq!(perms[0].email_address.as_deref(), Some("alice@example.com"));
    }

    #[tokio::test]
    async fn list_all_follows_next_page_token_to_exhaustion() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1/permissions"))
            .and(wiremock::matchers::query_param_is_missing("pageToken"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "permissions": [
                        {"id": "p1", "type": "user", "role": "reader", "emailAddress": "alice@example.com"},
                    ],
                    "nextPageToken": "page-2",
                })),
            )
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1/permissions"))
            .and(wiremock::matchers::query_param("pageToken", "page-2"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "permissions": [
                        {"id": "p2", "type": "user", "role": "reader", "emailAddress": "bob@example.com"},
                    ],
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let perms = PermissionsApi::new(&client).list_all("f1").await.unwrap();
        assert_eq!(perms.len(), 2);
        assert_eq!(perms[0].email_address.as_deref(), Some("alice@example.com"));
        assert_eq!(perms[1].email_address.as_deref(), Some("bob@example.com"));
    }

    #[tokio::test]
    async fn list_all_returns_empty_for_a_permission_less_response() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1/permissions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;

        let perms = PermissionsApi::new(&client).list_all("f1").await.unwrap();
        assert!(perms.is_empty());
    }

    #[tokio::test]
    async fn list_all_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/drive/v3/files/missing/permissions",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;

        let err = PermissionsApi::new(&client)
            .list_all("missing")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn list_all_works_for_a_folder_id_the_same_as_a_file_id() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/drive/v3/files/folder1/permissions",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "permissions": [
                        {"id": "p1", "type": "domain", "role": "reader", "domain": "example.com"},
                    ],
                })),
            )
            .mount(&server)
            .await;

        let perms = PermissionsApi::new(&client)
            .list_all("folder1")
            .await
            .unwrap();
        assert_eq!(perms[0].domain.as_deref(), Some("example.com"));
    }
}
