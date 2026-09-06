//! Drive file content edit — replaces an existing file's content (issue
//! #1574, [ADR-0071](../../docs/adrs/adr-0071.md)).
//!
//! The most structurally distinct of the three mutating verbs: unlike
//! `create`/`upload` (whose gate chain starts at the caller-given
//! `--parent`), `edit`'s chain starts at the target's *current* parent
//! folder(s) — `files.get` first, then
//! [`folder_ancestry::resolve_decision_for_file_target`], which consults a
//! `file_id` rule before the parents (issue #1612) and otherwise resolves
//! and combines a decision per parent for a legacy multi-parent file
//! (mirrors `visibility.rs`'s existing multi-parent-union contract; shared
//! with `drive permissions check`'s identical file-target case). A target
//! with no visible parent and no file rule is refused as
//! [`EditResult::RefusedNoVisibleParents`] rather than degenerating to a
//! bare default-policy `Blocked`.
//!
//! Still single-target, so this follows `create.rs`/`upload.rs`'s linear-
//! function shape, not `file_move.rs`'s batch Plan/Execute.

use std::time::{Duration, Instant};

use serde::Serialize;

use crate::cli::drive::format::{write_scalar_jsonl, JsonlSerialize};
use crate::drive::client::DriveClient;
use crate::drive::files_api::FilesApi;
use crate::drive::folder_ancestry;
use crate::drive::write_gate::{self, DecidingRule, DriveOperation, FolderPermissionRule};
use crate::request_log::{self, DriveMutationOutcome};

/// Per-call edit options.
#[derive(Debug, Clone)]
pub struct EditOptions {
    /// The file id to edit.
    pub file_id: String,
    /// The new content, already read into memory (and already
    /// size-checked) by the caller.
    pub content: Vec<u8>,
    /// The content's MIME type.
    pub content_type: String,
    /// When `true`, classify but never call `files.update`.
    pub dry_run: bool,
}

/// What happened (or, under `--dry-run`, would happen).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum EditResult {
    /// `--dry-run`, and the gate would allow it.
    WouldEdit,
    /// The target is a Google-native document (Docs/Sheets/Slides/...)
    /// with no fixed byte content a raw media PATCH can replace. Checked
    /// client-side, before the gate — this isn't a policy decision, the
    /// operation is simply nonsensical for this target.
    RefusedNativeDocument,
    /// The target has no parents this account can see and no `file_id`
    /// rule named it, so nothing could grant it (issue #1612).
    ///
    /// Mirrors `crate::drive::sheets::write::WriteResult`'s variant of the
    /// same name. Before #1612 this case fell into `Blocked { decided_by:
    /// None }`, which reads as "no rule matched, fix your rules" when no
    /// *folder* rule the operator could write would have helped — the
    /// latent gap [ADR-0073](../../docs/adrs/adr-0073.md) §4 flagged for
    /// `drive edit`.
    RefusedNoVisibleParents,
    /// The folder write-permission gate refused it.
    Blocked {
        /// The rule that decided the refusal, if any.
        decided_by: Option<DecidingRule>,
    },
    /// `files.update` (media) succeeded.
    Edited,
    /// An API/validation error.
    Failed {
        /// A human-readable summary of what failed.
        detail: String,
    },
}

impl EditResult {
    /// The request-log `status` string — mirrors
    /// `CreateResult`/`UploadResult`/`MoveResult::log_status`'s precedent.
    fn log_status(&self) -> &'static str {
        match self {
            Self::WouldEdit => "would-edit",
            Self::RefusedNativeDocument => "refused-native-document",
            Self::RefusedNoVisibleParents => "refused-no-visible-parents",
            Self::Blocked { .. } => "blocked",
            Self::Edited => "edited",
            Self::Failed { .. } => "failed",
        }
    }
}

