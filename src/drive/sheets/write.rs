//! `drive sheets write`/`append`/`clear` engines — cell mutation gated by
//! the ADR-0071 folder write-permission rules (issue #1589,
//! [ADR-0073](../../../docs/adrs/adr-0073.md)).
//!
//! Single-target, so this follows `rename.rs`/`content_edit.rs`'s linear
//! shape — a public wrapper that logs, and an `_inner` that classifies then
//! mutates — rather than `file_move.rs`'s Plan/Execute split, which exists
//! to amortize a shared-destination fetch across a batch these verbs don't
//! have. `--dry-run` and a real run therefore share the same gate
//! classification *by construction* (same function, same early return), not
//! by convention.
//!
//! Two refusals happen **before** the gate, because they are not policy
//! decisions — the operation is simply nonsensical for that target:
//! anything that is not a spreadsheet, and a shortcut (even one pointing at
//! a spreadsheet, since we don't follow shortcuts). This is the mirror image
//! of `content_edit.rs`'s Google-native refusal.
//!
//! A third refusal is policy-adjacent but distinct: a target whose `parents`
//! are not visible. See [`WriteResult::RefusedNoVisibleParents`].

use std::time::{Duration, Instant};

use serde::Serialize;

use crate::cli::drive::format::{write_scalar_jsonl, JsonlSerialize};
use crate::drive::client::DriveClient;
use crate::drive::files_api::FilesApi;
use crate::drive::folder_ancestry;
use crate::drive::sheets::a1;
use crate::drive::sheets::api::{SheetsApi, ValueInputOption};
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::types::UpdateValuesResponse;
use crate::drive::types::{GOOGLE_SHEET_MIME_TYPE, GOOGLE_SHORTCUT_MIME_TYPE};
use crate::drive::write_gate::{self, DecidingRule, DriveOperation, FolderPermissionRule};
use crate::request_log::{self, DriveMutationOutcome};

/// Which cell mutation to perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteVerb {
    /// Overwrite the range's values.
    Write,
    /// Append rows after the last row of the range's table.
    Append,
    /// Clear the range's values, leaving formatting intact.
    Clear,
}

impl WriteVerb {
    /// The `operation` this verb records in the request log.
    ///
    /// `build_drive_mutation_record` shapes `command` as `["drive",
    /// <operation>]`, so these read as `drive sheets-write` in the log even
    /// though the CLI spells them `drive sheets write`.
    const fn log_operation(self) -> &'static str {
        match self {
            Self::Write => "sheets-write",
            Self::Append => "sheets-append",
            Self::Clear => "sheets-clear",
        }
    }

    /// Human-readable present-tense verb for CLI output.
    const fn label(self) -> &'static str {
        match self {
            Self::Write => "write",
            Self::Append => "append",
            Self::Clear => "clear",
        }
    }
}

/// Per-call options.
#[derive(Debug, Clone)]
pub struct WriteOptions {
    /// Spreadsheet id.
    pub spreadsheet_id: String,
    /// Which mutation to perform.
    pub verb: WriteVerb,
    /// An explicit A1 range, which may carry its own `Sheet!` prefix.
    pub range: Option<String>,
    /// A sheet title, supplying a prefix for a bare `range`.
    pub sheet: Option<String>,
    /// Row-major values to write. Empty for [`WriteVerb::Clear`].
    pub values: Vec<Vec<String>>,
    /// How the API should interpret the values.
    pub input: ValueInputOption,
    /// Classify only; never call a mutating endpoint.
    pub dry_run: bool,
}

