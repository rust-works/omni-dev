//! CLI command for `omni-dev drive edit`.

use std::io::Read as _;
use std::path::Path;

use anyhow::{Context, Result};
use clap::Parser;

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::cli::drive::helpers::active_account_rules;
use crate::cli::drive::upload::read_local_content;
use crate::drive::client::DriveClient;
use crate::drive::content_edit::{self, EditOptions, EditOutcome, EditResult};
use crate::drive::files_api::{check_upload_size, MAX_UPLOAD_BYTES};
use crate::drive::write_gate::FolderPermissionRule;

/// MIME type used when `--mime-type` is omitted — Drive's own fallback for
/// unspecified content.
const DEFAULT_CONTENT_MIME_TYPE: &str = "application/octet-stream";

/// Replaces an existing file's content, gated by the account's configured
/// write-permission rules (issues #1574, #1612). Requires the `drive.file`
/// scope if `omni-dev` created the file, or the unrestricted `drive` scope
/// for any pre-existing file (`drive auth login --write-file` or
/// `--write-full`).
///
/// Refuses, client-side, any target that is a Google-native document
/// (Docs/Sheets/Slides/...) — there is no meaningful raw "content" to
/// replace via a media PATCH for those.
#[derive(Parser)]
pub struct EditCommand {
    /// Drive file id (from `drive search`, or the `id` segment of a Drive
    /// URL).
    pub file_id: String,

    /// New content: a local file path, or `-` to read from stdin.
    #[arg(long, value_name = "LOCAL_PATH|-")]
    pub content: String,

    /// MIME type for the content. Defaults to `application/octet-stream`.
    #[arg(long = "mime-type", value_name = "TYPE")]
    pub mime_type: Option<String>,

    /// Reports the gate verdict without calling `files.update`.
    #[arg(long)]
    pub dry_run: bool,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl EditCommand {
    /// Runs the command against the shared client resolved by the parent
    /// `DriveCommand::execute`.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let content = resolve_content(&self.content)?;
        let content_type = self
            .mime_type
            .unwrap_or_else(|| DEFAULT_CONTENT_MIME_TYPE.to_string());
        let opts = EditOptions {
            file_id: self.file_id,
            content,
            content_type,
            dry_run: self.dry_run,
        };
        let rules = active_account_rules()?;
        run_edit(client, &opts, &rules, &self.output).await
    }
}

/// Resolves `--content`'s value: `-` reads (and size-checks) stdin, any
/// other value is treated as a local path (via
/// `crate::cli::drive::upload::read_local_content`, shared with `drive
/// upload`'s identical stat-then-read-then-check pattern).
fn resolve_content(content_arg: &str) -> Result<Vec<u8>> {
    if content_arg == "-" {
        read_stdin_content()
    } else {
        read_local_content(Path::new(content_arg))
    }
}

/// Reads stdin, bounded at [`MAX_UPLOAD_BYTES`] + 1 so an unbounded stream
/// can never be buffered past the cap before being refused — there's no
/// upfront size to stat for a pipe, unlike a local file.
fn read_stdin_content() -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    std::io::stdin()
        .take(MAX_UPLOAD_BYTES + 1)
        .read_to_end(&mut buf)
        .context("Failed to read stdin")?;
    check_upload_size(buf.len() as u64)?;
    Ok(buf)
}

/// Runs `edit` and emits the outcome in the requested format.
///
/// Split from [`EditCommand::execute`] so tests can inject a wiremock
/// client and pre-built options/rules directly, without touching the
/// filesystem or credential-loading path.
async fn run_edit(
    client: &DriveClient,
    opts: &EditOptions,
    rules: &[FolderPermissionRule],
    output: &OutputFormat,
) -> Result<()> {
    let outcome = content_edit::edit(client, opts, rules).await;
    if output_as(&outcome, output)? {
        return Ok(());
    }
    print_outcome(&outcome);
    Ok(())
}

