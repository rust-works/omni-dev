//! Guarded Slides replacement (ADR-0093).
//!
//! Validate -> metadata/MIME -> permission gate -> snapshot/revision/filter ->
//! preview/dry run -> optional Drive lease -> one revision-controlled mutation.
//! Preview counts never suppress writes; notes/templates are excluded by explicit
//! ordinary-slide page IDs. Prose is never included in mutation log records.

use super::api::{is_stale_revision, SlidesApi};
use super::client::SlidesClient;
use super::read::{flatten, selected_slide_ids};
use super::write_types::SlidesRequest;
use crate::cli::drive::format::{write_scalar_jsonl, JsonlSerialize};
use crate::drive::client::DriveClient;
use crate::drive::files_api::FilesApi;
use crate::drive::folder_ancestry;
use crate::drive::lease::check::{
    conclude_native_leased_write, gate_optional_leased_write, FromLeaseRefusal, LeaseGateRefusal,
    LeasedWrite,
};
use crate::drive::types::{GOOGLE_SHORTCUT_MIME_TYPE, GOOGLE_SLIDES_MIME_TYPE};
use crate::drive::write_gate::{self, DecidingRule, DriveOperation, FolderPermissionRule};
use crate::request_log::{self, DriveMutationOutcome};
use serde::Serialize;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// One literal text replacement attempt.
#[derive(Debug, Clone)]
pub struct WriteOptions {
    /// Presentation identity.
    pub presentation_id: String,
    /// Literal text to find.
    pub search: String,
    /// Replacement text (may be empty).
    pub replace: String,
    /// Whether to match case.
    pub match_case: bool,
    /// Ordinary slide IDs; empty selects all ordinary slides.
    pub slides: Vec<String>,
    /// Preview without mutation or ledger consumption.
    pub dry_run: bool,
    /// Drive lease token when required by the deciding rule.
    pub lease_token: Option<String>,
    /// Lease ledger path.
    pub ledger_path: PathBuf,
}
impl WriteOptions {
    fn validate(&self) -> Result<(), String> {
        if self.search.is_empty() {
            return Err("--search cannot be empty".into());
        }
        if self.slides.iter().any(String::is_empty) {
            return Err("--slide cannot be empty".into());
        }
        Ok(())
    }
}

/// Result of a replacement or preview.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum WriteResult {
    /// Snapshot estimate, display-only.
    WouldReplace {
        /// Non-overlapping matches per text object.
        occurrences: usize,
    },
    /// Successful mutation with authoritative server count.
    Replaced {
        /// Server-reported count, including proto3 omitted zero.
        occurrences_changed: i64,
    },
    /// HTTP success means applied, even if its response cannot be decoded.
    AppliedResponseUnreadable {
        /// Parsing failure; do not retry automatically.
        detail: String,
    },
    /// Metadata is not a presentation.
    RefusedNotAPresentation {
        /// Actual MIME type.
        mime_type: String,
    },
    /// Shortcuts are never followed.
    RefusedShortcut,
    /// No visible parent and no file-specific grant.
    RefusedNoVisibleParents,
    /// No nonempty editor revision token to assert.
    RefusedNoRevisionId,
    /// Permission gate refusal, before any Slides read.
    Blocked {
        /// Rule that refused, if any.
        decided_by: Option<DecidingRule>,
    },
    /// Required Drive lease absent.
    RefusedNoLease,
    /// Lease expired or unknown.
    RefusedLeaseExpired,
    /// Lease belongs to another file.
    RefusedLeaseWrongFile,
    /// Drive version changed since lease acquisition.
    RefusedLeaseStale,
    /// Revision changed after the snapshot read; mutation refused atomically.
    StaleRevision {
        /// Revision asserted.
        required_revision_id: String,
        /// Server diagnostic.
        detail: String,
    },
    /// Other validation or API failure.
    Failed {
        /// Diagnostic.
        detail: String,
    },
}
impl FromLeaseRefusal for WriteResult {
    fn from_no_lease() -> Self {
        Self::RefusedNoLease
    }
    fn from_lease_expired() -> Self {
        Self::RefusedLeaseExpired
    }
    fn from_lease_wrong_file() -> Self {
        Self::RefusedLeaseWrongFile
    }
    fn from_lease_stale() -> Self {
        Self::RefusedLeaseStale
    }
    fn from_lease_failed(detail: String) -> Self {
        Self::Failed { detail }
    }
}
impl WriteResult {
    fn log_status(&self) -> &'static str {
        match self {
            Self::WouldReplace { .. } => "would-replace",
            Self::Replaced { .. } => "replaced",
            Self::AppliedResponseUnreadable { .. } => "applied-response-unreadable",
            Self::RefusedNotAPresentation { .. } => "refused-not-a-presentation",
            Self::RefusedShortcut => "refused-shortcut",
            Self::RefusedNoVisibleParents => "refused-no-visible-parents",
            Self::RefusedNoRevisionId => "refused-no-revision-id",
            Self::Blocked { .. } => "blocked",
            Self::RefusedNoLease => LeaseGateRefusal::NoLease.log_status(),
            Self::RefusedLeaseExpired => LeaseGateRefusal::Expired.log_status(),
            Self::RefusedLeaseWrongFile => LeaseGateRefusal::WrongFile.log_status(),
            Self::RefusedLeaseStale => LeaseGateRefusal::Stale.log_status(),
            Self::StaleRevision { .. } => "stale-revision",
            Self::Failed { .. } => "failed",
        }
    }
}

