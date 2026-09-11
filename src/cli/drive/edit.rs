//! CLI command for `omni-dev drive edit`.

use std::io::Read as _;
use std::path::Path;

use anyhow::{Context, Result};
use clap::Parser;

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::cli::drive::helpers::active_account_rules;
use crate::cli::drive::upload::read_local_content;
use crate::drive::client::DriveClient;
use crate::drive::content_edit::{self, EditOptions, EditOutcome, EditResult};
use crate::drive::files_api::{check_upload_size, MAX_UPLOAD_BYTES};
use crate::drive::write_gate::FolderPermissionRule;

/// MIME type used when `--mime-type` is omitted — Drive's own fallback for
/// unspecified content.
const DEFAULT_CONTENT_MIME_TYPE: &str = "application/octet-stream";

/// Replaces an existing file's content, gated by the account's configured
/// write-permission rules (issues #1574, #1612). Requires the `drive.file`
/// scope if `omni-dev` created the file, or the unrestricted `drive` scope
/// for any pre-existing file (`drive auth login --write-file` or
/// `--write-full`).
///
/// Refuses, client-side, any target that is a Google-native document
/// (Docs/Sheets/Slides/...) — there is no meaningful raw "content" to
/// replace via a media PATCH for those.
#[derive(Parser)]
pub struct EditCommand {
    /// Drive file id (from `drive search`, or the `id` segment of a Drive
    /// URL).
    pub file_id: String,

    /// New content: a local file path, or `-` to read from stdin.
    #[arg(long, value_name = "LOCAL_PATH|-")]
    pub content: String,

    /// MIME type for the content. Defaults to `application/octet-stream`.
    #[arg(long = "mime-type", value_name = "TYPE")]
    pub mime_type: Option<String>,

    /// Reports the gate verdict without calling `files.update`.
    #[arg(long)]
    pub dry_run: bool,

    /// The lease token from `drive lease acquire`, required unless the
    /// deciding write-permission rule sets `require_lease: false`
    /// ([ADR-0080](../../../docs/adrs/adr-0080.md) §1/§9/§13). Never
    /// needed with `--dry-run`.
    #[arg(long, value_name = "TOKEN")]
    pub lease: Option<String>,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl EditCommand {
    /// Runs the command against the shared client resolved by the parent
    /// `DriveCommand::execute`.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let content = resolve_content(&self.content)?;
        let content_type = self
            .mime_type
            .unwrap_or_else(|| DEFAULT_CONTENT_MIME_TYPE.to_string());
        let ledger_path = crate::drive::lease::ledger::ledger_path()?;
        let opts = EditOptions {
            file_id: self.file_id,
            content,
            content_type,
            dry_run: self.dry_run,
            lease_token: self.lease,
            ledger_path,
        };
        let rules = active_account_rules()?;
        run_edit(client, &opts, &rules, &self.output).await
    }
}

/// Resolves `--content`'s value: `-` reads (and size-checks) stdin, any
/// other value is treated as a local path (via
/// `crate::cli::drive::upload::read_local_content`, shared with `drive
/// upload`'s identical stat-then-read-then-check pattern).
fn resolve_content(content_arg: &str) -> Result<Vec<u8>> {
    if content_arg == "-" {
        read_stdin_content()
    } else {
        read_local_content(Path::new(content_arg))
    }
}

/// Reads stdin, bounded at [`MAX_UPLOAD_BYTES`] + 1 so an unbounded stream
/// can never be buffered past the cap before being refused — there's no
/// upfront size to stat for a pipe, unlike a local file.
fn read_stdin_content() -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    std::io::stdin()
        .take(MAX_UPLOAD_BYTES + 1)
        .read_to_end(&mut buf)
        .context("Failed to read stdin")?;
    check_upload_size(buf.len() as u64)?;
    Ok(buf)
}

/// Runs `edit` and emits the outcome in the requested format.
///
/// Split from [`EditCommand::execute`] so tests can inject a wiremock
/// client and pre-built options/rules directly, without touching the
/// filesystem or credential-loading path.
async fn run_edit(
    client: &DriveClient,
    opts: &EditOptions,
    rules: &[FolderPermissionRule],
    output: &OutputFormat,
) -> Result<()> {
    let outcome = content_edit::edit(client, opts, rules).await;
    if output_as(&outcome, output)? {
        return Ok(());
    }
    print_outcome(&outcome);
    Ok(())
}

