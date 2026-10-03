//! CLI commands for `omni-dev drive docs replace` and `drive docs append`.

use std::io::Read;

use anyhow::{Context, Result};
use clap::{ArgGroup, Parser};

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::cli::drive::helpers;
use crate::drive::client::DriveClient;
use crate::drive::docs::anchor::Side;
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

    #[command(flatten)]
    pub lease: crate::cli::drive::helpers::LeaseTokenArg,

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

    #[command(flatten)]
    pub lease: crate::cli::drive::helpers::LeaseTokenArg,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

/// Inserts text next to one unique anchor in tab bodies or a selected segment, under docs-write.
#[derive(Parser)]
#[command(group(ArgGroup::new("anchor").required(true).args(["before", "after"])))]
pub struct InsertCommand {
    /// Document id.
    pub document_id: String,
    /// Insert before this unique literal text (within one paragraph).
    #[arg(long)]
    pub before: Option<String>,
    /// Insert after this unique literal text (within one paragraph).
    #[arg(long)]
    pub after: Option<String>,
    /// Text to insert.
    #[arg(
        long,
        conflicts_with = "text_file",
        required_unless_present = "text_file"
    )]
    pub text: Option<String>,
    /// Read insertion text from a file, or `-` for stdin.
    #[arg(long, value_name = "PATH")]
    pub text_file: Option<String>,
    /// Existing header, footer or footnote ID (from docs read); omit for tab bodies.
    #[arg(long)]
    pub segment_id: Option<String>,
    /// Narrow a segment ID to this tab when it occurs in more than one tab.
    #[arg(long, requires = "segment_id")]
    pub tab_id: Option<String>,
    /// Match anchors case-insensitively using Unicode simple case folding.
    #[arg(long)]
    pub ignore_case: bool,
    /// Resolve and report the UTF-16 insertion point without writing.
    #[arg(long)]
    pub dry_run: bool,
    #[command(flatten)]
    pub lease: crate::cli::drive::helpers::LeaseTokenArg,
    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

/// Deletes one unique match or an inclusive anchor range, under docs-delete.
#[derive(Parser)]
#[command(group(ArgGroup::new("selection").required(true).args(["match_text", "from"])))]
pub struct DeleteCommand {
    /// Document id.
    pub document_id: String,
    /// Remove this unique literal match (within one paragraph).
    #[arg(long = "match", conflicts_with = "to")]
    pub match_text: Option<String>,
    /// Remove from the start of this unique anchor, inclusively.
    #[arg(long, requires = "to")]
    pub from: Option<String>,
    /// Remove through the end of this unique anchor, inclusively.
    #[arg(long, requires = "from")]
    pub to: Option<String>,
    /// Existing header, footer or footnote ID (from docs read); omit for tab bodies.
    #[arg(long)]
    pub segment_id: Option<String>,
    /// Narrow a segment ID to this tab when it occurs in more than one tab.
    #[arg(long, requires = "segment_id")]
    pub tab_id: Option<String>,
    /// Match anchors case-insensitively using Unicode simple case folding.
    #[arg(long)]
    pub ignore_case: bool,
    /// Resolve and report the UTF-16 range without writing.
    #[arg(long)]
    pub dry_run: bool,
    #[command(flatten)]
    pub lease: crate::cli::drive::helpers::LeaseTokenArg,
    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

/// CLI spelling of Google list presets, separate from engine wire types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum BulletPresetArg {
    /// Google `BULLET_DISC_CIRCLE_SQUARE` preset.
    #[value(name = "bullet-disc-circle-square")]
    BulletDiscCircleSquare,
    /// Google `BULLET_DIAMONDX_ARROW3D_SQUARE` preset.
    #[value(name = "bullet-diamondx-arrow3d-square")]
    BulletDiamondxArrow3DSquare,
    /// Google `BULLET_CHECKBOX` preset.
    #[value(name = "bullet-checkbox")]
    BulletCheckbox,
    /// Google `BULLET_ARROW_DIAMOND_DISC` preset.
    #[value(name = "bullet-arrow-diamond-disc")]
    BulletArrowDiamondDisc,
    /// Google `BULLET_STAR_CIRCLE_SQUARE` preset.
    #[value(name = "bullet-star-circle-square")]
    BulletStarCircleSquare,
    /// Google `BULLET_ARROW3D_CIRCLE_SQUARE` preset.
    #[value(name = "bullet-arrow3d-circle-square")]
    BulletArrow3DCircleSquare,
    /// Google `BULLET_LEFTTRIANGLE_DIAMOND_DISC` preset.
    #[value(name = "bullet-lefttriangle-diamond-disc")]
    BulletLefttriangleDiamondDisc,
    /// Google `BULLET_DIAMONDX_HOLLOWDIAMOND_SQUARE` preset.
    #[value(name = "bullet-diamondx-hollowdiamond-square")]
    BulletDiamondxHollowdiamondSquare,
    /// Google `BULLET_DIAMOND_CIRCLE_SQUARE` preset.
    #[value(name = "bullet-diamond-circle-square")]
    BulletDiamondCircleSquare,
    /// Google `NUMBERED_DECIMAL_ALPHA_ROMAN` preset.
    #[value(name = "numbered-decimal-alpha-roman")]
    NumberedDecimalAlphaRoman,
    /// Google `NUMBERED_DECIMAL_ALPHA_ROMAN_PARENS` preset.
    #[value(name = "numbered-decimal-alpha-roman-parens")]
    NumberedDecimalAlphaRomanParens,
    /// Google `NUMBERED_DECIMAL_NESTED` preset.
    #[value(name = "numbered-decimal-nested")]
    NumberedDecimalNested,
    /// Google `NUMBERED_UPPERALPHA_ALPHA_ROMAN` preset.
    #[value(name = "numbered-upperalpha-alpha-roman")]
    NumberedUpperalphaAlphaRoman,
    /// Google `NUMBERED_UPPERROMAN_UPPERALPHA_DECIMAL` preset.
    #[value(name = "numbered-upperroman-upperalpha-decimal")]
    NumberedUpperromanUpperalphaDecimal,
    /// Google `NUMBERED_ZERODECIMAL_ALPHA_ROMAN` preset.
    #[value(name = "numbered-zerodecimal-alpha-roman")]
    NumberedZerodecimalAlphaRoman,
}

