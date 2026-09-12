//! Shared target-resolution-and-gate step for every Sheets-mutating engine
//! (`write.rs`'s `write`/`append`/`clear`, `structure.rs`'s
//! `add-sheet`/`rename-sheet`/`insert-rows`/`insert-columns`, and —
//! since ADR-0080's Phase 3 — `format.rs`, `protection.rs` and
//! `validation.rs`'s own verbs).
//!
//! Every one of these engines follows the identical linear shape up to this
//! point: fetch the target's Drive metadata, refuse a shortcut/non-
//! spreadsheet target before ever consulting policy, then resolve the
//! write-permission gate — a `file_id` rule first, then the target's
//! current parents, then "no visible parents" if neither decided it (issue
//! #1612). [`resolve`] is that shape, factored out once so it cannot
//! quietly drift between the five engines the way copied-by-hand code
//! eventually does.
//!
//! What is deliberately *not* shared: building the engine's own `Outcome`
//! type (`WriteOutcome`/`StructureOutcome` carry different fields — a
//! composed A1 `range`, for instance, that only `write.rs` has) and the
//! `Verdict::Deny` → `Blocked` branch (trivial, and each engine's `Blocked`
//! variant already differs). [`TargetGateOutcome`] is total and
//! panic-free — every branch names an outcome a caller can act on, so no
//! caller ever needs an `unreachable!()` to handle an "impossible"
//! combination.

use crate::drive::client::DriveClient;
use crate::drive::files_api::FilesApi;
use crate::drive::folder_ancestry::{self, DecisionSource};
use crate::drive::types::{classify_sheet_target, DriveFile, SheetTargetRefusal};
use crate::drive::write_gate::{Decision, DriveOperation, FolderPermissionRule};

/// The result of resolving a target and, if nothing refused it first,
/// evaluating the write-permission gate against it.
pub(crate) enum TargetGateOutcome {
    /// The `files.get` for the target itself failed — no target name is
    /// known, so a caller's outcome has nothing to attach it to.
    MetadataFetchFailed {
        /// The underlying error, formatted for display.
        detail: String,
    },
    /// The target resolved, but was refused before the gate could grant it
    /// — either [`classify_sheet_target`] refused it outright, or the gate
    /// itself found no `file_id` rule naming the target **and** no visible
    /// parent to walk.
    Refused {
        /// The target's own Drive metadata, for a caller's outcome.
        target: DriveFile,
        /// Which check refused it.
        refusal: SheetTargetRefusal,
    },
    /// The target resolved and passed classification, but resolving the
    /// ancestor chain for the gate failed.
    GateFetchFailed {
        /// The target's own Drive metadata, for a caller's outcome.
        target: DriveFile,
        /// The underlying error, formatted for display.
        detail: String,
    },
    /// The gate ran to completion — `decision.verdict` may still be `Deny`;
    /// that branch is left to each caller, since every engine's `Blocked`
    /// variant differs.
    Gated {
        /// The target's own Drive metadata, for a caller's outcome.
        target: DriveFile,
        /// The gate's verdict and, when a rule decided it, which one.
        decision: Decision,
        /// The single folder the gate evaluated against, when the target
        /// had exactly one parent.
        resolved_folder_id: Option<String>,
        /// Whether the deciding rule (or, for a legacy multi-parent target,
        /// any parent's own rule) requires a `--lease` token for a
        /// mutating write (ADR-0080 §1/§9/§13). Already folds in every
        /// parent's own requirement — see
        /// `folder_ancestry::FileTargetDecision::requires_lease`'s doc
        /// comment — so a caller must use this rather than re-deriving it
        /// from `decision.decided_by` alone.
        requires_lease: bool,
    },
}

