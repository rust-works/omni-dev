//! `drive docs create` engine — creates a Google Doc, optionally seeded with
//! text (issue #1615, [ADR-0076](../../../docs/adrs/adr-0076.md) §9).
//!
//! Owns its verb rather than composing `drive create`'s public engine, which
//! logs internally and unconditionally — composing it would emit a phantom
//! `["drive","create"]` record for a command the user never typed, alongside
//! the `docs-create` one. Same reasoning as `sheets/create.rs`.
//!
//! One of ADR-0073 §11's arguments is *stronger* here, and it is worth
//! stating plainly: `documents.create` accepts only a title and drops the
//! new document in the caller's My Drive root — it **cannot set a parent
//! folder**. Since the folder is the write gate's entire input, going
//! through `FilesApi::create` is not merely the tidier option, it is the
//! only shape the gate can evaluate at all.
//!
//! The optional `--text` seed costs a second `documents.get`, purely to mint
//! a lease for a document created milliseconds earlier. That round-trip is
//! paid deliberately rather than adding a "we just made it, nobody can have
//! touched it" unleased write path — which is exactly the escape hatch the
//! next caller reaches for, and whose existence would void both the type
//! fence and the grep guard in `write_types.rs`/`api.rs`.

use std::time::{Duration, Instant};

use serde::Serialize;

use crate::cli::drive::format::{write_scalar_jsonl, JsonlSerialize};
use crate::drive::client::DriveClient;
use crate::drive::docs::api::{DocsApi, SuggestionsViewMode};
use crate::drive::docs::client::DocsClient;
use crate::drive::docs::write_types::DocsRequest;
use crate::drive::files_api::FilesApi;
use crate::drive::folder_ancestry;
use crate::drive::types::GOOGLE_DOC_MIME_TYPE;
use crate::drive::write_gate::{self, DecidingRule, DriveOperation, FolderPermissionRule};
use crate::request_log::{self, DriveMutationOutcome};

/// Per-call options.
#[derive(Debug, Clone)]
pub struct CreateOptions {
    /// The new document's title.
    pub name: String,
    /// The folder to create it in — also the gate's input.
    pub parent_folder_id: String,
    /// Optional initial body text.
    pub text: Option<String>,
    /// Classify and preview, but create nothing.
    pub dry_run: bool,
}

/// What happened (or, under `--dry-run`, would happen).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum CreateResult {
    /// A dry run.
    WouldCreate {
        /// Unicode scalar values that would be seeded.
        chars: usize,
        /// UTF-8 bytes that would be seeded.
        bytes: usize,
    },
    /// The gate refused it.
    Blocked {
        /// The rule that decided, when one did.
        decided_by: Option<DecidingRule>,
    },
    /// Created, and seeded if `--text` was given.
    Created {
        /// The new document's id.
        file_id: String,
        /// Characters seeded, when `--text` was given.
        #[serde(skip_serializing_if = "Option::is_none")]
        seeded_chars: Option<usize>,
    },
    /// Created, but the seeding write failed.
    ///
    /// Its own variant carrying the id, because there is no `files.delete`
    /// anywhere in this integration — the empty document cannot be rolled
    /// back, and reporting it as a plain failure would leave an orphan the
    /// user has no way to find.
    CreatedTextFailed {
        /// The new document's id, so it is findable.
        file_id: String,
        /// Why the seed failed.
        detail: String,
    },
    /// Anything else.
    Failed {
        /// The error, verbatim.
        detail: String,
    },
}

impl CreateResult {
    /// The kebab-case status for the request log.
    const fn log_status(&self) -> &'static str {
        match self {
            Self::WouldCreate { .. } => "would-create",
            Self::Blocked { .. } => "blocked",
            Self::Created { .. } => "created",
            Self::CreatedTextFailed { .. } => "created-text-failed",
            Self::Failed { .. } => "failed",
        }
    }
}

/// The full outcome of one attempt.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CreateOutcome {
    /// The requested title.
    pub name: String,
    /// The folder the gate evaluated against.
    pub resolved_folder_id: String,
    /// What happened.
    pub result: CreateResult,
}

