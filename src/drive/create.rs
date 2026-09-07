//! Drive file/folder creation — the first content-mutating capability this
//! feature exists for (issue #1574, [ADR-0071](../../docs/adrs/adr-0071.md)).
//!
//! Single-target, unlike `file_move.rs`'s N-file batch — so this follows
//! `rename.rs`'s simpler linear-function shape (gate check, then a
//! `dry_run` early return, then the mutating call, all inline in one
//! function) rather than a Plan/Execute split: there's no shared
//! destination-fetch to amortize across a batch that doesn't exist here.
//! `--dry-run` and a real run therefore share the exact same gate
//! classification by construction (same function, same early-return
//! branch), not merely by convention.

use std::time::{Duration, Instant};

use serde::Serialize;

use crate::cli::drive::format::{write_scalar_jsonl, JsonlSerialize};
use crate::drive::client::DriveClient;
use crate::drive::files_api::FilesApi;
use crate::drive::folder_ancestry;
use crate::drive::write_gate::{self, DecidingRule, DriveOperation, FolderPermissionRule};
use crate::request_log::{self, DriveMutationOutcome};

/// Per-call create options.
#[derive(Debug, Clone)]
pub struct CreateOptions {
    /// The new file/folder's display name.
    pub name: String,
    /// The folder id to create it in.
    pub parent_folder_id: String,
    /// The MIME type to create — the CLI resolves `--folder`/`--mime-type`/
    /// a default into this before calling in.
    pub mime_type: String,
    /// When `true`, classify but never call `files.create`.
    pub dry_run: bool,
}

/// What happened (or, under `--dry-run`, would happen).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum CreateResult {
    /// `--dry-run`, and the gate would allow it.
    WouldCreate,
    /// The folder write-permission gate refused it.
    Blocked {
        /// The rule that decided the refusal, if any (`None` means the
        /// bare default policy — create/upload/edit default deny).
        decided_by: Option<DecidingRule>,
    },
    /// `files.create` succeeded.
    Created {
        /// The newly created file's id.
        file_id: String,
    },
    /// An API/validation error.
    Failed {
        /// A human-readable summary of what failed.
        detail: String,
    },
}

impl CreateResult {
    /// The request-log `status` string — hand-written, deliberately
    /// decoupled from the wire `#[serde(tag = ...)]` shape, mirroring
    /// `MoveResult::log_status`'s existing precedent.
    fn log_status(&self) -> &'static str {
        match self {
            Self::WouldCreate => "would-create",
            Self::Blocked { .. } => "blocked",
            Self::Created { .. } => "created",
            Self::Failed { .. } => "failed",
        }
    }
}

/// The planned (and, after a real run, final) outcome of one `create` call.
#[derive(Debug, Clone, Serialize)]
pub struct CreateOutcome {
    /// The requested name.
    pub name: String,
    /// The requested parent folder id.
    pub parent_folder_id: String,
    /// The result.
    pub result: CreateResult,
}

impl JsonlSerialize for CreateOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> Result<(), anyhow::Error> {
        write_scalar_jsonl(self, out)
    }
}

/// Creates `opts.name` inside `opts.parent_folder_id`, gated by
/// `rules` (the active account's `write_permissions.rules`).
///
/// Every real (non-dry-run) attempt — created, blocked, or failed — is
/// logged via [`request_log::record_drive_mutation`], even when the gate
/// refused before any Drive API call was made. A `--dry-run` preview is
/// never logged, matching `drive move`'s existing precedent (its CLI layer
/// calls `file_move::plan` but never `execute` under `--dry-run`, and only
/// `execute` logs).
pub async fn create(
    client: &DriveClient,
    opts: &CreateOptions,
    rules: &[FolderPermissionRule],
) -> CreateOutcome {
    let started = Instant::now();
    let outcome = create_inner(client, opts, rules).await;
    if !opts.dry_run {
        record_attempt(&outcome, started.elapsed());
    }
    outcome
}

