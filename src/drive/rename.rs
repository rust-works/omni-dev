//! Drive file rename.
//!
//! Renaming only ever touches a file's `name` field and never changes
//! `parents`, so — unlike `move` — it can never change who can see the
//! file. There is nothing to gate: rename always proceeds (subject to the
//! usual API/auth failures), but it still goes through the same audit-log
//! path `move` does, since "every move/rename must be logged" (#1557) is an
//! invariant that applies to both operations equally.

use std::time::{Duration, Instant};

use anyhow::Result;
use serde::Serialize;

use crate::cli::drive::format::{write_scalar_jsonl, JsonlSerialize};
use crate::drive::client::DriveClient;
use crate::drive::files_api::FilesApi;
use crate::request_log::{self, DriveMutationOutcome};

/// The result of a successful rename.
#[derive(Debug, Clone, Serialize)]
pub struct RenameOutcome {
    /// The Drive file id acted on.
    pub file_id: String,
    /// The file's name before this rename.
    pub old_name: String,
    /// The file's name after this rename.
    pub new_name: String,
}

impl JsonlSerialize for RenameOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Renames `file_id` to `new_name`.
///
/// Fetches the current name first (`files.get`) — both an existence check
/// and what lets the log show old→new — then calls `files.update`. Always
/// records a [`DriveMutationOutcome`] via
/// [`request_log::record_drive_mutation`], on both success and failure:
/// logging happens here, inside the engine, rather than at the CLI call
/// site, so the "every move/rename must be logged" invariant holds for
/// every current and future caller (CLI today, a possible MCP tool later).
pub async fn rename(client: &DriveClient, file_id: &str, new_name: &str) -> Result<RenameOutcome> {
    let started = Instant::now();
    let result = rename_inner(client, file_id, new_name).await;
    record_attempt(file_id, new_name, &result, started.elapsed());
    result
}

async fn rename_inner(
    client: &DriveClient,
    file_id: &str,
    new_name: &str,
) -> Result<RenameOutcome> {
    let files = FilesApi::new(client);
    let existing = files.get_metadata(file_id).await?;
    files.rename(file_id, new_name).await?;
    Ok(RenameOutcome {
        file_id: file_id.to_string(),
        old_name: existing.name,
        new_name: new_name.to_string(),
    })
}

/// Builds and writes the [`DriveMutationOutcome`] for one `rename` attempt.
/// Split out from [`rename`] purely for readability — not otherwise reused.
fn record_attempt(
    file_id: &str,
    new_name: &str,
    result: &Result<RenameOutcome>,
    duration: Duration,
) {
    let (status, error) = match result {
        Ok(_) => ("renamed".to_string(), None),
        Err(err) => ("failed".to_string(), Some(err.to_string())),
    };
    request_log::record_drive_mutation(DriveMutationOutcome {
        operation: "rename",
        file_id: file_id.to_string(),
        // The target name — the best-known name whether or not `files.get`
        // resolved the current one first.
        file_name: new_name.to_string(),
        status,
        // Rename never changes `parents`, so it never has a visibility
        // diff to report.
        added_principals: Vec::new(),
        removed_principals: Vec::new(),
        crosses_drive_boundary: false,
        // Rename is never gated by the folder write-permission gate.
        resolved_folder_id: None,
        decided_by_folder_id: None,
        decided_by_depth: None,
        decided_by_file_id: None,
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

    #[tokio::test]
    async fn rename_fetches_old_name_then_renames() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "Old Name",
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "New Name",
                })),
            )
            .mount(&server)
            .await;

        let outcome = rename(&client, "f1", "New Name").await.unwrap();
        assert_eq!(outcome.file_id, "f1");
        assert_eq!(outcome.old_name, "Old Name");
        assert_eq!(outcome.new_name, "New Name");
    }

    #[tokio::test]
    async fn rename_propagates_a_missing_file_error() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/missing"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;

        let err = rename(&client, "missing", "New Name").await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn rename_propagates_a_files_update_error_after_a_successful_get() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "Old Name",
                })),
            )
            .mount(&server)
            .await;
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

        let err = rename(&client, "f1", "New Name").await.unwrap_err();
        assert!(
            err.to_string().contains("drive auth login --write"),
            "{err}"
        );
    }
}