impl JsonlSerialize for CreateOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> anyhow::Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Creates a document, optionally seeding it, logging every non-dry attempt.
///
/// Never returns `Err`: every failure is a [`CreateResult`] variant.
pub async fn create(
    drive: &DriveClient,
    docs: &DocsClient,
    opts: &CreateOptions,
    rules: &[FolderPermissionRule],
) -> CreateOutcome {
    let started = Instant::now();
    let outcome = create_inner(drive, docs, opts, rules).await;
    if !opts.dry_run {
        record_attempt(&outcome, started.elapsed());
    }
    outcome
}

async fn create_inner(
    drive: &DriveClient,
    docs: &DocsClient,
    opts: &CreateOptions,
    rules: &[FolderPermissionRule],
) -> CreateOutcome {
    let finish = |result| CreateOutcome {
        name: opts.name.clone(),
        resolved_folder_id: opts.parent_folder_id.clone(),
        result,
    };

    let files_api = FilesApi::new(drive);
    let decision = match folder_ancestry::resolve_decision(
        &files_api,
        &opts.parent_folder_id,
        DriveOperation::Create,
        rules,
    )
    .await
    {
        Ok(decision) => decision,
        Err(err) => {
            return finish(CreateResult::Failed {
                detail: err.to_string(),
            })
        }
    };

    if decision.verdict == write_gate::Verdict::Deny {
        return finish(CreateResult::Blocked {
            decided_by: decision.decided_by,
        });
    }

    // An explicit `deny: ["docs-write"]` on this folder still refuses the
    // seed, even though `Create` alone authorises it otherwise — see this
    // module's header and ADR-0076 §9. `decided_by.is_some()` is what
    // distinguishes that deliberate signal from the bare default policy (no
    // rule mentions `docs-write` at all), which must **not** block a folder
    // that only grants `create`.
    if opts.text.is_some() {
        let docs_write_decision = match folder_ancestry::resolve_decision(
            &files_api,
            &opts.parent_folder_id,
            DriveOperation::DocsWrite,
            rules,
        )
        .await
        {
            Ok(decision) => decision,
            Err(err) => {
                return finish(CreateResult::Failed {
                    detail: err.to_string(),
                })
            }
        };
        if docs_write_decision.verdict == write_gate::Verdict::Deny
            && docs_write_decision.decided_by.is_some()
        {
            return finish(CreateResult::Blocked {
                decided_by: docs_write_decision.decided_by,
            });
        }
    }

    let (chars, bytes) = opts
        .text
        .as_ref()
        .map_or((0, 0), |text| (text.chars().count(), text.len()));

    if opts.dry_run {
        return finish(CreateResult::WouldCreate { chars, bytes });
    }

    let created = match files_api
        .create(&opts.name, &opts.parent_folder_id, GOOGLE_DOC_MIME_TYPE)
        .await
    {
        Ok(created) => created,
        Err(err) => {
            return finish(CreateResult::Failed {
                detail: err.to_string(),
            })
        }
    };

    let Some(text) = opts.text.as_ref() else {
        return finish(CreateResult::Created {
            file_id: created.id,
            seeded_chars: None,
        });
    };

    // The seed pays for its own lease rather than taking an unleased path.
    let api = DocsApi::new(docs);
    let document = match api
        .get_document(&created.id, SuggestionsViewMode::default())
        .await
    {
        Ok(document) => document,
        Err(err) => {
            return finish(CreateResult::CreatedTextFailed {
                file_id: created.id,
                detail: err.to_string(),
            })
        }
    };
    // A document this invocation just created should always carry a
    // revision id — the account plainly has edit access to it — so this is
    // folded into the partial-failure variant rather than given a variant
    // that could never fire.
    let Some(revision_id) = document.revision_id else {
        return finish(CreateResult::CreatedTextFailed {
            file_id: created.id,
            detail: "the new document returned no revision id, so the seed could not be leased"
                .to_string(),
        });
    };

    match api
        .batch_update(
            &created.id,
            DocsRequest::insert_text_at_end(text),
            &revision_id,
        )
        .await
    {
        Ok(_) => finish(CreateResult::Created {
            file_id: created.id,
            seeded_chars: Some(chars),
        }),
        // A stale lease is not realistically reachable on a document created
        // milliseconds earlier, so it is absorbed here rather than given its
        // own variant; the detail carries the server's message either way.
        Err(err) => finish(CreateResult::CreatedTextFailed {
            file_id: created.id,
            detail: err.to_string(),
        }),
    }
}