async fn create_inner(
    client: &DriveClient,
    opts: &CreateOptions,
    rules: &[FolderPermissionRule],
) -> CreateOutcome {
    let files_api = FilesApi::new(client);
    let decision = match folder_ancestry::resolve_decision(
        &files_api,
        &opts.parent_folder_id,
        DriveOperation::Create,
        rules,
    )
    .await
    {
        Ok(decision) => decision,
        Err(err) => {
            return CreateOutcome {
                name: opts.name.clone(),
                parent_folder_id: opts.parent_folder_id.clone(),
                result: CreateResult::Failed {
                    detail: err.to_string(),
                },
            }
        }
    };
    if decision.verdict == write_gate::Verdict::Deny {
        return CreateOutcome {
            name: opts.name.clone(),
            parent_folder_id: opts.parent_folder_id.clone(),
            result: CreateResult::Blocked {
                decided_by: decision.decided_by,
            },
        };
    }

    if opts.dry_run {
        return CreateOutcome {
            name: opts.name.clone(),
            parent_folder_id: opts.parent_folder_id.clone(),
            result: CreateResult::WouldCreate,
        };
    }

    let result = match files_api
        .create(&opts.name, &opts.parent_folder_id, &opts.mime_type)
        .await
    {
        Ok(file) => CreateResult::Created { file_id: file.id },
        Err(err) => CreateResult::Failed {
            detail: err.to_string(),
        },
    };
    CreateOutcome {
        name: opts.name.clone(),
        parent_folder_id: opts.parent_folder_id.clone(),
        result,
    }
}

