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

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::cli::drive::format::{write_scalar_jsonl, JsonlSerialize};
use crate::drive::client::DriveClient;
use crate::drive::files_api::FilesApi;
use crate::drive::folder_ancestry;
use crate::drive::lease::check::{
    check_and_lock_lease, refresh_lease_after_write, LeaseCheckOutcome,
};
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
    /// The lease token presented via `--lease`. Checked only when the
    /// deciding rule requires one
    /// ([`write_gate::decided_rule_requires_lease`], ADR-0080 §1/§13);
    /// `None` is only ever valid when it does not.
    pub lease_token: Option<String>,
    /// Path to the lease ledger the token is checked against. Production
    /// callers pass `crate::drive::lease::ledger::ledger_path`'s own
    /// result; tests pass a path under a `tempdir`.
    pub ledger_path: PathBuf,
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
    /// No `--lease` was presented, and the deciding rule requires one
    /// (ADR-0080 §9).
    RefusedNoLease,
    /// The presented lease has expired, or was never a token this ledger
    /// knows about.
    RefusedLeaseExpired,
    /// The presented lease is bound to a different file id.
    RefusedLeaseWrongFile,
    /// The file has moved since the lease's recorded `version` — the
    /// staleness check (ADR-0080 §6).
    RefusedLeaseStale,
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
            Self::RefusedNoLease => "refused-no-lease",
            Self::RefusedLeaseExpired => "refused-lease-expired",
            Self::RefusedLeaseWrongFile => "refused-lease-wrong-file",
            Self::RefusedLeaseStale => "refused-lease-stale",
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
        requires_lease,
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

    // The lease check (ADR-0080 §9) sits here: after the permission gate
    // and the `--dry-run` branch, before the mutating call. A folder-
    // permission refusal above already made zero Drive API calls with the
    // lease never touched; a `--dry-run` never needs `--lease` at all.
    //
    // `requires_lease` already folds in every legacy multi-parent's own
    // requirement (see `FileTargetDecision::requires_lease`'s doc comment)
    // — it must not be re-derived from `decision.decided_by` alone here,
    // which would only see the one parent whose decision happened to win
    // the verdict.
    //
    // The staleness check below is re-fetched fresh here rather than
    // reusing `target.version` from the very first `get_metadata` call:
    // `resolve_decision_for_file_target` above can issue its own
    // `files.get` calls walking the ancestor chain, so by this point
    // `target.version` may already be stale relative to the live file —
    // exactly the same window ADR-0080 §6 introduces the check to guard
    // against. Re-fetching immediately before the check (and thus
    // immediately before `edit_content`) keeps that window as small as
    // the Sheets/Docs equivalent ("a `files.get` immediately before the
    // `batchUpdate`", ADR-0080 §6) rather than spanning the whole gate
    // evaluation.
    //
    // The ledger lock acquired by `check_and_lock_lease` below is held
    // across the `edit_content` call and released only after
    // `refresh_lease_after_write` — otherwise a second concurrent `drive
    // edit` presenting the same token could load the ledger before this
    // write's `record_write` lands, see the same (still non-stale)
    // recorded version, and pass its own staleness check even though this
    // write is about to invalidate it (a lease-token double-spend).
    let lease_lock = if requires_lease {
        let live_version = match files_api.get_metadata(&opts.file_id).await {
            Ok(fresh) => fresh.version,
            Err(err) => {
                return EditOutcome {
                    file_id: opts.file_id.clone(),
                    file_name: Some(target.name),
                    resolved_folder_id,
                    result: EditResult::Failed {
                        detail: err.to_string(),
                    },
                };
            }
        };
        match check_and_lock_lease(
            "drive edit",
            &opts.ledger_path,
            opts.lease_token.as_deref(),
            &opts.file_id,
            live_version.as_deref(),
        ) {
            LeaseCheckOutcome::Ok(lock) => Some(lock),
            LeaseCheckOutcome::NoLease => {
                return EditOutcome {
                    file_id: opts.file_id.clone(),
                    file_name: Some(target.name),
                    resolved_folder_id,
                    result: EditResult::RefusedNoLease,
                };
            }
            LeaseCheckOutcome::Expired => {
                return EditOutcome {
                    file_id: opts.file_id.clone(),
                    file_name: Some(target.name),
                    resolved_folder_id,
                    result: EditResult::RefusedLeaseExpired,
                };
            }
            LeaseCheckOutcome::WrongFile => {
                return EditOutcome {
                    file_id: opts.file_id.clone(),
                    file_name: Some(target.name),
                    resolved_folder_id,
                    result: EditResult::RefusedLeaseWrongFile,
                };
            }
            LeaseCheckOutcome::Stale => {
                return EditOutcome {
                    file_id: opts.file_id.clone(),
                    file_name: Some(target.name),
                    resolved_folder_id,
                    result: EditResult::RefusedLeaseStale,
                };
            }
            LeaseCheckOutcome::Failed(detail) => {
                return EditOutcome {
                    file_id: opts.file_id.clone(),
                    file_name: Some(target.name),
                    resolved_folder_id,
                    result: EditResult::Failed { detail },
                };
            }
        }
    } else {
        None
    };

    let result = match files_api
        .edit_content(&opts.file_id, &opts.content, &opts.content_type)
        .await
    {
        Ok(updated) => {
            if let (Some(token), Some(lock)) = (&opts.lease_token, &lease_lock) {
                refresh_lease_after_write(
                    "drive edit",
                    lock,
                    &opts.ledger_path,
                    token,
                    updated.version,
                    updated.modified_time,
                );
            }
            EditResult::Edited
        }
        Err(err) => EditResult::Failed {
            detail: err.to_string(),
        },
    };
    drop(lease_lock);
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
    use crate::drive::lease::ledger::LeaseLedger;
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

    /// `version: "1"` throughout — matches [`seed_lease`]'s default, so any
    /// test that seeds a lease and mounts a file via this helper has a
    /// live, non-stale lease by construction.
    fn mount_file(id: &str, mime_type: &str, parents: &[&str]) -> wiremock::Mock {
        let parents_json: Vec<&str> = parents.to_vec();
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!("/drive/v3/files/{id}")))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": id, "name": id, "mimeType": mime_type, "parents": parents_json,
                    "version": "1",
                })),
            )
    }

    /// Seeds `ledger_path` with a fresh, live lease for `file_id` at
    /// `version`, returning its token — for tests exercising the success
    /// path, which now requires a valid lease (ADR-0080 §9).
    fn seed_lease(ledger_path: &std::path::Path, file_id: &str, version: &str) -> String {
        let token = "test-lease-token".to_string();
        let mut ledger = LeaseLedger::default();
        ledger.insert(crate::drive::lease::ledger::LeaseRecord {
            token: token.clone(),
            file_id: file_id.to_string(),
            version: version.to_string(),
            modified_time: None,
            backup: crate::drive::lease::ledger::LeaseBackup::Bytes {
                path: std::path::PathBuf::from("/tmp/test-backup"),
                sha256: "deadbeef".to_string(),
                size: 0,
            },
            acquired_at: chrono::Utc::now(),
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(30),
            released_at: None,
        });
        ledger.save(ledger_path).unwrap();
        token
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

    /// No lease token and a ledger path that is never created — every test
    /// using this reaches a refusal (native document, no visible parents,
    /// blocked, a fetch failure) before the lease check would matter. Tests
    /// exercising the success path use [`opts_with_lease`] instead.
    fn opts_for(file_id: &str, dry_run: bool) -> EditOptions {
        EditOptions {
            file_id: file_id.to_string(),
            content: b"new content".to_vec(),
            content_type: "text/plain".to_string(),
            dry_run,
            lease_token: None,
            ledger_path: std::path::PathBuf::from("/nonexistent/lease-ledger.jsonl"),
        }
    }

    /// [`opts_for`] plus a lease already seeded (via [`seed_lease`]) into
    /// `ledger_path` for `file_id` at `version` — for tests exercising the
    /// success path.
    fn opts_with_lease(file_id: &str, ledger_path: &std::path::Path, version: &str) -> EditOptions {
        let token = seed_lease(ledger_path, file_id, version);
        EditOptions {
            lease_token: Some(token),
            ledger_path: ledger_path.to_path_buf(),
            ..opts_for(file_id, false)
        }
    }

    fn allow_rule() -> FolderPermissionRule {
        FolderPermissionRule {
            folder_id: Some("parent-1".to_string()),
            file_id: None,
            recursive: false,
            allow: std::iter::once(DriveOperation::Edit).collect(),
            deny: std::collections::HashSet::default(),
            require_lease: true,
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
                    "id": "file-1", "name": "file-1", "version": "2",
                })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("lease-ledger.jsonl");

        let outcome = edit(
            &client,
            &opts_with_lease("file-1", &ledger_path, "1"),
            &[allow_rule()],
        )
        .await;
        assert!(matches!(outcome.result, EditResult::Edited));

        let reloaded = LeaseLedger::load(&ledger_path).unwrap();
        assert_eq!(
            reloaded.get("test-lease-token").unwrap().version,
            "2",
            "a successful write refreshes the lease's recorded version"
        );
    }

    #[tokio::test]
    async fn staleness_check_uses_a_version_fetched_after_the_ancestor_walk_not_before() {
        // Regression test for the lease staleness-check TOCTOU (issue
        // #1664): the *first* `files.get` (used for the permission-gate's
        // ancestor walk) returns version "0" — stale, as if a foreign edit
        // landed while the gate was still resolving. A *second*,
        // freshly-fetched `files.get` returns "1", matching the lease. If
        // the staleness check reused the first call's snapshot (the bug),
        // this would incorrectly refuse as stale; using the fresh fetch,
        // it must succeed.
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/file-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "file-1", "name": "file-1", "mimeType": "text/plain",
                    "parents": ["parent-1"], "version": "0",
                })),
            )
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/file-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "file-1", "name": "file-1", "mimeType": "text/plain",
                    "parents": ["parent-1"], "version": "1",
                })),
            )
            .with_priority(2)
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/upload/drive/v3/files/file-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "file-1", "name": "file-1", "version": "2",
                })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("lease-ledger.jsonl");

        let outcome = edit(
            &client,
            &opts_with_lease("file-1", &ledger_path, "1"),
            &[allow_rule()],
        )
        .await;
        assert!(
            matches!(outcome.result, EditResult::Edited),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn a_failed_pre_lease_refetch_is_reported_as_failed_with_no_edit_call() {
        // The gate's ancestor walk succeeds off the first `files.get`, but
        // the fresh re-fetch feeding the staleness check (ADR-0080 §6)
        // fails — the edit must report `Failed` and never reach the
        // media PATCH.
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file("file-1", "text/plain", &["parent-1"])
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/file-1"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .with_priority(2)
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/upload/drive/v3/files/file-1"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("lease-ledger.jsonl");

        let outcome = edit(
            &client,
            &opts_with_lease("file-1", &ledger_path, "1"),
            &[allow_rule()],
        )
        .await;
        assert!(
            matches!(outcome.result, EditResult::Failed { .. }),
            "{:?}",
            outcome.result
        );
        assert_eq!(outcome.file_name.as_deref(), Some("file-1"));
    }

    /// A responder that makes the ledger's directory read-only the instant
    /// the media PATCH lands — i.e. after `check_and_lock_lease` has
    /// already locked and loaded the ledger, but before
    /// `refresh_lease_after_write` tries to save it. `tempfile::NamedTempFile::new_in`
    /// then fails to create its temp file, so `LeaseLedger::save` errors.
    #[cfg(unix)]
    struct MakeDirReadOnlyThenRespond {
        dir: std::path::PathBuf,
    }

    #[cfg(unix)]
    impl wiremock::Respond for MakeDirReadOnlyThenRespond {
        fn respond(&self, _req: &wiremock::Request) -> wiremock::ResponseTemplate {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o500)).unwrap();
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "file-1", "name": "file-1", "version": "2",
            }))
        }
    }

    /// A failed post-write lease refresh (ADR-0080 §5) must never turn an
    /// already-successful edit into a reported failure — see
    /// `refresh_lease_after_write`'s own doc comment.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_lease_refresh_after_a_successful_write_does_not_fail_the_edit() {
        use std::os::unix::fs::PermissionsExt;

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file("file-1", "text/plain", &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        let root = tempfile::tempdir().unwrap();
        let ledger_dir = root.path().join("ledger");
        std::fs::create_dir(&ledger_dir).unwrap();
        let ledger_path = ledger_dir.join("lease-ledger.jsonl");
        let opts = opts_with_lease("file-1", &ledger_path, "1");
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/upload/drive/v3/files/file-1"))
            .respond_with(MakeDirReadOnlyThenRespond {
                dir: ledger_dir.clone(),
            })
            .expect(1)
            .mount(&server)
            .await;

        let outcome = edit(&client, &opts, &[allow_rule()]).await;

        // Restore write permission before the tempdir is dropped, or its
        // own cleanup fails to remove a now-read-only directory.
        std::fs::set_permissions(&ledger_dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(
            matches!(outcome.result, EditResult::Edited),
            "a refresh failure must not fail the edit itself: {:?}",
            outcome.result
        );
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
            require_lease: true,
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
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("lease-ledger.jsonl");

        let outcome = edit(
            &client,
            &opts_with_lease("file-1", &ledger_path, "1"),
            &[allow_rule()],
        )
        .await;
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
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("lease-ledger.jsonl");

        let outcome = edit(
            &client,
            &opts_with_lease("file-1", &ledger_path, "1"),
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

    // ── the Drive write lease (ADR-0080 §9) ────────────────────────────

    #[tokio::test]
    async fn refuses_without_a_lease_when_the_rule_requires_one() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file("file-1", "text/plain", &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        // No PATCH mock mounted — a refusal must make zero mutating calls.

        let outcome = edit(&client, &opts(false), &[allow_rule()]).await;
        assert!(matches!(outcome.result, EditResult::RefusedNoLease));
        assert_eq!(outcome.result.log_status(), "refused-no-lease");
    }

    #[tokio::test]
    async fn refuses_an_unknown_lease_token() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file("file-1", "text/plain", &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        // Never seeded — the ledger exists nowhere near this token.

        let opts = EditOptions {
            lease_token: Some("bogus-token".to_string()),
            ledger_path,
            ..opts_for("file-1", false)
        };
        let outcome = edit(&client, &opts, &[allow_rule()]).await;
        assert!(matches!(outcome.result, EditResult::RefusedLeaseExpired));
    }

    #[tokio::test]
    async fn refuses_a_lease_when_the_ledger_is_unreadable() {
        // A directory in place of the ledger file makes `LeaseLedger::load`
        // fail with something other than a missing-file error — refused as
        // expired rather than trusting an unreadable ledger (fail-closed,
        // see `check_and_lock_lease`'s doc comment).
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file("file-1", "text/plain", &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        std::fs::create_dir(&ledger_path).unwrap();

        let opts = EditOptions {
            lease_token: Some("any-token".to_string()),
            ledger_path,
            ..opts_for("file-1", false)
        };
        let outcome = edit(&client, &opts, &[allow_rule()]).await;
        assert!(matches!(outcome.result, EditResult::RefusedLeaseExpired));
    }

    #[tokio::test]
    async fn refuses_an_expired_lease() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file("file-1", "text/plain", &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        let token = "expired-token".to_string();
        let mut ledger = LeaseLedger::default();
        ledger.insert(crate::drive::lease::ledger::LeaseRecord {
            token: token.clone(),
            file_id: "file-1".to_string(),
            version: "1".to_string(),
            modified_time: None,
            backup: crate::drive::lease::ledger::LeaseBackup::Bytes {
                path: std::path::PathBuf::from("/tmp/test-backup"),
                sha256: "deadbeef".to_string(),
                size: 0,
            },
            acquired_at: chrono::Utc::now() - chrono::Duration::hours(2),
            expires_at: chrono::Utc::now() - chrono::Duration::hours(1),
            released_at: None,
        });
        ledger.save(&ledger_path).unwrap();

        let opts = EditOptions {
            lease_token: Some(token),
            ledger_path,
            ..opts_for("file-1", false)
        };
        let outcome = edit(&client, &opts, &[allow_rule()]).await;
        assert!(matches!(outcome.result, EditResult::RefusedLeaseExpired));
    }

    #[tokio::test]
    async fn refuses_a_lease_bound_to_a_different_file() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file("file-1", "text/plain", &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        // Seeded for a *different* file id.
        let token = seed_lease(&ledger_path, "some-other-file", "1");

        let opts = EditOptions {
            lease_token: Some(token),
            ledger_path,
            ..opts_for("file-1", false)
        };
        let outcome = edit(&client, &opts, &[allow_rule()]).await;
        assert!(matches!(outcome.result, EditResult::RefusedLeaseWrongFile));
    }

    #[tokio::test]
    async fn refuses_a_stale_lease_when_the_file_has_moved() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        // mount_file always returns version "1"; the lease below was
        // acquired against version "0" — a foreign edit landed since.
        mount_file("file-1", "text/plain", &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        let token = seed_lease(&ledger_path, "file-1", "0");

        let opts = EditOptions {
            lease_token: Some(token),
            ledger_path,
            ..opts_for("file-1", false)
        };
        let outcome = edit(&client, &opts, &[allow_rule()]).await;
        assert!(matches!(outcome.result, EditResult::RefusedLeaseStale));
    }

    #[tokio::test]
    async fn edit_requires_a_lease_when_any_legacy_parent_requires_one_even_if_the_deciding_parent_opted_out(
    ) {
        // Regression test for the multi-parent lease-bypass (issue #1664):
        // `combine_across_parents` keeps only the *last* Allow decision's
        // `decided_by` — here that's `lenient-parent`, whose rule opts out
        // of the lease. `strict-parent`'s own requirement must still
        // apply, or a lease could be silently skipped just by attaching an
        // opted-out legacy parent to a file.
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file("file-1", "text/plain", &["strict-parent", "lenient-parent"])
            .mount(&server)
            .await;
        mount_folder("strict-parent").mount(&server).await;
        mount_folder("lenient-parent").mount(&server).await;
        // No PATCH mock mounted — a refusal must make zero mutating calls.
        let rules = [
            FolderPermissionRule::folder("strict-parent").allowing([DriveOperation::Edit]),
            FolderPermissionRule::folder("lenient-parent")
                .allowing([DriveOperation::Edit])
                .requiring_lease(false),
        ];

        let outcome = edit(&client, &opts(false), &rules).await;
        assert!(
            matches!(outcome.result, EditResult::RefusedNoLease),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn a_held_ledger_lock_fails_the_edit_rather_than_risking_a_lease_double_spend() {
        // Regression test for the lease-check/refresh TOCTOU (issue
        // #1664): simulates a concurrent `drive lease`/`drive edit`
        // operation already holding the ledger lock. The edit must fail
        // outright rather than silently reading the ledger unlocked.
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file("file-1", "text/plain", &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        // No PATCH mock mounted — a refusal must make zero mutating calls.
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("lease-ledger.jsonl");
        let token = seed_lease(&ledger_path, "file-1", "1");
        let mut lock_path = ledger_path.clone().into_os_string();
        lock_path.push(".lock");
        std::fs::File::create(std::path::PathBuf::from(lock_path)).unwrap();

        let opts = EditOptions {
            lease_token: Some(token),
            ledger_path,
            ..opts_for("file-1", false)
        };
        let outcome = edit(&client, &opts, &[allow_rule()]).await;
        assert!(
            matches!(outcome.result, EditResult::Failed { .. }),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn require_lease_false_skips_the_lease_check_entirely() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file("file-1", "text/plain", &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/upload/drive/v3/files/file-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "file-1", "name": "file-1"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let rule = FolderPermissionRule::folder("parent-1")
            .allowing([DriveOperation::Edit])
            .requiring_lease(false);

        // No lease token presented at all, and no ledger exists.
        let outcome = edit(&client, &opts(false), &[rule]).await;
        assert!(matches!(outcome.result, EditResult::Edited));
    }

    #[tokio::test]
    async fn a_dry_run_never_needs_a_lease() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_file("file-1", "text/plain", &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        // No PATCH mock, no lease token, no ledger — a dry run must not
        // need any of them.

        let outcome = edit(&client, &opts(true), &[allow_rule()]).await;
        assert!(matches!(outcome.result, EditResult::WouldEdit));
    }
}
