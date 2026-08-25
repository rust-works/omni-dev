//! Ancestor folder-chain resolution for `crate::drive::write_gate` (issue
//! #1574).
//!
//! Impure sibling to `write_gate` — mirrors `crate::drive::file_move`'s
//! role relative to `crate::drive::visibility`: this module does the
//! fetching, `write_gate` does the (pure) classifying.

use anyhow::Result;

use crate::drive::files_api::FilesApi;
use crate::drive::types::DriveFile;
use crate::drive::write_gate::{self, Decision, DriveOperation, FolderPermissionRule};

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

/// Resolves `op` against `start_folder_id`'s ancestor chain under `rules`,
/// walking one `files.get` at a time and stopping as soon as some depth
/// decides it.
///
/// [`write_gate::resolve`]'s "closest ancestor wins" tie-break means that
/// once *any* depth in the chain has a matching rule, nothing farther up
/// can ever produce a closer match — so unlike [`resolve_ancestor_chain`]
/// (which a caller wanting the *full* chain, e.g. for display, should keep
/// using), this never walks past the first decisive depth. Equivalent to
/// `write_gate::resolve(&resolve_ancestor_chain(..).await?.folder_ids(), op, rules)`,
/// but touches only as many ancestors as necessary instead of always
/// walking to Drive's root. The same fetch-failure-is-always-`Err`
/// invariant [`resolve_ancestor_chain`] documents applies here too.
pub async fn resolve_decision(
    files_api: &FilesApi<'_>,
    start_folder_id: &str,
    op: DriveOperation,
    rules: &[FolderPermissionRule],
) -> Result<Decision> {
    let start = files_api.get_metadata(start_folder_id).await?;
    resolve_decision_from(files_api, start, op, rules).await
}

/// [`resolve_decision`], but for a caller that already has the starting
/// folder's metadata in hand.
///
/// Avoids re-fetching `start` as this walk's own first `files.get`.
/// Shared by [`resolve_decision_for_parents`] and `drive permissions
/// check`'s folder-target path.
pub async fn resolve_decision_from(
    files_api: &FilesApi<'_>,
    start: DriveFile,
    op: DriveOperation,
    rules: &[FolderPermissionRule],
) -> Result<Decision> {
    let start_id = start.id.clone();
    let mut next_parent = start.parents.first().cloned();
    let mut folder_ids = vec![start.id];
    loop {
        // Re-resolving against the whole chain fetched so far on every
        // iteration is O(chain_len × rules_len) instead of O(rules_len)
        // once — negligible against a typically-small configured rule
        // list, and it's what lets this reuse `write_gate::resolve`
        // verbatim rather than re-implementing its tie-break logic here.
        let decision = write_gate::resolve(&folder_ids, op, rules);
        if decision.decided_by.is_some() {
            return Ok(decision);
        }
        let Some(parent_id) = next_parent else {
            return Ok(decision);
        };
        anyhow::ensure!(
            folder_ids.len() < MAX_CHAIN_DEPTH,
            "folder ancestry for '{start_id}' exceeds {MAX_CHAIN_DEPTH} levels; refusing to \
             resolve a possibly-cyclic or pathological chain rather than truncating it"
        );
        let folder = files_api.get_metadata(&parent_id).await?;
        next_parent = folder.parents.first().cloned();
        folder_ids.push(folder.id);
    }
}

