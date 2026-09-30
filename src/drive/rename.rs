//! Drive file rename.
//!
//! Renaming only ever touches a file's `name` field and never changes
//! `parents`, so — unlike `move` — it can never change who can see the
//! file. There is nothing to gate: rename always proceeds (subject to the
//! usual API/auth failures), but it still goes through the same audit-log
//! path `move` does, since "every move/rename must be logged" (#1557) is an
//! invariant that applies to both operations equally.

use std::time::{Duration, Instant};

use anyhow::{bail, Result};
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
    /// The file's name after this rename, as `files.update` returned it —
    /// not the requested string, since Drive can normalise or ignore one.
    pub new_name: String,
    /// The name that was asked for, set only when Drive stored a different
    /// one (so the rename was normalised or ignored). Omitted otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_name: Option<String>,
}

impl RenameOutcome {
    /// Whether the file already carried the resulting name, so the call
    /// changed nothing.
    #[must_use]
    pub fn is_unchanged(&self) -> bool {
        self.old_name == self.new_name
    }

    /// Whether Drive answered 200 but kept the old name although a different
    /// one was asked for — the rename did not happen.
    #[must_use]
    pub fn is_ignored(&self) -> bool {
        self.is_unchanged() && self.requested_name.is_some()
    }
}

/// Rejects a `new_name` that is empty, whitespace-only, or made only of
/// invisible characters (control characters, zero-width spaces, BOM).
///
/// Drive treats an empty `name` as "no change" and answers 200, so without
/// this check a no-op is reported as a successful rename (#1918). Only
/// blankness is rejected; the name that is sent is never trimmed.
pub fn validate_new_name(new_name: &str) -> Result<()> {
    let invisible = |c: char| {
        c.is_whitespace()
            || c.is_control()
            || matches!(c, '\u{200B}'..='\u{200D}' | '\u{2060}' | '\u{FEFF}')
    };
    if new_name.chars().all(invisible) {
        bail!("The new name must not be empty or whitespace-only");
    }
    Ok(())
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
    validate_new_name(new_name)?;
    let files = FilesApi::new(client);
    let existing = files.get_metadata(file_id).await?;
    let updated = files.rename(file_id, new_name).await?;
    let requested_name = (updated.name != new_name).then(|| new_name.to_string());
    Ok(RenameOutcome {
        file_id: file_id.to_string(),
        old_name: existing.name,
        new_name: updated.name,
        requested_name,
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
        Ok(outcome) if outcome.is_ignored() => ("ignored".to_string(), None),
        Ok(outcome) if outcome.is_unchanged() => ("unchanged".to_string(), None),
        Ok(_) => ("renamed".to_string(), None),
        Err(err) => ("failed".to_string(), Some(err.to_string())),
    };
    // On success, the name Drive actually stored.
    let new_name = result.as_ref().map_or(new_name, |o| o.new_name.as_str());
    request_log::record_drive_mutation(DriveMutationOutcome {
        operation: "rename",
        file_id: file_id.to_string(),
        // The target name (the stored one once known) — the best-known name
        // whether or not `files.get` resolved the current one first.
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

    fn mount_get(name: &str) -> wiremock::Mock {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": name,
                })),
            )
    }

    fn mount_patch(returned_name: &str) -> wiremock::Mock {
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": returned_name,
                })),
            )
    }

    #[test]
    fn validate_new_name_rejects_blank_names() {
        for blank in [
            "",
            " ",
            "   ",
            "\t",
            "\n",
            " \t\n ",
            "\u{a0}",
            "\u{3000}",
            "\u{200b}",
            "\u{feff}",
            " \u{200b}\u{2060} ",
        ] {
            let err = validate_new_name(blank).unwrap_err();
            assert!(
                err.to_string().contains("empty or whitespace-only"),
                "{blank:?}: {err}"
            );
        }
    }

    #[test]
    fn validate_new_name_accepts_names_with_content() {
        for ok in ["a", " a", "a ", "  Q3 Report  ", "-", "\u{1F4C4}"] {
            validate_new_name(ok).unwrap();
        }
    }

    #[tokio::test]
    async fn rename_rejects_a_blank_name_before_any_api_call() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        // No GET/PATCH mocks: an API call would surface as a different error.
        for blank in ["", "   "] {
            let err = rename(&client, "f1", blank).await.unwrap_err();
            assert!(
                err.to_string().contains("empty or whitespace-only"),
                "{err}"
            );
        }
        let requests = server.received_requests().await.unwrap();
        assert!(
            requests
                .iter()
                .all(|r| r.url.path() == "/token" || !r.url.path().contains("/files/")),
            "a blank name must not reach the files API: {requests:?}"
        );
    }

    #[tokio::test]
    async fn rename_reports_the_name_drive_returned_not_the_requested_one() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_get("Old Name").mount(&server).await;
        mount_patch("Normalised Name").mount(&server).await;

        let outcome = rename(&client, "f1", "  Requested  ").await.unwrap();
        assert_eq!(outcome.new_name, "Normalised Name");
        assert_eq!(outcome.requested_name.as_deref(), Some("  Requested  "));
        assert!(!outcome.is_unchanged());
    }

    #[tokio::test]
    async fn rename_flags_a_name_drive_ignored() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_get("Old Name").mount(&server).await;
        mount_patch("Old Name").mount(&server).await;

        let outcome = rename(&client, "f1", "New Name").await.unwrap();
        assert_eq!(outcome.new_name, "Old Name");
        assert_eq!(outcome.requested_name.as_deref(), Some("New Name"));
        assert!(outcome.is_unchanged());
        assert!(outcome.is_ignored());
    }

    #[tokio::test]
    async fn rename_to_the_current_name_is_unchanged_without_a_requested_name() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_get("Same").mount(&server).await;
        mount_patch("Same").mount(&server).await;

        let outcome = rename(&client, "f1", "Same").await.unwrap();
        assert!(outcome.is_unchanged());
        assert!(!outcome.is_ignored());
        assert_eq!(outcome.requested_name, None);
        let json = serde_json::to_value(&outcome).unwrap();
        assert!(json.get("requested_name").is_none(), "{json}");
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