/// What happened (or, under `--dry-run`, would happen).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum WriteResult {
    /// `--dry-run`, and the gate would allow it.
    WouldWrite {
        /// Rows of input parsed, so a transposed or ragged input is visible
        /// before it lands.
        rows: usize,
        /// Widest row, likewise.
        columns: usize,
    },
    /// The target is not a Google Sheet. Checked client-side before the
    /// gate: writing cells into a PDF isn't disallowed, it's meaningless.
    RefusedNotASpreadsheet {
        /// The target's actual MIME type.
        mime_type: String,
    },
    /// The target is a shortcut. Given its own variant rather than falling
    /// into `RefusedNotASpreadsheet`, because "this is not a spreadsheet" is
    /// a confusing thing to say about a shortcut *to* a spreadsheet — the
    /// same distinction `drive read --content` already draws.
    RefusedShortcut,
    /// The target has no parents this account can see, so the folder gate
    /// has no ancestor chain to evaluate and no rule could ever grant it.
    ///
    /// Distinct from `Blocked { decided_by: None }`, which means "a chain
    /// was resolved and no rule matched". Conflating them would tell an
    /// operator to fix their rules when no rule they could write would help
    /// — the fix is to put the Sheet in a folder they control. This is the
    /// common shape for a Sheet shared by link or email.
    RefusedNoVisibleParents,
    /// The folder write-permission gate refused it.
    Blocked {
        /// The rule that decided the refusal, if any (`None` means the bare
        /// default policy — every write defaults deny).
        decided_by: Option<DecidingRule>,
    },
    /// The mutation succeeded.
    ///
    /// Every count is optional because the API may omit it — which is why
    /// [`describe`] reads the *verb* to decide what happened rather than
    /// inferring "a clear" from an absent cell count.
    Written {
        /// The server-normalised range actually written or cleared.
        #[serde(skip_serializing_if = "Option::is_none")]
        updated_range: Option<String>,
        /// Rows the API reported changing. `None` for a clear, which
        /// reports no counts at all.
        #[serde(skip_serializing_if = "Option::is_none")]
        updated_rows: Option<i64>,
        /// Columns the API reported changing.
        #[serde(skip_serializing_if = "Option::is_none")]
        updated_columns: Option<i64>,
        /// Cells the API reported changing.
        #[serde(skip_serializing_if = "Option::is_none")]
        updated_cells: Option<i64>,
    },
    /// An API or validation error.
    Failed {
        /// A human-readable summary of what failed.
        detail: String,
    },
}

impl WriteResult {
    /// The `status` string the request log records.
    ///
    /// Hand-written rather than derived from the `#[serde(tag)]` shape,
    /// matching `MoveResult`/`EditResult`'s precedent of keeping the log's
    /// vocabulary decoupled from the wire format.
    fn log_status(&self) -> &'static str {
        match self {
            Self::WouldWrite { .. } => "would-write",
            Self::RefusedNotASpreadsheet { .. } => "refused-not-a-spreadsheet",
            Self::RefusedShortcut => "refused-shortcut",
            Self::RefusedNoVisibleParents => "refused-no-visible-parents",
            Self::Blocked { .. } => "blocked",
            Self::Written { .. } => "written",
            Self::Failed { .. } => "failed",
        }
    }
}

/// The full outcome of one attempt.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WriteOutcome {
    /// The spreadsheet acted on.
    pub spreadsheet_id: String,
    /// Its name, when the metadata fetch got that far.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    /// The composed A1 range, when composition succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<String>,
    /// The folder the gate evaluated against, when exactly one parent
    /// resolved it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_folder_id: Option<String>,
    /// What happened.
    pub result: WriteResult,
}

impl JsonlSerialize for WriteOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> anyhow::Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Runs one cell mutation, logging every attempt that isn't a dry run.
///
/// Never returns `Err`: every failure is a [`WriteResult`] variant, so the
/// caller renders one shape and the log records one shape. Exit code stays 0
/// regardless, matching ADR-0070 §10 / ADR-0071 §12.
pub async fn write(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &WriteOptions,
    rules: &[FolderPermissionRule],
) -> WriteOutcome {
    let started = Instant::now();
    let outcome = write_inner(drive, sheets, opts, rules).await;
    // The single logging site, and the only reason a dry run leaves no
    // record — `write_inner` never logs.
    if !opts.dry_run {
        record_attempt(&outcome, opts, started.elapsed());
    }
    outcome
}