/// The planned (and, after a real run, final) outcome of one `edit` call.
#[derive(Debug, Clone, Serialize)]
pub struct EditOutcome {
    /// The target file id.
    pub file_id: String,
    /// The file's name at the time of the attempt, once known (absent if
    /// the initial `files.get` itself failed).
    pub file_name: Option<String>,
    /// The folder the write-permission gate evaluated against — the
    /// target's resolved current parent, when the target has exactly one
    /// *and* the ancestor chain is what decided the verdict. `None` for an
    /// orphan target, a target refused before the gate ran
    /// (`RefusedNativeDocument`), a target with more than one current
    /// parent (no single folder to report), or a target decided by a
    /// `file_id` rule (issue #1612) — that short-circuits at depth −1
    /// before any parent is fetched, so no folder was evaluated even when
    /// the target has exactly one.
    pub resolved_folder_id: Option<String>,
    /// The result.
    pub result: EditResult,
}

impl JsonlSerialize for EditOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> Result<(), anyhow::Error> {
        write_scalar_jsonl(self, out)
    }
}

/// Replaces `opts.file_id`'s content with `opts.content`, gated by `rules`.
///
/// Every real (non-dry-run) attempt is logged; a `--dry-run` preview never
/// is, matching `create`/`upload`/`move`'s existing precedent.
pub async fn edit(
    client: &DriveClient,
    opts: &EditOptions,
    rules: &[FolderPermissionRule],
) -> EditOutcome {
    let started = Instant::now();
    let outcome = edit_inner(client, opts, rules).await;
    if !opts.dry_run {
        record_attempt(&outcome, started.elapsed());
    }
    outcome
}

async fn edit_inner(
    client: &DriveClient,
    opts: &EditOptions,
    rules: &[FolderPermissionRule],
) -> EditOutcome {
    let files_api = FilesApi::new(client);
    let target = match files_api.get_metadata(&opts.file_id).await {
        Ok(target) => target,
        Err(err) => {
            return EditOutcome {
                file_id: opts.file_id.clone(),
                file_name: None,
                resolved_folder_id: None,
                result: EditResult::Failed {
                    detail: err.to_string(),
                },
            }
        }
    };

    if target.is_google_native() {
        return EditOutcome {
            file_id: opts.file_id.clone(),
            file_name: Some(target.name),
            resolved_folder_id: None,
            result: EditResult::RefusedNativeDocument,
        };
    }

    // A `file_id` rule is consulted before the parents are, so a file
    // shared by link or email can still be granted (issue #1612).
    let evaluated = match folder_ancestry::resolve_decision_for_file_target(
        &files_api,
        &target,
        DriveOperation::Edit,
        rules,
    )
    .await
    {
        Ok(evaluated) => evaluated,
        Err(err) => {
            return EditOutcome {
                file_id: opts.file_id.clone(),
                file_name: Some(target.name),
                resolved_folder_id: None,
                result: EditResult::Failed {
                    detail: err.to_string(),
                },
            }
        }
    };

    if evaluated.source == folder_ancestry::DecisionSource::NoVisibleParents {
        return EditOutcome {
            file_id: opts.file_id.clone(),
            file_name: Some(target.name),
            resolved_folder_id: None,
            result: EditResult::RefusedNoVisibleParents,
        };
    }

    let folder_ancestry::FileTargetDecision {
        decision,
        resolved_folder_id,
        ..
    } = evaluated;

    if decision.verdict == write_gate::Verdict::Deny {
        return EditOutcome {
            file_id: opts.file_id.clone(),
            file_name: Some(target.name),
            resolved_folder_id,
            result: EditResult::Blocked {
                decided_by: decision.decided_by,
            },
        };
    }

    if opts.dry_run {
        return EditOutcome {
            file_id: opts.file_id.clone(),
            file_name: Some(target.name),
            resolved_folder_id,
            result: EditResult::WouldEdit,
        };
    }

    let result = match files_api
        .edit_content(&opts.file_id, &opts.content, &opts.content_type)
        .await
    {
        Ok(_) => EditResult::Edited,
        Err(err) => EditResult::Failed {
            detail: err.to_string(),
        },
    };
    EditOutcome {
        file_id: opts.file_id.clone(),
        file_name: Some(target.name),
        resolved_folder_id,
        result,
    }
}