/// Resolves `op` against `parents` (a target's *current* parent ids).
///
/// Combines the per-parent [`Decision`]s via
/// [`write_gate::combine_across_parents`] for a legacy multi-parent target
/// (deny wins across parents). Also returns the single resolved folder id
/// when there's exactly one parent — `None` for an orphan target or a
/// multi-parent target, where no single folder id would be accurate.
///
/// Shared by `drive edit` (`crate::drive::content_edit`) and `drive
/// permissions check` (`crate::cli::drive::permissions::check`) — the two
/// callers whose target's chain starts at its *current* parent(s) rather
/// than a caller-supplied `--parent`. Centralizing the combine loop here
/// is what lets `permissions check` claim it can never drift from actual
/// enforcement: previously only the primitives were shared and this
/// orchestration was duplicated at both call sites.
pub async fn resolve_decision_for_parents(
    files_api: &FilesApi<'_>,
    parents: &[String],
    op: DriveOperation,
    rules: &[FolderPermissionRule],
) -> Result<(Decision, Option<String>)> {
    let Some((first_parent, rest_parents)) = parents.split_first() else {
        return Ok((write_gate::resolve(&[], op, rules), None));
    };
    let mut combined = resolve_decision(files_api, first_parent, op, rules).await?;
    for parent_id in rest_parents {
        let decision = resolve_decision(files_api, parent_id, op, rules).await?;
        combined = write_gate::combine_across_parents(combined, [decision]);
    }
    let resolved_folder_id = rest_parents.is_empty().then(|| first_parent.clone());
    Ok((combined, resolved_folder_id))
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

    // ── resolve_decision / resolve_decision_from ───────────────────────

    #[tokio::test]
    async fn resolve_decision_stops_walking_once_a_rule_decides_it() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_folder("child", Some("parent")).mount(&server).await;
        // Deliberately no mock for "parent" — "child" already has a
        // matching rule, so a correctly short-circuiting walk never fetches
        // it; wiremock panics with "no matching mock" if it does.
        let files_api = FilesApi::new(&client);
        let rules = [FolderPermissionRule {
            folder_id: "child".to_string(),
            recursive: false,
            allow: [DriveOperation::Create].into_iter().collect(),
            deny: Default::default(),
        }];

        let decision = resolve_decision(&files_api, "child", DriveOperation::Create, &rules)
            .await
            .unwrap();
        assert_eq!(decision.verdict, write_gate::Verdict::Allow);
    }

    #[tokio::test]
    async fn resolve_decision_walks_to_root_when_nothing_matches() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_folder("child", Some("parent")).mount(&server).await;
        mount_folder("parent", Some("grandparent"))
            .mount(&server)
            .await;
        mount_folder("grandparent", None).mount(&server).await;
        let files_api = FilesApi::new(&client);

        let decision = resolve_decision(&files_api, "child", DriveOperation::Create, &[])
            .await
            .unwrap();
        assert_eq!(decision.verdict, write_gate::Verdict::Deny);
        assert_eq!(decision.decided_by, None);
    }

    #[tokio::test]
    async fn resolve_decision_fetch_failure_returns_err_not_allow() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_folder("child", Some("parent")).mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/parent"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("server error"))
            .mount(&server)
            .await;
        let files_api = FilesApi::new(&client);

        let result = resolve_decision(&files_api, "child", DriveOperation::Create, &[]).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn resolve_decision_from_never_refetches_the_supplied_start() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        // Deliberately no mock at all — proves resolve_decision_from never
        // issues a files.get for the folder its caller already fetched.
        let files_api = FilesApi::new(&client);
        let start = DriveFile {
            id: "child".to_string(),
            name: "child".to_string(),
            mime_type: "application/vnd.google-apps.folder".to_string(),
            parents: vec![],
            ..Default::default()
        };
        let rules = [FolderPermissionRule {
            folder_id: "child".to_string(),
            recursive: false,
            allow: [DriveOperation::Read].into_iter().collect(),
            deny: Default::default(),
        }];

        let decision = resolve_decision_from(&files_api, start, DriveOperation::Read, &rules)
            .await
            .unwrap();
        assert_eq!(decision.verdict, write_gate::Verdict::Allow);
    }

    // ── resolve_decision_for_parents ────────────────────────────────────

    #[tokio::test]
    async fn resolve_decision_for_parents_empty_parents_uses_default_policy() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let files_api = FilesApi::new(&client);

        let (decision, resolved_folder_id) =
            resolve_decision_for_parents(&files_api, &[], DriveOperation::Edit, &[])
                .await
                .unwrap();
        assert_eq!(decision.verdict, write_gate::Verdict::Deny);
        assert_eq!(resolved_folder_id, None);
    }

    #[tokio::test]
    async fn resolve_decision_for_parents_single_parent_reports_its_folder_id() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_folder("parent-1", None).mount(&server).await;
        let files_api = FilesApi::new(&client);
        let rules = [FolderPermissionRule {
            folder_id: "parent-1".to_string(),
            recursive: false,
            allow: [DriveOperation::Edit].into_iter().collect(),
            deny: Default::default(),
        }];

        let (decision, resolved_folder_id) = resolve_decision_for_parents(
            &files_api,
            &["parent-1".to_string()],
            DriveOperation::Edit,
            &rules,
        )
        .await
        .unwrap();
        assert_eq!(decision.verdict, write_gate::Verdict::Allow);
        assert_eq!(resolved_folder_id, Some("parent-1".to_string()));
    }

    #[tokio::test]
    async fn resolve_decision_for_parents_deny_wins_across_parents_and_reports_no_single_folder() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_folder("allow-parent", None).mount(&server).await;
        mount_folder("deny-parent", None).mount(&server).await;
        let files_api = FilesApi::new(&client);
        let rules = [
            FolderPermissionRule {
                folder_id: "allow-parent".to_string(),
                recursive: false,
                allow: [DriveOperation::Edit].into_iter().collect(),
                deny: Default::default(),
            },
            FolderPermissionRule {
                folder_id: "deny-parent".to_string(),
                recursive: false,
                allow: Default::default(),
                deny: [DriveOperation::Edit].into_iter().collect(),
            },
        ];

        let (decision, resolved_folder_id) = resolve_decision_for_parents(
            &files_api,
            &["allow-parent".to_string(), "deny-parent".to_string()],
            DriveOperation::Edit,
            &rules,
        )
        .await
        .unwrap();
        assert_eq!(decision.verdict, write_gate::Verdict::Deny);
        assert_eq!(resolved_folder_id, None);
    }
}