/// Serializable outcome of one invocation.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WriteOutcome {
    /// Presentation acted on.
    pub presentation_id: String,
    /// Name from Drive metadata.
    pub file_name: Option<String>,
    /// Folder evaluated by the gate.
    pub resolved_folder_id: Option<String>,
    /// Revision asserted by the mutation.
    pub required_revision_id: Option<String>,
    /// Applied, previewed or refused result.
    pub result: WriteResult,
}
impl JsonlSerialize for WriteOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> anyhow::Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Counts non-overlapping literal matches, for display only.
/// Unicode case folding may differ from Google's server implementation.
pub fn count_occurrences(text: &str, search: &str, match_case: bool) -> usize {
    if search.is_empty() {
        return 0;
    }
    if match_case {
        text.matches(search).count()
    } else {
        text.to_lowercase().matches(&search.to_lowercase()).count()
    }
}

/// Runs the guarded engine and logs every non-dry-run attempt.
pub async fn write(
    drive: &DriveClient,
    slides: &SlidesClient,
    opts: &WriteOptions,
    rules: &[FolderPermissionRule],
) -> WriteOutcome {
    let started = Instant::now();
    let outcome = write_inner(drive, slides, opts, rules).await;
    if !opts.dry_run {
        record_attempt(&outcome, started.elapsed());
    }
    outcome
}
async fn write_inner(
    drive: &DriveClient,
    slides: &SlidesClient,
    opts: &WriteOptions,
    rules: &[FolderPermissionRule],
) -> WriteOutcome {
    let bare = |result| WriteOutcome {
        presentation_id: opts.presentation_id.clone(),
        file_name: None,
        resolved_folder_id: None,
        required_revision_id: None,
        result,
    };

    if let Err(detail) = opts.validate() {
        return bare(WriteResult::Failed { detail });
    }

    let files_api = FilesApi::new(drive);
    let target = match files_api.get_metadata(&opts.presentation_id).await {
        Ok(target) => target,
        Err(err) => {
            return bare(WriteResult::Failed {
                detail: err.to_string(),
            })
        }
    };

    let with_target = |result| WriteOutcome {
        presentation_id: opts.presentation_id.clone(),
        file_name: Some(target.name.clone()),
        resolved_folder_id: None,
        required_revision_id: None,
        result,
    };

    if target.mime_type == GOOGLE_SHORTCUT_MIME_TYPE {
        return with_target(WriteResult::RefusedShortcut);
    }
    if target.mime_type != GOOGLE_SLIDES_MIME_TYPE {
        return with_target(WriteResult::RefusedNotAPresentation {
            mime_type: target.mime_type.clone(),
        });
    }

    let evaluated = match folder_ancestry::resolve_decision_for_file_target(
        &files_api,
        &target,
        DriveOperation::SlidesWrite,
        rules,
    )
    .await
    {
        Ok(evaluated) => evaluated,
        Err(err) => {
            return with_target(WriteResult::Failed {
                detail: err.to_string(),
            })
        }
    };

    if evaluated.source == folder_ancestry::DecisionSource::NoVisibleParents {
        return with_target(WriteResult::RefusedNoVisibleParents);
    }

    let folder_ancestry::FileTargetDecision {
        decision,
        resolved_folder_id,
        requires_lease,
        ..
    } = evaluated;

    let gated = |result, revision: Option<String>| WriteOutcome {
        presentation_id: opts.presentation_id.clone(),
        file_name: Some(target.name.clone()),
        resolved_folder_id: resolved_folder_id.clone(),
        required_revision_id: revision,
        result,
    };

    if decision.verdict == write_gate::Verdict::Deny {
        return gated(
            WriteResult::Blocked {
                decided_by: decision.decided_by,
            },
            None,
        );
    }

    let api = SlidesApi::new(slides);
    let presentation = match api.get_presentation(&opts.presentation_id).await {
        Ok(presentation) => presentation,
        Err(err) => {
            return gated(
                WriteResult::Failed {
                    detail: err.to_string(),
                },
                None,
            )
        }
    };

    let Some(revision_id) = presentation.revision_id.clone().filter(|id| !id.is_empty()) else {
        return gated(WriteResult::RefusedNoRevisionId, None);
    };

    let pages = match selected_slide_ids(&presentation, &opts.slides) {
        Ok(pages) if !pages.is_empty() => pages,
        Ok(_) => {
            return gated(
                WriteResult::Failed {
                    detail: "No ordinary slides to replace text on".into(),
                },
                Some(revision_id),
            )
        }
        Err(err) => {
            return gated(
                WriteResult::Failed {
                    detail: err.to_string(),
                },
                Some(revision_id),
            )
        }
    };
    let preview = WriteResult::WouldReplace {
        occurrences: flatten(&presentation, &pages, false)
            .iter()
            .map(|element| count_occurrences(&element.text, &opts.search, opts.match_case))
            .sum(),
    };

    if opts.dry_run {
        return gated(preview, Some(revision_id));
    }

    let leased = LeasedWrite {
        log_prefix: "drive slides write",
        operation: "slides-replace",
        ledger_path: &opts.ledger_path,
        file_id: &opts.presentation_id,
    };
    let lease_grant = match gate_optional_leased_write(
        leased,
        &files_api,
        requires_lease,
        opts.lease_token.as_deref(),
    )
    .await
    {
        Ok(grant) => grant,
        Err(err) => return gated(err.into_result(), Some(revision_id)),
    };

    let result = match conclude_native_leased_write(
        leased,
        &lease_grant,
        &files_api,
        api.batch_update(
            &opts.presentation_id,
            SlidesRequest::replace_all_text(&opts.search, &opts.replace, opts.match_case, &pages),
            &revision_id,
        )
        .await,
        |_| "Slides batchUpdate failed; inspect the command outcome for details".to_string(),
    )
    .await
    {
        Ok(Ok(response)) => WriteResult::Replaced {
            occurrences_changed: response.occurrences_changed_for_replace(),
        },
        Ok(Err(detail)) => WriteResult::AppliedResponseUnreadable { detail },
        Err(err) => {
            let detail = err.to_string();
            if is_stale_revision(&err) {
                WriteResult::StaleRevision {
                    required_revision_id: revision_id.clone(),
                    detail,
                }
            } else {
                WriteResult::Failed { detail }
            }
        }
    };
    drop(lease_grant);
    gated(result, Some(revision_id))
}

