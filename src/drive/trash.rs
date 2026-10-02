//! Gated, single-file Trash and restore operations (ADR-0092).

use std::time::{Duration, Instant};

use serde::Serialize;

use crate::cli::drive::format::{write_scalar_jsonl, JsonlSerialize};
use crate::drive::client::DriveClient;
use crate::drive::files_api::FilesApi;
use crate::drive::folder_ancestry::{self, DecisionSource};
use crate::drive::types::GOOGLE_FOLDER_MIME_TYPE;
use crate::drive::write_gate::{self, DecidingRule, DriveOperation, FolderPermissionRule, Verdict};
use crate::request_log::{self, DriveMutationOutcome};

/// Options for changing an individual file's trashed state.
#[derive(Debug, Clone)]
pub struct TrashOptions {
    /// Target Drive file ID.
    pub file_id: String,
    /// Restore from Trash instead of moving to Trash.
    pub restore: bool,
    /// Evaluate permissions and report the action without mutating.
    pub dry_run: bool,
}

/// The result of a Trash or restore attempt.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum TrashResult {
    /// The permitted preview would move the file to Trash.
    WouldTrash,
    /// The file was moved to Trash.
    Trashed,
    /// The permitted preview would restore the file.
    WouldUntrash,
    /// The file was restored.
    Untrashed,
    /// The permitted target is already in Trash; no PATCH was sent.
    AlreadyTrashed,
    /// The permitted target is already outside Trash; no PATCH was sent.
    NotTrashed,
    /// Folders affect descendants and are refused even with a file-ID grant.
    RefusedFolder,
    /// There are no visible parents and no file-ID rule granting access.
    RefusedNoVisibleParents,
    /// The permission gate denied access.
    Blocked {
        /// The deciding rule, or `None` for the default policy.
        decided_by: Option<DecidingRule>,
    },
    /// Metadata lookup, permission evaluation or mutation failed.
    Failed {
        /// Human-readable error detail.
        detail: String,
    },
}

impl TrashResult {
    /// Stable status used by the mutation audit log.
    fn log_status(&self) -> &'static str {
        match self {
            Self::WouldTrash => "would-trash",
            Self::Trashed => "trashed",
            Self::WouldUntrash => "would-untrash",
            Self::Untrashed => "untrashed",
            Self::AlreadyTrashed => "already-trashed",
            Self::NotTrashed => "not-trashed",
            Self::RefusedFolder => "refused-folder",
            Self::RefusedNoVisibleParents => "refused-no-visible-parents",
            Self::Blocked { .. } => "blocked",
            Self::Failed { .. } => "failed",
        }
    }
}

/// Structured outcome, shared by terminal output, JSON and audit logging.
#[derive(Debug, Clone, Serialize)]
pub struct TrashOutcome {
    /// Target Drive file ID.
    pub file_id: String,
    /// Target's name, once metadata has been fetched.
    pub file_name: Option<String>,
    /// The single current parent evaluated by the gate, if applicable.
    /// Absent for file-ID rules, multiple parents and pre-gate refusals.
    pub resolved_folder_id: Option<String>,
    /// The attempt's result.
    pub result: TrashResult,
}

impl JsonlSerialize for TrashOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> Result<(), anyhow::Error> {
        write_scalar_jsonl(self, out)
    }
}

/// Changes the target's trashed state after evaluating `trash` permissions.
///
/// Every real attempt writes a best-effort mutation record, including refusals
/// and no-ops. Dry runs never mutate or write mutation records.
pub async fn trash(
    client: &DriveClient,
    opts: &TrashOptions,
    rules: &[FolderPermissionRule],
) -> TrashOutcome {
    let started = Instant::now();
    let outcome = trash_inner(client, opts, rules).await;
    if !opts.dry_run {
        record_attempt(&outcome, opts.restore, started.elapsed());
    }
    outcome
}

