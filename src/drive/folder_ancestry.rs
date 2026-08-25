//! Ancestor folder-chain resolution for `crate::drive::write_gate` (issue
//! #1574).
//!
//! Impure sibling to `write_gate` — mirrors `crate::drive::file_move`'s
//! role relative to `crate::drive::visibility`: this module does the
//! fetching, `write_gate` does the (pure) classifying.

use anyhow::Result;

use crate::drive::files_api::FilesApi;
use crate::drive::types::DriveFile;

/// Defensive cap against a cyclic or pathological parent chain, mirroring
/// `crate::drive::permissions_api::MAX_PERMISSIONS`'s rationale.
const MAX_CHAIN_DEPTH: usize = 100;

/// A target folder's resolved ancestor chain.
///
/// `folders[0]` is the target folder itself, `folders[1]` its parent, and
/// so on up to Drive's root (whose `parents` is empty).
#[derive(Debug, Clone)]
pub struct AncestorChain {
    /// The chain, in walk order (depth 0 first).
    pub folders: Vec<DriveFile>,
}

impl AncestorChain {
    /// The folder ids in walk order, ready for `write_gate::resolve`.
    #[must_use]
    pub fn folder_ids(&self) -> Vec<String> {
        self.folders.iter().map(|f| f.id.clone()).collect()
    }
}

/// Walks `start_folder_id` → parent → grandparent → ... via `files.get`,
/// stopping at Drive's root (a folder with no `parents`).
///
/// **Any** failure — a `files.get` error, or exceeding [`MAX_CHAIN_DEPTH`]
/// — is an `Err`, never a silently truncated `Ok` chain: a truncated chain
/// could hide a deny/allow rule configured above the truncation point.
/// This is the same hard invariant [ADR-0070](../../docs/adrs/adr-0070.md)
/// §3 established for `permissions.list` fetch failures (no
/// `unwrap_or_default()` empty-set fallback) — callers must turn this
/// `Err` into a refusal, never a silent default-allow.
///
/// v1 boundary: a folder with a legacy multi-parent (rare — Drive no
/// longer permits creating new multi-parent folders) walks its *first*
/// parent only, beyond depth 0.
pub async fn resolve_ancestor_chain(
    files_api: &FilesApi<'_>,
    start_folder_id: &str,
) -> Result<AncestorChain> {
    let mut folders = Vec::new();
    let mut current_id = start_folder_id.to_string();
    loop {
        anyhow::ensure!(
            folders.len() < MAX_CHAIN_DEPTH,
            "folder ancestry for '{start_folder_id}' exceeds {MAX_CHAIN_DEPTH} levels; \
             refusing to resolve a possibly-cyclic or pathological chain rather than \
             truncating it"
        );
        let folder = files_api.get_metadata(&current_id).await?;
        let next_parent = folder.parents.first().cloned();
        folders.push(folder);
        match next_parent {
            Some(parent_id) => current_id = parent_id,
            None => break,
        }
    }
    Ok(AncestorChain { folders })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::client::DriveClient;
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

    fn mount_folder(id: &str, parent: Option<&str>) -> wiremock::Mock {
        let parents: Vec<&str> = parent.into_iter().collect();
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!("/drive/v3/files/{id}")))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": id,
                    "name": id,
                    "mimeType": "application/vnd.google-apps.folder",
                    "parents": parents,
                })),
            )
    }

    #[tokio::test]
    async fn resolves_a_single_folder_with_no_parent_as_a_one_element_chain() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_folder("root", None).mount(&server).await;
        let files_api = FilesApi::new(&client);

        let chain = resolve_ancestor_chain(&files_api, "root").await.unwrap();
        assert_eq!(chain.folder_ids(), vec!["root".to_string()]);
    }

    #[tokio::test]
    async fn walks_the_full_ancestor_chain_via_files_get() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_folder("child", Some("parent")).mount(&server).await;
        mount_folder("parent", Some("grandparent"))
            .mount(&server)
            .await;
        mount_folder("grandparent", None).mount(&server).await;
        let files_api = FilesApi::new(&client);

        let chain = resolve_ancestor_chain(&files_api, "child").await.unwrap();
        assert_eq!(
            chain.folder_ids(),
            vec![
                "child".to_string(),
                "parent".to_string(),
                "grandparent".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn legacy_multi_parent_folder_walks_the_first_parent_only() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/child"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "child",
                    "name": "child",
                    "mimeType": "application/vnd.google-apps.folder",
                    "parents": ["first-parent", "second-parent"],
                })),
            )
            .mount(&server)
            .await;
        mount_folder("first-parent", None).mount(&server).await;
        // Deliberately no mock for "second-parent" — asserting it's never
        // fetched proves the documented v1 boundary (first-parent-only).
        let files_api = FilesApi::new(&client);

        let chain = resolve_ancestor_chain(&files_api, "child").await.unwrap();
        assert_eq!(
            chain.folder_ids(),
            vec!["child".to_string(), "first-parent".to_string()]
        );
    }

    /// The single highest-value test in this module (mirrors ADR-0070 §3's
    /// `permissions.list`-failure precedent): a fetch failure mid-walk must
    /// be an explicit `Err`, never a chain silently truncated at the point
    /// of failure — a truncated chain could hide a deny/allow rule
    /// configured on an ancestor above the failure point, manufacturing a
    /// false "no rule applies here" reading.
    #[tokio::test]
    async fn ancestor_chain_fetch_failure_returns_err_not_a_truncated_chain() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_folder("child", Some("parent")).mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/parent"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("server error"))
            .mount(&server)
            .await;
        let files_api = FilesApi::new(&client);

        let result = resolve_ancestor_chain(&files_api, "child").await;
        assert!(
            result.is_err(),
            "a mid-walk fetch failure must be Err, not an Ok chain truncated at \"child\""
        );
    }

    #[tokio::test]
    async fn missing_start_folder_is_an_error() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/missing"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;
        let files_api = FilesApi::new(&client);

        let result = resolve_ancestor_chain(&files_api, "missing").await;
        assert!(result.is_err());
    }
}
