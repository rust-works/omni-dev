//! CLI command for `omni-dev drive upload`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::cli::drive::helpers::active_account_rules;
use crate::drive::client::DriveClient;
use crate::drive::files_api::check_upload_size;
use crate::drive::upload::{self, UploadOptions, UploadOutcome, UploadResult};
use crate::drive::write_gate::FolderPermissionRule;

/// MIME type used when `--mime-type` is omitted — Drive's own fallback for
/// unspecified content.
const DEFAULT_CONTENT_MIME_TYPE: &str = "application/octet-stream";

/// Uploads local content as a new file, gated by the account's configured
/// folder write-permission rules (issue #1574). Requires the `drive.file`
/// or `drive` scope (`drive auth login --write-file`/`--write-full`).
#[derive(Parser)]
pub struct UploadCommand {
    /// Local file to upload.
    pub local_path: PathBuf,

    /// The folder id to upload it into.
    #[arg(long, value_name = "FOLDER_ID")]
    pub parent: String,

    /// The new file's display name. Defaults to `local_path`'s file name.
    #[arg(long)]
    pub name: Option<String>,

    /// MIME type for the content. Defaults to `application/octet-stream`.
    #[arg(long = "mime-type", value_name = "TYPE")]
    pub mime_type: Option<String>,

    /// Reports the gate verdict without calling `files.create`.
    #[arg(long)]
    pub dry_run: bool,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl UploadCommand {
    /// Runs the command against the shared client resolved by the parent
    /// `DriveCommand::execute`.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let name = match self.name {
            Some(name) => name,
            None => self
                .local_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .context("Cannot infer a name from local_path; pass --name explicitly")?,
        };
        let content_type = self
            .mime_type
            .unwrap_or_else(|| DEFAULT_CONTENT_MIME_TYPE.to_string());
        let content = read_local_content(&self.local_path)?;

        let opts = UploadOptions {
            name,
            parent_folder_id: self.parent,
            content,
            content_type,
            dry_run: self.dry_run,
        };
        let rules = active_account_rules()?;
        run_upload(client, &opts, &rules, &self.output).await
    }
}

/// Stats `path` and refuses it *before* reading if it exceeds
/// [`MAX_UPLOAD_BYTES`](crate::drive::files_api::MAX_UPLOAD_BYTES) —
/// avoids ever buffering an oversized file, and
/// fires identically whether or not `--dry-run` is set, since this runs
/// before `UploadOptions` is even constructed. Reused by
/// `crate::cli::drive::edit` for its `--content <LOCAL_PATH>` case.
pub(crate) fn read_local_content(path: &std::path::Path) -> Result<Vec<u8>> {
    let len = std::fs::metadata(path)
        .with_context(|| format!("Failed to stat {}", path.display()))?
        .len();
    check_upload_size(len)?;
    std::fs::read(path).with_context(|| format!("Failed to read {}", path.display()))
}

/// Runs `upload` and emits the outcome in the requested format.
///
/// Split from [`UploadCommand::execute`] so tests can inject a wiremock
/// client and pre-built options/rules directly, without touching the
/// filesystem or credential-loading path.
async fn run_upload(
    client: &DriveClient,
    opts: &UploadOptions,
    rules: &[FolderPermissionRule],
    output: &OutputFormat,
) -> Result<()> {
    let outcome = upload::upload(client, opts, rules).await;
    if output_as(&outcome, output)? {
        return Ok(());
    }
    print_outcome(&outcome);
    Ok(())
}

fn print_outcome(outcome: &UploadOutcome) {
    let name = sanitize_for_terminal(&outcome.name);
    let parent = sanitize_for_terminal(&outcome.parent_folder_id);
    match &outcome.result {
        UploadResult::WouldUpload => {
            println!("Would upload: {name} in {parent}");
        }
        UploadResult::Blocked { decided_by } => {
            println!("Blocked: {name} in {parent}");
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
        UploadResult::Uploaded { file_id } => {
            println!(
                "Uploaded: {name} ({}) in {parent}",
                sanitize_for_terminal(file_id)
            );
        }
        UploadResult::Failed { detail } => {
            println!(
                "Failed: {name} in {parent}: {}",
                sanitize_for_terminal(detail)
            );
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::files_api::MAX_UPLOAD_BYTES;
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
    async fn dry_run_reports_verdict_without_calling_the_upload_endpoint() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/parent-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "parent-1", "name": "parent-1", "mimeType": GOOGLE_FOLDER_MIME_TYPE,
                })),
            )
            .mount(&server)
            .await;
        // No POST mock mounted — dry-run must never call files.create.

        run_upload(&client, &opts(true), &[allow_rule()], &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_upload_json_path_returns_ok() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/parent-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "parent-1", "name": "parent-1", "mimeType": GOOGLE_FOLDER_MIME_TYPE,
                })),
            )
            .mount(&server)
            .await;

        run_upload(&client, &opts(false), &[], &OutputFormat::Json)
            .await
            .unwrap();
    }

    // ── read_local_content ────────────────────────────────────────────

    #[test]
    fn read_local_content_reads_a_small_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.txt");
        std::fs::write(&path, b"hello").unwrap();
        let content = read_local_content(&path).unwrap();
        assert_eq!(content, b"hello");
    }

    #[test]
    fn read_local_content_refuses_an_oversized_file_before_reading() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_UPLOAD_BYTES + 1).unwrap();

        let err = read_local_content(&path).unwrap_err();
        assert!(err.to_string().contains("refusing to upload"), "{err}");
    }

    #[test]
    fn read_local_content_missing_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.txt");
        assert!(read_local_content(&path).is_err());
    }

    #[test]
    fn print_outcome_smoke_test_every_variant() {
        print_outcome(&UploadOutcome {
            name: "f".to_string(),
            parent_folder_id: "p".to_string(),
            result: UploadResult::WouldUpload,
        });
        print_outcome(&UploadOutcome {
            name: "f".to_string(),
            parent_folder_id: "p".to_string(),
            result: UploadResult::Blocked { decided_by: None },
        });
        print_outcome(&UploadOutcome {
            name: "f".to_string(),
            parent_folder_id: "p".to_string(),
            result: UploadResult::Blocked {
                decided_by: Some(crate::drive::write_gate::DecidingRule::Folder {
                    folder_id: "parent-1".to_string(),
                    depth: 0,
                }),
            },
        });
        print_outcome(&UploadOutcome {
            name: "f".to_string(),
            parent_folder_id: "p".to_string(),
            result: UploadResult::Uploaded {
                file_id: "id1".to_string(),
            },
        });
        print_outcome(&UploadOutcome {
            name: "f".to_string(),
            parent_folder_id: "p".to_string(),
            result: UploadResult::Failed {
                detail: "boom".to_string(),
            },
        });
    }
}