/// Emits the `kind: "drivemutation"` record.
///
/// The seed **text is never recorded**, only its size — same reasoning as
/// `write.rs::record_attempt`.
fn record_attempt(outcome: &CreateOutcome, duration: Duration) {
    let error = match &outcome.result {
        CreateResult::Failed { detail } | CreateResult::CreatedTextFailed { detail, .. } => {
            Some(detail.clone())
        }
        _ => None,
    };
    let decided_by = match &outcome.result {
        CreateResult::Blocked { decided_by } => decided_by.as_ref(),
        _ => None,
    };
    let decided_by = write_gate::decided_by_log_fields(decided_by);
    let file_id = match &outcome.result {
        CreateResult::Created { file_id, .. } | CreateResult::CreatedTextFailed { file_id, .. } => {
            file_id.clone()
        }
        _ => String::new(),
    };
    let inserted_chars = match &outcome.result {
        CreateResult::Created {
            seeded_chars: Some(chars),
            ..
        } => Some(*chars as i64),
        _ => None,
    };

    request_log::record_drive_mutation(DriveMutationOutcome {
        operation: "docs-create",
        file_id,
        file_name: outcome.name.clone(),
        status: outcome.result.log_status().to_string(),
        resolved_folder_id: Some(outcome.resolved_folder_id.clone()),
        decided_by_folder_id: decided_by.folder_id,
        decided_by_depth: decided_by.depth,
        decided_by_file_id: decided_by.file_id,
        inserted_chars,
        error,
        duration,
        ..Default::default()
    });
}