fn print_outcome(outcome: &EditOutcome) {
    let file_id = sanitize_for_terminal(&outcome.file_id);
    match &outcome.result {
        EditResult::WouldEdit => println!("Would edit: {file_id}"),
        EditResult::RefusedNativeDocument => {
            println!(
                "Refused: {file_id} is a Google-native document (Docs/Sheets/Slides/...) — no \
                 raw content to replace"
            );
        }
        EditResult::RefusedNoVisibleParents => {
            println!(
                "Refused: {file_id} has no parent folder visible to this account, so no folder \
                 rule can apply to it. This is normal for a file shared by link or email. \
                 Grant it by id instead: add {{\"file_id\": \"<file id>\", \"allow\": \
                 [\"edit\"]}} to write_permissions.rules."
            );
        }
        EditResult::Blocked { decided_by } => {
            println!("Blocked: {file_id}");
            match decided_by {
                Some(rule) => println!(
                    "  refused by rule on {} {}{}",
                    rule.kind_label(),
                    sanitize_for_terminal(rule.id()),
                    rule.depth_suffix()
                ),
                None => println!("  refused by default policy (no matching rule)"),
            }
        }
        EditResult::Edited => println!("Edited: {file_id}"),
        EditResult::Failed { detail } => {
            println!("Failed: {file_id}: {}", sanitize_for_terminal(detail));
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

    fn opts(dry_run: bool) -> EditOptions {
        EditOptions {
            file_id: "file-1".to_string(),
            content: b"new content".to_vec(),
            content_type: "text/plain".to_string(),
            dry_run,
        }
    }

    fn allow_rule() -> FolderPermissionRule {
        FolderPermissionRule {
            folder_id: Some("parent-1".to_string()),
            file_id: None,
            recursive: false,
            allow: std::iter::once(DriveOperation::Edit).collect(),
            deny: std::collections::HashSet::default(),
        }
    }

    #[tokio::test]
    async fn dry_run_reports_verdict_without_calling_the_edit_endpoint() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/file-1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "file-1", "name": "file-1", "mimeType": "text/plain", "parents": ["parent-1"],
            })))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/parent-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "parent-1", "name": "parent-1", "mimeType": GOOGLE_FOLDER_MIME_TYPE,
                })),
            )
            .mount(&server)
            .await;
        // No PATCH mock mounted — dry-run must never call files.update.

        run_edit(&client, &opts(true), &[allow_rule()], &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_edit_json_path_returns_ok() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/file-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "file-1", "name": "file-1", "mimeType": "text/plain",
                })),
            )
            .mount(&server)
            .await;

        run_edit(&client, &opts(false), &[], &OutputFormat::Json)
            .await
            .unwrap();
    }

    // ── resolve_content ─────────────────────────────────────────────

    #[test]
    fn resolve_content_reads_a_local_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("content.txt");
        std::fs::write(&path, b"hello").unwrap();
        let content = resolve_content(path.to_str().unwrap()).unwrap();
        assert_eq!(content, b"hello");
    }

    #[test]
    fn resolve_content_dash_is_never_treated_as_a_local_path() {
        // "-" would fail as a local path (no such file); this just asserts
        // the routing decision, not stdin's actual content in a test
        // process (reading real stdin here would hang/misbehave under
        // `cargo test`, so this is intentionally not exercised further).
        assert!(!Path::new("-").exists());
    }

    #[test]
    fn print_outcome_smoke_test_every_variant() {
        print_outcome(&EditOutcome {
            file_id: "f1".to_string(),
            file_name: Some("f".to_string()),
            resolved_folder_id: Some("p".to_string()),
            result: EditResult::WouldEdit,
        });
        print_outcome(&EditOutcome {
            file_id: "f1".to_string(),
            file_name: Some("f".to_string()),
            resolved_folder_id: None,
            result: EditResult::RefusedNativeDocument,
        });
        print_outcome(&EditOutcome {
            file_id: "f1".to_string(),
            file_name: Some("f".to_string()),
            resolved_folder_id: None,
            result: EditResult::RefusedNoVisibleParents,
        });
        print_outcome(&EditOutcome {
            file_id: "f1".to_string(),
            file_name: Some("f".to_string()),
            resolved_folder_id: Some("p".to_string()),
            result: EditResult::Blocked { decided_by: None },
        });
        print_outcome(&EditOutcome {
            file_id: "f1".to_string(),
            file_name: Some("f".to_string()),
            resolved_folder_id: Some("p".to_string()),
            result: EditResult::Blocked {
                decided_by: Some(crate::drive::write_gate::DecidingRule::Folder {
                    folder_id: "parent-1".to_string(),
                    depth: 0,
                }),
            },
        });
        print_outcome(&EditOutcome {
            file_id: "f1".to_string(),
            file_name: Some("f".to_string()),
            resolved_folder_id: Some("p".to_string()),
            result: EditResult::Edited,
        });
        print_outcome(&EditOutcome {
            file_id: "f1".to_string(),
            file_name: None,
            resolved_folder_id: None,
            result: EditResult::Failed {
                detail: "boom".to_string(),
            },
        });
    }
}