impl From<BulletPresetArg> for crate::drive::docs::write_types::BulletPreset {
    fn from(preset: BulletPresetArg) -> Self {
        match preset {
            BulletPresetArg::BulletDiscCircleSquare => Self::BulletDiscCircleSquare,
            BulletPresetArg::BulletDiamondxArrow3DSquare => Self::BulletDiamondxArrow3DSquare,
            BulletPresetArg::BulletCheckbox => Self::BulletCheckbox,
            BulletPresetArg::BulletArrowDiamondDisc => Self::BulletArrowDiamondDisc,
            BulletPresetArg::BulletStarCircleSquare => Self::BulletStarCircleSquare,
            BulletPresetArg::BulletArrow3DCircleSquare => Self::BulletArrow3DCircleSquare,
            BulletPresetArg::BulletLefttriangleDiamondDisc => Self::BulletLefttriangleDiamondDisc,
            BulletPresetArg::BulletDiamondxHollowdiamondSquare => {
                Self::BulletDiamondxHollowdiamondSquare
            }
            BulletPresetArg::BulletDiamondCircleSquare => Self::BulletDiamondCircleSquare,
            BulletPresetArg::NumberedDecimalAlphaRoman => Self::NumberedDecimalAlphaRoman,
            BulletPresetArg::NumberedDecimalAlphaRomanParens => {
                Self::NumberedDecimalAlphaRomanParens
            }
            BulletPresetArg::NumberedDecimalNested => Self::NumberedDecimalNested,
            BulletPresetArg::NumberedUpperalphaAlphaRoman => Self::NumberedUpperalphaAlphaRoman,
            BulletPresetArg::NumberedUpperromanUpperalphaDecimal => {
                Self::NumberedUpperromanUpperalphaDecimal
            }
            BulletPresetArg::NumberedZerodecimalAlphaRoman => Self::NumberedZerodecimalAlphaRoman,
        }
    }
}