/// Builds and writes the [`DriveMutationOutcome`] for one `edit` attempt.
fn record_attempt(outcome: &EditOutcome, duration: Duration) {
    let error = match &outcome.result {
        EditResult::Failed { detail } => Some(detail.clone()),
        _ => None,
    };
    let decided_by = match &outcome.result {
        EditResult::Blocked { decided_by } => decided_by.as_ref(),
        _ => None,
    };
    let decided_by = write_gate::decided_by_log_fields(decided_by);
    request_log::record_drive_mutation(DriveMutationOutcome {
        operation: "edit",
        file_id: outcome.file_id.clone(),
        file_name: outcome.file_name.clone().unwrap_or_default(),
        status: outcome.result.log_status().to_string(),
        added_principals: Vec::new(),
        removed_principals: Vec::new(),
        crosses_drive_boundary: false,
        resolved_folder_id: outcome.resolved_folder_id.clone(),
        decided_by_folder_id: decided_by.folder_id,
        decided_by_depth: decided_by.depth,
        decided_by_file_id: decided_by.file_id,
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

    fn opts(dry_run: bool) -> EditOptions {
        opts_for("file-1", dry_run)
    }

    fn opts_for(file_id: &str, dry_run: bool) -> EditOptions {
        EditOptions {
            file_id: file_id.to_string(),
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
    async fn allowed_target_succeeds_and_calls_edit_endpoint_once() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file("file-1", "text/plain", &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/upload/drive/v3/files/file-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "file-1", "name": "file-1",
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let outcome = edit(&client, &opts(false), &[allow_rule()]).await;
        assert!(matches!(outcome.result, EditResult::Edited));
    }

    #[tokio::test]
    async fn denied_target_refuses_with_zero_edit_calls() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file("file-1", "text/plain", &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        // No PATCH /upload/drive/v3/files/file-1 mock mounted — an
        // accidental edit attempt fails loudly with "no matching mock".

        let outcome = edit(&client, &opts(false), &[]).await;
        assert!(matches!(outcome.result, EditResult::Blocked { .. }));
    }

    #[tokio::test]
    async fn google_native_document_is_refused_before_any_gate_or_network_call() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file(
            "doc-1",
            "application/vnd.google-apps.document",
            &["parent-1"],
        )
        .mount(&server)
        .await;
        // Deliberately no mock for parent-1 (the gate never runs) and no
        // PATCH mock — proves the refusal happens strictly before the
        // ancestor-chain walk and before any mutating call, even though
        // an allow-everything rule set would otherwise permit it.
        let permissive_rule = FolderPermissionRule {
            folder_id: Some("parent-1".to_string()),
            file_id: None,
            recursive: true,
            allow: std::iter::once(DriveOperation::Edit).collect(),
            deny: std::collections::HashSet::default(),
        };

        let outcome = edit(&client, &opts_for("doc-1", false), &[permissive_rule]).await;
        assert!(matches!(outcome.result, EditResult::RefusedNativeDocument));
    }

    #[tokio::test]
    async fn orphan_file_is_refused_as_having_no_visible_parents() {
        // Before issue #1612 this reported `Blocked { decided_by: None }`,
        // i.e. "no rule matched" — true but unhelpful, since no *folder*
        // rule could ever match a target with no chain. It is now its own
        // outcome, whose message names the `file_id` rule that would work.
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file("orphan", "text/plain", &[]).mount(&server).await;

        let outcome = edit(&client, &opts_for("orphan", false), &[]).await;
        assert!(matches!(
            outcome.result,
            EditResult::RefusedNoVisibleParents
        ));
        assert_eq!(outcome.resolved_folder_id, None);
    }

    #[tokio::test]
    async fn edit_denies_when_any_current_parent_denies_even_if_another_allows() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file("file-1", "text/plain", &["allow-parent", "deny-parent"])
            .mount(&server)
            .await;
        mount_folder("allow-parent").mount(&server).await;
        mount_folder("deny-parent").mount(&server).await;
        let rules = [allow_rule()]; // only "parent-1" is allowed; neither
                                    // allow-parent nor deny-parent match it,
                                    // so both fall to the default deny —
                                    // this asserts deny-wins-across-parents
                                    // even when BOTH parents individually
                                    // resolve to the same (deny) verdict,
                                    // and the multi-parent path is exercised.

        let outcome = edit(&client, &opts(false), &rules).await;
        assert!(matches!(outcome.result, EditResult::Blocked { .. }));
        assert_eq!(
            outcome.resolved_folder_id, None,
            "multi-parent targets report no single resolved folder id"
        );
    }

    #[tokio::test]
    async fn ancestor_chain_fetch_failure_produces_failed_not_allow() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file("file-1", "text/plain", &["parent-1"])
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/parent-1"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("server error"))
            .mount(&server)
            .await;

        let outcome = edit(&client, &opts(false), &[allow_rule()]).await;
        assert!(
            matches!(outcome.result, EditResult::Failed { .. }),
            "a fetch failure must never silently fall through to Edited/WouldEdit"
        );
    }

    #[tokio::test]
    async fn insufficient_scope_403_surfaces_both_write_flags() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file("file-1", "text/plain", &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/upload/drive/v3/files/file-1"))
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

        let outcome = edit(&client, &opts(false), &[allow_rule()]).await;
        let EditResult::Failed { detail } = outcome.result else {
            panic!("expected Failed, got {:?}", outcome.result);
        };
        assert!(detail.contains("--write-file"), "{detail}");
        assert!(detail.contains("--write-full"), "{detail}");
    }

    #[tokio::test]
    async fn dry_run_never_calls_edit_endpoint() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file("file-1", "text/plain", &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;

        let outcome = edit(&client, &opts(true), &[allow_rule()]).await;
        assert!(matches!(outcome.result, EditResult::WouldEdit));
    }

    #[tokio::test]
    async fn dry_run_surfaces_the_same_blocked_reasoning_as_a_real_denied_run() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file("file-1", "text/plain", &["parent-1"])
            .expect(2)
            .mount(&server)
            .await;
        mount_folder("parent-1").expect(2).mount(&server).await;

        let dry_run_outcome = edit(&client, &opts(true), &[]).await;
        let real_outcome = edit(&client, &opts(false), &[]).await;
        assert!(matches!(dry_run_outcome.result, EditResult::Blocked { .. }));
        assert!(matches!(real_outcome.result, EditResult::Blocked { .. }));
    }

    // ── file-id rules (issue #1612) ────────────────────────────────────

    #[tokio::test]
    async fn a_file_rule_grants_a_target_with_no_visible_parents() {
        // The `drive edit` half of the shared-file gap: a binary file
        // shared by link arrives with no parents, so no folder rule could
        // ever apply to it.
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file("file-1", "text/plain", &[]).mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/upload/drive/v3/files/file-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "file-1", "name": "file-1"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let outcome = edit(
            &client,
            &opts(false),
            &[FolderPermissionRule::file("file-1").allowing([DriveOperation::Edit])],
        )
        .await;

        assert!(
            matches!(outcome.result, EditResult::Edited),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn a_parentless_target_is_now_refused_distinctly_not_generically_blocked() {
        // Before #1612 this reported `Blocked { decided_by: None }`, which
        // reads as "fix your rules" when no folder rule would have helped.
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file("file-1", "text/plain", &[]).mount(&server).await;

        let outcome = edit(&client, &opts(false), &[]).await;

        assert!(matches!(
            outcome.result,
            EditResult::RefusedNoVisibleParents
        ));
        assert_eq!(outcome.result.log_status(), "refused-no-visible-parents");
    }

    #[tokio::test]
    async fn a_file_deny_beats_an_allowing_parent_folder() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file("file-1", "text/plain", &["parent-1"])
            .mount(&server)
            .await;
        // No parent-1 mock and no PATCH mock: the file rule must decide
        // before either is reached.

        let outcome = edit(
            &client,
            &opts(false),
            &[
                allow_rule(),
                FolderPermissionRule::file("file-1").denying([DriveOperation::Edit]),
            ],
        )
        .await;

        match &outcome.result {
            EditResult::Blocked { decided_by } => {
                let rule = decided_by.as_ref().expect("a file rule decided this");
                assert_eq!(rule.kind_label(), "file");
                assert_eq!(rule.id(), "file-1");
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }
}