async fn write_inner(
    drive: &DriveClient,
    sheets: &SheetsClient,
    opts: &WriteOptions,
    rules: &[FolderPermissionRule],
) -> WriteOutcome {
    let bare = |result| WriteOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: None,
        range: None,
        resolved_folder_id: None,
        result,
    };

    // Compose the range first: it is pure, and a conflicting
    // --sheet/--range pair should fail without spending a request.
    let range = match a1::compose(opts.sheet.as_deref(), opts.range.as_deref()) {
        Ok(range) => range,
        Err(err) => {
            return bare(WriteResult::Failed {
                detail: err.to_string(),
            })
        }
    };

    let files_api = FilesApi::new(drive);
    let target = match files_api.get_metadata(&opts.spreadsheet_id).await {
        Ok(target) => target,
        Err(err) => {
            return bare(WriteResult::Failed {
                detail: err.to_string(),
            })
        }
    };

    let with_target = |result| WriteOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: Some(target.name.clone()),
        range: Some(range.clone()),
        resolved_folder_id: None,
        result,
    };

    // ── Refusals that precede the gate ─────────────────────────────────
    if target.mime_type == GOOGLE_SHORTCUT_MIME_TYPE {
        return with_target(WriteResult::RefusedShortcut);
    }
    if target.mime_type != GOOGLE_SHEET_MIME_TYPE {
        return with_target(WriteResult::RefusedNotASpreadsheet {
            mime_type: target.mime_type.clone(),
        });
    }
    if target.parents.is_empty() {
        return with_target(WriteResult::RefusedNoVisibleParents);
    }

    // ── The gate ───────────────────────────────────────────────────────
    let (decision, resolved_folder_id) = match folder_ancestry::resolve_decision_for_parents(
        &files_api,
        &target.parents,
        DriveOperation::SheetsWrite,
        rules,
    )
    .await
    {
        Ok(pair) => pair,
        // A chain that could not be resolved is a refusal, never a silent
        // allow — ADR-0071 §3's highest-priority invariant.
        Err(err) => {
            return with_target(WriteResult::Failed {
                detail: err.to_string(),
            })
        }
    };

    let gated = |result| WriteOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        file_name: Some(target.name.clone()),
        range: Some(range.clone()),
        resolved_folder_id: resolved_folder_id.clone(),
        result,
    };

    if decision.verdict == write_gate::Verdict::Deny {
        return gated(WriteResult::Blocked {
            decided_by: decision.decided_by,
        });
    }

    if opts.dry_run {
        return gated(WriteResult::WouldWrite {
            rows: opts.values.len(),
            columns: opts.values.iter().map(Vec::len).max().unwrap_or(0),
        });
    }

    // ── The mutation ───────────────────────────────────────────────────
    let api = SheetsApi::new(sheets);
    let result = match opts.verb {
        WriteVerb::Write => api
            .values_update(&opts.spreadsheet_id, &range, &opts.values, opts.input)
            .await
            .map(into_written),
        WriteVerb::Append => api
            .values_append(&opts.spreadsheet_id, &range, &opts.values, opts.input)
            .await
            .map(|response| into_written(response.updates.unwrap_or_default())),
        WriteVerb::Clear => api
            .values_clear(&opts.spreadsheet_id, &range)
            .await
            .map(|response| WriteResult::Written {
                updated_range: response.cleared_range,
                updated_rows: None,
                updated_columns: None,
                updated_cells: None,
            }),
    };

    gated(result.unwrap_or_else(|err| WriteResult::Failed {
        detail: format!("{err:#}"),
    }))
}

/// Carries **every** count the API reported through to the outcome, not just
/// the cell count: `updated_rows`/`updated_columns` are what let a
/// transposed write be spotted in the request log after the fact, and
/// `docs/log.md` documents them as recorded.
fn into_written(response: UpdateValuesResponse) -> WriteResult {
    WriteResult::Written {
        updated_range: response.updated_range,
        updated_rows: response.updated_rows,
        updated_columns: response.updated_columns,
        updated_cells: response.updated_cells,
    }
}