async fn trash_inner(
    client: &DriveClient,
    opts: &TrashOptions,
    rules: &[FolderPermissionRule],
) -> TrashOutcome {
    let mut outcome = TrashOutcome {
        file_id: opts.file_id.clone(),
        file_name: None,
        resolved_folder_id: None,
        result: TrashResult::Failed {
            detail: String::new(),
        },
    };
    let files_api = FilesApi::new(client);
    let target = match files_api.get_metadata(&opts.file_id).await {
        Ok(target) => target,
        Err(err) => {
            outcome.result = TrashResult::Failed {
                detail: err.to_string(),
            };
            return outcome;
        }
    };
    outcome.file_name = Some(target.name.clone());
    if target.mime_type == GOOGLE_FOLDER_MIME_TYPE {
        outcome.result = TrashResult::RefusedFolder;
        return outcome;
    }
    let evaluated = match folder_ancestry::resolve_decision_for_file_target(
        &files_api,
        &target,
        DriveOperation::Trash,
        rules,
    )
    .await
    {
        Ok(evaluated) => evaluated,
        Err(err) => {
            outcome.result = TrashResult::Failed {
                detail: err.to_string(),
            };
            return outcome;
        }
    };
    if evaluated.source == DecisionSource::NoVisibleParents {
        outcome.result = TrashResult::RefusedNoVisibleParents;
        return outcome;
    }
    outcome.resolved_folder_id = evaluated.resolved_folder_id;
    if evaluated.decision.verdict == Verdict::Deny {
        outcome.result = TrashResult::Blocked {
            decided_by: evaluated.decision.decided_by,
        };
        return outcome;
    }
    let Some(trashed) = target.trashed else {
        outcome.result = TrashResult::Failed {
            detail: "Drive metadata omitted the requested trashed state".to_string(),
        };
        return outcome;
    };
    if trashed != opts.restore {
        outcome.result = if opts.restore {
            TrashResult::NotTrashed
        } else {
            TrashResult::AlreadyTrashed
        };
        return outcome;
    }
    if opts.dry_run {
        outcome.result = if opts.restore {
            TrashResult::WouldUntrash
        } else {
            TrashResult::WouldTrash
        };
        return outcome;
    }
    // ADR-0092 extends ADR-0080's lease exemptions. Ignore
    // FileTargetDecision::requires_lease as its documentation requires for
    // operations that can never take a lease: Trash provides recovery itself.
    let updated = if opts.restore {
        files_api.untrash(&opts.file_id).await
    } else {
        files_api.trash(&opts.file_id).await
    };
    outcome.result = match updated {
        Ok(updated) if updated.trashed != Some(!opts.restore) => TrashResult::Failed {
            detail: if opts.restore {
                "Drive did not confirm restoration; the file may still be in a trashed parent folder"
            } else {
                "Drive did not confirm the requested trashed state"
            }.to_string(),
        },
        Ok(_) if opts.restore => TrashResult::Untrashed,
        Ok(_) => TrashResult::Trashed,
        Err(err) => TrashResult::Failed {
            detail: err.to_string(),
        },
    };
    outcome
}