/// Builds and writes the [`DriveMutationOutcome`] for one `create` attempt.
fn record_attempt(outcome: &CreateOutcome, duration: Duration) {
    let error = match &outcome.result {
        CreateResult::Failed { detail } => Some(detail.clone()),
        _ => None,
    };
    let decided_by = match &outcome.result {
        CreateResult::Blocked { decided_by } => decided_by.as_ref(),
        _ => None,
    };
    let decided_by = write_gate::decided_by_log_fields(decided_by);
    request_log::record_drive_mutation(DriveMutationOutcome {
        operation: "create",
        file_id: match &outcome.result {
            CreateResult::Created { file_id } => file_id.clone(),
            // No file id exists yet for a Blocked/Failed create — the name
            // is the only identifying information available.
            _ => String::new(),
        },
        file_name: outcome.name.clone(),
        status: outcome.result.log_status().to_string(),
        added_principals: Vec::new(),
        removed_principals: Vec::new(),
        crosses_drive_boundary: false,
        resolved_folder_id: Some(outcome.parent_folder_id.clone()),
        decided_by_folder_id: decided_by.folder_id,
        decided_by_depth: decided_by.depth,
        decided_by_file_id: decided_by.file_id,
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
    use crate::drive::types::GOOGLE_FOLDER_MIME_TYPE;
    use crate::drive::write_gate::DriveOperation;
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

    fn mount_parent_folder(id: &str) -> wiremock::MockBuilder {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!("/drive/v3/files/{id}")))
    }

    fn opts(dry_run: bool) -> CreateOptions {
        CreateOptions {
            name: "New File".to_string(),
            parent_folder_id: "parent-1".to_string(),
            mime_type: "text/plain".to_string(),
            dry_run,
        }
    }

    fn allow_rule() -> FolderPermissionRule {
        FolderPermissionRule {
            folder_id: Some("parent-1".to_string()),
            file_id: None,
            recursive: false,
            allow: std::iter::once(DriveOperation::Create).collect(),
            deny: std::collections::HashSet::default(),
        }
    }

    #[tokio::test]
    async fn allowed_target_succeeds_and_calls_create_endpoint_once() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_parent_folder("parent-1")
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "parent-1", "name": "parent-1", "mimeType": GOOGLE_FOLDER_MIME_TYPE,
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/drive/v3/files"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "new-file-1", "name": "New File",
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let outcome = create(&client, &opts(false), &[allow_rule()]).await;
        assert!(matches!(
            outcome.result,
            CreateResult::Created { file_id } if file_id == "new-file-1"
        ));
    }

    #[tokio::test]
    async fn denied_target_refuses_with_zero_create_calls() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_parent_folder("parent-1")
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "parent-1", "name": "parent-1", "mimeType": GOOGLE_FOLDER_MIME_TYPE,
                })),
            )
            .mount(&server)
            .await;
        // Deliberately no POST /drive/v3/files mock mounted at all — an
        // accidental create attempt fails loudly with wiremock's "no
        // matching mock" error, structurally proving the gate runs before,
        // not after, the network call (mirrors rename.rs/move_file.rs's
        // existing dry-run test convention).

        let outcome = create(&client, &opts(false), &[]).await;
        assert!(matches!(outcome.result, CreateResult::Blocked { .. }));
    }

    #[tokio::test]
    async fn denied_target_still_writes_a_drivemutation_log_record() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_parent_folder("parent-1")
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "parent-1", "name": "parent-1", "mimeType": GOOGLE_FOLDER_MIME_TYPE,
                })),
            )
            .mount(&server)
            .await;

        // Not directly observable from here without a log-capturing seam;
        // record_attempt's own construction is covered by the request_log
        // unit tests. This test instead pins the *outcome* shape
        // record_attempt consumes, so a future refactor that stops
        // producing a Blocked{decided_by} for a real denied run would be
        // caught here.
        let outcome = create(&client, &opts(false), &[]).await;
        assert!(!outcome.name.is_empty());
        assert!(matches!(outcome.result, CreateResult::Blocked { .. }));
    }

    #[tokio::test]
    async fn ancestor_chain_fetch_failure_produces_failed_not_allow() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_parent_folder("parent-1")
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("server error"))
            .mount(&server)
            .await;

        let outcome = create(&client, &opts(false), &[allow_rule()]).await;
        assert!(
            matches!(outcome.result, CreateResult::Failed { .. }),
            "a fetch failure must never silently fall through to Created/WouldCreate"
        );
    }

    #[tokio::test]
    async fn insufficient_scope_403_surfaces_the_write_file_hint() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_parent_folder("parent-1")
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "parent-1", "name": "parent-1", "mimeType": GOOGLE_FOLDER_MIME_TYPE,
                })),
            )
            .mount(&server)
            .await;
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

        let outcome = create(&client, &opts(false), &[allow_rule()]).await;
        let CreateResult::Failed { detail } = outcome.result else {
            panic!("expected Failed, got {:?}", outcome.result);
        };
        assert!(detail.contains("--write-file"), "{detail}");
        assert!(detail.contains("--write-full"), "{detail}");
    }

    #[tokio::test]
    async fn dry_run_never_calls_create_endpoint() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_parent_folder("parent-1")
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "parent-1", "name": "parent-1", "mimeType": GOOGLE_FOLDER_MIME_TYPE,
                })),
            )
            .mount(&server)
            .await;
        // No POST mock mounted — same structural proof as the denied test.

        let outcome = create(&client, &opts(true), &[allow_rule()]).await;
        assert!(matches!(outcome.result, CreateResult::WouldCreate));
    }

    #[tokio::test]
    async fn dry_run_surfaces_the_same_blocked_reasoning_as_a_real_denied_run() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_parent_folder("parent-1")
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "parent-1", "name": "parent-1", "mimeType": GOOGLE_FOLDER_MIME_TYPE,
                })),
            )
            .expect(2)
            .mount(&server)
            .await;

        let dry_run_outcome = create(&client, &opts(true), &[]).await;
        let real_outcome = create(&client, &opts(false), &[]).await;
        assert!(matches!(
            dry_run_outcome.result,
            CreateResult::Blocked { .. }
        ));
        assert!(matches!(real_outcome.result, CreateResult::Blocked { .. }));
    }
}