/// Renders an outcome as a single human-readable line.
#[must_use]
pub fn describe(outcome: &CreateOutcome) -> String {
    let name = &outcome.name;
    let folder = &outcome.resolved_folder_id;
    match &outcome.result {
        CreateResult::WouldCreate { chars, bytes } if *chars > 0 => format!(
            "Would create: document '{name}' in {folder}, seeded with {chars} char(s) / \
             {bytes} byte(s)"
        ),
        CreateResult::WouldCreate { .. } => {
            format!("Would create: document '{name}' in {folder}")
        }
        CreateResult::Blocked { decided_by } => match decided_by {
            Some(rule) => format!(
                "Blocked: '{name}' in {folder} — refused by rule on {} {}{}",
                rule.kind_label(),
                rule.id(),
                rule.depth_suffix()
            ),
            None => format!(
                "Blocked: '{name}' in {folder} — refused by default policy (no matching rule)"
            ),
        },
        CreateResult::Created {
            file_id,
            seeded_chars,
        } => match seeded_chars {
            Some(chars) => {
                format!("Created: '{name}' ({file_id}) in {folder}, seeded with {chars} char(s)")
            }
            None => format!("Created: '{name}' ({file_id}) in {folder}"),
        },
        CreateResult::CreatedTextFailed { file_id, detail } => format!(
            "Partially failed: created '{name}' ({file_id}) in {folder}, but seeding its text \
             failed: {detail} The document exists and is empty — it cannot be rolled back \
             automatically."
        ),
        CreateResult::Failed { detail } => format!("Failed: '{name}' in {folder}: {detail}"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::docs::client::DOCS_API_URL;
    use crate::test_support::env::MapEnv;
    use crate::utils::secret::Secret;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_credentials() -> DriveCredentials {
        DriveCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: DriveGrantedScopes::READONLY,
        }
    }

    async fn clients(server: &MockServer) -> (DriveClient, DocsClient) {
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "test-token", "expires_in": 3600,
            })))
            .mount(server)
            .await;
        let mut drive = DriveClient::new(&server.uri(), &test_credentials()).unwrap();
        crate::drive::client::test_support::replace_session(
            &mut drive,
            &test_credentials(),
            &format!("{}/token", server.uri()),
        );
        let env = MapEnv::new().with(DOCS_API_URL, &server.uri());
        let docs = DocsClient::from_drive_client_with(&env, &drive).unwrap();
        (drive, docs)
    }

    fn mount_folder(id: &str) -> Mock {
        Mock::given(method("GET"))
            .and(path(format!("/drive/v3/files/{id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": id, "name": id,
                "mimeType": "application/vnd.google-apps.folder", "parents": [],
            })))
    }

    fn mount_files_create() -> Mock {
        Mock::given(method("POST"))
            .and(path("/drive/v3/files"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "new-1", "name": "New Doc", "mimeType": GOOGLE_DOC_MIME_TYPE,
            })))
    }

    fn mount_new_document(revision: Option<&str>) -> Mock {
        let mut body = serde_json::json!({"documentId": "new-1", "body": {"content": []}});
        if let Some(revision) = revision {
            body["revisionId"] = serde_json::json!(revision);
        }
        Mock::given(method("GET"))
            .and(path("/v1/documents/new-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
    }

    fn rule(ops: &[DriveOperation], denies: &[DriveOperation]) -> FolderPermissionRule {
        FolderPermissionRule {
            folder_id: Some("folder-1".to_string()),
            file_id: None,
            recursive: true,
            allow: ops.iter().copied().collect(),
            deny: denies.iter().copied().collect::<HashSet<_>>(),
            require_lease: true,
        }
    }

    fn opts(text: Option<&str>, dry_run: bool) -> CreateOptions {
        CreateOptions {
            name: "New Doc".to_string(),
            parent_folder_id: "folder-1".to_string(),
            text: text.map(str::to_string),
            dry_run,
        }
    }

    #[tokio::test]
    async fn create_without_text_makes_one_files_create_and_zero_docs_calls() {
        let server = MockServer::start().await;
        let (drive, docs) = clients(&server).await;
        mount_folder("folder-1").mount(&server).await;
        mount_files_create().expect(1).mount(&server).await;
        // No Docs mocks: an unseeded create must not touch the Docs API.

        let outcome = create(
            &drive,
            &docs,
            &opts(None, false),
            &[rule(&[DriveOperation::Create], &[])],
        )
        .await;
        assert_eq!(
            outcome.result,
            CreateResult::Created {
                file_id: "new-1".to_string(),
                seeded_chars: None,
            }
        );
    }

    /// ADR-0076 §9: the seed mints its own lease rather than writing
    /// unleased, so a `documents.get` on the *new* id must precede the
    /// batchUpdate and its revision must be presented.
    #[tokio::test]
    async fn create_with_text_reads_the_new_document_for_a_lease_before_seeding() {
        let server = MockServer::start().await;
        let (drive, docs) = clients(&server).await;
        mount_folder("folder-1").mount(&server).await;
        mount_files_create().mount(&server).await;
        mount_new_document(Some("rev-new"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/documents/new-1:batchUpdate"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "writeControl": {"requiredRevisionId": "rev-new"},
            })))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"documentId": "new-1", "replies": [{}]})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let outcome = create(
            &drive,
            &docs,
            &opts(Some("hello"), false),
            &[rule(&[DriveOperation::Create], &[])],
        )
        .await;
        assert_eq!(
            outcome.result,
            CreateResult::Created {
                file_id: "new-1".to_string(),
                seeded_chars: Some(5),
            }
        );
    }

    /// There is no `files.delete` in this integration, so a failed seed must
    /// name the orphan rather than report a clean failure.
    #[tokio::test]
    async fn a_seed_failure_reports_created_text_failed_carrying_the_id() {
        let server = MockServer::start().await;
        let (drive, docs) = clients(&server).await;
        mount_folder("folder-1").mount(&server).await;
        mount_files_create().mount(&server).await;
        mount_new_document(Some("rev-new")).mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/v1/documents/new-1:batchUpdate"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let outcome = create(
            &drive,
            &docs,
            &opts(Some("hello"), false),
            &[rule(&[DriveOperation::Create], &[])],
        )
        .await;
        match &outcome.result {
            CreateResult::CreatedTextFailed { file_id, .. } => assert_eq!(file_id, "new-1"),
            other => panic!("expected CreatedTextFailed, got {other:?}"),
        }
        let text = describe(&outcome);
        assert!(
            text.contains("new-1"),
            "the orphan must be findable: {text}"
        );
        assert!(text.contains("cannot be rolled back"), "{text}");
    }

    /// The ADR-0073 §11 / ADR-0076 §9 distinction: a bare default deny must
    /// **not** block the seed, or `--text` would be refused on every folder
    /// that grants only `create`.
    #[tokio::test]
    async fn a_bare_default_deny_does_not_block_the_seed() {
        let server = MockServer::start().await;
        let (drive, docs) = clients(&server).await;
        mount_folder("folder-1").mount(&server).await;
        mount_files_create().mount(&server).await;
        mount_new_document(Some("rev-new")).mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/v1/documents/new-1:batchUpdate"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"documentId": "new-1", "replies": [{}]})),
            )
            .mount(&server)
            .await;

        // Grants `create` only — no rule mentions `docs-write` at all.
        let outcome = create(
            &drive,
            &docs,
            &opts(Some("hello"), false),
            &[rule(&[DriveOperation::Create], &[])],
        )
        .await;
        assert!(matches!(outcome.result, CreateResult::Created { .. }));
    }

    /// …but an *explicit* deny is a deliberate operator signal and is
    /// honoured, before anything is created.
    #[tokio::test]
    async fn an_explicit_docs_write_deny_blocks_the_seed_and_creates_nothing() {
        let server = MockServer::start().await;
        let (drive, docs) = clients(&server).await;
        mount_folder("folder-1").mount(&server).await;
        // No `files.create` mock: nothing may be created.

        let outcome = create(
            &drive,
            &docs,
            &opts(Some("hello"), false),
            &[rule(
                &[DriveOperation::Create],
                &[DriveOperation::DocsWrite],
            )],
        )
        .await;
        assert!(matches!(outcome.result, CreateResult::Blocked { .. }));
    }

    /// The same explicit deny must not block a create with *no* seed, since
    /// no text is written.
    #[tokio::test]
    async fn an_explicit_docs_write_deny_does_not_block_an_unseeded_create() {
        let server = MockServer::start().await;
        let (drive, docs) = clients(&server).await;
        mount_folder("folder-1").mount(&server).await;
        mount_files_create().mount(&server).await;

        let outcome = create(
            &drive,
            &docs,
            &opts(None, false),
            &[rule(
                &[DriveOperation::Create],
                &[DriveOperation::DocsWrite],
            )],
        )
        .await;
        assert!(matches!(outcome.result, CreateResult::Created { .. }));
    }

    #[tokio::test]
    async fn a_denied_create_is_blocked_and_creates_nothing() {
        let server = MockServer::start().await;
        let (drive, docs) = clients(&server).await;
        mount_folder("folder-1").mount(&server).await;

        let outcome = create(&drive, &docs, &opts(None, false), &[]).await;
        assert!(matches!(outcome.result, CreateResult::Blocked { .. }));
    }

    #[tokio::test]
    async fn dry_run_creates_nothing() {
        let server = MockServer::start().await;
        let (drive, docs) = clients(&server).await;
        mount_folder("folder-1").mount(&server).await;
        // No POST mocks at all.

        let outcome = create(
            &drive,
            &docs,
            &opts(Some("hello"), true),
            &[rule(&[DriveOperation::Create], &[])],
        )
        .await;
        assert_eq!(
            outcome.result,
            CreateResult::WouldCreate { chars: 5, bytes: 5 }
        );
        assert!(describe(&outcome).contains("seeded with 5 char(s)"));
    }
}