fn record_attempt(outcome: &TrashOutcome, restore: bool, duration: Duration) {
    let error = match &outcome.result {
        TrashResult::Failed { detail } => Some(detail.clone()),
        _ => None,
    };
    let decided_by = match &outcome.result {
        TrashResult::Blocked { decided_by } => decided_by.as_ref(),
        _ => None,
    };
    let decided_by = write_gate::decided_by_log_fields(decided_by);
    request_log::record_drive_mutation(DriveMutationOutcome {
        operation: if restore { "untrash" } else { "trash" },
        file_id: outcome.file_id.clone(),
        file_name: outcome.file_name.clone().unwrap_or_default(),
        status: outcome.result.log_status().to_string(),
        resolved_folder_id: outcome.resolved_folder_id.clone(),
        decided_by_folder_id: decided_by.folder_id,
        decided_by_file_id: decided_by.file_id,
        decided_by_depth: decided_by.depth,
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
    use wiremock::matchers::{body_json, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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

    fn mount_file(id: &str, mime_type: &str, parents: &[&str]) -> wiremock::Mock {
        let parents_json: Vec<&str> = parents.to_vec();
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!("/drive/v3/files/{id}")))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": id, "name": id, "mimeType": mime_type, "parents": parents_json,
                    "version": "1", "trashed": false,
                })),
            )
    }

    fn mount_folder(id: &str) -> wiremock::Mock {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!("/drive/v3/files/{id}")))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": id, "name": id, "mimeType": GOOGLE_FOLDER_MIME_TYPE,
                })),
            )
    }

    fn opts(restore: bool, dry_run: bool) -> TrashOptions {
        TrashOptions {
            file_id: "file-1".into(),
            restore,
            dry_run,
        }
    }

    fn allow() -> FolderPermissionRule {
        // Default require_lease=true deliberately proves Trash is exempt.
        FolderPermissionRule::folder("parent-1").allowing([DriveOperation::Trash])
    }

    async fn no_patch(server: &MockServer) {
        Mock::given(method("PATCH"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(server)
            .await;
    }

    async fn trashed_file(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/drive/v3/files/file-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "file-1", "name": "file-1", "mimeType": "text/plain",
                "parents": ["parent-1"], "trashed": true
            })))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn gate_refusals_and_folder_boundaries_never_patch() {
        for (mime, parents, rules, expected) in [
            ("text/plain", vec!["parent-1"], vec![], "blocked"),
            (
                "text/plain",
                vec!["parent-1"],
                vec![FolderPermissionRule::folder("parent-1").denying([DriveOperation::Trash])],
                "blocked",
            ),
            (
                "text/plain",
                vec!["parent-1"],
                vec![
                    allow(),
                    FolderPermissionRule::file("file-1").denying([DriveOperation::Trash]),
                ],
                "blocked",
            ),
            (
                "text/plain",
                vec![],
                vec![allow()],
                "refused-no-visible-parents",
            ),
            (
                GOOGLE_FOLDER_MIME_TYPE,
                vec!["parent-1"],
                vec![FolderPermissionRule::file("file-1").allowing([DriveOperation::Trash])],
                "refused-folder",
            ),
        ] {
            for restore in [false, true] {
                let server = MockServer::start().await;
                let client = client_with_bootstrapped_token(&server).await;
                mount_file("file-1", mime, &parents).mount(&server).await;
                mount_folder("parent-1").mount(&server).await;
                no_patch(&server).await;
                let outcome = trash(&client, &opts(restore, true), &rules).await;
                assert_eq!(outcome.result.log_status(), expected);
            }
        }
    }

    #[tokio::test]
    async fn dry_runs_and_noops_never_patch() {
        for (restore, trashed, dry_run, expected) in [
            (false, false, true, "would-trash"),
            (true, true, true, "would-untrash"),
            (false, true, true, "already-trashed"),
            (true, false, true, "not-trashed"),
        ] {
            let server = MockServer::start().await;
            let client = client_with_bootstrapped_token(&server).await;
            if trashed {
                trashed_file(&server).await;
            } else {
                mount_file("file-1", "text/plain", &["parent-1"])
                    .mount(&server)
                    .await;
            }
            mount_folder("parent-1").mount(&server).await;
            no_patch(&server).await;
            let outcome = trash(&client, &opts(restore, dry_run), &[allow()]).await;
            assert_eq!(outcome.result.log_status(), expected);
            assert_eq!(outcome.resolved_folder_id.as_deref(), Some("parent-1"));
        }
    }

    #[tokio::test]
    async fn noops_are_still_gated() {
        let server = MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        trashed_file(&server).await;
        mount_folder("parent-1").mount(&server).await;
        no_patch(&server).await;
        let outcome = trash(&client, &opts(false, true), &[]).await;
        assert!(matches!(outcome.result, TrashResult::Blocked { .. }));
    }

    #[tokio::test]
    async fn file_rule_grants_parentless_native_document() {
        let server = MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file("file-1", "application/vnd.google-apps.spreadsheet", &[])
            .mount(&server)
            .await;
        no_patch(&server).await;
        let rule = FolderPermissionRule::file("file-1").allowing([DriveOperation::Trash]);
        let outcome = trash(&client, &opts(false, true), &[rule]).await;
        assert!(matches!(outcome.result, TrashResult::WouldTrash));
        assert_eq!(outcome.resolved_folder_id, None);
    }

    #[tokio::test]
    async fn ancestor_allow_and_multiple_parent_deny_use_the_shared_gate() {
        let server = MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file("file-1", "text/plain", &["parent-1", "parent-2"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        mount_file("parent-2", GOOGLE_FOLDER_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        no_patch(&server).await;
        let rules = [allow().recursive(true)];
        let outcome = trash(&client, &opts(false, true), &rules).await;
        assert!(matches!(outcome.result, TrashResult::WouldTrash));
        assert_eq!(outcome.resolved_folder_id, None);
        let rules = [
            allow().recursive(true),
            FolderPermissionRule::folder("parent-2").denying([DriveOperation::Trash]),
        ];
        let outcome = trash(&client, &opts(false, true), &rules).await;
        assert!(matches!(outcome.result, TrashResult::Blocked { .. }));
    }

    #[tokio::test]
    async fn metadata_and_ancestry_errors_never_patch() {
        for failed_id in ["file-1", "parent-1"] {
            let server = MockServer::start().await;
            let client = client_with_bootstrapped_token(&server).await;
            if failed_id == "parent-1" {
                mount_file("file-1", "text/plain", &["parent-1"])
                    .mount(&server)
                    .await;
            }
            Mock::given(method("GET"))
                .and(path(format!("/drive/v3/files/{failed_id}")))
                .respond_with(ResponseTemplate::new(500).set_body_string("lookup failed"))
                .mount(&server)
                .await;
            no_patch(&server).await;
            let outcome = trash(&client, &opts(false, true), &[allow()]).await;
            assert!(matches!(outcome.result, TrashResult::Failed { .. }));
        }
    }

    #[tokio::test]
    async fn missing_requested_state_never_reports_a_noop_or_patches() {
        let server = MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        Mock::given(method("GET"))
            .and(path("/drive/v3/files/file-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "file-1", "name": "file-1", "mimeType": "text/plain"
            })))
            .mount(&server)
            .await;
        no_patch(&server).await;
        let rules = [FolderPermissionRule::file("file-1").allowing([DriveOperation::Trash])];
        for restore in [false, true] {
            let outcome = trash(&client, &opts(restore, true), &rules).await;
            assert!(
                matches!(outcome.result, TrashResult::Failed { ref detail } if detail.contains("omitted"))
            );
        }
    }

    #[tokio::test]
    async fn successful_patch_must_confirm_the_requested_state() {
        let _env = crate::drive::test_support::EnvGuard::take();
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("mutations.jsonl");
        let _log =
            crate::utils::env::ScopedEnvVar::set("OMNI_DEV_LOG_FILE", log_path.to_str().unwrap());
        for (restore, returned) in [(true, Some(true)), (false, Some(false)), (true, None)] {
            let server = MockServer::start().await;
            let client = client_with_bootstrapped_token(&server).await;
            if restore {
                trashed_file(&server).await;
            } else {
                mount_file("file-1", "text/plain", &["parent-1"])
                    .mount(&server)
                    .await;
            }
            mount_folder("parent-1").mount(&server).await;
            Mock::given(method("PATCH"))
                .and(path("/drive/v3/files/file-1"))
                .and(body_json(serde_json::json!({"trashed": !restore})))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "file-1", "name": "file-1", "trashed": returned
                })))
                .expect(1)
                .mount(&server)
                .await;
            let outcome = trash(&client, &opts(restore, false), &[allow()]).await;
            assert!(
                matches!(outcome.result, TrashResult::Failed { ref detail } if detail.contains("did not confirm")),
                "{outcome:?}"
            );
        }
    }

    #[tokio::test]
    async fn mutations_noops_failures_and_previews_record_the_expected_log() {
        let _env = crate::drive::test_support::EnvGuard::take();
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("mutations.jsonl");
        let _log =
            crate::utils::env::ScopedEnvVar::set("OMNI_DEV_LOG_FILE", log_path.to_str().unwrap());
        let _enabled = crate::utils::env::ScopedEnvVar::set("OMNI_DEV_LOG_DISABLE", "false");
        for (restore, trashed, dry_run, code, expected, count) in [
            (false, false, false, 200, "trashed", 1),
            (true, true, false, 200, "untrashed", 1),
            (false, false, false, 403, "failed", 1),
            (true, true, false, 403, "failed", 1),
            (false, true, false, 200, "already-trashed", 0),
            (true, false, false, 200, "not-trashed", 0),
            (false, false, true, 200, "would-trash", 0),
            (false, false, false, 200, "blocked", 0),
        ] {
            std::fs::write(&log_path, "").unwrap();
            let server = MockServer::start().await;
            let client = client_with_bootstrapped_token(&server).await;
            if trashed {
                trashed_file(&server).await;
            } else {
                mount_file("file-1", "text/plain", &["parent-1"])
                    .mount(&server)
                    .await;
            }
            mount_folder("parent-1").mount(&server).await;
            Mock::given(method("PATCH"))
                .and(path("/drive/v3/files/file-1"))
                .and(query_param("supportsAllDrives", "true"))
                .and(body_json(serde_json::json!({"trashed": !restore})))
                .respond_with(
                    ResponseTemplate::new(code).set_body_json(serde_json::json!({
                        "id": "file-1", "name": "file-1", "trashed": !restore,
                        "error": {"message": "permission denied"}
                    })),
                )
                .expect(count)
                .mount(&server)
                .await;
            let rules = if expected == "blocked" {
                vec![]
            } else {
                vec![allow()]
            };
            assert!(allow().require_lease);
            let outcome = trash(&client, &opts(restore, dry_run), &rules).await;
            assert_eq!(outcome.result.log_status(), expected, "{outcome:?}");
            let records: Vec<serde_json::Value> = std::fs::read_to_string(&log_path)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .filter(|rec: &serde_json::Value| rec["kind"] == "drivemutation")
                .collect();
            assert_eq!(records.len(), usize::from(!dry_run));
            if !dry_run {
                assert_eq!(records[0]["context"]["status"], expected);
                assert_eq!(records[0]["context"]["file_id"], "file-1");
                assert_eq!(
                    records[0]["command"][1],
                    if restore { "untrash" } else { "trash" }
                );
                if expected == "failed" {
                    assert!(records[0]["error"].as_str().unwrap().contains("403"));
                }
            }
        }
    }
}