/// Emits the `kind: "drivemutation"` record.
///
/// Inside the engine, never the CLI layer, so a future MCP caller cannot
/// bypass it — and so a `Blocked` outcome, which makes zero API calls, still
/// leaves a trace. Same reasoning as `content_edit.rs::record_attempt`.
fn record_attempt(outcome: &WriteOutcome, opts: &WriteOptions, duration: Duration) {
    let error = match &outcome.result {
        WriteResult::Failed { detail } => Some(detail.clone()),
        _ => None,
    };
    let decided_by = match &outcome.result {
        WriteResult::Blocked { decided_by } => decided_by.as_ref(),
        _ => None,
    };
    let (decided_by_folder_id, decided_by_depth) = write_gate::decided_by_log_fields(decided_by);
    let (updated_range, updated_rows, updated_columns, updated_cells) = match &outcome.result {
        WriteResult::Written {
            updated_range,
            updated_rows,
            updated_columns,
            updated_cells,
        } => (
            updated_range.clone(),
            *updated_rows,
            *updated_columns,
            *updated_cells,
        ),
        _ => (None, None, None, None),
    };

    request_log::record_drive_mutation(DriveMutationOutcome {
        operation: opts.verb.log_operation(),
        file_id: outcome.spreadsheet_id.clone(),
        file_name: outcome.file_name.clone().unwrap_or_default(),
        status: outcome.result.log_status().to_string(),
        resolved_folder_id: outcome.resolved_folder_id.clone(),
        decided_by_folder_id,
        decided_by_depth,
        range: outcome.range.clone(),
        updated_range,
        updated_rows,
        updated_columns,
        updated_cells,
        error,
        duration,
        ..Default::default()
    });
}

