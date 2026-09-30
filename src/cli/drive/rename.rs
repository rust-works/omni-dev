//! CLI command for `omni-dev drive rename`.

use anyhow::Result;
use clap::Parser;
use serde::Serialize;

use crate::cli::drive::format::{
    output_as, sanitize_for_terminal, write_scalar_jsonl, JsonlSerialize, OutputFormat,
};
use crate::drive::client::DriveClient;
use crate::drive::files_api::FilesApi;
use crate::drive::rename::{self, RenameOutcome};

/// Renames a single Drive file. Always safe: renaming only touches `name`
/// and never changes `parents`, so it never affects visibility — unlike
/// `move`, there is no `--allow-*` gate here.
#[derive(Parser)]
pub struct RenameCommand {
    /// Drive file id (from `drive search`, or the `id` segment of a Drive
    /// URL).
    pub file_id: String,
    /// The new name.
    pub new_name: String,

    /// Reports the current name without renaming.
    #[arg(long)]
    pub dry_run: bool,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl RenameCommand {
    /// Runs the command against the shared client resolved by the parent
    /// `DriveCommand::execute`.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        run_rename(
            client,
            &self.file_id,
            &self.new_name,
            self.dry_run,
            &self.output,
        )
        .await
    }
}

/// A `--dry-run` report: the current name and what it would become.
#[derive(Debug, Clone, Serialize)]
struct DryRunReport {
    file_id: String,
    old_name: String,
    new_name: String,
}

impl JsonlSerialize for DryRunReport {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Renames the file (or, with `dry_run`, just reports its current name) and
/// emits the result in the requested format.
///
/// Split from [`RenameCommand::execute`] so tests can inject a wiremock
/// client without going through the credential-loading path.
async fn run_rename(
    client: &DriveClient,
    file_id: &str,
    new_name: &str,
    dry_run: bool,
    output: &OutputFormat,
) -> Result<()> {
    if dry_run {
        rename::validate_new_name(new_name)?;
        let existing = FilesApi::new(client).get_metadata(file_id).await?;
        let report = DryRunReport {
            file_id: file_id.to_string(),
            old_name: existing.name,
            new_name: new_name.to_string(),
        };
        if output_as(&report, output)? {
            return Ok(());
        }
        if report.old_name == report.new_name {
            // Same predicate as `RenameOutcome::is_unchanged`.
            println!(
                "Would not rename: already named {} ({})",
                sanitize_for_terminal(&report.old_name),
                sanitize_for_terminal(&report.file_id)
            );
        } else {
            println!(
                "Would rename: {} -> {} ({})",
                sanitize_for_terminal(&report.old_name),
                sanitize_for_terminal(&report.new_name),
                sanitize_for_terminal(&report.file_id)
            );
        }
        return Ok(());
    }

    let outcome = rename::rename(client, file_id, new_name).await?;
    if outcome.is_ignored() {
        // Structured callers still get the record; either way the command
        // fails, since the requested rename did not happen.
        output_as(&outcome, output)?;
        anyhow::bail!(
            "Drive accepted the request but kept the name {} instead of {} ({})",
            sanitize_for_terminal(&outcome.old_name),
            sanitize_for_terminal(outcome.requested_name.as_deref().unwrap_or_default()),
            sanitize_for_terminal(&outcome.file_id)
        );
    }
    print_outcome(&outcome, output)
}

fn print_outcome(outcome: &RenameOutcome, output: &OutputFormat) -> Result<()> {
    if output_as(outcome, output)? {
        return Ok(());
    }
    if let Some(requested) = &outcome.requested_name {
        eprintln!(
            "Note: requested {} but Drive stored {}",
            sanitize_for_terminal(requested),
            sanitize_for_terminal(&outcome.new_name)
        );
    }
    // `is_ignored` never reaches here: `run_rename` fails it first.
    if outcome.is_unchanged() {
        println!(
            "Already named: {} ({})",
            sanitize_for_terminal(&outcome.new_name),
            sanitize_for_terminal(&outcome.file_id)
        );
    } else {
        println!(
            "Renamed: {} -> {} ({})",
            sanitize_for_terminal(&outcome.old_name),
            sanitize_for_terminal(&outcome.new_name),
            sanitize_for_terminal(&outcome.file_id)
        );
    }
    Ok(())
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
    async fn dry_run_reports_current_name_without_renaming() {
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
        // No PATCH mock mounted at all — a --dry-run call that somehow sent
        // one would fail with "no matching mock" rather than silently
        // renaming, so this test doubles as the "never mutates" assertion.

        run_rename(&client, "f1", "New Name", true, &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_rename_renames_and_propagates_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;

        let err = run_rename(&client, "f1", "New Name", false, &OutputFormat::Table)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn dry_run_rejects_a_blank_name_before_any_api_call() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        // No files mocks mounted: reaching `files.get` would 404 instead.
        for blank in ["", "   "] {
            let err = run_rename(&client, "f1", blank, true, &OutputFormat::Table)
                .await
                .unwrap_err();
            assert!(
                err.to_string().contains("empty or whitespace-only"),
                "{err}"
            );
        }
    }

    #[tokio::test]
    async fn run_rename_rejects_a_blank_name() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let err = run_rename(&client, "f1", "  ", false, &OutputFormat::Table)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("empty or whitespace-only"));
    }

    #[tokio::test]
    async fn dry_run_and_rename_to_the_current_name_report_already_named() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        for method in ["GET", "PATCH"] {
            wiremock::Mock::given(wiremock::matchers::method(method))
                .and(wiremock::matchers::path("/drive/v3/files/f1"))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({
                        "id": "f1", "name": "Same",
                    }),
                ))
                .mount(&server)
                .await;
        }

        run_rename(&client, "f1", "Same", true, &OutputFormat::Table)
            .await
            .unwrap();
        run_rename(&client, "f1", "Same", false, &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_rename_notes_a_name_drive_stored_differently() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "Old",
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "Stored",
                })),
            )
            .mount(&server)
            .await;

        run_rename(&client, "f1", "Asked", false, &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_rename_fails_when_drive_ignores_the_name() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        for method in ["GET", "PATCH"] {
            wiremock::Mock::given(wiremock::matchers::method(method))
                .and(wiremock::matchers::path("/drive/v3/files/f1"))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({
                        "id": "f1", "name": "Old",
                    }),
                ))
                .mount(&server)
                .await;
        }

        for output in [OutputFormat::Table, OutputFormat::Json] {
            let err = run_rename(&client, "f1", "Asked", false, &output)
                .await
                .unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("kept the name Old instead of Asked"), "{msg}");
        }
    }
}
