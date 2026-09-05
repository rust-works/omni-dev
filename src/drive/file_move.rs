//! Drive file move — the security-gated capability this whole feature
//! exists for ([ADR-0070](../../docs/adrs/adr-0070.md)).
//!
//! Adapts `crate::git::worktree_push`'s two-phase Plan/Execute shape, with
//! one deliberate divergence from its doc comment's claim: `worktree_push`
//! says "planning does not touch the network" because there's a local ACL
//! cache (`refs/remotes/<remote>/<branch>`) to plan from — Drive has
//! nothing analogous. What actually transfers, and matters here, is
//! narrower: **planning never calls the one mutating endpoint**
//! (`files.update`), so the plan a `--dry-run` shows is classified from the
//! exact same `permissions.list` reads [`execute`] will act on.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::drive::client::DriveClient;
use crate::drive::files_api::FilesApi;
use crate::drive::permissions_api::PermissionsApi;
use crate::drive::types::{DrivePermission, GOOGLE_FOLDER_MIME_TYPE};
use crate::drive::visibility::{self, BlockReasons, MoveGateFlags, VisibilityDiff};
use crate::request_log::{self, DriveMutationOutcome};

/// Per-batch move options.
#[derive(Debug, Clone)]
pub struct MoveOptions {
    /// The single shared destination folder every file in the batch moves
    /// into. One destination per invocation — different files to different
    /// destinations in one call is an explicit v1 non-goal.
    pub dest_folder_id: String,
    /// Allows a move that would grant new principals access.
    pub allow_visibility_increase: bool,
    /// Allows a move that would revoke existing principals' access.
    pub allow_visibility_decrease: bool,
    /// Allows a move across a My Drive / Shared Drive boundary.
    pub allow_drive_boundary_crossing: bool,
}

impl MoveOptions {
    fn gate_flags(&self) -> MoveGateFlags {
        MoveGateFlags {
            allow_visibility_increase: self.allow_visibility_increase,
            allow_visibility_decrease: self.allow_visibility_decrease,
            allow_drive_boundary_crossing: self.allow_drive_boundary_crossing,
        }
    }
}

/// A CLI/log-friendly rendering of a [`VisibilityDiff`].
///
/// Principal display strings rather than the `Principal` enum. `None` on a
/// [`MoveOutcome`] (rather than an always-present-but-possibly-empty
/// report) means "nothing to report," so a clear `WouldMove`/`Moved`
/// prints no visibility section at all.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct VisibilityDiffReport {
    /// Principals gaining access, as `"user:alice@example.com"` /
    /// `"group:..."` / `"domain:..."` / `"anyone"`.
    pub added: Vec<String>,
    /// Principals losing access, same rendering as `added`.
    pub removed: Vec<String>,
}

impl From<&VisibilityDiff> for VisibilityDiffReport {
    fn from(diff: &VisibilityDiff) -> Self {
        Self {
            added: diff.added.iter().map(ToString::to_string).collect(),
            removed: diff.removed.iter().map(ToString::to_string).collect(),
        }
    }
}

/// The planned (and, after [`execute`], final) batch.
#[derive(Debug, Clone, Serialize)]
pub struct MovePlan {
    /// The shared destination every file in `files` was planned against.
    pub dest_folder_id: String,
    /// One entry per requested file id, in request order.
    pub files: Vec<MoveOutcome>,
}

/// What happened (or, in a plan, would happen) to one file.
#[derive(Debug, Clone, Serialize)]
pub struct MoveOutcome {
    /// The Drive file id acted on.
    pub file_id: String,
    /// The file's name at the time it was planned.
    pub name: String,
    /// The file's parent folder ids before this move — what [`execute`]
    /// passes as `removeParents`. Also shown in `--dry-run` output as the
    /// "from" side of the move.
    pub current_parents: Vec<String>,
    /// Whether the moved item is itself a folder — its own visibility
    /// changes without its contents' visibility being evaluated (folder
    /// moves don't recurse in v1). The CLI warns loudly on this.
    pub is_folder: bool,
    /// Whether this move crosses a My Drive / Shared Drive boundary.
    pub crosses_drive_boundary: bool,
    /// The visibility diff, when the move would change anything. `None`
    /// when the diff is empty (nothing to report) — see the type's doc.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<VisibilityDiffReport>,
    /// The classification / outcome.
    #[serde(flatten)]
    pub result: MoveResult,
}

