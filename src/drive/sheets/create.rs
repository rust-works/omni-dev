//! `drive sheets create` engine — creates a spreadsheet and optionally
//! seeds its cells (issue #1589,
//! [ADR-0073](../../../docs/adrs/adr-0073.md) §11).
//!
//! Mirrors `create.rs` with a fixed spreadsheet MIME type rather than
//! composing it, deliberately. `drive::create::create` logs internally and
//! unconditionally — that non-bypassability is the point of it — so
//! composing it would emit a phantom `["drive", "create"]` record for a
//! command the user never typed, alongside this verb's own. One user-visible
//! verb, one gate check, one log record.
//!
//! **The seeding write is ungated**, and that is a decision rather than an
//! oversight. Routing it through the `sheets write` engine would re-run the
//! gate under `SheetsWrite` against the *new* file's parents, which defaults
//! to deny — so `sheets create --values` would create an empty spreadsheet
//! and then report itself blocked. The `Create` verdict authorises the pair.
//! That is only defensible because the target id is always the one
//! `files.create` just returned inside an already-cleared folder, never a
//! caller-supplied id; `drive upload` sets the precedent of writing content
//! under `Upload` rather than `Edit`.

use std::time::{Duration, Instant};

use serde::Serialize;

use crate::cli::drive::format::{write_scalar_jsonl, JsonlSerialize};
use crate::drive::client::DriveClient;
use crate::drive::files_api::FilesApi;
use crate::drive::folder_ancestry;
use crate::drive::sheets::api::{SheetsApi, ValueInputOption};
use crate::drive::sheets::client::SheetsClient;
use crate::drive::types::GOOGLE_SHEET_MIME_TYPE;
use crate::drive::write_gate::{self, DecidingRule, DriveOperation, FolderPermissionRule};
use crate::request_log::{self, DriveMutationOutcome};

/// Where seeded values land.
///
/// Google names the first tab of a new spreadsheet `Sheet1`, so this is the
/// anchor for an unqualified seed. Quoted, like every range this crate
/// builds.
const SEED_RANGE: &str = "'Sheet1'!A1";

/// Per-call options.
#[derive(Debug, Clone)]
pub struct CreateOptions {
    /// The new spreadsheet's title.
    pub name: String,
    /// The folder to create it in — what the gate is evaluated against.
    pub parent_folder_id: String,
    /// Optional initial cell values, written to `Sheet1!A1` onwards.
    pub values: Vec<Vec<String>>,
    /// How the API should interpret those values.
    pub input: ValueInputOption,
    /// Classify only; never create or write.
    pub dry_run: bool,
}

/// What happened (or, under `--dry-run`, would happen).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum CreateResult {
    /// `--dry-run`, and the gate would allow it.
    WouldCreate {
        /// Rows of seed input parsed, so a transposed input is visible
        /// before it lands.
        rows: usize,
        /// Widest seed row.
        columns: usize,
    },
    /// The folder write-permission gate refused it.
    Blocked {
        /// The rule that decided the refusal, if any.
        decided_by: Option<DecidingRule>,
    },
    /// The spreadsheet was created (and seeded, if `--values` was given).
    Created {
        /// The new spreadsheet's id.
        file_id: String,
        /// Cells seeded, when seeding happened.
        #[serde(skip_serializing_if = "Option::is_none")]
        seeded_cells: Option<i64>,
    },
    /// The spreadsheet was created but seeding it failed.
    ///
    /// Its own variant, carrying the id, because there is **no**
    /// `files.delete` anywhere in this integration — the empty spreadsheet
    /// cannot be rolled back, so reporting this as a plain `Failed` would
    /// leave an orphan the user has no way to find.
    CreatedValuesFailed {
        /// The new spreadsheet's id — it exists and is empty.
        file_id: String,
        /// Why seeding failed.
        detail: String,
    },
    /// Creation itself failed; nothing was created.
    Failed {
        /// A human-readable summary.
        detail: String,
    },
}

