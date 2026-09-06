//! Drive content upload — creates a new file from local content (issue
//! #1574, [ADR-0071](../../docs/adrs/adr-0071.md)).
//!
//! Structurally identical to `create.rs` (single-target, `rename.rs`'s
//! linear-function shape, not `file_move.rs`'s batch Plan/Execute) — the
//! only difference is the mutating call itself (`FilesApi::upload`'s
//! multipart request instead of `FilesApi::create`'s plain JSON body).
//! The local-file size check happens in the CLI layer, *before*
//! `UploadOptions` is even constructed, so it fires identically whether or
//! not `--dry-run` is set — by the time this module sees `content`, it's
//! already known to fit.

use std::time::{Duration, Instant};

use serde::Serialize;

use crate::cli::drive::format::{write_scalar_jsonl, JsonlSerialize};
use crate::drive::client::DriveClient;
use crate::drive::files_api::FilesApi;
use crate::drive::folder_ancestry;
use crate::drive::write_gate::{self, DecidingRule, DriveOperation, FolderPermissionRule};
use crate::request_log::{self, DriveMutationOutcome};

/// Per-call upload options.
#[derive(Debug, Clone)]
pub struct UploadOptions {
    /// The new file's display name.
    pub name: String,
    /// The folder id to upload it into.
    pub parent_folder_id: String,
    /// The file's content, already read into memory (and already
    /// size-checked) by the caller.
    pub content: Vec<u8>,
    /// The content's MIME type.
    pub content_type: String,
    /// When `true`, classify but never call `files.create`.
    pub dry_run: bool,
}

/// What happened (or, under `--dry-run`, would happen).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum UploadResult {
    /// `--dry-run`, and the gate would allow it.
    WouldUpload,
    /// The folder write-permission gate refused it.
    Blocked {
        /// The rule that decided the refusal, if any.
        decided_by: Option<DecidingRule>,
    },
    /// The multipart upload succeeded.
    Uploaded {
        /// The newly created file's id.
        file_id: String,
    },
    /// An API/validation error.
    Failed {
        /// A human-readable summary of what failed.
        detail: String,
    },
}

impl UploadResult {
    /// The request-log `status` string — mirrors
    /// `CreateResult::log_status`/`MoveResult::log_status`'s precedent.
    fn log_status(&self) -> &'static str {
        match self {
            Self::WouldUpload => "would-upload",
            Self::Blocked { .. } => "blocked",
            Self::Uploaded { .. } => "uploaded",
            Self::Failed { .. } => "failed",
        }
    }
}

/// The planned (and, after a real run, final) outcome of one `upload` call.
#[derive(Debug, Clone, Serialize)]
pub struct UploadOutcome {
    /// The requested name.
    pub name: String,
    /// The requested parent folder id.
    pub parent_folder_id: String,
    /// The result.
    pub result: UploadResult,
}

impl JsonlSerialize for UploadOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> Result<(), anyhow::Error> {
        write_scalar_jsonl(self, out)
    }
}

/// Uploads `opts.content` as `opts.name` inside `opts.parent_folder_id`,
/// gated by `rules`. See the module doc for why there is no separate
/// upload-side size check here — the caller already did it.
///
/// Every real (non-dry-run) attempt is logged; a `--dry-run` preview never
/// is, matching `create`/`move`'s existing precedent.
pub async fn upload(
    client: &DriveClient,
    opts: &UploadOptions,
    rules: &[FolderPermissionRule],
) -> UploadOutcome {
    let started = Instant::now();
    let outcome = upload_inner(client, opts, rules).await;
    if !opts.dry_run {
        record_attempt(&outcome, started.elapsed());
    }
    outcome
}