/// Fetches `spreadsheet_id`'s Drive metadata, classifies it, and — if
/// nothing refused it first — resolves `operation` against it: a `file_id`
/// rule naming the target directly, then its current parents.
///
/// ADR-0071 §3's highest-priority invariant applies to both fetches here
/// exactly as it does in each engine that used to inline this: a failure is
/// always [`TargetGateOutcome::MetadataFetchFailed`]/
/// [`TargetGateOutcome::GateFetchFailed`], never a silent allow.
pub(crate) async fn resolve(
    drive: &DriveClient,
    spreadsheet_id: &str,
    operation: DriveOperation,
    rules: &[FolderPermissionRule],
) -> TargetGateOutcome {
    let files_api = FilesApi::new(drive);
    let target = match files_api.get_metadata(spreadsheet_id).await {
        Ok(target) => target,
        Err(err) => {
            return TargetGateOutcome::MetadataFetchFailed {
                detail: err.to_string(),
            }
        }
    };

    if let Err(refusal) = classify_sheet_target(&target) {
        return TargetGateOutcome::Refused { target, refusal };
    }

    match folder_ancestry::resolve_decision_for_file_target(&files_api, &target, operation, rules)
        .await
    {
        Ok(evaluated) if evaluated.source == DecisionSource::NoVisibleParents => {
            // Only reached once the `file_id` lookup has come up empty —
            // reporting it sooner would refuse a target an explicit rule
            // had already granted.
            TargetGateOutcome::Refused {
                target,
                refusal: SheetTargetRefusal::NoVisibleParents,
            }
        }
        Ok(evaluated) => TargetGateOutcome::Gated {
            target,
            decision: evaluated.decision,
            resolved_folder_id: evaluated.resolved_folder_id,
            requires_lease: evaluated.requires_lease,
        },
        Err(err) => TargetGateOutcome::GateFetchFailed {
            target,
            detail: err.to_string(),
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::types::GOOGLE_SHEET_MIME_TYPE;
    use crate::drive::write_gate::Verdict;
    use crate::utils::secret::Secret;
    use std::collections::HashSet;

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
                    "access_token": "test-token", "expires_in": 3600,
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
        let parents: Vec<&str> = parents.to_vec();
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!("/drive/v3/files/{id}")))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": id, "name": id, "mimeType": mime_type, "parents": parents,
                })),
            )
    }

    fn mount_folder(id: &str) -> wiremock::Mock {
        mount_file(id, "application/vnd.google-apps.folder", &[])
    }

    fn allow_rule(folder: &str) -> FolderPermissionRule {
        FolderPermissionRule {
            folder_id: Some(folder.to_string()),
            file_id: None,
            recursive: true,
            allow: std::iter::once(DriveOperation::SheetsWrite).collect(),
            deny: HashSet::default(),
            require_lease: true,
        }
    }

    #[tokio::test]
    async fn a_metadata_fetch_failure_is_reported_distinctly() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/missing"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;

        let outcome = resolve(&client, "missing", DriveOperation::SheetsWrite, &[]).await;
        assert!(matches!(
            outcome,
            TargetGateOutcome::MetadataFetchFailed { .. }
        ));
    }

    #[tokio::test]
    async fn a_shortcut_is_refused_before_the_gate_runs() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file(
            "sheet-1",
            "application/vnd.google-apps.shortcut",
            &["parent-1"],
        )
        .mount(&server)
        .await;
        // No mock for parent-1: proves the gate never runs.
        let outcome = resolve(
            &client,
            "sheet-1",
            DriveOperation::SheetsWrite,
            &[allow_rule("parent-1")],
        )
        .await;
        assert!(matches!(
            outcome,
            TargetGateOutcome::Refused {
                refusal: SheetTargetRefusal::Shortcut,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn a_gate_ancestor_fetch_failure_is_reported_distinctly_from_a_metadata_failure() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/parent-1"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let outcome = resolve(
            &client,
            "sheet-1",
            DriveOperation::SheetsWrite,
            &[allow_rule("parent-1")],
        )
        .await;
        assert!(matches!(outcome, TargetGateOutcome::GateFetchFailed { .. }));
    }

    #[tokio::test]
    async fn a_resolvable_target_reaches_gated_with_the_right_verdict() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;

        let outcome = resolve(
            &client,
            "sheet-1",
            DriveOperation::SheetsWrite,
            &[allow_rule("parent-1")],
        )
        .await;
        let TargetGateOutcome::Gated {
            target,
            decision,
            resolved_folder_id,
            ..
        } = outcome
        else {
            panic!("expected Gated");
        };
        assert_eq!(target.id, "sheet-1");
        assert_eq!(decision.verdict, Verdict::Allow);
        assert_eq!(resolved_folder_id, Some("parent-1".to_string()));
    }

    #[tokio::test]
    async fn a_target_with_no_visible_parents_and_no_file_rule_is_refused() {
        // An empty `parents` list with no `file_id` rule naming the target
        // is a `Refused`, never a `Gated` with an empty chain — the gate
        // has no ancestor to evaluate at all.
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &[])
            .mount(&server)
            .await;

        let outcome = resolve(&client, "sheet-1", DriveOperation::SheetsWrite, &[]).await;
        assert!(matches!(
            outcome,
            TargetGateOutcome::Refused {
                refusal: SheetTargetRefusal::NoVisibleParents,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn a_file_rule_grants_a_target_with_no_visible_parents() {
        // The case issue #1612 exists for: before file rules there was no
        // rule an operator could write that would permit this at all.
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &[])
            .mount(&server)
            .await;

        let outcome = resolve(
            &client,
            "sheet-1",
            DriveOperation::SheetsWrite,
            &[FolderPermissionRule::file("sheet-1").allowing([DriveOperation::SheetsWrite])],
        )
        .await;
        let TargetGateOutcome::Gated {
            decision,
            resolved_folder_id,
            ..
        } = outcome
        else {
            panic!("expected Gated");
        };
        assert_eq!(decision.verdict, Verdict::Allow);
        assert_eq!(resolved_folder_id, None);
    }

    #[tokio::test]
    async fn a_target_with_no_matching_rule_reaches_gated_with_the_default_policy() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;

        // Deliberately no rules: the gate must still run to completion
        // (`Gated`, not skipped), landing on the bare default policy.
        let outcome = resolve(&client, "sheet-1", DriveOperation::SheetsWrite, &[]).await;
        let TargetGateOutcome::Gated {
            decision,
            resolved_folder_id,
            ..
        } = outcome
        else {
            panic!("expected Gated");
        };
        assert_eq!(decision.verdict, Verdict::Deny);
        assert_eq!(decision.decided_by, None);
        assert_eq!(resolved_folder_id, Some("parent-1".to_string()));
    }
}