impl CreateResult {
    fn log_status(&self) -> &'static str {
        match self {
            Self::WouldCreate { .. } => "would-create",
            Self::Blocked { .. } => "blocked",
            Self::Created { .. } => "created",
            Self::CreatedValuesFailed { .. } => "created-values-failed",
            Self::Failed { .. } => "failed",
        }
    }

    /// The new file's id, when one exists — including the partial-failure
    /// case, which is the whole reason that variant carries it.
    fn file_id(&self) -> Option<&str> {
        match self {
            Self::Created { file_id, .. } | Self::CreatedValuesFailed { file_id, .. } => {
                Some(file_id)
            }
            _ => None,
        }
    }
}

/// The full outcome of one attempt.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CreateOutcome {
    /// The requested title.
    pub name: String,
    /// The folder it was to be created in.
    pub parent_folder_id: String,
    /// What happened.
    pub result: CreateResult,
}

impl JsonlSerialize for CreateOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> anyhow::Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Creates a spreadsheet, optionally seeding its cells.
///
/// Never returns `Err`; every failure is a [`CreateResult`] variant, and the
/// process exits 0 regardless (ADR-0070 §10, ADR-0071 §12).
pub async fn create(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &CreateOptions,
    rules: &[FolderPermissionRule],
) -> CreateOutcome {
    let started = Instant::now();
    let outcome = create_inner(drive, sheets, opts, rules).await;
    if !opts.dry_run {
        record_attempt(&outcome, started.elapsed());
    }
    outcome
}

