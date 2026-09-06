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
use crate::drive::folder_ancestry::{self, DecisionSource, FileTargetDecision};
use crate::drive::types::GOOGLE_FOLDER_MIME_TYPE;
use crate::drive::write_gate::{self, DriveOperation, FolderPermissionRule, Verdict};

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
}

impl From<OperationArg> for DriveOperation {
    fn from(arg: OperationArg) -> Self {
        match arg {
            OperationArg::Read => Self::Read,
            OperationArg::Create => Self::Create,
            OperationArg::Upload => Self::Upload,
            OperationArg::Edit => Self::Edit,
            OperationArg::SheetsWrite => Self::SheetsWrite,
        }
    }
}

/// Evaluates the configured write-permission rules against a real target
/// and prints the verdict — the same [`folder_ancestry::resolve_decision`]/
/// [`folder_ancestry::resolve_decision_for_file_target`] the real
/// `create`/`upload`/`edit`/`sheets write` engine modules call, so this
/// diagnostic can never drift from actual enforcement.
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
    /// The folder id of the rule that decided this, if a folder rule did.
    pub decided_by_folder_id: Option<String>,
    /// How many levels above the target that rule's folder sits.
    pub decided_by_depth: Option<usize>,
    /// The file id of the rule that decided this, if a **file** rule did
    /// (issue #1612). Mutually exclusive with `decided_by_folder_id`.
    pub decided_by_file_id: Option<String>,
    /// How the verdict was reached: `"file-rule"`, `"folder-chain"` or
    /// `"no-visible-parents"`.
    ///
    /// `"no-visible-parents"` is the answer to "why can't I grant this
    /// with a folder rule?" — the diagnostic this whole report exists to
    /// give. Always `"folder-chain"` for a folder target, which never
    /// consults file rules.
    pub evaluated_via: String,
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
    let evaluated = evaluate_target(&files_api, target_id, op, rules).await?;
    let decision = evaluated.decision;
    let log_fields = write_gate::decided_by_log_fields(decision.decided_by.as_ref());
    let report = CheckReport {
        target_id: target_id.to_string(),
        operation: op.to_string(),
        verdict: match decision.verdict {
            Verdict::Allow => "allow".to_string(),
            Verdict::Deny => "deny".to_string(),
        },
        decided_by_folder_id: log_fields.folder_id,
        decided_by_depth: log_fields.depth,
        decided_by_file_id: log_fields.file_id,
        evaluated_via: evaluated_via(evaluated.source).to_string(),
    };
    if output_as(&report, output)? {
        return Ok(());
    }
    print_report(&report);
    Ok(())
}

/// Fetches `target_id` and evaluates `op` against it.
///
/// A **folder** target's chain starts at itself (mirrors `create`/
/// `upload`'s `--parent` semantics, reusing the already-fetched metadata
/// via [`folder_ancestry::resolve_decision_from`] rather than re-fetching
/// it). `file_id` rules deliberately do **not** apply to a folder target:
/// a folder target means "create or upload something inside this", and a
/// `file_id` rule naming a folder id would just be a worse spelling of a
/// non-recursive `folder_id` rule.
///
/// A **file** target goes through
/// [`folder_ancestry::resolve_decision_for_file_target`] — the same single
/// entry point `drive edit` and `drive sheets write` use, which is what
/// lets this diagnostic claim it can never drift from actual enforcement.
async fn evaluate_target(
    files_api: &FilesApi<'_>,
    target_id: &str,
    op: DriveOperation,
    rules: &[FolderPermissionRule],
) -> Result<FileTargetDecision> {
    let target = files_api.get_metadata(target_id).await?;
    if target.mime_type == GOOGLE_FOLDER_MIME_TYPE {
        let decision = folder_ancestry::resolve_decision_from(files_api, target, op, rules).await?;
        return Ok(FileTargetDecision {
            decision,
            resolved_folder_id: None,
            source: DecisionSource::FolderChain,
        });
    }
    folder_ancestry::resolve_decision_for_file_target(files_api, &target, op, rules).await
}

/// The `evaluated_via` string for a [`DecisionSource`].
///
/// Kebab-case to match every other machine-readable string this CLI emits.
const fn evaluated_via(source: DecisionSource) -> &'static str {
    match source {
        DecisionSource::FileRule => "file-rule",
        DecisionSource::FolderChain => "folder-chain",
        DecisionSource::NoVisibleParents => "no-visible-parents",
    }
}