/// Whole-paragraph selection shared by list formatting verbs.
#[derive(clap::Args)]
#[command(group(ArgGroup::new("selection").required(true).args(["match_text", "from"])))]
pub struct ListSelection {
    /// Document id.
    pub document_id: String,
    /// Format the entire paragraph containing this unique literal match.
    #[arg(long = "match", conflicts_with = "to")]
    pub match_text: Option<String>,
    /// First paragraph anchor, inclusive.
    #[arg(long, requires = "to")]
    pub from: Option<String>,
    /// Last paragraph anchor, inclusive; must be in the same body or table cell.
    #[arg(long, requires = "from")]
    pub to: Option<String>,
    /// Match anchors with Unicode simple case folding.
    #[arg(long)]
    pub ignore_case: bool,
    /// Report the pre-write range and leading tabs removed, without writing.
    #[arg(long)]
    pub dry_run: bool,
    #[command(flatten)]
    pub lease: crate::cli::drive::helpers::LeaseTokenArg,
    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

/// Creates bullets or numbering under docs-write; leading tabs become nesting.
#[derive(Parser)]
pub struct CreateBulletsCommand {
    #[command(flatten)]
    pub selection: ListSelection,
    /// Concrete Google list preset. Matching preceding lists may be joined.
    #[arg(long, value_enum)]
    pub preset: BulletPresetArg,
}

/// Removes bullets under docs-write, preserving prose and visual indentation.
#[derive(Parser)]
pub struct DeleteBulletsCommand {
    #[command(flatten)]
    pub selection: ListSelection,
}

impl ListSelection {
    async fn execute(
        self,
        client: &DriveClient,
        preset: Option<crate::drive::docs::write_types::BulletPreset>,
    ) -> Result<()> {
        let docs = DocsClient::from_drive_client(client)?;
        let (from, to) = match (self.match_text, self.from, self.to) {
            (Some(text), None, None) => (text, None),
            (None, Some(from), Some(to)) => (from, Some(to)),
            _ => anyhow::bail!("use --match or both --from and --to"),
        };
        let opts = WriteOptions {
            document_id: self.document_id,
            payload: WritePayload::List {
                from,
                to,
                preset,
                match_case: !self.ignore_case,
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_write(
            client,
            &docs,
            &opts,
            &helpers::active_account_rules()?,
            &self.output,
        )
        .await
    }
}

impl CreateBulletsCommand {
    /// Apply list formatting through the shared gated write engine.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        self.selection
            .execute(client, Some(self.preset.into()))
            .await
    }
}

impl DeleteBulletsCommand {
    /// Remove list formatting through the shared gated write engine.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        self.selection.execute(client, None).await
    }
}

impl InsertCommand {
    /// Resolve an anchor and insert through the shared gated engine.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let docs = DocsClient::from_drive_client(client)?;
        let (anchor, side) = match (self.before, self.after) {
            (Some(anchor), None) => (anchor, Side::Before),
            (None, Some(anchor)) => (anchor, Side::After),
            _ => anyhow::bail!("exactly one of --before or --after is required"),
        };
        let text = match (self.text, self.text_file.as_deref()) {
            (Some(text), None) => text,
            (None, Some(source)) => read_text(source)?,
            _ => anyhow::bail!("exactly one of --text or --text-file is required"),
        };
        let opts = WriteOptions {
            document_id: self.document_id,
            payload: WritePayload::Insert {
                anchor,
                side,
                text,
                match_case: !self.ignore_case,
                selection: crate::drive::docs::anchor::SegmentSelection {
                    segment_id: self.segment_id,
                    tab_id: self.tab_id,
                },
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_write(
            client,
            &docs,
            &opts,
            &helpers::active_account_rules()?,
            &self.output,
        )
        .await
    }
}

impl DeleteCommand {
    /// Resolve a range and delete through the separately gated engine.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let docs = DocsClient::from_drive_client(client)?;
        let (from, to) = match (self.match_text, self.from, self.to) {
            (Some(text), None, None) => (text, None),
            (None, Some(from), Some(to)) => (from, Some(to)),
            _ => anyhow::bail!("use --match or both --from and --to"),
        };
        let opts = WriteOptions {
            document_id: self.document_id,
            payload: WritePayload::Delete {
                from,
                to,
                match_case: !self.ignore_case,
                selection: crate::drive::docs::anchor::SegmentSelection {
                    segment_id: self.segment_id,
                    tab_id: self.tab_id,
                },
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_write(
            client,
            &docs,
            &opts,
            &helpers::active_account_rules()?,
            &self.output,
        )
        .await
    }
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
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
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
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        let rules = helpers::active_account_rules()?;
        run_write(client, &docs, &opts, &rules, &self.output).await
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
    fn segment_selectors_parse_and_tab_requires_segment() {
        let insert = InsertCommand::try_parse_from([
            "insert",
            "doc",
            "--after",
            "anchor",
            "--text",
            "x",
            "--segment-id",
            "header",
            "--tab-id",
            "child",
        ])
        .unwrap();
        assert_eq!(insert.segment_id.as_deref(), Some("header"));
        assert_eq!(insert.tab_id.as_deref(), Some("child"));
        let delete = DeleteCommand::try_parse_from([
            "delete",
            "doc",
            "--match",
            "anchor",
            "--segment-id",
            "note",
        ])
        .unwrap();
        assert_eq!(delete.segment_id.as_deref(), Some("note"));
        assert!(InsertCommand::try_parse_from([
            "insert", "doc", "--after", "anchor", "--text", "x", "--tab-id", "child"
        ])
        .is_err());
        assert!(DeleteCommand::try_parse_from([
            "delete", "doc", "--match", "anchor", "--tab-id", "child"
        ])
        .is_err());
    }

    #[test]
    fn list_cli_requires_a_valid_selection_and_concrete_creation_preset() {
        assert!(
            CreateBulletsCommand::try_parse_from(["create-bullets", "d1", "--match", "a"]).is_err()
        );
        assert!(CreateBulletsCommand::try_parse_from([
            "create-bullets",
            "d1",
            "--match",
            "a",
            "--preset",
            "raw"
        ])
        .is_err());
        assert!(
            DeleteBulletsCommand::try_parse_from(["delete-bullets", "d1", "--from", "a"]).is_err()
        );
        assert!(
            DeleteBulletsCommand::try_parse_from(["delete-bullets", "d1", "--to", "a"]).is_err()
        );
        assert!(DeleteBulletsCommand::try_parse_from([
            "delete-bullets",
            "d1",
            "--match",
            "a",
            "--from",
            "b",
            "--to",
            "c"
        ])
        .is_err());
        assert!(DeleteBulletsCommand::try_parse_from(["delete-bullets", "d1"]).is_err());
        assert!(DeleteBulletsCommand::try_parse_from([
            "delete-bullets",
            "d1",
            "--match",
            "a",
            "--preset",
            "bullet-checkbox"
        ])
        .is_err());
        let cmd = CreateBulletsCommand::try_parse_from([
            "create-bullets",
            "d1",
            "--match",
            "a",
            "--preset",
            "bullet-checkbox",
        ])
        .unwrap();
        assert!(!cmd.selection.ignore_case);
        assert_eq!(
            crate::drive::docs::write_types::BulletPreset::from(cmd.preset),
            crate::drive::docs::write_types::BulletPreset::BulletCheckbox
        );
        let cmd = DeleteBulletsCommand::try_parse_from([
            "delete-bullets",
            "d1",
            "--from",
            "a",
            "--to",
            "b",
            "--ignore-case",
            "--dry-run",
        ])
        .unwrap();
        assert!(cmd.selection.ignore_case && cmd.selection.dry_run);
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
    #[test]
    fn insert_requires_one_anchor_and_one_text_source() {
        assert!(InsertCommand::try_parse_from(["insert", "d", "--text", "x"]).is_err());
        assert!(InsertCommand::try_parse_from([
            "insert", "d", "--before", "a", "--after", "b", "--text", "x"
        ])
        .is_err());
        assert!(InsertCommand::try_parse_from(["insert", "d", "--before", "a"]).is_err());
        assert!(InsertCommand::try_parse_from([
            "insert",
            "d",
            "--before",
            "a",
            "--text",
            "x",
            "--text-file",
            "f"
        ])
        .is_err());
        assert!(
            InsertCommand::try_parse_from(["insert", "d", "--after", "a", "--text-file", "f"])
                .is_ok()
        );
        let cmd =
            InsertCommand::try_parse_from(["insert", "d", "--before", "a", "--text", "x"]).unwrap();
        assert!(!cmd.ignore_case);
    }

    #[test]
    fn delete_requires_a_match_or_both_range_anchors() {
        for args in [
            vec!["delete", "d"],
            vec!["delete", "d", "--from", "a"],
            vec!["delete", "d", "--to", "b"],
            vec!["delete", "d", "--match", "x", "--to", "b"],
            vec!["delete", "d", "--match", "x", "--from", "a", "--to", "b"],
        ] {
            assert!(DeleteCommand::try_parse_from(args).is_err());
        }
        assert!(DeleteCommand::try_parse_from(["delete", "d", "--from", "a", "--to", "b"]).is_ok());
        assert!(
            !DeleteCommand::try_parse_from(["delete", "d", "--match", "x"])
                .unwrap()
                .ignore_case
        );
        assert!(
            DeleteCommand::try_parse_from(["delete", "d", "--match", "x", "--ignore-case"])
                .unwrap()
                .ignore_case
        );
    }
}