fn record_attempt(outcome: &WriteOutcome, duration: Duration) {
    let decided_by = match &outcome.result {
        WriteResult::Blocked { decided_by } => decided_by.as_ref(),
        _ => None,
    };
    let fields = write_gate::decided_by_log_fields(decided_by);
    let occurrences_changed = match outcome.result {
        WriteResult::Replaced {
            occurrences_changed,
        } => Some(occurrences_changed),
        _ => None,
    };
    // Server diagnostics may echo request prose. Keep them out of logs; the
    // interactive/structured outcome still carries the diagnostic for support.
    request_log::record_drive_mutation(DriveMutationOutcome {
        operation: "slides-replace",
        file_id: outcome.presentation_id.clone(),
        file_name: outcome.file_name.clone().unwrap_or_default(),
        status: outcome.result.log_status().into(),
        resolved_folder_id: outcome.resolved_folder_id.clone(),
        decided_by_folder_id: fields.folder_id,
        decided_by_depth: fields.depth,
        decided_by_file_id: fields.file_id,
        occurrences_changed,
        required_revision_id: outcome.required_revision_id.clone(),
        duration,
        ..Default::default()
    });
}

/// Describes one attempt without exposing searched/replacement prose.
pub fn describe(outcome: &WriteOutcome) -> String {
    let name = outcome
        .file_name
        .as_deref()
        .unwrap_or(&outcome.presentation_id);
    match &outcome.result {
        WriteResult::WouldReplace { occurrences } => format!("Would replace: {occurrences} estimated occurrence(s) in '{name}' (ordinary slides only)"),
        WriteResult::Replaced { occurrences_changed } => format!("Replaced: {occurrences_changed} occurrence(s) in '{name}'"),
        WriteResult::AppliedResponseUnreadable { detail } => format!("Applied: '{name}', but the response was unreadable; inspect the presentation before retrying: {detail}"),
        WriteResult::RefusedNotAPresentation { mime_type } => format!("Refused: '{name}' is not a Google Slides presentation (mimeType: {mime_type})"),
        WriteResult::RefusedShortcut => format!("Refused: '{name}' is a shortcut; use the target presentation id instead"),
        WriteResult::RefusedNoVisibleParents => format!("Refused: '{name}' has no visible parent; grant its file_id the slides-write operation in write_permissions.rules"),
        WriteResult::RefusedNoRevisionId => format!("Refused: '{name}' returned no revision id; request edit access or check the account in use"),
        WriteResult::Blocked { decided_by } => match decided_by {
            Some(rule) => format!("Blocked: '{name}' — refused by rule on {} {}{}", rule.kind_label(), rule.id(), rule.depth_suffix()),
            None => format!("Blocked: '{name}' — refused by default policy (no matching rule)"),
        },
        WriteResult::RefusedNoLease => LeaseGateRefusal::NoLease.describe_line(&outcome.presentation_id, &format!("'{name}'")).unwrap_or_default(),
        WriteResult::RefusedLeaseExpired => LeaseGateRefusal::Expired.describe_line(&outcome.presentation_id, &format!("'{name}'")).unwrap_or_default(),
        WriteResult::RefusedLeaseWrongFile => LeaseGateRefusal::WrongFile.describe_line(&outcome.presentation_id, &format!("'{name}'")).unwrap_or_default(),
        WriteResult::RefusedLeaseStale => LeaseGateRefusal::Stale.describe_line(&outcome.presentation_id, &format!("'{name}'")).unwrap_or_default(),
        WriteResult::StaleRevision { .. } => format!("Refused: '{name}' changed since it was read; nothing was written. Re-run against the current revision."),
        WriteResult::Failed { detail } => format!("Failed: '{name}': {detail}"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::slides::client::SLIDES_API_URL;
    use crate::drive::test_support::seed_lease;
    use crate::drive::types::GOOGLE_DOC_MIME_TYPE;
    use crate::test_support::env::MapEnv;
    use crate::utils::secret::Secret;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn clients(server: &MockServer) -> (DriveClient, SlidesClient) {
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"access_token":"test", "expires_in":3600})),
            )
            .mount(server)
            .await;
        let credentials = DriveCredentials {
            client_id: "client".into(),
            client_secret: Secret::new("secret"),
            refresh_token: Secret::new("refresh"),
            scope: DriveGrantedScopes::READONLY,
        };
        let mut drive = DriveClient::new(&server.uri(), &credentials).unwrap();
        crate::drive::client::test_support::replace_session(
            &mut drive,
            &credentials,
            &format!("{}/token", server.uri()),
        );
        let slides = SlidesClient::from_drive_client_with(
            &MapEnv::new().with(SLIDES_API_URL, &server.uri()),
            &drive,
        )
        .unwrap();
        (drive, slides)
    }
    fn opts() -> WriteOptions {
        WriteOptions {
            presentation_id: "p1".into(),
            search: "Q3".into(),
            replace: "Q4".into(),
            match_case: true,
            slides: vec![],
            dry_run: false,
            lease_token: None,
            ledger_path: PathBuf::new(),
        }
    }
    fn rule(require_lease: bool) -> FolderPermissionRule {
        FolderPermissionRule {
            file_id: Some("p1".into()),
            folder_id: None,
            recursive: false,
            allow: std::iter::once(DriveOperation::SlidesWrite).collect(),
            deny: std::collections::HashSet::default(),
            require_lease,
        }
    }
    async fn mount_file(server: &MockServer, mime: &str) {
        Mock::given(method("GET"))
            .and(path("/drive/v3/files/p1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"id":"p1","name":"Deck","mimeType":mime,"version":"1"}),
            ))
            .mount(server)
            .await;
    }
    fn text_element(id: &str, text: &str) -> serde_json::Value {
        serde_json::json!({"objectId":id,"shape":{"text":{"textElements":[{"textRun":{"content":text}}]}}})
    }
    fn presentation(revision: Option<&str>) -> serde_json::Value {
        let mut p = serde_json::json!({"presentationId":"p1", "slides":[
            {"objectId":"s1","pageElements":[text_element("e1","Q3 and q3")],"slideProperties":{"notesPage":{"objectId":"n1","pageElements":[text_element("note","Q3")]}}},
            {"objectId":"s2","pageElements":[text_element("e2","Q3")]}
        ], "masters":[{"objectId":"master","pageElements":[text_element("m","Q3")]}], "layouts":[{"objectId":"layout","pageElements":[text_element("l","Q3")]}]});
        if let Some(revision) = revision {
            p["revisionId"] = revision.into();
        }
        p
    }
    async fn mount_presentation(server: &MockServer, p: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path("/v1/presentations/p1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(p))
            .expect(1)
            .mount(server)
            .await;
    }
    fn batch() -> wiremock::MockBuilder {
        Mock::given(method("POST")).and(path("/v1/presentations/p1:batchUpdate"))
    }
    async fn forbid_batch(server: &MockServer) {
        batch()
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(server)
            .await;
    }
    fn expected_body(pages: &[&str], case: bool) -> serde_json::Value {
        serde_json::json!({"requests":[{"replaceAllText":{"containsText":{"text":"Q3","matchCase":case},"replaceText":"Q4","pageObjectIds":pages}}],"writeControl":{"requiredRevisionId":"fresh-revision"}})
    }
    async fn ready(server: &MockServer) -> (DriveClient, SlidesClient) {
        let clients = clients(server).await;
        mount_file(server, GOOGLE_SLIDES_MIME_TYPE).await;
        mount_presentation(server, presentation(Some("fresh-revision"))).await;
        clients
    }

    #[tokio::test]
    async fn mime_and_shortcut_refusals_precede_gate_and_slides_requests() {
        for (mime, result) in [
            (
                GOOGLE_DOC_MIME_TYPE,
                WriteResult::RefusedNotAPresentation {
                    mime_type: GOOGLE_DOC_MIME_TYPE.into(),
                },
            ),
            (GOOGLE_SHORTCUT_MIME_TYPE, WriteResult::RefusedShortcut),
        ] {
            let server = MockServer::start().await;
            let (drive, slides) = clients(&server).await;
            mount_file(&server, mime).await;
            Mock::given(path("/v1/presentations/p1"))
                .respond_with(ResponseTemplate::new(500))
                .expect(0)
                .mount(&server)
                .await;
            forbid_batch(&server).await;
            assert_eq!(
                write_inner(&drive, &slides, &opts(), &[rule(false)])
                    .await
                    .result,
                result
            );
        }
    }
    #[tokio::test]
    async fn denied_target_makes_zero_slides_calls() {
        let server = MockServer::start().await;
        let (drive, slides) = clients(&server).await;
        mount_file(&server, GOOGLE_SLIDES_MIME_TYPE).await;
        Mock::given(path("/v1/presentations/p1"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        forbid_batch(&server).await;
        let mut deny = rule(false);
        deny.allow.clear();
        deny.deny.insert(DriveOperation::SlidesWrite);
        assert!(matches!(
            write_inner(&drive, &slides, &opts(), &[deny]).await.result,
            WriteResult::Blocked { .. }
        ));
    }
    #[tokio::test]
    async fn validation_precedes_every_network_call() {
        let server = MockServer::start().await;
        let (drive, slides) = clients(&server).await;
        for invalid in [
            WriteOptions {
                search: String::new(),
                ..opts()
            },
            WriteOptions {
                slides: vec![String::new()],
                ..opts()
            },
        ] {
            assert!(matches!(
                write_inner(&drive, &slides, &invalid, &[rule(false)])
                    .await
                    .result,
                WriteResult::Failed { .. }
            ));
        }
        assert!(server.received_requests().await.unwrap().is_empty());
    }
    #[tokio::test]
    async fn missing_or_empty_revision_is_refused() {
        for revision in [None, Some("")] {
            let server = MockServer::start().await;
            let (drive, slides) = clients(&server).await;
            mount_file(&server, GOOGLE_SLIDES_MIME_TYPE).await;
            mount_presentation(&server, presentation(revision)).await;
            forbid_batch(&server).await;
            assert_eq!(
                write_inner(&drive, &slides, &opts(), &[rule(false)])
                    .await
                    .result,
                WriteResult::RefusedNoRevisionId
            );
        }
    }
    #[tokio::test]
    async fn one_request_asserts_fresh_revision_and_explicit_ordinary_slide_ids() {
        let server = MockServer::start().await;
        let (drive, slides) = ready(&server).await;
        batch()
            .and(body_json(expected_body(&["s1", "s2"], true)))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"replies":[{"replaceAllText":{"occurrencesChanged":2}}]}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        let result = write_inner(&drive, &slides, &opts(), &[rule(false)]).await;
        assert_eq!(
            result.result,
            WriteResult::Replaced {
                occurrences_changed: 2
            }
        );
        assert_eq!(
            result.required_revision_id.as_deref(),
            Some("fresh-revision")
        );
    }
    #[tokio::test]
    async fn slide_filter_and_case_option_reach_the_wire() {
        let server = MockServer::start().await;
        let (drive, slides) = ready(&server).await;
        batch()
            .and(body_json(expected_body(&["s2"], false)))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"replies":[{"replaceAllText":{}}]})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let opts = WriteOptions {
            slides: vec!["s2".into(), "s2".into()],
            match_case: false,
            ..opts()
        };
        assert_eq!(
            write_inner(&drive, &slides, &opts, &[rule(false)])
                .await
                .result,
            WriteResult::Replaced {
                occurrences_changed: 0
            }
        );
    }
    #[tokio::test]
    async fn dry_run_counts_only_selected_slide_text_and_never_requires_lease() {
        for (filter, case, count) in [
            (vec![], true, 2),
            (vec![], false, 3),
            (vec!["s2".into()], true, 1),
        ] {
            let server = MockServer::start().await;
            let (drive, slides) = ready(&server).await;
            forbid_batch(&server).await;
            let opts = WriteOptions {
                dry_run: true,
                slides: filter,
                match_case: case,
                ..opts()
            };
            assert_eq!(
                write_inner(&drive, &slides, &opts, &[rule(true)])
                    .await
                    .result,
                WriteResult::WouldReplace { occurrences: count }
            );
        }
    }
    #[tokio::test]
    async fn preview_joins_runs_but_never_matches_across_objects_or_cells() {
        let server = MockServer::start().await;
        let (drive, slides) = clients(&server).await;
        mount_file(&server, GOOGLE_SLIDES_MIME_TYPE).await;
        mount_presentation(&server, serde_json::json!({"revisionId":"r","slides":[{"objectId":"s","pageElements":[text_element("a","Q"),text_element("b","3"),
            {"objectId":"c","shape":{"text":{"textElements":[{"textRun":{"content":"Q"}},{"textRun":{"content":"3"}}]}}},
            {"objectId":"t","table":{"tableRows":[{"tableCells":[{"text":{"textElements":[{"textRun":{"content":"Q"}}]}},{"text":{"textElements":[{"textRun":{"content":"3"}}]}}]}]}}
        ]}]})).await;
        forbid_batch(&server).await;
        assert_eq!(
            write_inner(
                &drive,
                &slides,
                &WriteOptions {
                    dry_run: true,
                    ..opts()
                },
                &[rule(true)]
            )
            .await
            .result,
            WriteResult::WouldReplace { occurrences: 1 }
        );
    }
    #[tokio::test]
    async fn a_zero_estimate_still_sends_the_mutation() {
        let server = MockServer::start().await;
        let (drive, slides) = clients(&server).await;
        mount_file(&server, GOOGLE_SLIDES_MIME_TYPE).await;
        mount_presentation(&server, serde_json::json!({"revisionId":"fresh-revision","slides":[{"objectId":"s1","pageElements":[]}]})).await;
        batch()
            .and(body_json(expected_body(&["s1"], true)))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"replies":[{}]})),
            )
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            write_inner(&drive, &slides, &opts(), &[rule(false)])
                .await
                .result,
            WriteResult::Replaced {
                occurrences_changed: 0
            }
        );
    }
    #[tokio::test]
    async fn invalid_filters_and_empty_decks_never_mutate() {
        for filter in ["notes", "master", "unknown", "empty-deck"] {
            let server = MockServer::start().await;
            let (drive, slides) = clients(&server).await;
            mount_file(&server, GOOGLE_SLIDES_MIME_TYPE).await;
            let mut p = presentation(Some("r"));
            let slides_filter = if filter == "empty-deck" {
                p["slides"] = serde_json::json!([]);
                vec![]
            } else {
                vec![filter.into()]
            };
            mount_presentation(&server, p).await;
            forbid_batch(&server).await;
            assert!(matches!(
                write_inner(
                    &drive,
                    &slides,
                    &WriteOptions {
                        slides: slides_filter,
                        ..opts()
                    },
                    &[rule(false)]
                )
                .await
                .result,
                WriteResult::Failed { .. }
            ));
        }
    }
    #[tokio::test]
    async fn stale_revision_is_distinct_from_unrelated_bad_request() {
        for (message, stale) in [
            (
                "The required revision ID 'r' does not match the latest revision.",
                true,
            ),
            ("Unknown page object ID", false),
        ] {
            let server = MockServer::start().await;
            let (drive, slides) = ready(&server).await;
            batch().respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({"error":{"code":400,"status":"INVALID_ARGUMENT","message":message}}))).expect(1).mount(&server).await;
            let result = write_inner(&drive, &slides, &opts(), &[rule(false)])
                .await
                .result;
            assert_eq!(matches!(result, WriteResult::StaleRevision { .. }), stale);
            if !stale {
                assert!(matches!(result, WriteResult::Failed { .. }));
            }
        }
    }
    #[tokio::test]
    async fn unreadable_success_is_applied_and_concludes_the_lease_audit() {
        let server = MockServer::start().await;
        let (drive, slides) = ready(&server).await;
        batch()
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("ledger.jsonl");
        let lease_token = Some(seed_lease(&ledger_path, "p1", "1"));
        let audit = crate::test_support::AuditLogGuard::redirect(dir.path());
        let outcome = write_inner(
            &drive,
            &slides,
            &WriteOptions {
                lease_token,
                ledger_path,
                ..opts()
            },
            &[rule(true)],
        )
        .await;
        assert!(matches!(
            outcome.result,
            WriteResult::AppliedResponseUnreadable { .. }
        ));
        assert_eq!(audit.verdicts(), ["pending", "allowed"]);
    }
    #[tokio::test]
    async fn drive_lease_refusals_never_mutate_even_when_optional() {
        for (mode, expected) in [
            ("missing", WriteResult::RefusedNoLease),
            ("expired", WriteResult::RefusedLeaseExpired),
            ("wrong-file", WriteResult::RefusedLeaseWrongFile),
            ("stale", WriteResult::RefusedLeaseStale),
            ("optional-stale", WriteResult::RefusedLeaseStale),
        ] {
            let server = MockServer::start().await;
            let (drive, slides) = ready(&server).await;
            forbid_batch(&server).await;
            let dir = tempfile::tempdir().unwrap();
            let ledger_path = dir.path().join("ledger.jsonl");
            let lease_token = match mode {
                "missing" => None,
                "expired" => Some("unknown-token".into()),
                "wrong-file" => Some(seed_lease(&ledger_path, "other", "1")),
                _ => Some(seed_lease(&ledger_path, "p1", "0")),
            };
            let opts = WriteOptions {
                lease_token,
                ledger_path,
                ..opts()
            };
            assert_eq!(
                write_inner(&drive, &slides, &opts, &[rule(mode != "optional-stale")])
                    .await
                    .result,
                expected
            );
        }
    }
    #[tokio::test]
    async fn successful_leased_write_concludes_audit_and_does_not_log_prose() {
        let server = MockServer::start().await;
        let (drive, slides) = ready(&server).await;
        batch()
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"replies":[{"replaceAllText":{"occurrencesChanged":1}}]}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("ledger.jsonl");
        let lease_token = Some(seed_lease(&ledger_path, "p1", "1"));
        let audit = crate::test_support::AuditLogGuard::redirect(dir.path());
        let outcome = write_inner(
            &drive,
            &slides,
            &WriteOptions {
                lease_token,
                ledger_path,
                ..opts()
            },
            &[rule(true)],
        )
        .await;
        assert_eq!(
            outcome.result,
            WriteResult::Replaced {
                occurrences_changed: 1
            }
        );
        assert_eq!(audit.verdicts(), ["pending", "allowed"]);
        let records = format!("{:?}", audit.records());
        assert!(records.contains("slides-replace"));
        assert!(!records.contains("Q3"));
        assert!(!records.contains("Q4"));
    }
    #[tokio::test]
    async fn folder_grants_and_default_deny_use_the_same_gate_before_slides_reads() {
        for allow in [false, true] {
            let server = MockServer::start().await;
            let (drive, slides) = clients(&server).await;
            Mock::given(method("GET")).and(path("/drive/v3/files/p1"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id":"p1", "name":"Deck", "mimeType":GOOGLE_SLIDES_MIME_TYPE,"parents":["folder"],"version":"1"})))
                .mount(&server).await;
            Mock::given(method("GET")).and(path("/drive/v3/files/folder"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id":"folder","name":"Folder","mimeType":"application/vnd.google-apps.folder"})))
                .mount(&server).await;
            forbid_batch(&server).await;
            let rules = if allow {
                mount_presentation(&server, presentation(Some("r"))).await;
                vec![FolderPermissionRule {
                    folder_id: Some("folder".into()),
                    file_id: None,
                    recursive: true,
                    ..rule(true)
                }]
            } else {
                Mock::given(path("/v1/presentations/p1"))
                    .respond_with(ResponseTemplate::new(500))
                    .expect(0)
                    .mount(&server)
                    .await;
                vec![]
            };
            let outcome = write_inner(
                &drive,
                &slides,
                &WriteOptions {
                    dry_run: true,
                    ..opts()
                },
                &rules,
            )
            .await;
            assert_eq!(outcome.resolved_folder_id.as_deref(), Some("folder"));
            if allow {
                assert_eq!(outcome.result, WriteResult::WouldReplace { occurrences: 2 });
            } else {
                assert!(matches!(
                    outcome.result,
                    WriteResult::Blocked { decided_by: None }
                ));
            }
        }
    }

    #[tokio::test]
    async fn failed_lease_audit_does_not_record_server_echoes_of_prose() {
        let server = MockServer::start().await;
        let (drive, slides) = ready(&server).await;
        batch()
            .respond_with(ResponseTemplate::new(400).set_body_json(
                serde_json::json!({"error":{"code":400,"message":"Invalid replacement Q3 to Q4"}}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("ledger.jsonl");
        let lease_token = Some(seed_lease(&ledger_path, "p1", "1"));
        let audit = crate::test_support::AuditLogGuard::redirect(dir.path());
        let outcome = write_inner(
            &drive,
            &slides,
            &WriteOptions {
                lease_token,
                ledger_path,
                ..opts()
            },
            &[rule(true)],
        )
        .await;
        assert!(
            matches!(outcome.result, WriteResult::Failed { ref detail } if detail.contains("Q3"))
        );
        assert_eq!(audit.verdicts(), ["pending", "failed"]);
        let records = format!("{:?}", audit.records());
        assert!(!records.contains("Q3"));
        assert!(!records.contains("Q4"));
    }

    #[test]
    fn case_and_multibyte_counts_are_display_only_and_nonoverlapping() {
        assert_eq!(count_occurrences("aaaa", "aa", true), 2);
        assert_eq!(count_occurrences("ÉCOLE école", "école", false), 2);
        assert_eq!(count_occurrences("😀 😀", "😀", true), 2);
        assert_eq!(count_occurrences("x", "", false), 0);
    }
}