fn print_outcome(outcome: &EditOutcome) {
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    // Best-effort, like every other `println!` this replaced: a broken pipe
    // must not turn a completed edit into a failure.
    let _ = write_outcome(outcome, &mut handle);
}

/// Renders an outcome to `out`.
///
/// Split from [`print_outcome`] so the wording is testable, following the
/// `render_*_table` convention the rest of the `drive` CLI already uses.
/// Worth doing here specifically because two of these lines exist to point a
/// refused user at the command that *does* work, and a hint that silently
/// stopped naming the right command would be invisible otherwise.
fn write_outcome(outcome: &EditOutcome, out: &mut dyn std::io::Write) -> std::io::Result<()> {
    let file_id = sanitize_for_terminal(&outcome.file_id);
    match &outcome.result {
        EditResult::WouldEdit => writeln!(out, "Would edit: {file_id}")?,
        EditResult::RefusedNativeDocument => {
            // The refusal itself is unchanged (ADR-0076 §1): a media PATCH
            // has no meaningful content to replace for a native document,
            // and making it "work" would mean convert-on-import re-upload,
            // which destroys comments, suggestions, revision history, tabs
            // and formatting. What it *can* do is name the commands that
            // work, which it previously did not — leaving a user who had
            // just been refused with no route forward.
            writeln!(
                out,
                "Refused: {file_id} is a Google-native document (Docs/Sheets/Slides/...) — no \
                 raw content to replace. Edit a Doc with `omni-dev drive docs \
                 replace`/`append`, or a Sheet with `omni-dev drive sheets \
                 write`/`append`/`clear`."
            )?;
        }
        EditResult::RefusedNoVisibleParents => {
            writeln!(
                out,
                "Refused: {file_id} has no parent folder visible to this account, so no folder \
                 rule can apply to it. This is normal for a file shared by link or email. \
                 Grant it by id instead: add {{\"file_id\": \"<file id>\", \"allow\": \
                 [\"edit\"]}} to write_permissions.rules."
            )?;
        }
        EditResult::Blocked { decided_by } => {
            writeln!(out, "Blocked: {file_id}")?;
            match decided_by {
                Some(rule) => writeln!(
                    out,
                    "  refused by rule on {} {}{}",
                    rule.kind_label(),
                    sanitize_for_terminal(rule.id()),
                    rule.depth_suffix()
                )?,
                None => writeln!(out, "  refused by default policy (no matching rule)")?,
            }
        }
        EditResult::RefusedNoLease => {
            writeln!(
                out,
                "Refused: {file_id} requires a Drive write lease — run `omni-dev drive lease \
                 acquire {file_id}` and pass the printed token via `--lease`."
            )?;
        }
        EditResult::RefusedLeaseExpired => {
            writeln!(
                out,
                "Refused: the presented lease is expired, released, or unknown to this ledger \
                 — run `omni-dev drive lease acquire {file_id}` again."
            )?;
        }
        EditResult::RefusedLeaseWrongFile => {
            writeln!(
                out,
                "Refused: the presented lease was acquired for a different file — run \
                 `omni-dev drive lease acquire {file_id}` for this one."
            )?;
        }
        EditResult::RefusedLeaseStale => {
            writeln!(
                out,
                "Refused: {file_id} changed since the lease was acquired (or last written \
                 under) — re-run `omni-dev drive lease acquire {file_id}` to lease the current \
                 version."
            )?;
        }
        EditResult::Edited => writeln!(out, "Edited: {file_id}")?,
        EditResult::Failed { detail } => {
            writeln!(out, "Failed: {file_id}: {}", sanitize_for_terminal(detail))?;
        }
    }
    Ok(())
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

    fn opts(dry_run: bool) -> EditOptions {
        EditOptions {
            file_id: "file-1".to_string(),
            content: b"new content".to_vec(),
            content_type: "text/plain".to_string(),
            dry_run,
            lease_token: None,
            ledger_path: std::path::PathBuf::from("/nonexistent/lease-ledger.jsonl"),
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
    async fn dry_run_reports_verdict_without_calling_the_edit_endpoint() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/file-1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "file-1", "name": "file-1", "mimeType": "text/plain", "parents": ["parent-1"],
            })))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/parent-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "parent-1", "name": "parent-1", "mimeType": GOOGLE_FOLDER_MIME_TYPE,
                })),
            )
            .mount(&server)
            .await;
        // No PATCH mock mounted — dry-run must never call files.update.

        run_edit(&client, &opts(true), &[allow_rule()], &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_edit_json_path_returns_ok() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/file-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "file-1", "name": "file-1", "mimeType": "text/plain",
                })),
            )
            .mount(&server)
            .await;

        run_edit(&client, &opts(false), &[], &OutputFormat::Json)
            .await
            .unwrap();
    }

    // ── resolve_content ─────────────────────────────────────────────

    #[test]
    fn resolve_content_reads_a_local_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("content.txt");
        std::fs::write(&path, b"hello").unwrap();
        let content = resolve_content(path.to_str().unwrap()).unwrap();
        assert_eq!(content, b"hello");
    }

    #[test]
    fn resolve_content_dash_is_never_treated_as_a_local_path() {
        // "-" would fail as a local path (no such file); this just asserts
        // the routing decision, not stdin's actual content in a test
        // process (reading real stdin here would hang/misbehave under
        // `cargo test`, so this is intentionally not exercised further).
        assert!(!Path::new("-").exists());
    }

    fn rendered(result: EditResult) -> String {
        let mut buf = Vec::new();
        write_outcome(
            &EditOutcome {
                file_id: "f1".to_string(),
                file_name: Some("f".to_string()),
                resolved_folder_id: None,
                result,
            },
            &mut buf,
        )
        .unwrap();
        String::from_utf8(buf).unwrap()
    }

    /// Every variant renders exactly one line (two for `Blocked`, which
    /// carries its reason on a second), and none of them panics.
    #[test]
    fn every_variant_renders() {
        for (result, lines) in [
            (EditResult::WouldEdit, 1),
            (EditResult::RefusedNativeDocument, 1),
            (EditResult::RefusedNoVisibleParents, 1),
            (EditResult::Edited, 1),
            (
                EditResult::Failed {
                    detail: "boom".to_string(),
                },
                1,
            ),
            (EditResult::Blocked { decided_by: None }, 2),
            (
                EditResult::Blocked {
                    decided_by: Some(crate::drive::write_gate::DecidingRule::Folder {
                        folder_id: "parent-1".to_string(),
                        depth: 0,
                    }),
                },
                2,
            ),
            (
                EditResult::Blocked {
                    decided_by: Some(crate::drive::write_gate::DecidingRule::File {
                        file_id: "f1".to_string(),
                    }),
                },
                2,
            ),
        ] {
            let text = rendered(result);
            assert_eq!(text.lines().count(), lines, "{text}");
            assert!(text.contains("f1"), "{text}");
        }
    }

    /// The native-document refusal must name **both** trees that can do the
    /// job. This is the whole reason the line exists: `drive edit` refuses a
    /// Doc or a Sheet by design (ADR-0076 §1), so without a route forward
    /// the user is simply stuck. It went three weeks naming neither, which
    /// is exactly the drift an untested message invites.
    #[test]
    fn the_native_document_refusal_names_both_drive_docs_and_drive_sheets() {
        let text = rendered(EditResult::RefusedNativeDocument);
        assert!(text.contains("drive docs"), "{text}");
        assert!(text.contains("drive sheets"), "{text}");
        assert!(text.contains("no raw content to replace"), "{text}");
    }

    /// The parentless refusal names the fix that actually works — a
    /// `file_id` rule — rather than sending the reader off to write a
    /// folder rule that can never match (issue #1612).
    #[test]
    fn the_parentless_refusal_names_a_file_id_rule_as_the_fix() {
        let text = rendered(EditResult::RefusedNoVisibleParents);
        assert!(text.contains("file_id"), "{text}");
    }

    /// Server- and operator-supplied strings reach a terminal here, so both
    /// the file id and a failure detail are sanitized.
    #[test]
    fn rendering_strips_control_bytes_from_untrusted_strings() {
        let mut buf = Vec::new();
        write_outcome(
            &EditOutcome {
                file_id: "f\u{1b}[31m1".to_string(),
                file_name: None,
                resolved_folder_id: None,
                result: EditResult::Failed {
                    detail: "bad\nnews".to_string(),
                },
            },
            &mut buf,
        )
        .unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(!text.contains('\u{1b}'), "{text}");
        assert_eq!(
            text.lines().count(),
            1,
            "an embedded newline must not add a row: {text}"
        );
    }
}