/// The per-file classification and outcome.
///
/// [`plan`] only ever produces `AlreadyInFolder` / `WouldMove` / `Blocked`
/// / `Failed`; [`execute`] turns each `WouldMove` into `Moved` or `Failed`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum MoveResult {
    /// The destination is already the file's sole parent — a no-op,
    /// detected before any `permissions.list` call.
    AlreadyInFolder,
    /// Clear to move: no gate is blocking it.
    WouldMove,
    /// Refused by at least one safety gate.
    Blocked {
        /// Every gate that blocked this move — a move can fail more than
        /// one simultaneously.
        reasons: BlockReasons,
    },
    /// Moved.
    Moved,
    /// The plan step or the `files.update` call failed.
    Failed {
        /// The error, as displayed.
        detail: String,
    },
}

impl MoveResult {
    /// Whether [`execute`] still has something to do for this outcome.
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        matches!(self, Self::WouldMove)
    }

    /// The `kind:` string [`record_attempt`] logs — matches this variant's
    /// `#[serde(rename_all = "kebab-case")]` tag exactly, kept as a
    /// hand-written match (not derived) since the request log's `status`
    /// field is deliberately decoupled from the wire `#[serde(tag = ...)]`
    /// shape.
    fn log_status(&self) -> &'static str {
        match self {
            Self::AlreadyInFolder => "already-in-folder",
            Self::WouldMove => "would-move",
            Self::Blocked { .. } => "blocked",
            Self::Moved => "moved",
            Self::Failed { .. } => "failed",
        }
    }
}

/// Plans a batch move, classifying every requested file against
/// `opts.dest_folder_id`.
///
/// Validates the destination resolves to an actual folder once, up front,
/// for the whole batch — a single clear error rather than N confusing
/// per-file failures when it doesn't. A per-file failure during planning
/// (a bad file id, a `permissions.list` error) does **not** abort the
/// batch: it becomes that file's own [`MoveResult::Failed`] outcome, so one
/// bad id can't take down an otherwise-valid batch.
///
/// **Contacts no mutating endpoint** — see the module doc.
pub async fn plan(
    client: &DriveClient,
    file_ids: &[String],
    opts: &MoveOptions,
) -> Result<MovePlan> {
    let files_api = FilesApi::new(client);
    let permissions_api = PermissionsApi::new(client);

    let dest_folder = files_api
        .get_metadata(&opts.dest_folder_id)
        .await
        .with_context(|| {
            format!(
                "Failed to resolve destination folder '{}'",
                opts.dest_folder_id
            )
        })?;
    anyhow::ensure!(
        dest_folder.mime_type == GOOGLE_FOLDER_MIME_TYPE,
        "'{}' ({}) is not a folder — `drive move` can only move files into a folder",
        dest_folder.name,
        opts.dest_folder_id
    );

    // Pre-populate the cache with the destination's permissions — every
    // file in the batch shares this same fetch, verified by wiremock
    // call-count assertions in tests.
    let mut permission_cache: HashMap<String, Vec<DrivePermission>> = HashMap::new();
    let dest_perms = permissions_api.list_all(&opts.dest_folder_id).await?;
    permission_cache.insert(opts.dest_folder_id.clone(), dest_perms);

    let mut files = Vec::with_capacity(file_ids.len());
    for file_id in file_ids {
        files.push(
            plan_one(
                &files_api,
                &permissions_api,
                &mut permission_cache,
                file_id,
                &dest_folder,
                opts,
            )
            .await,
        );
    }

    Ok(MovePlan {
        dest_folder_id: opts.dest_folder_id.clone(),
        files,
    })
}

/// Plans one file, catching every internal error into a [`MoveResult::Failed`]
/// outcome rather than propagating it — the per-file isolation [`plan`]'s
/// doc promises.
async fn plan_one(
    files_api: &FilesApi<'_>,
    permissions_api: &PermissionsApi<'_>,
    permission_cache: &mut HashMap<String, Vec<DrivePermission>>,
    file_id: &str,
    dest_folder: &crate::drive::types::DriveFile,
    opts: &MoveOptions,
) -> MoveOutcome {
    match plan_one_inner(
        files_api,
        permissions_api,
        permission_cache,
        file_id,
        dest_folder,
        opts,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(err) => MoveOutcome {
            file_id: file_id.to_string(),
            name: String::new(),
            current_parents: Vec::new(),
            is_folder: false,
            crosses_drive_boundary: false,
            visibility: None,
            result: MoveResult::Failed {
                detail: err.to_string(),
            },
        },
    }
}

