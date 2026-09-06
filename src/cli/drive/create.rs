//! CLI command for `omni-dev drive create`.

use anyhow::Result;
use clap::Parser;

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::cli::drive::helpers::active_account_rules;
use crate::drive::client::DriveClient;
use crate::drive::create::{self, CreateOptions, CreateOutcome, CreateResult};
use crate::drive::types::GOOGLE_FOLDER_MIME_TYPE;
use crate::drive::write_gate::FolderPermissionRule;

/// MIME type used when creating a plain file with neither `--folder` nor
/// `--mime-type` given — Drive's own fallback for a `files.create` call
/// with no content and no explicit type.
const DEFAULT_FILE_MIME_TYPE: &str = "application/octet-stream";

/// Creates a new file or folder, gated by the account's configured
/// folder write-permission rules (issue #1574). Requires the `drive.file`
/// or `drive` scope (`drive auth login --write-file`/`--write-full`).
#[derive(Parser)]
pub struct CreateCommand {
    /// The new file/folder's display name.
    #[arg(long)]
    pub name: String,

    /// The folder id to create it in.
    #[arg(long, value_name = "FOLDER_ID")]
    pub parent: String,

    /// Create a folder instead of a plain file. Conflicts with `--mime-type`.
    #[arg(long, conflicts_with = "mime_type")]
    pub folder: bool,

    /// MIME type for a plain file. Defaults to `application/octet-stream`
    /// when omitted. Conflicts with `--folder`.
    #[arg(long = "mime-type", value_name = "TYPE")]
    pub mime_type: Option<String>,

    /// Reports the gate verdict without calling `files.create`.
    #[arg(long)]
    pub dry_run: bool,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl CreateCommand {
    /// Runs the command against the shared client resolved by the parent
    /// `DriveCommand::execute`.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let mime_type = if self.folder {
            GOOGLE_FOLDER_MIME_TYPE.to_string()
        } else {
            self.mime_type
                .unwrap_or_else(|| DEFAULT_FILE_MIME_TYPE.to_string())
        };
        let opts = CreateOptions {
            name: self.name,
            parent_folder_id: self.parent,
            mime_type,
            dry_run: self.dry_run,
        };
        let rules = active_account_rules()?;
        run_create(client, &opts, &rules, &self.output).await
    }
}

/// Runs `create` and emits the outcome in the requested format.
///
/// Split from [`CreateCommand::execute`] so tests can inject a wiremock
/// client and a constructed rule set directly.
async fn run_create(
    client: &DriveClient,
    opts: &CreateOptions,
    rules: &[FolderPermissionRule],
    output: &OutputFormat,
) -> Result<()> {
    let outcome = create::create(client, opts, rules).await;
    if output_as(&outcome, output)? {
        return Ok(());
    }
    print_outcome(&outcome);
    Ok(())
}

fn print_outcome(outcome: &CreateOutcome) {
    let name = sanitize_for_terminal(&outcome.name);
    let parent = sanitize_for_terminal(&outcome.parent_folder_id);
    match &outcome.result {
        CreateResult::WouldCreate => {
            println!("Would create: {name} in {parent}");
        }
        CreateResult::Blocked { decided_by } => {
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
        CreateResult::Created { file_id } => {
            println!(
                "Created: {name} ({}) in {parent}",
                sanitize_for_terminal(file_id)
            );
        }
        CreateResult::Failed { detail } => {
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
    async fn dry_run_reports_verdict_without_calling_the_create_endpoint() {
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

        run_create(&client, &opts(true), &[allow_rule()], &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_create_json_path_returns_ok() {
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

        run_create(&client, &opts(false), &[], &OutputFormat::Json)
            .await
            .unwrap();
    }

    #[test]
    fn print_outcome_smoke_test_every_variant() {
        print_outcome(&CreateOutcome {
            name: "f".to_string(),
            parent_folder_id: "p".to_string(),
            result: CreateResult::WouldCreate,
        });
        print_outcome(&CreateOutcome {
            name: "f".to_string(),
            parent_folder_id: "p".to_string(),
            result: CreateResult::Blocked { decided_by: None },
        });
        print_outcome(&CreateOutcome {
            name: "f".to_string(),
            parent_folder_id: "p".to_string(),
            result: CreateResult::Blocked {
                decided_by: Some(crate::drive::write_gate::DecidingRule::Folder {
                    folder_id: "parent-1".to_string(),
                    depth: 0,
                }),
            },
        });
        print_outcome(&CreateOutcome {
            name: "f".to_string(),
            parent_folder_id: "p".to_string(),
            result: CreateResult::Created {
                file_id: "id1".to_string(),
            },
        });
        print_outcome(&CreateOutcome {
            name: "f".to_string(),
            parent_folder_id: "p".to_string(),
            result: CreateResult::Failed {
                detail: "boom".to_string(),
            },
        });
    }
}
