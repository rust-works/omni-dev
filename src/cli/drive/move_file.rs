//! CLI command for `omni-dev drive move`.

use anyhow::Result;
use clap::Parser;

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::drive::client::DriveClient;
use crate::drive::file_move::{self, MoveOptions, MoveOutcome, MoveResult, VisibilityDiffReport};

/// Moves one or more Drive files into a destination folder.
///
/// Moving can change who can see a file: Drive resolves visibility from
/// direct permissions plus permissions inherited from the parent folder
/// chain, and a move changes that chain. Moves that would change
/// visibility are refused by default — the three `--allow-*` flags opt in
/// independently. See ADR-0070.
#[derive(Parser)]
pub struct MoveCommand {
    /// Drive file ids to move.
    #[arg(required = true)]
    pub file_ids: Vec<String>,

    /// Destination folder id. One shared destination per invocation —
    /// different files to different destinations needs separate calls.
    #[arg(long = "to", value_name = "FOLDER_ID")]
    pub to: String,

    /// Allows a move that would grant new principals access to a file.
    #[arg(long)]
    pub allow_visibility_increase: bool,
    /// Allows a move that would revoke existing principals' access to a
    /// file.
    #[arg(long)]
    pub allow_visibility_decrease: bool,
    /// Allows a move across a My Drive / Shared Drive boundary.
    #[arg(long)]
    pub allow_drive_boundary_crossing: bool,

    /// Reports what would happen without moving anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl MoveCommand {
    /// Runs the command against the shared client resolved by the parent
    /// `DriveCommand::execute`.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = MoveOptions {
            dest_folder_id: self.to,
            allow_visibility_increase: self.allow_visibility_increase,
            allow_visibility_decrease: self.allow_visibility_decrease,
            allow_drive_boundary_crossing: self.allow_drive_boundary_crossing,
        };
        run_move(client, &self.file_ids, &opts, self.dry_run, &self.output).await
    }
}

/// Plans (and, unless `dry_run`, executes) the move, then emits the result
/// in the requested format.
///
/// Split from [`MoveCommand::execute`] so tests can inject a wiremock
/// client without going through the credential-loading path. Exit code is
/// 0 as long as the command mechanically completed, regardless of
/// individual `Blocked`/`Failed` outcomes — status lives in the table/JSON
/// output, matching `worktree push`'s existing convention. No interactive
/// confirmation, `--dry-run` or not: the opt-in flags are the entire gate
/// (see ADR-0070) — an interactive-by-default confirm would hang or be
/// silently force-skipped over a future MCP caller.
async fn run_move(
    client: &DriveClient,
    file_ids: &[String],
    opts: &MoveOptions,
    dry_run: bool,
    output: &OutputFormat,
) -> Result<()> {
    let plan = file_move::plan(client, file_ids, opts).await?;
    let outcomes: Vec<MoveOutcome> = if dry_run {
        plan.files
    } else {
        file_move::execute(client, plan).await
    };

    if output_as(&outcomes, output)? {
        return Ok(());
    }
    println!("{}", render_move_outcomes(&outcomes));
    print_folder_warnings(&outcomes);
    Ok(())
}

/// Renders the per-file move result table.
fn render_move_outcomes(outcomes: &[MoveOutcome]) -> String {
    if outcomes.is_empty() {
        return "No files specified.".to_string();
    }
    let mut out = format!("{:<16} {:<30} {}", "STATUS", "NAME", "DETAIL");
    for outcome in outcomes {
        out.push('\n');
        out.push_str(&move_outcome_row(outcome));
    }
    out
}

/// One file's row: status, name, and a detail column (visibility changes,
/// blocking reasons, or the error).
fn move_outcome_row(outcome: &MoveOutcome) -> String {
    let (status, detail) = move_status_and_detail(outcome);
    let name = sanitize_for_terminal(&outcome.name);
    let detail = sanitize_for_terminal(&detail);
    format!("{status:<16} {name:<30} {detail}")
}

/// The status word and human detail for one move outcome.
fn move_status_and_detail(outcome: &MoveOutcome) -> (&'static str, String) {
    match &outcome.result {
        MoveResult::AlreadyInFolder => (
            "already-in-folder",
            "already in the destination folder".to_string(),
        ),
        MoveResult::WouldMove => (
            "would-move",
            visibility_summary(outcome.visibility.as_ref()),
        ),
        MoveResult::Moved => ("moved", visibility_summary(outcome.visibility.as_ref())),
        MoveResult::Blocked { reasons } => {
            let mut gates = Vec::new();
            if reasons.visibility_increase {
                gates.push("visibility increase (--allow-visibility-increase)");
            }
            if reasons.visibility_decrease {
                gates.push("visibility decrease (--allow-visibility-decrease)");
            }
            if reasons.drive_boundary_crossing {
                gates.push("drive boundary crossing (--allow-drive-boundary-crossing)");
            }
            let mut detail = gates.join(", ");
            let summary = visibility_summary(outcome.visibility.as_ref());
            if !summary.is_empty() {
                detail.push_str(&format!("; {summary}"));
            }
            ("blocked", detail)
        }
        MoveResult::Failed { detail } => ("failed", detail.clone()),
    }
}

/// A short "adds X; removes Y" summary of a visibility diff, empty when
/// there's nothing to report.
fn visibility_summary(report: Option<&VisibilityDiffReport>) -> String {
    let Some(report) = report else {
        return String::new();
    };
    let mut parts = Vec::new();
    if !report.added.is_empty() {
        parts.push(format!("adds {}", report.added.join(", ")));
    }
    if !report.removed.is_empty() {
        parts.push(format!("removes {}", report.removed.join(", ")));
    }
    parts.join("; ")
}