async fn plan_one_inner(
    files_api: &FilesApi<'_>,
    permissions_api: &PermissionsApi<'_>,
    permission_cache: &mut HashMap<String, Vec<DrivePermission>>,
    file_id: &str,
    dest_folder: &crate::drive::types::DriveFile,
    opts: &MoveOptions,
) -> Result<MoveOutcome> {
    let file = files_api.get_metadata(file_id).await?;
    let is_folder = file.mime_type == GOOGLE_FOLDER_MIME_TYPE;

    // No-op short-circuit: the destination is already the file's sole
    // parent. Checked before any `permissions.list` call — nothing to
    // diff, since nothing would change.
    if file.parents.len() == 1 && file.parents[0] == opts.dest_folder_id {
        return Ok(MoveOutcome {
            file_id: file_id.to_string(),
            name: file.name,
            current_parents: file.parents,
            is_folder,
            crosses_drive_boundary: false,
            visibility: None,
            result: MoveResult::AlreadyInFolder,
        });
    }

    let file_perms = permissions_api.list_all(file_id).await?;

    let mut current_parent_perms = Vec::new();
    for parent_id in &file.parents {
        current_parent_perms
            .extend(fetch_cached(permissions_api, permission_cache, parent_id).await?);
    }

    let dest_perms = fetch_cached(permissions_api, permission_cache, &opts.dest_folder_id).await?;

    let diff = visibility::diff_visibility(&file_perms, &current_parent_perms, &dest_perms);
    let crosses_boundary = file.drive_id != dest_folder.drive_id;

    let block_reasons = visibility::classify(&diff, crosses_boundary, opts.gate_flags());
    let visibility_report = if diff.added.is_empty() && diff.removed.is_empty() {
        None
    } else {
        Some(VisibilityDiffReport::from(&diff))
    };

    let result = match block_reasons {
        Some(reasons) => MoveResult::Blocked { reasons },
        None => MoveResult::WouldMove,
    };

    Ok(MoveOutcome {
        file_id: file_id.to_string(),
        name: file.name,
        current_parents: file.parents,
        is_folder,
        crosses_drive_boundary: crosses_boundary,
        visibility: visibility_report,
        result,
    })
}

/// Fetches `folder_id`'s permissions, reusing `cache` when another file in
/// the same batch already fetched it (the destination, or a shared current
/// parent) — the batch-wide `permissions.list` cache-hit behavior tested
/// via wiremock call-count assertions.
async fn fetch_cached(
    permissions_api: &PermissionsApi<'_>,
    cache: &mut HashMap<String, Vec<DrivePermission>>,
    folder_id: &str,
) -> Result<Vec<DrivePermission>> {
    if let Some(cached) = cache.get(folder_id) {
        return Ok(cached.clone());
    }
    let perms = permissions_api.list_all(folder_id).await?;
    cache.insert(folder_id.to_string(), perms.clone());
    Ok(perms)
}

/// Executes a [`MovePlan`], moving every file still `WouldMove`.
///
/// The rest (`AlreadyInFolder`/`Blocked`/already-`Failed`) pass through
/// unchanged. A `files.update` failure becomes that file's own
/// [`MoveResult::Failed`] — the batch continues regardless. Every outcome
/// — moved, blocked, already-in-folder, or failed — is logged via
/// [`request_log::record_drive_mutation`] from inside this function, not
/// the CLI layer: "every move must be logged" needs to hold for every
/// current and future caller, and a `Blocked` outcome makes no API call at
/// all, so this is the only place that refusal is ever recorded.
#[must_use]
pub async fn execute(client: &DriveClient, plan: MovePlan) -> Vec<MoveOutcome> {
    let files_api = FilesApi::new(client);
    let dest_folder_id = plan.dest_folder_id;

    let mut outcomes = Vec::with_capacity(plan.files.len());
    for mut outcome in plan.files {
        let started = Instant::now();
        if outcome.result.is_pending() {
            let remove_parents = outcome.current_parents.join(",");
            outcome.result = match files_api
                .move_to(&outcome.file_id, &dest_folder_id, &remove_parents)
                .await
            {
                Ok(_) => MoveResult::Moved,
                Err(err) => MoveResult::Failed {
                    detail: err.to_string(),
                },
            };
        }
        record_attempt(&outcome, started.elapsed());
        outcomes.push(outcome);
    }
    outcomes
}

