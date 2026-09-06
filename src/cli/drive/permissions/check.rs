//! CLI command for `omni-dev drive permissions check`.

use anyhow::Result;
use clap::Parser;
use serde::Serialize;

use crate::cli::drive::format::{
    output_as, sanitize_for_terminal, write_scalar_jsonl, JsonlSerialize, OutputFormat,
};
use crate::cli::drive::helpers::active_account_rules;
use crate::drive::client::DriveClient;
use crate::drive::files_api::FilesApi;
use crate::drive::folder_ancestry;
use crate::drive::types::GOOGLE_FOLDER_MIME_TYPE;
use crate::drive::write_gate::{Decision, DriveOperation, FolderPermissionRule, Verdict};

/// `--operation`'s value set — a thin CLI-layer copy of
/// [`DriveOperation`], kept separate so the pure engine module has no
/// `clap` dependency (mirrors how `crate::drive::visibility::MoveGateFlags`
/// stays free of the CLI layer's option-parsing types).
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum OperationArg {
    Read,
    Create,
    Upload,
    Edit,
    /// Writing cells into a Google Sheet — distinct from `Edit`, see
    /// [`DriveOperation::SheetsWrite`].
    SheetsWrite,
    /// Structurally editing a Google Sheet — distinct from `SheetsWrite`,
    /// see [`DriveOperation::SheetsStructure`].
    SheetsStructure,
}

impl From<OperationArg> for DriveOperation {
    fn from(arg: OperationArg) -> Self {
        match arg {
            OperationArg::Read => Self::Read,
            OperationArg::Create => Self::Create,
            OperationArg::Upload => Self::Upload,
            OperationArg::Edit => Self::Edit,
            OperationArg::SheetsWrite => Self::SheetsWrite,
            OperationArg::SheetsStructure => Self::SheetsStructure,
        }
    }
}

/// Evaluates the configured write-permission rules against a real target
/// and prints the verdict — the same [`folder_ancestry::resolve_decision`]/
/// [`folder_ancestry::resolve_decision_for_parents`] the real
/// `create`/`upload`/`edit` engine modules call, so this diagnostic can
/// never drift from actual enforcement.
#[derive(Parser)]
pub struct CheckCommand {
    /// The folder or file id to evaluate.
    pub id: String,

    /// Which operation to check.
    #[arg(long, value_enum)]
    pub operation: OperationArg,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl CheckCommand {
    /// Runs the command against the shared client resolved by
    /// `PermissionsCommand::execute`.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let rules = active_account_rules()?;
        run_check(
            client,
            &self.id,
            self.operation.into(),
            &rules,
            &self.output,
        )
        .await
    }
}

/// Report shape shared by table/JSON/YAML/JSONL rendering.
#[derive(Debug, Clone, Serialize)]
pub struct CheckReport {
    /// The evaluated target id.
    pub target_id: String,
    /// The evaluated operation.
    pub operation: String,
    /// `"allow"` or `"deny"`.
    pub verdict: String,
    /// The folder id of the rule that decided this, if any.
    pub decided_by_folder_id: Option<String>,
    /// How many levels above the target that rule's folder sits.
    pub decided_by_depth: Option<usize>,
}