/// Whether `print_report` should emit the `no visible parents` note.
///
/// Gated on the verdict as well as the source. `read` defaults to
/// [`Verdict::Allow`] on an empty chain, so a link-shared target checked for
/// `read` reaches the renderer having been *permitted* — advice on how to
/// grant it would read as a refusal that isn't one. The note only ever helps
/// an operator staring at a `deny` they cannot name a folder rule to fix.
fn should_note_no_visible_parents(evaluated_via: &str, verdict: &str) -> bool {
    evaluated_via == "no-visible-parents" && verdict == "deny"
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
        _ => match &report.decided_by_file_id {
            Some(file_id) => println!(
                "decided by: rule on file {}",
                sanitize_for_terminal(file_id)
            ),
            None => println!("decided by: default policy (no matching rule)"),
        },
    }
    // The one line this diagnostic exists to print: it names the *only*
    // rule shape that could ever change this verdict.
    if should_note_no_visible_parents(&report.evaluated_via, &report.verdict) {
        println!(
            "note:       this target has no parent folder visible to this account, so no\n\
             \x20           folder_id rule can apply — grant it with a file_id rule instead"
        );
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
        FolderPermissionRule::folder(folder_id)
            .recursive(recursive)
            .allowing(allow.iter().copied())
    }

    fn file_rule(file_id: &str, allow: &[DriveOperation]) -> FolderPermissionRule {
        FolderPermissionRule::file(file_id).allowing(allow.iter().copied())
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
        assert_eq!(decision.decision.verdict, Verdict::Allow);
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
        assert_eq!(decision.decision.verdict, Verdict::Allow);
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
        assert_eq!(decision.decision.verdict, Verdict::Allow);
        let decision = evaluate_target(&files_api, "orphan", DriveOperation::Edit, &[])
            .await
            .unwrap();
        assert_eq!(decision.decision.verdict, Verdict::Deny);
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
            decision.decision.verdict,
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
            decided_by_file_id: None,
            evaluated_via: "folder-chain".to_string(),
        };
        // Smoke test only — print_report writes to stdout directly.
        print_report(&report);
    }

    #[test]
    fn print_report_renders_a_file_rule_and_the_no_visible_parents_note() {
        print_report(&CheckReport {
            target_id: "x1".to_string(),
            operation: "sheets-write".to_string(),
            verdict: "allow".to_string(),
            decided_by_folder_id: None,
            decided_by_depth: None,
            decided_by_file_id: Some("x1".to_string()),
            evaluated_via: "file-rule".to_string(),
        });
        print_report(&CheckReport {
            target_id: "x2".to_string(),
            operation: "sheets-write".to_string(),
            verdict: "deny".to_string(),
            decided_by_folder_id: None,
            decided_by_depth: None,
            decided_by_file_id: None,
            evaluated_via: "no-visible-parents".to_string(),
        });
    }

    #[test]
    fn the_no_visible_parents_note_is_gated_on_a_deny() {
        assert!(should_note_no_visible_parents("no-visible-parents", "deny"));
        // `read` defaults to allow on an empty chain, so this pairing is
        // reachable — and must not advise granting what was permitted.
        assert!(!should_note_no_visible_parents(
            "no-visible-parents",
            "allow"
        ));
        assert!(!should_note_no_visible_parents("folder-chain", "deny"));
        assert!(!should_note_no_visible_parents("file-rule", "deny"));
    }

    #[test]
    fn evaluated_via_maps_every_decision_source() {
        assert_eq!(evaluated_via(DecisionSource::FileRule), "file-rule");
        assert_eq!(evaluated_via(DecisionSource::FolderChain), "folder-chain");
        assert_eq!(
            evaluated_via(DecisionSource::NoVisibleParents),
            "no-visible-parents"
        );
    }

    // ── file-id rules (issue #1612) ────────────────────────────────────

    /// Mounts `GET /drive/v3/files/<id>` returning a plain file with
    /// `parents`.
    fn mount_plain_file(id: &'static str, parents: &[&str]) -> wiremock::Mock {
        let parents: Vec<String> = parents.iter().map(|p| (*p).to_string()).collect();
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!("/drive/v3/files/{id}")))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": id, "name": id, "mimeType": "text/plain", "parents": parents,
                })),
            )
    }

    #[tokio::test]
    async fn a_file_rule_decides_a_file_target_without_walking_its_parents() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        // `never-mounted` is deliberately absent: wiremock panics if the
        // ancestor walk happens at all.
        mount_plain_file("file-1", &["never-mounted"])
            .mount(&server)
            .await;
        let files_api = FilesApi::new(&client);
        let rules = [file_rule("file-1", &[DriveOperation::SheetsWrite])];

        let evaluated = evaluate_target(&files_api, "file-1", DriveOperation::SheetsWrite, &rules)
            .await
            .unwrap();

        assert_eq!(evaluated.decision.verdict, Verdict::Allow);
        assert_eq!(evaluated.source, DecisionSource::FileRule);
    }

    #[tokio::test]
    async fn a_parentless_file_target_is_reported_as_no_visible_parents() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_plain_file("file-1", &[]).mount(&server).await;
        let files_api = FilesApi::new(&client);

        let evaluated = evaluate_target(&files_api, "file-1", DriveOperation::Edit, &[])
            .await
            .unwrap();

        assert_eq!(evaluated.source, DecisionSource::NoVisibleParents);
        assert_eq!(evaluated.decision.verdict, Verdict::Deny);
    }

    #[tokio::test]
    async fn no_visible_parents_still_answers_allow_for_read() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_plain_file("file-1", &[]).mount(&server).await;
        let files_api = FilesApi::new(&client);

        let evaluated = evaluate_target(&files_api, "file-1", DriveOperation::Read, &[])
            .await
            .unwrap();

        assert_eq!(evaluated.decision.verdict, Verdict::Allow);
        assert_eq!(evaluated.source, DecisionSource::NoVisibleParents);
    }

    #[tokio::test]
    async fn a_file_rule_does_not_apply_to_a_folder_target() {
        // A folder target means "create/upload something inside this", and
        // a `file_id` rule naming a folder id would just be a worse
        // spelling of a non-recursive `folder_id` rule. Deliberately inert.
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
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
        let rules = [file_rule("folder-1", &[DriveOperation::Create])];

        let evaluated = evaluate_target(&files_api, "folder-1", DriveOperation::Create, &rules)
            .await
            .unwrap();

        assert_eq!(
            evaluated.decision.verdict,
            Verdict::Deny,
            "a file_id rule must not grant a folder target"
        );
        assert_eq!(evaluated.decision.decided_by, None);
    }
}