async fn upload_inner(
    client: &DriveClient,
    opts: &UploadOptions,
    rules: &[FolderPermissionRule],
) -> UploadOutcome {
    let files_api = FilesApi::new(client);
    let decision = match folder_ancestry::resolve_decision(
        &files_api,
        &opts.parent_folder_id,
        DriveOperation::Upload,
        rules,
    )
    .await
    {
        Ok(decision) => decision,
        Err(err) => {
            return UploadOutcome {
                name: opts.name.clone(),
                parent_folder_id: opts.parent_folder_id.clone(),
                result: UploadResult::Failed {
                    detail: err.to_string(),
                },
            }
        }
    };
    if decision.verdict == write_gate::Verdict::Deny {
        return UploadOutcome {
            name: opts.name.clone(),
            parent_folder_id: opts.parent_folder_id.clone(),
            result: UploadResult::Blocked {
                decided_by: decision.decided_by,
            },
        };
    }

    if opts.dry_run {
        return UploadOutcome {
            name: opts.name.clone(),
            parent_folder_id: opts.parent_folder_id.clone(),
            result: UploadResult::WouldUpload,
        };
    }

    let result = match files_api
        .upload(
            &opts.name,
            &opts.parent_folder_id,
            &opts.content,
            &opts.content_type,
        )
        .await
    {
        Ok(file) => UploadResult::Uploaded { file_id: file.id },
        Err(err) => UploadResult::Failed {
            detail: err.to_string(),
        },
    };
    UploadOutcome {
        name: opts.name.clone(),
        parent_folder_id: opts.parent_folder_id.clone(),
        result,
    }
}

/// Builds and writes the [`DriveMutationOutcome`] for one `upload` attempt.
fn record_attempt(outcome: &UploadOutcome, duration: Duration) {
    let error = match &outcome.result {
        UploadResult::Failed { detail } => Some(detail.clone()),
        _ => None,
    };
    let decided_by = match &outcome.result {
        UploadResult::Blocked { decided_by } => decided_by.as_ref(),
        _ => None,
    };
    let decided_by = write_gate::decided_by_log_fields(decided_by);
    request_log::record_drive_mutation(DriveMutationOutcome {
        operation: "upload",
        file_id: match &outcome.result {
            UploadResult::Uploaded { file_id } => file_id.clone(),
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

    fn opts(dry_run: bool) -> UploadOptions {
        UploadOptions {
            name: "photo.jpg".to_string(),
            parent_folder_id: "parent-1".to_string(),
            content: b"JPEGDATA".to_vec(),
            content_type: "image/jpeg".to_string(),
            dry_run,
        }
    }

    fn allow_rule() -> FolderPermissionRule {
        FolderPermissionRule {
            folder_id: Some("parent-1".to_string()),
            file_id: None,
            recursive: false,
            allow: std::iter::once(DriveOperation::Upload).collect(),
            deny: std::collections::HashSet::default(),
        }
    }

    #[tokio::test]
    async fn allowed_target_succeeds_and_calls_upload_endpoint_once() {
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
            .and(wiremock::matchers::path("/upload/drive/v3/files"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "new-upload-1", "name": "photo.jpg",
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let outcome = upload(&client, &opts(false), &[allow_rule()]).await;
        assert!(matches!(
            outcome.result,
            UploadResult::Uploaded { file_id } if file_id == "new-upload-1"
        ));
    }

    #[tokio::test]
    async fn denied_target_refuses_with_zero_upload_calls() {
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
        // No POST /upload/drive/v3/files mock mounted — an accidental
        // upload attempt fails loudly with "no matching mock" instead of
        // silently succeeding.

        let outcome = upload(&client, &opts(false), &[]).await;
        assert!(matches!(outcome.result, UploadResult::Blocked { .. }));
    }

    #[tokio::test]
    async fn ancestor_chain_fetch_failure_produces_failed_not_allow() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_parent_folder("parent-1")
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("server error"))
            .mount(&server)
            .await;

        let outcome = upload(&client, &opts(false), &[allow_rule()]).await;
        assert!(
            matches!(outcome.result, UploadResult::Failed { .. }),
            "a fetch failure must never silently fall through to Uploaded/WouldUpload"
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

        let outcome = upload(&client, &opts(false), &[allow_rule()]).await;
        let UploadResult::Failed { detail } = outcome.result else {
            panic!("expected Failed, got {:?}", outcome.result);
        };
        assert!(detail.contains("--write-file"), "{detail}");
        assert!(detail.contains("--write-full"), "{detail}");
    }

    #[tokio::test]
    async fn dry_run_never_calls_upload_endpoint() {
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

        let outcome = upload(&client, &opts(true), &[allow_rule()]).await;
        assert!(matches!(outcome.result, UploadResult::WouldUpload));
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

        let dry_run_outcome = upload(&client, &opts(true), &[]).await;
        let real_outcome = upload(&client, &opts(false), &[]).await;
        assert!(matches!(
            dry_run_outcome.result,
            UploadResult::Blocked { .. }
        ));
        assert!(matches!(real_outcome.result, UploadResult::Blocked { .. }));
    }
}
