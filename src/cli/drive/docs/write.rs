//! CLI commands for `omni-dev drive docs replace` and `drive docs append`.

use std::io::Read;

use anyhow::{Context, Result};
use clap::Parser;

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::cli::drive::helpers;
use crate::drive::client::DriveClient;
use crate::drive::docs::client::DocsClient;
use crate::drive::docs::write::{describe, write, WriteOptions, WritePayload};
use crate::drive::write_gate::FolderPermissionRule;

/// Replaces every occurrence of some text in a Google Doc.
#[derive(Parser)]
pub struct ReplaceCommand {
    /// Document id (the `/d/<ID>/` segment of a Docs URL).
    pub document_id: String,

    /// The literal text to find. Not a regular expression.
    #[arg(long)]
    pub search: String,

    /// What to put in its place. May be empty, which deletes the match.
    #[arg(long)]
    pub replace: String,

    /// Match case-insensitively.
    ///
    /// Matching is **case-sensitive by default**, which inverts the Docs
    /// API's own default. Under Google's default, `--search it` also
    /// rewrites `It` and `IT` — in a verb with no undo.
    #[arg(long)]
    pub ignore_case: bool,

    /// Report what would change without sending anything.
    #[arg(long)]
    pub dry_run: bool,

    /// The lease token from `drive lease acquire`, required unless the
    /// deciding write-permission rule sets `require_lease: false`
    /// ([ADR-0080](../../../../docs/adrs/adr-0080.md) §1/§9/§13). Never
    /// needed with `--dry-run`.
    #[arg(long, value_name = "TOKEN")]
    pub lease: Option<String>,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

/// Appends text to the end of a Google Doc.
#[derive(Parser)]
pub struct AppendCommand {
    /// Document id (the `/d/<ID>/` segment of a Docs URL).
    pub document_id: String,

    /// The text to append.
    #[arg(
        long,
        conflicts_with = "text_file",
        required_unless_present = "text_file"
    )]
    pub text: Option<String>,

    /// Read the text to append from a file, or `-` for stdin.
    #[arg(long, value_name = "PATH")]
    pub text_file: Option<String>,

    /// Report what would change without sending anything.
    #[arg(long)]
    pub dry_run: bool,

    /// The lease token from `drive lease acquire`, required unless the
    /// deciding write-permission rule sets `require_lease: false`
    /// ([ADR-0080](../../../../docs/adrs/adr-0080.md) §1/§9/§13). Never
    /// needed with `--dry-run`.
    #[arg(long, value_name = "TOKEN")]
    pub lease: Option<String>,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ReplaceCommand {
    /// Runs the command, deriving a Docs client from the shared Drive one.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let docs = DocsClient::from_drive_client(client)?;
        let opts = WriteOptions {
            document_id: self.document_id,
            payload: WritePayload::Replace {
                search: self.search,
                replace: self.replace,
                match_case: !self.ignore_case,
            },
            dry_run: self.dry_run,
            lease_token: self.lease,
            ledger_path: resolve_ledger_path(self.dry_run)?,
        };
        let rules = helpers::active_account_rules()?;
        run_write(client, &docs, &opts, &rules, &self.output).await
    }
}

impl AppendCommand {
    /// Runs the command, deriving a Docs client from the shared Drive one.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let docs = DocsClient::from_drive_client(client)?;
        let text = match (self.text, self.text_file.as_deref()) {
            (Some(text), _) => text,
            (None, Some(source)) => read_text(source)?,
            // Unreachable: clap enforces one of the two.
            (None, None) => anyhow::bail!("one of --text or --text-file is required"),
        };
        let opts = WriteOptions {
            document_id: self.document_id,
            payload: WritePayload::Append { text },
            dry_run: self.dry_run,
            lease_token: self.lease,
            ledger_path: resolve_ledger_path(self.dry_run)?,
        };
        let rules = helpers::active_account_rules()?;
        run_write(client, &docs, &opts, &rules, &self.output).await
    }
}

/// Resolves the lease ledger path for one of this module's commands.
///
/// A dry run never checks a lease (`write_inner` returns a preview before
/// the ledger is ever touched, mirroring `drive edit`'s own `--dry-run`
/// reasoning) — resolving a real path here would make a purely read-only
/// preview depend on the state directory existing at all.
fn resolve_ledger_path(dry_run: bool) -> Result<std::path::PathBuf> {
    if dry_run {
        Ok(std::path::PathBuf::new())
    } else {
        crate::drive::lease::ledger::ledger_path()
    }
}