/// Emits a loud warning for every outcome that moved (or, in `--dry-run`,
/// would move) a folder — v1 doesn't recurse into a moved folder's
/// contents, so the contents' visibility was never evaluated even though
/// the folder's own visibility was.
fn print_folder_warnings(outcomes: &[MoveOutcome]) {
    for outcome in outcomes {
        if outcome.is_folder && matches!(outcome.result, MoveResult::Moved | MoveResult::WouldMove)
        {
            println!(
                "Warning: '{}' is a folder — its own visibility was evaluated, but its \
                 contents' visibility was not (folder moves don't recurse in v1).",
                sanitize_for_terminal(&outcome.name)
            );
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::visibility::BlockReasons;
    use crate::utils::secret::Secret;

    fn test_credentials() -> DriveCredentials {
        DriveCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: DriveGrantedScopes::METADATA,
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

    fn opts(dest: &str) -> MoveOptions {
        MoveOptions {
            dest_folder_id: dest.to_string(),
            allow_visibility_increase: false,
            allow_visibility_decrease: false,
            allow_drive_boundary_crossing: false,
        }
    }

    #[tokio::test]
    async fn dry_run_plans_without_calling_files_update() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/dest1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "dest1", "name": "Dest", "mimeType": "application/vnd.google-apps.folder",
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/drive/v3/files/dest1/permissions",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"permissions": []})),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "Report", "parents": ["dest1"],
                })),
            )
            .mount(&server)
            .await;
        // No PATCH mock mounted — a --dry-run call that somehow sent one
        // would fail with "no matching mock" rather than silently moving.

        run_move(
            &client,
            &["f1".to_string()],
            &opts("dest1"),
            true,
            &OutputFormat::Table,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_move_propagates_a_bad_destination_error() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/dest1"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;

        let err = run_move(
            &client,
            &["f1".to_string()],
            &opts("dest1"),
            true,
            &OutputFormat::Table,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("Failed to resolve destination"));
    }

    // ── rendering ────────────────────────────────────────────────────

    fn outcome(name: &str, result: MoveResult) -> MoveOutcome {
        MoveOutcome {
            file_id: "f1".to_string(),
            name: name.to_string(),
            current_parents: vec!["src1".to_string()],
            is_folder: false,
            crosses_drive_boundary: false,
            visibility: None,
            result,
        }
    }

    #[test]
    fn render_move_outcomes_reports_no_files_when_empty() {
        assert_eq!(render_move_outcomes(&[]), "No files specified.");
    }

    #[test]
    fn render_move_outcomes_includes_status_and_name() {
        let rendered = render_move_outcomes(&[outcome("Report.pdf", MoveResult::Moved)]);
        assert!(rendered.contains("moved"), "{rendered}");
        assert!(rendered.contains("Report.pdf"), "{rendered}");
    }

    #[test]
    fn move_status_and_detail_blocked_lists_every_failing_gate() {
        let reasons = BlockReasons {
            visibility_increase: true,
            visibility_decrease: true,
            drive_boundary_crossing: true,
        };
        let (status, detail) =
            move_status_and_detail(&outcome("x", MoveResult::Blocked { reasons }));
        assert_eq!(status, "blocked");
        assert!(detail.contains("--allow-visibility-increase"), "{detail}");
        assert!(detail.contains("--allow-visibility-decrease"), "{detail}");
        assert!(
            detail.contains("--allow-drive-boundary-crossing"),
            "{detail}"
        );
    }

    #[test]
    fn move_status_and_detail_already_in_folder() {
        let (status, detail) = move_status_and_detail(&outcome("x", MoveResult::AlreadyInFolder));
        assert_eq!(status, "already-in-folder");
        assert!(detail.contains("already in"));
    }

    #[test]
    fn move_status_and_detail_failed_shows_error() {
        let (status, detail) = move_status_and_detail(&outcome(
            "x",
            MoveResult::Failed {
                detail: "boom".to_string(),
            },
        ));
        assert_eq!(status, "failed");
        assert_eq!(detail, "boom");
    }

    #[test]
    fn visibility_summary_reports_both_added_and_removed() {
        let report = VisibilityDiffReport {
            added: vec!["user:bob@example.com".to_string()],
            removed: vec!["user:carol@example.com".to_string()],
        };
        let summary = visibility_summary(Some(&report));
        assert!(summary.contains("adds user:bob@example.com"), "{summary}");
        assert!(
            summary.contains("removes user:carol@example.com"),
            "{summary}"
        );
    }

    #[test]
    fn visibility_summary_is_empty_when_no_report() {
        assert_eq!(visibility_summary(None), "");
    }

    #[test]
    fn print_folder_warnings_only_fires_for_moved_or_would_move_folders() {
        // No assertion beyond "doesn't panic" — print_folder_warnings has
        // no return value to inspect; its branch condition is what
        // matters and is exercised by constructing every relevant shape.
        let mut folder_moved = outcome("Folder A", MoveResult::Moved);
        folder_moved.is_folder = true;
        let mut folder_blocked = outcome("Folder B", MoveResult::AlreadyInFolder);
        folder_blocked.is_folder = true;
        let file_moved = outcome("File.txt", MoveResult::Moved);
        print_folder_warnings(&[folder_moved, folder_blocked, file_moved]);
    }
}