/// Builds and writes the [`DriveMutationOutcome`] for one `move` attempt.
fn record_attempt(outcome: &MoveOutcome, duration: Duration) {
    let error = match &outcome.result {
        MoveResult::Failed { detail } => Some(detail.clone()),
        _ => None,
    };
    let (added_principals, removed_principals) = outcome
        .visibility
        .as_ref()
        .map(|v| (v.added.clone(), v.removed.clone()))
        .unwrap_or_default();

    request_log::record_drive_mutation(DriveMutationOutcome {
        operation: "move",
        file_id: outcome.file_id.clone(),
        file_name: outcome.name.clone(),
        status: outcome.result.log_status().to_string(),
        added_principals,
        removed_principals,
        crosses_drive_boundary: outcome.crosses_drive_boundary,
        // Move is gated by the visibility diff, not the folder
        // write-permission gate.
        resolved_folder_id: None,
        decided_by_folder_id: None,
        decided_by_depth: None,
        error,
        duration,
        ..Default::default()
    });
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

    fn opts(dest: &str) -> MoveOptions {
        MoveOptions {
            dest_folder_id: dest.to_string(),
            allow_visibility_increase: false,
            allow_visibility_decrease: false,
            allow_drive_boundary_crossing: false,
        }
    }

    async fn mount_file(server: &wiremock::MockServer, id: &str, body: serde_json::Value) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!("/drive/v3/files/{id}")))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    fn permissions_body(entries: &[(&str, &str)]) -> serde_json::Value {
        serde_json::json!({
            "permissions": entries.iter().map(|(id, email)| serde_json::json!({
                "id": id, "type": "user", "role": "reader", "emailAddress": email,
            })).collect::<Vec<_>>(),
        })
    }

    // ── plan: destination validation ────────────────────────────────

    #[tokio::test]
    async fn plan_errors_when_destination_is_not_a_folder() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/dest1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "dest1", "name": "not-a-folder.txt", "mimeType": "text/plain",
                })),
            )
            .mount(&server)
            .await;

        let err = plan(&client, &["f1".to_string()], &opts("dest1"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("is not a folder"), "{err}");
    }

    #[tokio::test]
    async fn plan_errors_when_destination_fetch_fails() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/dest1"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;

        let err = plan(&client, &["f1".to_string()], &opts("dest1"))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("Failed to resolve destination"),
            "{err}"
        );
    }

    // ── plan: per-file outcomes ──────────────────────────────────────

    #[tokio::test]
    async fn plan_detects_already_in_folder_without_any_permissions_call() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/dest1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "dest1", "name": "Dest", "mimeType": GOOGLE_FOLDER_MIME_TYPE,
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "Already Here", "parents": ["dest1"],
                })),
            )
            .mount(&server)
            .await;
        // No `permissions` mock mounted at all for either id — asserting
        // the short-circuit via absence would only prove "nothing 404'd";
        // this instead asserts on a mock that would fail the test if
        // called more than the expected zero times for the file, while
        // the destination's own permissions fetch is expected exactly
        // once (plan() always primes the cache with it up front,
        // independent of any file's outcome).
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/drive/v3/files/dest1/permissions",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(permissions_body(&[])))
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1/permissions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(permissions_body(&[])))
            .expect(0)
            .mount(&server)
            .await;

        let result = plan(&client, &["f1".to_string()], &opts("dest1"))
            .await
            .unwrap();
        assert!(matches!(
            result.files[0].result,
            MoveResult::AlreadyInFolder
        ));
    }

    #[tokio::test]
    async fn plan_detects_a_clear_move_with_no_visibility_change() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file(
            &server,
            "dest1",
            serde_json::json!({"id": "dest1", "name": "Dest", "mimeType": GOOGLE_FOLDER_MIME_TYPE}),
        )
        .await;
        mount_file(
            &server,
            "f1",
            serde_json::json!({"id": "f1", "name": "Report", "parents": ["src1"]}),
        )
        .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1/permissions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(permissions_body(&[("p1", "alice@example.com")])),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/src1/permissions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(permissions_body(&[("p1", "alice@example.com")])),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/drive/v3/files/dest1/permissions",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(permissions_body(&[("p1", "alice@example.com")])),
            )
            .mount(&server)
            .await;

        let result = plan(&client, &["f1".to_string()], &opts("dest1"))
            .await
            .unwrap();
        assert!(matches!(result.files[0].result, MoveResult::WouldMove));
        assert!(result.files[0].visibility.is_none());
    }

    #[tokio::test]
    async fn plan_blocks_a_visibility_increase_by_default_and_allows_when_opted_in() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file(
            &server,
            "dest1",
            serde_json::json!({"id": "dest1", "name": "Dest", "mimeType": GOOGLE_FOLDER_MIME_TYPE}),
        )
        .await;
        mount_file(
            &server,
            "f1",
            serde_json::json!({"id": "f1", "name": "Report", "parents": ["src1"]}),
        )
        .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1/permissions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(permissions_body(&[("p1", "alice@example.com")])),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/src1/permissions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(permissions_body(&[("p1", "alice@example.com")])),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/drive/v3/files/dest1/permissions",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(permissions_body(&[
                    ("p1", "alice@example.com"),
                    ("p2", "bob@example.com"),
                ])),
            )
            .mount(&server)
            .await;

        let blocked = plan(&client, &["f1".to_string()], &opts("dest1"))
            .await
            .unwrap();
        let MoveResult::Blocked { reasons } = &blocked.files[0].result else {
            panic!("expected Blocked, got {:?}", blocked.files[0].result);
        };
        assert!(reasons.visibility_increase);
        assert_eq!(
            blocked.files[0].visibility.as_ref().unwrap().added,
            vec!["user:bob@example.com".to_string()]
        );

        let mut allowed_opts = opts("dest1");
        allowed_opts.allow_visibility_increase = true;
        let allowed = plan(&client, &["f1".to_string()], &allowed_opts)
            .await
            .unwrap();
        assert!(matches!(allowed.files[0].result, MoveResult::WouldMove));
    }

    #[tokio::test]
    async fn plan_blocks_a_drive_boundary_crossing_even_with_no_visibility_change() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file(
            &server,
            "dest1",
            serde_json::json!({
                "id": "dest1", "name": "Dest", "mimeType": GOOGLE_FOLDER_MIME_TYPE,
                "driveId": "shared-drive-1",
            }),
        )
        .await;
        mount_file(
            &server,
            "f1",
            serde_json::json!({"id": "f1", "name": "Report", "parents": ["src1"]}),
        )
        .await;
        for path in [
            "/drive/v3/files/f1/permissions",
            "/drive/v3/files/src1/permissions",
            "/drive/v3/files/dest1/permissions",
        ] {
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path(path))
                .respond_with(
                    wiremock::ResponseTemplate::new(200).set_body_json(permissions_body(&[])),
                )
                .mount(&server)
                .await;
        }

        let result = plan(&client, &["f1".to_string()], &opts("dest1"))
            .await
            .unwrap();
        let MoveResult::Blocked { reasons } = &result.files[0].result else {
            panic!("expected Blocked, got {:?}", result.files[0].result);
        };
        assert!(reasons.drive_boundary_crossing);
        assert!(!reasons.visibility_increase);
        assert!(!reasons.visibility_decrease);
    }

    // ── plan: batch behavior ─────────────────────────────────────────

    #[tokio::test]
    async fn plan_caches_dest_and_shared_current_parent_fetches_across_the_batch() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file(
            &server,
            "dest1",
            serde_json::json!({"id": "dest1", "name": "Dest", "mimeType": GOOGLE_FOLDER_MIME_TYPE}),
        )
        .await;
        mount_file(
            &server,
            "f1",
            serde_json::json!({"id": "f1", "name": "A", "parents": ["shared_parent"]}),
        )
        .await;
        mount_file(
            &server,
            "f2",
            serde_json::json!({"id": "f2", "name": "B", "parents": ["shared_parent"]}),
        )
        .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1/permissions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(permissions_body(&[])))
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f2/permissions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(permissions_body(&[])))
            .expect(1)
            .mount(&server)
            .await;
        // Shared parent and destination must each be fetched exactly once,
        // despite two files sharing them.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/drive/v3/files/shared_parent/permissions",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(permissions_body(&[])))
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/drive/v3/files/dest1/permissions",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(permissions_body(&[])))
            .expect(1)
            .mount(&server)
            .await;

        let result = plan(
            &client,
            &["f1".to_string(), "f2".to_string()],
            &opts("dest1"),
        )
        .await
        .unwrap();
        assert_eq!(result.files.len(), 2);
        // wiremock's .expect(1) assertions above are the real check —
        // this just confirms both outcomes were classified successfully.
        assert!(result
            .files
            .iter()
            .all(|f| matches!(f.result, MoveResult::WouldMove)));
    }

    #[tokio::test]
    async fn plan_a_permissions_list_failure_yields_failed_not_an_empty_set_fallback() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file(
            &server,
            "dest1",
            serde_json::json!({"id": "dest1", "name": "Dest", "mimeType": GOOGLE_FOLDER_MIME_TYPE}),
        )
        .await;
        mount_file(
            &server,
            "f1",
            serde_json::json!({"id": "f1", "name": "A", "parents": ["src1"]}),
        )
        .await;
        // The batch-level destination-permissions precondition must succeed
        // so the failure below is isolated to f1's own permissions.list call.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/drive/v3/files/dest1/permissions",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(permissions_body(&[])))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1/permissions"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let result = plan(&client, &["f1".to_string()], &opts("dest1"))
            .await
            .unwrap();
        assert!(
            matches!(result.files[0].result, MoveResult::Failed { .. }),
            "expected Failed, got {:?} — a permissions.list failure must never silently \
             degrade to an empty-set fallback",
            result.files[0].result
        );
    }

    #[tokio::test]
    async fn plan_one_bad_file_id_fails_only_that_file_batch_continues() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file(
            &server,
            "dest1",
            serde_json::json!({"id": "dest1", "name": "Dest", "mimeType": GOOGLE_FOLDER_MIME_TYPE}),
        )
        .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/drive/v3/files/dest1/permissions",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(permissions_body(&[])))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/missing"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;
        mount_file(
            &server,
            "f1",
            serde_json::json!({"id": "f1", "name": "A", "parents": ["dest1"]}),
        )
        .await;

        let result = plan(
            &client,
            &["missing".to_string(), "f1".to_string()],
            &opts("dest1"),
        )
        .await
        .unwrap();
        assert!(matches!(result.files[0].result, MoveResult::Failed { .. }));
        assert!(matches!(
            result.files[1].result,
            MoveResult::AlreadyInFolder
        ));
    }

    // ── execute ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn execute_moves_a_would_move_outcome_and_leaves_others_unchanged() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param("addParents", "dest1"))
            .and(wiremock::matchers::query_param("removeParents", "src1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "A", "parents": ["dest1"],
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let move_plan = MovePlan {
            dest_folder_id: "dest1".to_string(),
            files: vec![
                MoveOutcome {
                    file_id: "f1".to_string(),
                    name: "A".to_string(),
                    current_parents: vec!["src1".to_string()],
                    is_folder: false,
                    crosses_drive_boundary: false,
                    visibility: None,
                    result: MoveResult::WouldMove,
                },
                MoveOutcome {
                    file_id: "f2".to_string(),
                    name: "B".to_string(),
                    current_parents: vec!["dest1".to_string()],
                    is_folder: false,
                    crosses_drive_boundary: false,
                    visibility: None,
                    result: MoveResult::AlreadyInFolder,
                },
            ],
        };

        let outcomes = execute(&client, move_plan).await;
        assert!(matches!(outcomes[0].result, MoveResult::Moved));
        assert!(matches!(outcomes[1].result, MoveResult::AlreadyInFolder));
    }

    #[tokio::test]
    async fn execute_records_a_failed_move_to_call() {
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

        let move_plan = MovePlan {
            dest_folder_id: "dest1".to_string(),
            files: vec![MoveOutcome {
                file_id: "f1".to_string(),
                name: "A".to_string(),
                current_parents: vec!["src1".to_string()],
                is_folder: false,
                crosses_drive_boundary: false,
                visibility: None,
                result: MoveResult::WouldMove,
            }],
        };

        let outcomes = execute(&client, move_plan).await;
        let MoveResult::Failed { detail } = &outcomes[0].result else {
            panic!("expected Failed, got {:?}", outcomes[0].result);
        };
        assert!(detail.contains("drive auth login --write"), "{detail}");
    }

    // ── MoveResult::log_status ───────────────────────────────────────

    #[test]
    fn log_status_matches_the_serde_tag_for_every_variant() {
        assert_eq!(
            MoveResult::AlreadyInFolder.log_status(),
            "already-in-folder"
        );
        assert_eq!(MoveResult::WouldMove.log_status(), "would-move");
        assert_eq!(
            MoveResult::Blocked {
                reasons: BlockReasons::default()
            }
            .log_status(),
            "blocked"
        );
        assert_eq!(MoveResult::Moved.log_status(), "moved");
        assert_eq!(
            MoveResult::Failed {
                detail: "x".to_string()
            }
            .log_status(),
            "failed"
        );
    }
}