impl JsonlSerialize for CheckReport {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Resolves `target_id`'s evaluation chain (or chains, for a file with
/// multiple current parents) and prints the verdict.
///
/// Split from [`CheckCommand::execute`] so tests can inject a wiremock
/// client and a constructed rule set directly.
async fn run_check(
    client: &DriveClient,
    target_id: &str,
    op: DriveOperation,
    rules: &[FolderPermissionRule],
    output: &OutputFormat,
) -> Result<()> {
    let files_api = FilesApi::new(client);
    let decision = evaluate_target(&files_api, target_id, op, rules).await?;
    let report = CheckReport {
        target_id: target_id.to_string(),
        operation: op.to_string(),
        verdict: match decision.verdict {
            Verdict::Allow => "allow".to_string(),
            Verdict::Deny => "deny".to_string(),
        },
        decided_by_folder_id: decision.decided_by.as_ref().map(|d| d.folder_id.clone()),
        decided_by_depth: decision.decided_by.as_ref().map(|d| d.depth),
    };
    if output_as(&report, output)? {
        return Ok(());
    }
    print_report(&report);
    Ok(())
}

/// Fetches `target_id` and evaluates `op` against it: a folder target's
/// chain starts at itself (mirrors `create`/`upload`'s `--parent`
/// semantics, reusing the already-fetched metadata via
/// [`folder_ancestry::resolve_decision_from`] rather than re-fetching it);
/// a file target's chain starts at its *current* parent(s), unioned across
/// every legacy multi-parent via
/// [`folder_ancestry::resolve_decision_for_parents`] (mirrors `drive
/// edit`'s semantics) — an orphan file with no parent degenerates to the
/// bare default policy.
async fn evaluate_target(
    files_api: &FilesApi<'_>,
    target_id: &str,
    op: DriveOperation,
    rules: &[FolderPermissionRule],
) -> Result<Decision> {
    let target = files_api.get_metadata(target_id).await?;
    if target.mime_type == GOOGLE_FOLDER_MIME_TYPE {
        return folder_ancestry::resolve_decision_from(files_api, target, op, rules).await;
    }
    let (decision, _resolved_folder_id) =
        folder_ancestry::resolve_decision_for_parents(files_api, &target.parents, op, rules)
            .await?;
    Ok(decision)
}

/// Prints a `CheckReport` in the plain-text (non-`output_as`) form.
fn print_report(report: &CheckReport) {
    println!("target:     {}", sanitize_for_terminal(&report.target_id));
    println!("operation:  {}", report.operation);
    println!("verdict:    {}", report.verdict);
    match (&report.decided_by_folder_id, report.decided_by_depth) {
        (Some(folder_id), Some(depth)) => {
            println!(
                "decided by: rule on folder {} (depth {depth})",
                sanitize_for_terminal(folder_id)
            );
        }
        _ => println!("decided by: default policy (no matching rule)"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::utils::secret::Secret;

    #[test]
    fn operation_arg_maps_onto_every_drive_operation() {
        assert_eq!(
            DriveOperation::from(OperationArg::Read),
            DriveOperation::Read
        );
        assert_eq!(
            DriveOperation::from(OperationArg::Create),
            DriveOperation::Create
        );
        assert_eq!(
            DriveOperation::from(OperationArg::Upload),
            DriveOperation::Upload
        );
        assert_eq!(
            DriveOperation::from(OperationArg::Edit),
            DriveOperation::Edit
        );
        assert_eq!(
            DriveOperation::from(OperationArg::SheetsWrite),
            DriveOperation::SheetsWrite
        );
        assert_eq!(
            DriveOperation::from(OperationArg::SheetsStructure),
            DriveOperation::SheetsStructure
        );
    }

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

    fn rule(folder_id: &str, recursive: bool, allow: &[DriveOperation]) -> FolderPermissionRule {
        FolderPermissionRule {
            folder_id: folder_id.to_string(),
            recursive,
            allow: allow.iter().copied().collect(),
            deny: std::collections::HashSet::default(),
        }
    }

    #[tokio::test]
    async fn folder_target_evaluates_from_itself() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/folder-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "folder-1", "name": "folder-1", "mimeType": GOOGLE_FOLDER_MIME_TYPE,
                })),
            )
            // Exactly once: evaluate_target must reuse the metadata it
            // already fetched to decide the target is a folder, not
            // re-fetch it as the ancestor walk's own first call.
            .expect(1)
            .mount(&server)
            .await;
        let files_api = FilesApi::new(&client);
        let rules = [rule("folder-1", false, &[DriveOperation::Create])];

        let decision = evaluate_target(&files_api, "folder-1", DriveOperation::Create, &rules)
            .await
            .unwrap();
        assert_eq!(decision.verdict, Verdict::Allow);
    }

    #[tokio::test]
    async fn file_target_evaluates_from_its_parent() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/file-1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "file-1", "name": "file-1", "mimeType": "text/plain", "parents": ["folder-1"],
            })))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/folder-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "folder-1", "name": "folder-1", "mimeType": GOOGLE_FOLDER_MIME_TYPE,
                })),
            )
            .mount(&server)
            .await;
        let files_api = FilesApi::new(&client);
        let rules = [rule("folder-1", false, &[DriveOperation::Edit])];

        let decision = evaluate_target(&files_api, "file-1", DriveOperation::Edit, &rules)
            .await
            .unwrap();
        assert_eq!(decision.verdict, Verdict::Allow);
    }

    #[tokio::test]
    async fn orphan_file_uses_default_policy() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/orphan"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "orphan", "name": "orphan", "mimeType": "text/plain",
                })),
            )
            .mount(&server)
            .await;
        let files_api = FilesApi::new(&client);

        let decision = evaluate_target(&files_api, "orphan", DriveOperation::Read, &[])
            .await
            .unwrap();
        assert_eq!(decision.verdict, Verdict::Allow);
        let decision = evaluate_target(&files_api, "orphan", DriveOperation::Edit, &[])
            .await
            .unwrap();
        assert_eq!(decision.verdict, Verdict::Deny);
    }

    #[tokio::test]
    async fn multi_parent_file_denies_when_any_parent_denies() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/file-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "file-1", "name": "file-1", "mimeType": "text/plain",
                    "parents": ["allow-parent", "deny-parent"],
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/allow-parent"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "allow-parent", "name": "allow-parent", "mimeType": GOOGLE_FOLDER_MIME_TYPE,
            })))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/deny-parent"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "deny-parent", "name": "deny-parent", "mimeType": GOOGLE_FOLDER_MIME_TYPE,
                })),
            )
            .mount(&server)
            .await;
        let files_api = FilesApi::new(&client);
        let rules = [rule("allow-parent", false, &[DriveOperation::Edit])];

        let decision = evaluate_target(&files_api, "file-1", DriveOperation::Edit, &rules)
            .await
            .unwrap();
        assert_eq!(
            decision.verdict,
            Verdict::Deny,
            "deny-parent has no matching rule and edit defaults deny, which must win over allow-parent"
        );
    }

    #[tokio::test]
    async fn ancestor_chain_fetch_failure_propagates_as_err() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/folder-1"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("server error"))
            .mount(&server)
            .await;
        let files_api = FilesApi::new(&client);

        let result = evaluate_target(&files_api, "folder-1", DriveOperation::Read, &[]).await;
        assert!(result.is_err());
    }

    #[test]
    fn print_report_no_match_says_default_policy() {
        let report = CheckReport {
            target_id: "f1".to_string(),
            operation: "create".to_string(),
            verdict: "deny".to_string(),
            decided_by_folder_id: None,
            decided_by_depth: None,
        };
        // Smoke test only — print_report writes to stdout directly.
        print_report(&report);
    }
}