/// Renders an outcome as a single human-readable line.
///
/// Lives here rather than in the CLI layer so the CLI and a future MCP
/// caller describe an outcome identically.
#[must_use]
pub fn describe(outcome: &WriteOutcome, verb: WriteVerb) -> String {
    let name = outcome
        .file_name
        .as_deref()
        .unwrap_or(&outcome.spreadsheet_id);
    let range = outcome.range.as_deref().unwrap_or("(unresolved range)");
    match &outcome.result {
        // A clear carries no values, so its dimensions are always 0 x 0 —
        // printing them would be noise, not reassurance.
        WriteResult::WouldWrite { .. } if verb == WriteVerb::Clear => {
            format!("Would clear: {range} of '{name}'")
        }
        WriteResult::WouldWrite { rows, columns } => format!(
            "Would {}: {rows} row(s) x {columns} column(s) into {range} of '{name}'",
            verb.label()
        ),
        WriteResult::RefusedNotASpreadsheet { mime_type } => format!(
            "Refused: '{name}' is not a Google Sheet (mimeType: {mime_type}); \
             `drive sheets {}` only works on spreadsheets",
            verb.label()
        ),
        WriteResult::RefusedShortcut => format!(
            "Refused: '{name}' is a shortcut; `drive sheets {}` doesn't follow shortcuts — \
             resolve the target spreadsheet's id and use that instead",
            verb.label()
        ),
        WriteResult::RefusedNoVisibleParents => format!(
            "Refused: '{name}' has no parent folder visible to this account, so no \
             write-permission rule can apply to it. This is normal for a Sheet shared by link \
             or email. Add it to a folder in your own Drive, then grant that folder \
             `sheets-write`."
        ),
        WriteResult::Blocked { decided_by } => match decided_by {
            Some(rule) => format!(
                "Blocked: {range} of '{name}' — refused by rule on folder {} (depth {})",
                rule.folder_id, rule.depth
            ),
            None => format!(
                "Blocked: {range} of '{name}' — refused by default policy (no matching rule)"
            ),
        },
        WriteResult::Written {
            updated_range,
            updated_cells,
            ..
        } => {
            let where_ = updated_range.as_deref().unwrap_or(range);
            // Keyed on the verb, never on an absent cell count: the API is
            // allowed to omit the counts (see `UpdateValuesResponse`), and
            // inferring "a clear" from that would report a destructive
            // outcome for a write that was nothing of the sort.
            match (verb, updated_cells) {
                (WriteVerb::Clear, _) => format!("Cleared {where_} of '{name}'"),
                (_, Some(cells)) => format!("Wrote {cells} cell(s) to {where_} of '{name}'"),
                (_, None) => format!("Wrote to {where_} of '{name}'"),
            }
        }
        WriteResult::Failed { detail } => {
            format!("Failed: {range} of '{name}': {detail}")
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::sheets::client::SHEETS_API_URL;
    use crate::drive::write_gate::Verdict;
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

    /// Both clients against one wiremock server, sharing an OAuth session.
    ///
    /// `replace_session` must run before the derive: it swaps the Drive
    /// client's whole transport, so deriving first would leave the Sheets
    /// client pointed at the real `oauth2.googleapis.com`.
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
            folder_id: folder.to_string(),
            recursive: true,
            allow: std::iter::once(DriveOperation::SheetsWrite).collect(),
            deny: HashSet::default(),
        }
    }

    fn opts(verb: WriteVerb, dry_run: bool) -> WriteOptions {
        WriteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb,
            range: Some("A1:B2".to_string()),
            sheet: None,
            values: vec![vec!["a".to_string(), "b".to_string()]],
            input: ValueInputOption::UserEntered,
            dry_run,
        }
    }

    // ── refusals that must precede the gate and the network ────────────

    #[tokio::test]
    async fn non_spreadsheet_is_refused_before_any_gate_or_sheets_call() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", "application/pdf", &["parent-1"])
            .mount(&server)
            .await;
        // Deliberately no mock for parent-1 (the gate never runs) and none
        // for any Sheets endpoint — proves the refusal short-circuits both,
        // even though the rule set below would otherwise permit the write.
        let outcome = write(
            &drive,
            &sheets,
            &opts(WriteVerb::Write, false),
            &[allow_rule("parent-1")],
        )
        .await;
        assert!(matches!(
            outcome.result,
            WriteResult::RefusedNotASpreadsheet { .. }
        ));
    }

    #[tokio::test]
    async fn shortcut_is_refused_with_its_own_message_not_the_generic_one() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file(
            "sheet-1",
            "application/vnd.google-apps.shortcut",
            &["parent-1"],
        )
        .mount(&server)
        .await;
        let outcome = write(
            &drive,
            &sheets,
            &opts(WriteVerb::Write, false),
            &[allow_rule("parent-1")],
        )
        .await;
        assert!(matches!(outcome.result, WriteResult::RefusedShortcut));
        let text = describe(&outcome, WriteVerb::Write);
        assert!(text.contains("is a shortcut"), "{text}");
        assert!(!text.contains("is not a Google Sheet"), "{text}");
    }

    #[tokio::test]
    async fn a_sheet_with_no_visible_parents_is_refused_distinctly_from_a_blocked_one() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        // The shape a Sheet shared by link comes back as.
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &[])
            .mount(&server)
            .await;
        let outcome = write(&drive, &sheets, &opts(WriteVerb::Write, false), &[]).await;
        assert!(matches!(
            outcome.result,
            WriteResult::RefusedNoVisibleParents
        ));
        // The message must not send the operator off to fix rules that
        // could never apply.
        let text = describe(&outcome, WriteVerb::Write);
        assert!(text.contains("no parent folder visible"), "{text}");
        assert!(text.contains("Add it to a folder"), "{text}");
    }

    // ── the gate ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn denied_target_makes_zero_sheets_calls() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        // No Sheets mock: any call would 404 and surface as Failed.
        let outcome = write(&drive, &sheets, &opts(WriteVerb::Write, false), &[]).await;
        assert!(
            matches!(outcome.result, WriteResult::Blocked { decided_by: None }),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn an_edit_rule_alone_does_not_permit_a_cell_write() {
        // The consequence of ADR-0073 §3, asserted end-to-end: an existing
        // `allow: ["edit"]` rule must not silently grant cell writes.
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        let edit_only = FolderPermissionRule {
            folder_id: "parent-1".to_string(),
            recursive: true,
            allow: std::iter::once(DriveOperation::Edit).collect(),
            deny: HashSet::default(),
        };
        let outcome = write(
            &drive,
            &sheets,
            &opts(WriteVerb::Write, false),
            &[edit_only],
        )
        .await;
        assert!(matches!(outcome.result, WriteResult::Blocked { .. }));
    }

    #[tokio::test]
    async fn ancestor_chain_fetch_failure_produces_failed_not_allow() {
        // ADR-0071 §3's highest-priority invariant, inherited here: an
        // unresolvable chain must never read as "no rule applies".
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/parent-1"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let outcome = write(
            &drive,
            &sheets,
            &opts(WriteVerb::Write, false),
            &[allow_rule("parent-1")],
        )
        .await;
        assert!(matches!(outcome.result, WriteResult::Failed { .. }));
    }

    // ── dry run ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn dry_run_reports_dimensions_and_calls_no_values_endpoint() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        // No Sheets mock mounted.
        let mut o = opts(WriteVerb::Write, true);
        o.values = vec![
            vec!["a".to_string(), "b".to_string(), "c".to_string()],
            vec!["d".to_string()],
        ];
        let outcome = write(&drive, &sheets, &o, &[allow_rule("parent-1")]).await;
        assert_eq!(
            outcome.result,
            WriteResult::WouldWrite {
                rows: 2,
                columns: 3
            }
        );
        let text = describe(&outcome, WriteVerb::Write);
        assert!(text.contains("2 row(s) x 3 column(s)"), "{text}");
    }

    #[tokio::test]
    async fn dry_run_surfaces_the_same_blocked_reasoning_as_a_real_denied_run() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .expect(2)
            .mount(&server)
            .await;
        mount_folder("parent-1").expect(2).mount(&server).await;

        let dry = write(&drive, &sheets, &opts(WriteVerb::Write, true), &[]).await;
        let real = write(&drive, &sheets, &opts(WriteVerb::Write, false), &[]).await;
        assert_eq!(dry.result, real.result);
    }

    // ── successful mutations ───────────────────────────────────────────

    #[tokio::test]
    async fn allowed_write_calls_values_update_once_with_the_input_option() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/A1:B2",
            ))
            .and(wiremock::matchers::query_param(
                "valueInputOption",
                "USER_ENTERED",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "updatedRange": "'Q1'!A1:B2", "updatedRows": 1, "updatedCells": 2,
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let outcome = write(
            &drive,
            &sheets,
            &opts(WriteVerb::Write, false),
            &[allow_rule("parent-1")],
        )
        .await;
        assert_eq!(
            outcome.result,
            WriteResult::Written {
                updated_range: Some("'Q1'!A1:B2".to_string()),
                updated_rows: Some(1),
                updated_columns: None,
                updated_cells: Some(2),
            }
        );
        assert!(describe(&outcome, WriteVerb::Write).starts_with("Wrote 2 cell(s) "));
    }

    #[tokio::test]
    async fn raw_input_option_is_sent_when_requested() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::query_param("valueInputOption", "RAW"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(&server)
            .await;

        let mut o = opts(WriteVerb::Write, false);
        o.input = ValueInputOption::Raw;
        let outcome = write(&drive, &sheets, &o, &[allow_rule("parent-1")]).await;
        assert!(matches!(outcome.result, WriteResult::Written { .. }));
    }

    // ── describing an outcome ──────────────────────────────────────────

    #[tokio::test]
    async fn a_write_whose_response_omits_the_counts_is_not_described_as_a_clear() {
        // `UpdateValuesResponse` deliberately tolerates missing counts, so
        // "no cell count" must never stand in for "this was a clear" — that
        // would report a destructive outcome for a plain write.
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;

        let outcome = write(
            &drive,
            &sheets,
            &opts(WriteVerb::Write, false),
            &[allow_rule("parent-1")],
        )
        .await;
        let text = describe(&outcome, WriteVerb::Write);
        assert!(!text.contains("Cleared"), "{text}");
        assert!(text.starts_with("Wrote to "), "{text}");
    }

    #[tokio::test]
    async fn a_clear_dry_run_omits_the_always_zero_dimensions() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;

        let outcome = write(
            &drive,
            &sheets,
            &opts(WriteVerb::Clear, true),
            &[allow_rule("parent-1")],
        )
        .await;
        let text = describe(&outcome, WriteVerb::Clear);
        assert!(!text.contains("row(s)"), "{text}");
        assert_eq!(text, "Would clear: A1:B2 of 'sheet-1'");
    }

    #[tokio::test]
    async fn append_reads_its_counts_from_the_nested_updates_object() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/A1:B2:append",
            ))
            .and(wiremock::matchers::query_param(
                "insertDataOption",
                "INSERT_ROWS",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "tableRange": "'Q1'!A1:B3",
                    "updates": {"updatedRange": "'Q1'!A4:B4", "updatedCells": 2},
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let outcome = write(
            &drive,
            &sheets,
            &opts(WriteVerb::Append, false),
            &[allow_rule("parent-1")],
        )
        .await;
        assert_eq!(
            outcome.result,
            WriteResult::Written {
                updated_range: Some("'Q1'!A4:B4".to_string()),
                updated_rows: None,
                updated_columns: None,
                updated_cells: Some(2),
            }
        );
    }

    #[tokio::test]
    async fn clear_calls_the_clear_endpoint_and_reports_the_cleared_range() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/A1:B2:clear",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"clearedRange": "'Q1'!A1:B2"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let outcome = write(
            &drive,
            &sheets,
            &opts(WriteVerb::Clear, false),
            &[allow_rule("parent-1")],
        )
        .await;
        assert_eq!(
            outcome.result,
            WriteResult::Written {
                updated_range: Some("'Q1'!A1:B2".to_string()),
                updated_rows: None,
                updated_columns: None,
                updated_cells: None,
            }
        );
        assert!(describe(&outcome, WriteVerb::Clear).starts_with("Cleared "));
    }

    #[tokio::test]
    async fn a_403_surfaces_the_write_scope_hint() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        // The google.rpc envelope Sheets actually returns — the hint only
        // fires because `error_reason` understands `status` too.
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .respond_with(
                wiremock::ResponseTemplate::new(403).set_body_json(serde_json::json!({
                    "error": {"code": 403, "message": "The caller does not have permission",
                              "status": "PERMISSION_DENIED"},
                })),
            )
            .mount(&server)
            .await;

        let outcome = write(
            &drive,
            &sheets,
            &opts(WriteVerb::Write, false),
            &[allow_rule("parent-1")],
        )
        .await;
        let WriteResult::Failed { detail } = &outcome.result else {
            panic!("expected Failed, got {:?}", outcome.result);
        };
        assert!(detail.contains("--write-file"), "{detail}");
        assert!(detail.contains("--write-full"), "{detail}");
    }

    // ── range composition ──────────────────────────────────────────────

    #[tokio::test]
    async fn a_conflicting_sheet_and_range_fails_before_any_request() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        // No mocks at all — not even files.get.
        let mut o = opts(WriteVerb::Write, false);
        o.range = Some("Other!A1".to_string());
        o.sheet = Some("Mine".to_string());
        let outcome = write(&drive, &sheets, &o, &[]).await;
        let WriteResult::Failed { detail } = &outcome.result else {
            panic!("expected Failed, got {:?}", outcome.result);
        };
        assert!(detail.contains("already names a sheet"), "{detail}");
    }

    #[tokio::test]
    async fn a_sheet_title_with_a_space_is_encoded_on_the_wire() {
        let server = wiremock::MockServer::start().await;
        let (drive, sheets) = clients(&server).await;
        mount_file("sheet-1", GOOGLE_SHEET_MIME_TYPE, &["parent-1"])
            .mount(&server)
            .await;
        mount_folder("parent-1").mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/'My%20Sheet'!A1:B2",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(&server)
            .await;

        let mut o = opts(WriteVerb::Write, false);
        o.sheet = Some("My Sheet".to_string());
        let outcome = write(&drive, &sheets, &o, &[allow_rule("parent-1")]).await;
        assert!(matches!(outcome.result, WriteResult::Written { .. }));
    }

    // ── verb metadata ──────────────────────────────────────────────────

    #[test]
    fn log_operations_are_distinct_and_kebab_cased() {
        assert_eq!(WriteVerb::Write.log_operation(), "sheets-write");
        assert_eq!(WriteVerb::Append.log_operation(), "sheets-append");
        assert_eq!(WriteVerb::Clear.log_operation(), "sheets-clear");
    }

    #[test]
    fn gate_denies_sheets_write_by_default() {
        let decision =
            write_gate::resolve(&["folder".to_string()], DriveOperation::SheetsWrite, &[]);
        assert_eq!(decision.verdict, Verdict::Deny);
    }
}