/// Reads append text from a file or stdin, bounded by the same cap
/// `drive upload`/`edit` use.
///
/// Mirrors `sheets/write.rs::read_values`, including the reason the stdin
/// branch differs: a pipe has no upfront size to stat, so it is read through
/// a `take(cap + 1)` and refused *after* one byte past the cap rather than
/// being buffered unboundedly first.
pub(crate) fn read_text(source: &str) -> Result<String> {
    let cap = crate::drive::files_api::MAX_UPLOAD_BYTES;
    if source == "-" {
        let mut buf = Vec::new();
        std::io::stdin()
            .take(cap + 1)
            .read_to_end(&mut buf)
            .context("Failed to read --text-file from stdin")?;
        anyhow::ensure!(
            buf.len() as u64 <= cap,
            "--text-file from stdin is over the {cap} byte cap"
        );
        return String::from_utf8(buf).context("--text-file from stdin is not valid UTF-8");
    }
    let metadata = std::fs::metadata(source)
        .with_context(|| format!("Failed to stat --text-file {source}"))?;
    anyhow::ensure!(
        metadata.len() <= cap,
        "--text-file {source} is {} bytes, over the {cap} byte cap",
        metadata.len()
    );
    std::fs::read_to_string(source).with_context(|| format!("Failed to read --text-file {source}"))
}

/// Runs the engine and renders the outcome.
///
/// Shared by both verbs so the gate wiring, `--dry-run` handling, rendering
/// and logging cannot drift between them. Split from each `execute` so tests
/// can inject wiremock clients and pre-built options without touching the
/// filesystem or the credential-loading path.
async fn run_write(
    drive: &DriveClient,
    docs: &DocsClient,
    opts: &WriteOptions,
    rules: &[FolderPermissionRule],
    output: &OutputFormat,
) -> Result<()> {
    let outcome = write(drive, docs, opts, rules).await;
    if output_as(&outcome, output)? {
        return Ok(());
    }
    // The engine's `describe` embeds operator-supplied ids raw, so the whole
    // rendered line is sanitized here — the same split `sheets write` uses.
    println!(
        "{}",
        sanitize_for_terminal(&describe(&outcome, opts.payload.verb()))
    );
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn resolve_ledger_path_for_a_real_run_resolves_the_ledger_path() {
        // A dry run short-circuits to an empty path (tested via `execute`
        // with `--dry-run`); a real run delegates to the shared resolver.
        let path = resolve_ledger_path(false).unwrap();
        assert!(path.ends_with("lease-ledger.jsonl"), "{}", path.display());
        assert_eq!(
            resolve_ledger_path(true).unwrap(),
            std::path::PathBuf::new()
        );
    }

    #[test]
    fn read_text_reads_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.txt");
        std::fs::write(&path, "hello world").unwrap();
        assert_eq!(read_text(path.to_str().unwrap()).unwrap(), "hello world");
    }

    #[test]
    fn read_text_refuses_a_file_over_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.txt");
        let oversized = vec![b'a'; (crate::drive::files_api::MAX_UPLOAD_BYTES + 1) as usize];
        std::fs::write(&path, oversized).unwrap();
        let err = read_text(path.to_str().unwrap()).unwrap_err().to_string();
        assert!(err.contains("over the"), "{err}");
    }

    #[test]
    fn read_text_reports_a_missing_file() {
        let err = read_text("/nonexistent/definitely-not-here.txt")
            .unwrap_err()
            .to_string();
        assert!(err.contains("Failed to stat"), "{err}");
    }

    #[test]
    fn read_text_refuses_non_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bin.txt");
        std::fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
        assert!(read_text(path.to_str().unwrap()).is_err());
    }

    /// `--ignore-case` inverts into the engine's `match_case`, and the
    /// default is the *safe* one rather than the API's.
    #[test]
    fn replace_defaults_to_case_sensitive_and_ignore_case_inverts_it() {
        use clap::Parser as _;

        let cmd =
            ReplaceCommand::try_parse_from(["replace", "d1", "--search", "a", "--replace", "b"])
                .unwrap();
        assert!(!cmd.ignore_case, "case-sensitive by default");

        let cmd = ReplaceCommand::try_parse_from([
            "replace",
            "d1",
            "--search",
            "a",
            "--replace",
            "b",
            "--ignore-case",
        ])
        .unwrap();
        assert!(cmd.ignore_case);
    }

    #[test]
    fn append_requires_exactly_one_of_text_or_text_file() {
        use clap::Parser as _;

        assert!(AppendCommand::try_parse_from(["append", "d1"]).is_err());
        assert!(AppendCommand::try_parse_from([
            "append",
            "d1",
            "--text",
            "x",
            "--text-file",
            "f.txt"
        ])
        .is_err());
        assert!(AppendCommand::try_parse_from(["append", "d1", "--text", "x"]).is_ok());
        assert!(AppendCommand::try_parse_from(["append", "d1", "--text-file", "f.txt"]).is_ok());
    }
}
