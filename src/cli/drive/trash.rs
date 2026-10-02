//! CLI commands for trashing and restoring individual Drive files.

use anyhow::Result;
use clap::Parser;

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::cli::drive::helpers::active_account_rules;
use crate::drive::client::DriveClient;
use crate::drive::trash::{self, TrashOptions, TrashOutcome, TrashResult};
use crate::drive::write_gate::FolderPermissionRule;

/// Changes a file's trashed state under the `trash` permission. Refuses
/// folders and needs metadata write access (`drive auth login --write`).
/// Recovery is available until Drive purges the file, normally after 30 days.
#[derive(Parser)]
pub struct TrashCommand {
    /// Drive file ID.
    pub file_id: String,
    /// Reports the permission verdict and action without mutating.
    #[arg(long)]
    pub dry_run: bool,
    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl TrashCommand {
    /// Executes trash or untrash with rules for the selected account.
    pub async fn execute(self, client: &DriveClient, restore: bool) -> Result<()> {
        let opts = TrashOptions {
            file_id: self.file_id,
            restore,
            dry_run: self.dry_run,
        };
        let rules = active_account_rules()?;
        run_trash(client, &opts, &rules, &self.output).await
    }
}

async fn run_trash(
    client: &DriveClient,
    opts: &TrashOptions,
    rules: &[FolderPermissionRule],
    output: &OutputFormat,
) -> Result<()> {
    let outcome = trash::trash(client, opts, rules).await;
    if !output_as(&outcome, output)? {
        // Best-effort terminal output follows the existing mutation commands.
        let _ = write_outcome(&outcome, &mut std::io::stdout().lock());
    }
    Ok(())
}

fn write_outcome(outcome: &TrashOutcome, out: &mut dyn std::io::Write) -> std::io::Result<()> {
    let id = sanitize_for_terminal(&outcome.file_id);
    match &outcome.result {
        TrashResult::WouldTrash => writeln!(out, "Would trash: {id}"),
        TrashResult::Trashed => writeln!(
            out,
            "Trashed: {id}. Restore with `omni-dev drive untrash {id}`."
        ),
        TrashResult::WouldUntrash => writeln!(out, "Would untrash: {id}"),
        TrashResult::Untrashed => writeln!(out, "Untrashed: {id}"),
        TrashResult::AlreadyTrashed => writeln!(out, "Already trashed: {id}"),
        TrashResult::NotTrashed => writeln!(out, "Not trashed: {id}"),
        TrashResult::RefusedFolder => writeln!(
            out,
            "Refused: {id} is a folder; trash/untrash affects descendants and is not supported."
        ),
        TrashResult::RefusedNoVisibleParents => writeln!(
            out,
            "Refused: {id} has no visible parent folder. Grant it by file_id with allow: [trash]."
        ),
        TrashResult::Blocked { decided_by } => {
            writeln!(out, "Blocked: {id}")?;
            match decided_by {
                Some(rule) => writeln!(
                    out,
                    "  refused by rule on {} {}{}",
                    rule.kind_label(),
                    sanitize_for_terminal(rule.id()),
                    rule.depth_suffix()
                ),
                None => writeln!(out, "  refused by default policy (no matching trash rule)"),
            }
        }
        TrashResult::Failed { detail } => {
            writeln!(out, "Failed: {id}: {}", sanitize_for_terminal(detail))
        }
    }
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

    #[test]
    fn both_subcommands_parse_their_flags() {
        for verb in ["trash", "untrash"] {
            let cmd = crate::cli::drive::DriveCommand::try_parse_from([
                "drive",
                verb,
                "file-1",
                "--dry-run",
                "-o",
                "json",
            ])
            .unwrap();
            let leaf = match cmd.command {
                crate::cli::drive::DriveSubcommands::Trash(cmd)
                | crate::cli::drive::DriveSubcommands::Untrash(cmd) => cmd,
                _ => panic!("wrong subcommand"),
            };
            assert_eq!(leaf.file_id, "file-1");
            assert!(leaf.dry_run);
            assert!(matches!(leaf.output, OutputFormat::Json));
        }
    }

    #[tokio::test]
    async fn dry_run_table_and_json_paths_complete_without_patch() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file("file-1", "text/plain", &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let rules = [FolderPermissionRule::folder("parent-1").allowing([DriveOperation::Trash])];
        for restore in [false, true] {
            for output in [OutputFormat::Table, OutputFormat::Json] {
                run_trash(
                    &client,
                    &TrashOptions {
                        file_id: "file-1".into(),
                        restore,
                        dry_run: true,
                    },
                    &rules,
                    &output,
                )
                .await
                .unwrap();
            }
        }
    }

    #[test]
    fn terminal_output_covers_results_and_sanitizes_server_text() {
        let results = [
            TrashResult::WouldTrash,
            TrashResult::Trashed,
            TrashResult::WouldUntrash,
            TrashResult::Untrashed,
            TrashResult::AlreadyTrashed,
            TrashResult::NotTrashed,
            TrashResult::RefusedFolder,
            TrashResult::RefusedNoVisibleParents,
            TrashResult::Blocked { decided_by: None },
            TrashResult::Failed {
                detail: "bad\x1b[31m error".into(),
            },
        ];
        for result in results {
            let mut bytes = Vec::new();
            write_outcome(
                &TrashOutcome {
                    file_id: "f\x1b[31m".into(),
                    file_name: None,
                    resolved_folder_id: None,
                    result,
                },
                &mut bytes,
            )
            .unwrap();
            let text = String::from_utf8(bytes).unwrap();
            assert!(!text.contains('\x1b'));
            assert!(!text.is_empty());
        }
    }

    #[test]
    fn json_output_has_the_scriptable_status() {
        let outcome = TrashOutcome {
            file_id: "f".into(),
            file_name: Some("name".into()),
            resolved_folder_id: None,
            result: TrashResult::WouldUntrash,
        };
        let mut bytes = Vec::new();
        crate::cli::drive::format::JsonlSerialize::write_jsonl(&outcome, &mut bytes).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["result"]["status"], "would-untrash");
        assert_eq!(value["file_id"], "f");
    }
}