async fn create_inner(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &CreateOptions,
    rules: &[FolderPermissionRule],
) -> CreateOutcome {
    let finish = |result| CreateOutcome {
        name: opts.name.clone(),
        parent_folder_id: opts.parent_folder_id.clone(),
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
        // An unresolvable chain is a refusal, never a silent allow.
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

    if opts.dry_run {
        return finish(CreateResult::WouldCreate {
            rows: opts.values.len(),
            columns: opts.values.iter().map(Vec::len).max().unwrap_or(0),
        });
    }

    let created = match files_api
        .create(&opts.name, &opts.parent_folder_id, GOOGLE_SHEET_MIME_TYPE)
        .await
    {
        Ok(file) => file,
        Err(err) => {
            return finish(CreateResult::Failed {
                detail: format!("{err:#}"),
            })
        }
    };

    if opts.values.is_empty() {
        return finish(CreateResult::Created {
            file_id: created.id,
            seeded_cells: None,
        });
    }

    // The ungated seed. `created.id` is the id `files.create` just returned,
    // never anything the caller supplied — see this module's header.
    match SheetsApi::new(sheets)
        .values_update(&created.id, SEED_RANGE, &opts.values, opts.input)
        .await
    {
        Ok(response) => finish(CreateResult::Created {
            file_id: created.id,
            seeded_cells: response.updated_cells,
        }),
        Err(err) => finish(CreateResult::CreatedValuesFailed {
            file_id: created.id,
            detail: format!("{err:#}"),
        }),
    }
}

fn record_attempt(outcome: &CreateOutcome, duration: Duration) {
    let error = match &outcome.result {
        CreateResult::Failed { detail } | CreateResult::CreatedValuesFailed { detail, .. } => {
            Some(detail.clone())
        }
        _ => None,
    };
    let decided_by = match &outcome.result {
        CreateResult::Blocked { decided_by } => decided_by.as_ref(),
        _ => None,
    };
    let (decided_by_folder_id, decided_by_depth) = write_gate::decided_by_log_fields(decided_by);
    let updated_cells = match &outcome.result {
        CreateResult::Created { seeded_cells, .. } => *seeded_cells,
        _ => None,
    };

    request_log::record_drive_mutation(DriveMutationOutcome {
        operation: "sheets-create",
        file_id: outcome.result.file_id().unwrap_or_default().to_string(),
        file_name: outcome.name.clone(),
        status: outcome.result.log_status().to_string(),
        resolved_folder_id: Some(outcome.parent_folder_id.clone()),
        decided_by_folder_id,
        decided_by_depth,
        updated_cells,
        error,
        duration,
        ..Default::default()
    });
}

/// Renders an outcome as a single human-readable line.
#[must_use]
pub fn describe(outcome: &CreateOutcome) -> String {
    let name = &outcome.name;
    let parent = &outcome.parent_folder_id;
    match &outcome.result {
        CreateResult::WouldCreate { rows, columns } if *rows == 0 => {
            format!("Would create: spreadsheet '{name}' in {parent}")
        }
        CreateResult::WouldCreate { rows, columns } => format!(
            "Would create: spreadsheet '{name}' in {parent}, seeded with {rows} row(s) x \
             {columns} column(s)"
        ),
        CreateResult::Blocked { decided_by } => match decided_by {
            Some(rule) => format!(
                "Blocked: '{name}' in {parent} — refused by rule on folder {} (depth {})",
                rule.folder_id, rule.depth
            ),
            None => format!(
                "Blocked: '{name}' in {parent} — refused by default policy (no matching rule)"
            ),
        },
        CreateResult::Created {
            file_id,
            seeded_cells,
        } => match seeded_cells {
            Some(cells) => {
                format!("Created: '{name}' ({file_id}) in {parent}, seeded {cells} cell(s)")
            }
            None => format!("Created: '{name}' ({file_id}) in {parent}"),
        },
        CreateResult::CreatedValuesFailed { file_id, detail } => format!(
            "Partially failed: created '{name}' ({file_id}) in {parent}, but writing its \
             values failed: {detail}. The spreadsheet exists and is empty — it cannot be \
             rolled back automatically."
        ),
        CreateResult::Failed { detail } => format!("Failed: '{name}' in {parent}: {detail}"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::sheets::client::SHEETS_API_URL;
    use crate::test_support::env::MapEnv;
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

    async fn clients(server: &wiremock::MockServer) -> (DriveClient, SheetsClient) {
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/token"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "test-token", "expires_in": 3600,
                })),
            )
            .mount(server)
            .await;
        let mut drive = DriveClient::new(&server.uri(), &test_credentials()).unwrap();
        crate::drive::client::test_support::replace_session(
            &mut drive,
            &test_credentials(),
            &format!("{}/token", server.uri()),
        );
        let env = MapEnv::new().with(SHEETS_API_URL, &server.uri());
        let sheets = SheetsClient::from_drive_client_with(&env, &drive).unwrap();
        (drive, sheets)
    }

    fn mount_folder(id: &str) -> wiremock::Mock {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!("/drive/v3/files/{id}")))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": id, "name": id,
                    "mimeType": "application/vnd.google-apps.folder", "parents": [],
                })),
            )
    }

    fn mount_create() -> wiremock::Mock {
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/drive/v3/files"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "new-sheet", "name": "Budget",
                    "mimeType": "application/vnd.google-apps.spreadsheet",
                })),
            )
    }

    fn allow_rule() -> FolderPermissionRule {
        FolderPermissionRule {
            folder_id: "parent-1".to_string(),
            recursive: true,
            allow: std::iter::once(DriveOperation::Create).collect(),
            deny: HashSet::default(),
        }
    }

    fn opts(values: Vec<Vec<String>>, dry_run: bool) -> CreateOptions {
        CreateOptions {
            name: "Budget".to_string(),
            parent_folder_id: "parent-1".to_string(),
            values,
            input: ValueInputOption::UserEntered,
            dry_run,
        }
    }

    #[tokio::test]
    async fn creates_a_spreadsheet_mime_type_without_values() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_folder("parent-1").mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/drive/v3/files"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "mimeType": "application/vnd.google-apps.spreadsheet",
            })))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "new-sheet", "name": "Budget"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        // No Sheets mock: with no --values there must be no seeding call.

        let outcome = create(&drive, &sheets, &opts(Vec::new(), false), &[allow_rule()]).await;
        assert_eq!(
            outcome.result,
            CreateResult::Created {
                file_id: "new-sheet".to_string(),
                seeded_cells: None,
            }
        );
    }

    #[tokio::test]
    async fn seeds_values_into_sheet1_a1_with_one_ungated_write() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        // `parent-1` is mounted once: the gate runs for Create only. A
        // second gate check for the seed would need it again, and its
        // absence here is what proves the seed is ungated.
        mount_folder("parent-1").expect(1).mount(&server).await;
        mount_create().mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/new-sheet/values/'Sheet1'!A1",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"updatedCells": 4})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let values = vec![
            vec!["a".to_string(), "b".to_string()],
            vec!["c".to_string(), "d".to_string()],
        ];
        let outcome = create(&drive, &sheets, &opts(values, false), &[allow_rule()]).await;
        assert_eq!(
            outcome.result,
            CreateResult::Created {
                file_id: "new-sheet".to_string(),
                seeded_cells: Some(4),
            }
        );
    }

    #[tokio::test]
    async fn a_create_only_rule_is_enough_to_seed() {
        // The consequence of the ungated seed: a folder granting `create`
        // but not `sheets-write` must still allow `--values`. If the seed
        // were gated this would come back Blocked.
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_folder("parent-1").mount(&server).await;
        mount_create().mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"updatedCells": 1})),
            )
            .mount(&server)
            .await;

        let values = vec![vec!["x".to_string()]];
        let outcome = create(&drive, &sheets, &opts(values, false), &[allow_rule()]).await;
        assert!(matches!(outcome.result, CreateResult::Created { .. }));
    }

    #[tokio::test]
    async fn a_failed_seed_surfaces_the_orphaned_file_id() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_folder("parent-1").mount(&server).await;
        mount_create().mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let values = vec![vec!["x".to_string()]];
        let outcome = create(&drive, &sheets, &opts(values, false), &[allow_rule()]).await;
        let CreateResult::CreatedValuesFailed { file_id, .. } = &outcome.result else {
            panic!("expected CreatedValuesFailed, got {:?}", outcome.result);
        };
        assert_eq!(file_id, "new-sheet");
        // The message must not read as a clean failure: the spreadsheet
        // exists and there is no delete API to undo it.
        let text = describe(&outcome);
        assert!(text.contains("Partially failed"), "{text}");
        assert!(text.contains("new-sheet"), "{text}");
        assert!(text.contains("cannot be rolled back"), "{text}");
    }

    #[tokio::test]
    async fn a_denied_parent_creates_nothing() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_folder("parent-1").mount(&server).await;
        // No create mock and no Sheets mock.
        let outcome = create(&drive, &sheets, &opts(Vec::new(), false), &[]).await;
        assert!(matches!(
            outcome.result,
            CreateResult::Blocked { decided_by: None }
        ));
    }

    #[tokio::test]
    async fn dry_run_reports_seed_dimensions_and_creates_nothing() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_folder("parent-1").mount(&server).await;
        // No create mock: a dry run must not create.
        let values = vec![vec!["a".to_string(), "b".to_string(), "c".to_string()]];
        let outcome = create(&drive, &sheets, &opts(values, true), &[allow_rule()]).await;
        assert_eq!(
            outcome.result,
            CreateResult::WouldCreate {
                rows: 1,
                columns: 3
            }
        );
        assert!(describe(&outcome).contains("1 row(s) x 3 column(s)"));
    }

    #[tokio::test]
    async fn ancestor_chain_fetch_failure_produces_failed_not_allow() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/parent-1"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let outcome = create(&drive, &sheets, &opts(Vec::new(), false), &[allow_rule()]).await;
        assert!(matches!(outcome.result, CreateResult::Failed { .. }));
    }

    #[test]
    fn file_id_is_exposed_for_both_created_variants() {
        assert_eq!(
            CreateResult::Created {
                file_id: "a".to_string(),
                seeded_cells: None
            }
            .file_id(),
            Some("a")
        );
        assert_eq!(
            CreateResult::CreatedValuesFailed {
                file_id: "b".to_string(),
                detail: String::new()
            }
            .file_id(),
            Some("b")
        );
        assert_eq!(
            CreateResult::Failed {
                detail: String::new()
            }
            .file_id(),
            None
        );
    }
}
