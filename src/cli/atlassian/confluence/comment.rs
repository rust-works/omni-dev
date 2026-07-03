//! CLI commands for Confluence page comments.

use std::io::{self, BufRead, Write};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

use crate::atlassian::adf::AdfDocument;
use crate::atlassian::adf_validated::{markdown_to_validated_adf, ValidatedAdfDocument};
use crate::atlassian::confluence_api::{
    CommentKind, ConfluenceApi, ConfluenceComment, InlineAnchor,
};
use crate::atlassian::convert::adf_to_markdown;
use crate::atlassian::document::JfmDocument;
use crate::atlassian::inline_comment::{
    audit_inline_comments, reanchor_inline_comment, CommentDrift, DriftStatus,
};
use crate::cli::atlassian::confirm::{guard_destructive_with_io, GuardOptions, GuardOutcome};
use crate::cli::atlassian::format::{output_as, ContentFormat, OutputFormat};
use crate::cli::atlassian::helpers::{create_client, read_input};

/// Manages comments on a Confluence page.
#[derive(Parser)]
pub struct CommentCommand {
    /// The comment subcommand to execute.
    #[command(subcommand)]
    pub command: CommentSubcommands,
}

/// Comment subcommands.
#[derive(Subcommand)]
pub enum CommentSubcommands {
    /// Lists comments on a Confluence page (mirrors the `confluence_comment_list` MCP tool).
    List(ListCommand),
    /// Adds a footer comment to a Confluence page (mirrors the `confluence_comment_add` MCP tool).
    Add(AddCommand),
    /// Adds an inline (anchored) comment to a Confluence page (mirrors the `confluence_comment_add_inline` MCP tool).
    AddInline(AddInlineCommand),
    /// Lists the replies of a comment (mirrors the `confluence_comment_replies` MCP tool).
    Replies(RepliesCommand),
    /// Audits inline comments for anchor drift (mirrors the `confluence_comment_audit` MCP tool).
    Audit(AuditCommand),
    /// Moves an inline comment's anchor to a new text run (mirrors the `confluence_comment_reanchor` MCP tool).
    Reanchor(ReanchorCommand),
}

impl CommentCommand {
    /// Executes the comment command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            CommentSubcommands::List(cmd) => cmd.execute().await,
            CommentSubcommands::Add(cmd) => cmd.execute().await,
            CommentSubcommands::AddInline(cmd) => cmd.execute().await,
            CommentSubcommands::Replies(cmd) => cmd.execute().await,
            CommentSubcommands::Audit(cmd) => cmd.execute().await,
            CommentSubcommands::Reanchor(cmd) => cmd.execute().await,
        }
    }
}

/// `--kind` filter for `confluence comment list`.
///
/// `All` (the default) issues both the footer and inline list calls and
/// concatenates the results so a single invocation surfaces every comment on
/// the page.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum CommentKindFilter {
    /// Page-level footer comments only.
    Footer,
    /// Inline (anchored) comments only.
    Inline,
    /// Both kinds, concatenated and sorted by creation time.
    All,
}

/// `--kind` selector for `confluence comment replies` (no `All` — replies live
/// on a kind-specific endpoint, so the caller must commit to one).
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum CommentKindArg {
    /// Replies under a footer comment.
    Footer,
    /// Replies under an inline comment.
    Inline,
}

impl From<CommentKindArg> for CommentKind {
    fn from(k: CommentKindArg) -> Self {
        match k {
            CommentKindArg::Footer => Self::Footer,
            CommentKindArg::Inline => Self::Inline,
        }
    }
}

/// Lists comments on a Confluence page.
///
/// Comment authors are returned as Atlassian account IDs — resolve them to
/// display names with `omni-dev atlassian confluence user get`.
#[derive(Parser)]
pub struct ListCommand {
    /// Confluence page ID.
    pub id: String,

    /// Which kind of comments to show.
    #[arg(long, value_enum, default_value_t = CommentKindFilter::All)]
    pub kind: CommentKindFilter,

    /// Maximum number of comments to display.
    #[arg(long, default_value_t = 25)]
    pub limit: usize,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ListCommand {
    /// Fetches and displays comments.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);
        run_list_comments(&api, &self.id, self.kind, self.limit, &self.output).await
    }
}

/// Adds a footer comment to a Confluence page.
#[derive(Parser)]
pub struct AddCommand {
    /// Confluence page ID.
    pub id: String,

    /// Input file (reads from stdin if omitted or "-").
    pub file: Option<String>,

    /// Input format.
    #[arg(long, value_enum, default_value_t = ContentFormat::Jfm)]
    pub format: ContentFormat,
}

impl AddCommand {
    /// Reads input, converts to ADF, and posts the comment.
    pub async fn execute(self) -> Result<()> {
        let adf = parse_comment_input(self.file.as_deref(), self.format)?;

        let (client, _instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);
        run_add_comment(&api, &self.id, &adf).await
    }
}

/// Adds an inline (anchored) comment to a Confluence page.
#[derive(Parser)]
pub struct AddInlineCommand {
    /// Confluence page ID.
    pub id: String,

    /// Input file (reads from stdin if omitted or "-").
    pub file: Option<String>,

    /// Input format.
    #[arg(long, value_enum, default_value_t = ContentFormat::Jfm)]
    pub format: ContentFormat,

    /// Exact text on the page that the comment should anchor to.
    #[arg(long)]
    pub anchor_text: String,

    /// 1-based occurrence to anchor to when `--anchor-text` appears more than
    /// once on the page. Required for ambiguous anchors; rejected if out of
    /// range.
    #[arg(long)]
    pub match_index: Option<usize>,
}

impl AddInlineCommand {
    /// Reads input, resolves the anchor, and posts the inline comment.
    pub async fn execute(self) -> Result<()> {
        let adf = parse_comment_input(self.file.as_deref(), self.format)?;

        let (client, _instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);
        let anchor = api
            .resolve_anchor(&self.id, &self.anchor_text, self.match_index)
            .await?;
        run_add_inline_comment(&api, &self.id, &adf, &anchor).await
    }
}

/// Lists the replies of a comment.
#[derive(Parser)]
pub struct RepliesCommand {
    /// Comment ID.
    pub id: String,

    /// Whether the parent is a footer or inline comment (the Confluence API
    /// requires this to pick the right endpoint).
    #[arg(long, value_enum)]
    pub kind: CommentKindArg,

    /// Maximum number of replies to display.
    #[arg(long, default_value_t = 25)]
    pub limit: usize,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl RepliesCommand {
    /// Fetches and displays the replies of a comment.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);
        run_list_replies(&api, &self.id, self.kind.into(), self.limit, &self.output).await
    }
}

/// Audits every inline comment on a page for anchor drift.
///
/// Inline-comment anchors do NOT follow text edits: when the annotated text is
/// rewritten, Confluence keeps the mark on whatever original characters survive.
/// This compares each comment's currently-anchored text against the reviewer's
/// original highlight and reports the drift. Read-only.
#[derive(Parser)]
pub struct AuditCommand {
    /// Confluence page ID.
    pub id: String,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl AuditCommand {
    /// Fetches inline comments and the page ADF, then reports drift.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);
        let drifts = audit_inline_comments(&api, &self.id).await?;
        if output_as(&drifts, &self.output)? {
            return Ok(());
        }
        print_drifts(&drifts);
        Ok(())
    }
}

/// Moves an inline comment's anchor to a new run of text in the current-version
/// ADF and writes the page back.
///
/// Operates entirely on ADF (it never round-trips through JFM, which would
/// discard the annotation marks the anchor depends on). Destructive — the page
/// is updated — so it prompts for confirmation unless `--force`.
#[derive(Parser)]
pub struct ReanchorCommand {
    /// Confluence page ID.
    pub id: String,

    /// The inline comment ID to re-anchor.
    #[arg(long)]
    pub comment: String,

    /// Exact text on the current page to move the comment's anchor to.
    #[arg(long)]
    pub anchor_text: String,

    /// 1-based occurrence to anchor to when `--anchor-text` appears more than
    /// once on the page. Required for ambiguous anchors; rejected if out of range.
    #[arg(long)]
    pub match_index: Option<usize>,

    /// Skips the confirmation prompt.
    #[arg(long)]
    pub force: bool,

    /// Prints what would change without writing the page.
    #[arg(long)]
    pub dry_run: bool,
}

impl ReanchorCommand {
    /// Executes the re-anchor command.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);
        let mut reader = io::BufReader::new(io::stdin());
        let mut writer = io::stdout();
        self.execute_with_io(&api, &mut reader, &mut writer).await
    }

    /// Inner form taking explicit API and IO handles, for unit tests.
    async fn execute_with_io(
        self,
        api: &ConfluenceApi,
        reader: &mut (dyn BufRead + Send),
        writer: &mut (dyn Write + Send),
    ) -> Result<()> {
        let prompt = format!(
            "Re-anchor inline comment {} on page {} to {:?}? [y/N] ",
            self.comment, self.id, self.anchor_text
        );
        let dry_run_message = format!(
            "Would re-anchor inline comment {} on page {} to {:?}.",
            self.comment, self.id, self.anchor_text
        );
        let outcome = guard_destructive_with_io(
            &GuardOptions {
                prompt: &prompt,
                dry_run_message: &dry_run_message,
                force: self.force,
                dry_run: self.dry_run,
            },
            reader,
            writer,
        )?;
        match outcome {
            GuardOutcome::Cancelled | GuardOutcome::DryRun => return Ok(()),
            GuardOutcome::Proceed => {}
        }

        let outcome = reanchor_inline_comment(
            api,
            &self.id,
            &self.comment,
            &self.anchor_text,
            self.match_index,
            false,
        )
        .await?;
        writeln!(
            writer,
            "Re-anchored inline comment {} to {:?}.",
            outcome.comment_id, outcome.new_anchor_text
        )?;
        Ok(())
    }
}

/// Prints inline-comment drift reports in a readable format.
fn print_drifts(drifts: &[CommentDrift]) {
    if drifts.is_empty() {
        println!("No inline comments.");
        return;
    }

    for (i, drift) in drifts.iter().enumerate() {
        if i > 0 {
            println!();
        }
        let status = match drift.status {
            DriftStatus::Ok => "OK",
            DriftStatus::Torn => "TORN",
            DriftStatus::MarkLost => "MARK LOST",
            DriftStatus::Drifted => "DRIFTED",
        };
        println!("--- {} | {} ---", drift.comment_id, status);
        println!("  original:  {:?}", drift.original_selection);
        match &drift.current_anchored_text {
            Some(text) => println!("  current:   {text:?}"),
            None => println!("  current:   (mark not present)"),
        }
        if let Some(suggestion) = &drift.suggested_new_anchor {
            let count = drift.suggested_match_count.unwrap_or(0);
            println!("  suggested: {suggestion:?} (appears {count}x on the current page)");
        }
    }
}

/// Parses a comment input file (or stdin) into a validated ADF document.
///
/// Shared by `comment add` (footer) and `comment add-inline` so both surfaces
/// accept the same input formats — JFM with frontmatter, raw markdown, or
/// pre-built ADF JSON.
fn parse_comment_input(file: Option<&str>, format: ContentFormat) -> Result<ValidatedAdfDocument> {
    let input = read_input(file)?;

    let validated: ValidatedAdfDocument = match format {
        ContentFormat::Jfm => {
            if input.starts_with("---\n") {
                let doc = JfmDocument::parse(&input)?;
                markdown_to_validated_adf(&doc.body)?
            } else {
                markdown_to_validated_adf(&input)?
            }
        }
        ContentFormat::Adf => {
            let adf: AdfDocument =
                serde_json::from_str(&input).context("Failed to parse ADF JSON input")?;
            ValidatedAdfDocument::try_new(adf)?
        }
    };
    Ok(validated)
}

/// Fetches and displays comments for a page.
async fn run_list_comments(
    api: &ConfluenceApi,
    id: &str,
    kind: CommentKindFilter,
    limit: usize,
    output: &OutputFormat,
) -> Result<()> {
    let mut comments = match kind {
        CommentKindFilter::Footer => api.get_page_comments(id).await?,
        CommentKindFilter::Inline => api.get_page_inline_comments(id).await?,
        CommentKindFilter::All => {
            let mut footer = api.get_page_comments(id).await?;
            let inline = api.get_page_inline_comments(id).await?;
            footer.extend(inline);
            footer.sort_by(|a, b| a.created.cmp(&b.created));
            footer
        }
    };
    comments.truncate(limit);
    if output_as(&comments, output)? {
        return Ok(());
    }
    print_comments(&comments);
    Ok(())
}

/// Fetches and displays the replies of a single comment.
async fn run_list_replies(
    api: &ConfluenceApi,
    comment_id: &str,
    kind: CommentKind,
    limit: usize,
    output: &OutputFormat,
) -> Result<()> {
    let mut replies = api.get_comment_replies(comment_id, kind).await?;
    replies.truncate(limit);
    if output_as(&replies, output)? {
        return Ok(());
    }
    print_comments(&replies);
    Ok(())
}

/// Posts a footer comment to a page.
async fn run_add_comment(api: &ConfluenceApi, id: &str, adf: &ValidatedAdfDocument) -> Result<()> {
    api.add_page_comment(id, adf).await?;
    println!("Comment added to page {id}.");
    Ok(())
}

/// Posts an inline comment to a page.
async fn run_add_inline_comment(
    api: &ConfluenceApi,
    id: &str,
    adf: &ValidatedAdfDocument,
    anchor: &InlineAnchor,
) -> Result<()> {
    api.add_inline_page_comment(id, adf, anchor).await?;
    println!(
        "Inline comment added to page {id} anchored to {:?} (occurrence {} of {}).",
        anchor.text,
        anchor.match_index + 1,
        anchor.match_count
    );
    Ok(())
}

/// Prints comments in a readable format.
fn print_comments(comments: &[ConfluenceComment]) {
    if comments.is_empty() {
        println!("No comments.");
        return;
    }

    for (i, comment) in comments.iter().enumerate() {
        if i > 0 {
            println!();
        }

        let timestamp = format_timestamp(&comment.created);
        println!(
            "--- {} | {} | {} ---",
            comment.author, timestamp, comment.kind
        );
        println!("{}", format_comment_body(&comment.body_adf));
    }
}

/// Formats a comment body for display.
fn format_comment_body(body_adf: &Option<serde_json::Value>) -> String {
    let Some(adf_value) = body_adf else {
        return "[empty]".to_string();
    };

    let Ok(adf) = serde_json::from_value::<AdfDocument>(adf_value.clone()) else {
        return "[ADF content]".to_string();
    };

    let Ok(md) = adf_to_markdown(&adf) else {
        return "[ADF content]".to_string();
    };

    let trimmed = md.trim();
    if trimmed.is_empty() {
        "[empty]".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Formats an ISO 8601 timestamp to a shorter display format.
fn format_timestamp(ts: &str) -> &str {
    // Return just the date+time portion (before the timezone offset or milliseconds)
    // e.g., "2026-04-01T10:00:00.000Z" -> "2026-04-01T10:00:00"
    ts.split('.').next().unwrap_or(ts)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::await_holding_lock)]
mod tests {
    use super::*;
    use std::fs;

    fn sample_comment(
        id: &str,
        author: &str,
        body_adf: Option<serde_json::Value>,
    ) -> ConfluenceComment {
        ConfluenceComment {
            id: id.to_string(),
            author: author.to_string(),
            kind: CommentKind::Footer,
            body_adf,
            created: "2026-04-01T10:30:00.000Z".to_string(),
            inline_marker_ref: None,
            inline_original_selection: None,
        }
    }

    fn sample_inline_comment(
        id: &str,
        author: &str,
        body_adf: Option<serde_json::Value>,
    ) -> ConfluenceComment {
        ConfluenceComment {
            id: id.to_string(),
            author: author.to_string(),
            kind: CommentKind::Inline,
            body_adf,
            created: "2026-04-02T10:30:00.000Z".to_string(),
            inline_marker_ref: Some("marker-1".to_string()),
            inline_original_selection: Some("original highlighted text".to_string()),
        }
    }

    // ── print_comments ─────────────────────────────────────────────

    #[test]
    fn print_comments_empty() {
        print_comments(&[]);
    }

    #[test]
    fn print_comments_with_adf_body() {
        let adf = serde_json::json!({
            "version": 1,
            "type": "doc",
            "content": [{"type": "paragraph", "content": [{"type": "text", "text": "Hello world"}]}]
        });
        let comments = vec![sample_comment("1", "Alice", Some(adf))];
        print_comments(&comments);
    }

    #[test]
    fn print_comments_with_null_body() {
        let comments = vec![sample_comment("1", "Bob", None)];
        print_comments(&comments);
    }

    #[test]
    fn print_comments_with_invalid_adf() {
        let invalid = serde_json::json!({"not": "adf"});
        let comments = vec![sample_comment("1", "Carol", Some(invalid))];
        print_comments(&comments);
    }

    #[test]
    fn print_comments_multiple() {
        let adf = serde_json::json!({
            "version": 1,
            "type": "doc",
            "content": [{"type": "paragraph", "content": [{"type": "text", "text": "First"}]}]
        });
        let comments = vec![
            sample_comment("1", "Alice", Some(adf)),
            sample_inline_comment("2", "Bob", None),
        ];
        print_comments(&comments);
    }

    // ── format_comment_body ─────────────────────────────────────────

    #[test]
    fn format_body_none() {
        assert_eq!(format_comment_body(&None), "[empty]");
    }

    #[test]
    fn format_body_valid_adf_with_text() {
        let adf = serde_json::json!({
            "version": 1,
            "type": "doc",
            "content": [{"type": "paragraph", "content": [{"type": "text", "text": "Hello"}]}]
        });
        let result = format_comment_body(&Some(adf));
        assert_eq!(result, "Hello");
    }

    #[test]
    fn format_body_valid_adf_empty_content() {
        let adf = serde_json::json!({
            "version": 1,
            "type": "doc",
            "content": []
        });
        let result = format_comment_body(&Some(adf));
        assert_eq!(result, "[empty]");
    }

    #[test]
    fn format_body_invalid_adf() {
        let invalid = serde_json::json!({"not": "adf"});
        assert_eq!(format_comment_body(&Some(invalid)), "[ADF content]");
    }

    // ── format_timestamp ───────────────────────────────────────────

    #[test]
    fn format_timestamp_with_millis() {
        assert_eq!(
            format_timestamp("2026-04-01T10:30:00.000Z"),
            "2026-04-01T10:30:00"
        );
    }

    #[test]
    fn format_timestamp_without_millis() {
        assert_eq!(
            format_timestamp("2026-04-01T10:30:00"),
            "2026-04-01T10:30:00"
        );
    }

    #[test]
    fn format_timestamp_empty() {
        assert_eq!(format_timestamp(""), "");
    }

    // ── parse_comment_input ────────────────────────────────────────

    #[test]
    fn parse_input_raw_markdown() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("comment.md");
        fs::write(&file_path, "Hello **world**\n").unwrap();

        let adf =
            parse_comment_input(Some(file_path.to_str().unwrap()), ContentFormat::Jfm).unwrap();
        assert!(!adf.content.is_empty());
    }

    #[test]
    fn parse_input_jfm_with_frontmatter() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("comment.md");
        let content =
            "---\ntype: confluence\ninstance: https://org.atlassian.net\nid: \"12345\"\ntitle: Test\nspace_key: ENG\n---\n\nComment body\n";
        fs::write(&file_path, content).unwrap();

        let adf =
            parse_comment_input(Some(file_path.to_str().unwrap()), ContentFormat::Jfm).unwrap();
        assert!(!adf.content.is_empty());
    }

    #[test]
    fn parse_input_adf_format() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("comment.json");
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"Hello"}]}]}"#;
        fs::write(&file_path, adf_json).unwrap();

        let adf =
            parse_comment_input(Some(file_path.to_str().unwrap()), ContentFormat::Adf).unwrap();
        assert_eq!(adf.content.len(), 1);
    }

    #[test]
    fn parse_input_invalid_adf() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("bad.json");
        fs::write(&file_path, "not json").unwrap();

        assert!(
            parse_comment_input(Some(file_path.to_str().unwrap()), ContentFormat::Adf).is_err()
        );
    }

    #[test]
    fn parse_input_jfm_rejects_invalid_adf_nesting() {
        // Issue #714: JFM that converts to invalid ADF (panel→expand) must
        // be rejected at parse time, before the API call.
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("bad.md");
        fs::write(
            &file_path,
            ":::panel{type=info}\n:::expand{title=\"x\"}\nbody\n:::\n:::",
        )
        .unwrap();

        let err =
            parse_comment_input(Some(file_path.to_str().unwrap()), ContentFormat::Jfm).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid ADF nesting"));
        assert!(msg.contains("`expand` cannot be a child of `panel`"));
    }

    // ── CommentCommand dispatch ────────────────────────────────────

    #[test]
    fn comment_command_list_variant() {
        let cmd = CommentCommand {
            command: CommentSubcommands::List(ListCommand {
                id: "12345".to_string(),
                kind: CommentKindFilter::All,
                limit: 25,
                output: OutputFormat::Table,
            }),
        };
        assert!(matches!(cmd.command, CommentSubcommands::List(_)));
    }

    #[test]
    fn comment_command_add_variant() {
        let cmd = CommentCommand {
            command: CommentSubcommands::Add(AddCommand {
                id: "12345".to_string(),
                file: None,
                format: ContentFormat::Jfm,
            }),
        };
        assert!(matches!(cmd.command, CommentSubcommands::Add(_)));
    }

    #[test]
    fn comment_command_add_inline_variant() {
        let cmd = CommentCommand {
            command: CommentSubcommands::AddInline(AddInlineCommand {
                id: "12345".to_string(),
                file: None,
                format: ContentFormat::Jfm,
                anchor_text: "phrase".to_string(),
                match_index: None,
            }),
        };
        assert!(matches!(cmd.command, CommentSubcommands::AddInline(_)));
    }

    #[test]
    fn comment_command_replies_variant() {
        let cmd = CommentCommand {
            command: CommentSubcommands::Replies(RepliesCommand {
                id: "abc".to_string(),
                kind: CommentKindArg::Inline,
                limit: 25,
                output: OutputFormat::Table,
            }),
        };
        assert!(matches!(cmd.command, CommentSubcommands::Replies(_)));
    }

    // ── CommentKindArg → CommentKind conversion ────────────────────

    #[test]
    fn comment_kind_arg_into_footer() {
        let k: CommentKind = CommentKindArg::Footer.into();
        assert_eq!(k, CommentKind::Footer);
    }

    #[test]
    fn comment_kind_arg_into_inline() {
        let k: CommentKind = CommentKindArg::Inline.into();
        assert_eq!(k, CommentKind::Inline);
    }

    // ── run_list_comments / run_add_comment ────────────────────────

    fn mock_api(server: &wiremock::MockServer) -> ConfluenceApi {
        let client =
            crate::atlassian::client::AtlassianClient::new(&server.uri(), "user@test.com", "token")
                .unwrap();
        ConfluenceApi::new(client)
    }

    #[tokio::test]
    async fn run_list_comments_success() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/pages/12345/footer-comments",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .mount(&server)
            .await;

        let api = mock_api(&server);
        assert!(run_list_comments(
            &api,
            "12345",
            CommentKindFilter::Footer,
            25,
            &OutputFormat::Table,
        )
        .await
        .is_ok());
    }

    #[tokio::test]
    async fn run_list_comments_json_output() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/pages/12345/footer-comments",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .mount(&server)
            .await;

        let api = mock_api(&server);
        assert!(run_list_comments(
            &api,
            "12345",
            CommentKindFilter::Footer,
            25,
            &OutputFormat::Json,
        )
        .await
        .is_ok());
    }

    #[tokio::test]
    async fn run_list_comments_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/pages/99999/footer-comments",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;

        let api = mock_api(&server);
        let err = run_list_comments(
            &api,
            "99999",
            CommentKindFilter::Footer,
            25,
            &OutputFormat::Table,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn run_list_comments_inline_kind_hits_inline_endpoint() {
        // `Inline` filter must NOT touch the footer endpoint.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/pages/12345/inline-comments",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let api = mock_api(&server);
        assert!(run_list_comments(
            &api,
            "12345",
            CommentKindFilter::Inline,
            25,
            &OutputFormat::Json,
        )
        .await
        .is_ok());
    }

    #[tokio::test]
    async fn run_list_comments_all_kind_fetches_both() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/pages/12345/footer-comments",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{
                        "id": "f1",
                        "version": {"authorId": "alice", "createdAt": "2026-04-01T10:00:00Z"}
                    }]
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/pages/12345/inline-comments",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{
                        "id": "i1",
                        "version": {"authorId": "bob", "createdAt": "2026-04-02T10:00:00Z"}
                    }]
                })),
            )
            .mount(&server)
            .await;

        let api = mock_api(&server);
        assert!(run_list_comments(
            &api,
            "12345",
            CommentKindFilter::All,
            25,
            &OutputFormat::Json,
        )
        .await
        .is_ok());
    }

    #[tokio::test]
    async fn run_list_replies_success() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/inline-comments/abc/children",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .mount(&server)
            .await;

        let api = mock_api(&server);
        assert!(
            run_list_replies(&api, "abc", CommentKind::Inline, 25, &OutputFormat::Table,)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn run_add_comment_success() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/wiki/api/v2/footer-comments"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "200"})),
            )
            .mount(&server)
            .await;

        let api = mock_api(&server);
        let adf = ValidatedAdfDocument::empty();
        assert!(run_add_comment(&api, "12345", &adf).await.is_ok());
    }

    #[tokio::test]
    async fn run_add_comment_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/wiki/api/v2/footer-comments"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;

        let api = mock_api(&server);
        let adf = ValidatedAdfDocument::empty();
        let err = run_add_comment(&api, "12345", &adf).await.unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn run_add_inline_comment_success() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/wiki/api/v2/inline-comments"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "300"})),
            )
            .mount(&server)
            .await;

        let api = mock_api(&server);
        let adf = ValidatedAdfDocument::empty();
        let anchor = InlineAnchor {
            text: "phrase".to_string(),
            match_index: 0,
            match_count: 1,
        };
        assert!(run_add_inline_comment(&api, "12345", &adf, &anchor)
            .await
            .is_ok());
    }

    // ── *Command::execute (env-mutex serialised) ───────────────────
    //
    // Each `*::execute()` is a thin wrapper around `create_client()` and a
    // `run_*` helper. Exercising the `Err` propagation through `?` plus one
    // happy-path dispatch is enough to cover the wrapper; the underlying
    // helper logic is covered by the `run_*` tests above.

    fn set_atlassian_env(uri: &str) {
        std::env::set_var(crate::atlassian::auth::ATLASSIAN_INSTANCE_URL, uri);
        std::env::set_var(crate::atlassian::auth::ATLASSIAN_EMAIL, "user@test.com");
        std::env::set_var(crate::atlassian::auth::ATLASSIAN_API_TOKEN, "t");
    }

    fn clear_atlassian_env() {
        std::env::remove_var(crate::atlassian::auth::ATLASSIAN_INSTANCE_URL);
        std::env::remove_var(crate::atlassian::auth::ATLASSIAN_EMAIL);
        std::env::remove_var(crate::atlassian::auth::ATLASSIAN_API_TOKEN);
    }

    #[tokio::test]
    async fn list_command_execute_propagates_create_client_error() {
        let guard = crate::atlassian::auth::test_util::EnvGuard::take();
        let _home = guard.clear_credentials();

        let cmd = ListCommand {
            id: "12345".to_string(),
            kind: CommentKindFilter::All,
            limit: 25,
            output: OutputFormat::Yaml,
        };
        assert!(cmd.execute().await.is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn list_command_execute_runs_through_dispatch() {
        let _lock = crate::atlassian::auth::test_util::AUTH_ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/pages/12345/footer-comments",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/pages/12345/inline-comments",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .mount(&server)
            .await;

        set_atlassian_env(&server.uri());
        let cmd = CommentCommand {
            command: CommentSubcommands::List(ListCommand {
                id: "12345".to_string(),
                kind: CommentKindFilter::All,
                limit: 25,
                output: OutputFormat::Json,
            }),
        };
        let result = cmd.execute().await;
        clear_atlassian_env();
        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    async fn add_command_execute_propagates_create_client_error() {
        let guard = crate::atlassian::auth::test_util::EnvGuard::take();
        let _home = guard.clear_credentials();

        let temp_dir = tempfile::tempdir().unwrap();
        let body_path = temp_dir.path().join("body.md");
        fs::write(&body_path, "Hello").unwrap();

        let cmd = AddCommand {
            id: "12345".to_string(),
            file: Some(body_path.to_str().unwrap().to_string()),
            format: ContentFormat::Jfm,
        };
        assert!(cmd.execute().await.is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn add_command_execute_runs_through_dispatch() {
        let _lock = crate::atlassian::auth::test_util::AUTH_ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/wiki/api/v2/footer-comments"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "c9"})),
            )
            .mount(&server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        let body_path = temp_dir.path().join("body.md");
        fs::write(&body_path, "Hello").unwrap();

        set_atlassian_env(&server.uri());
        let cmd = CommentCommand {
            command: CommentSubcommands::Add(AddCommand {
                id: "12345".to_string(),
                file: Some(body_path.to_str().unwrap().to_string()),
                format: ContentFormat::Jfm,
            }),
        };
        let result = cmd.execute().await;
        clear_atlassian_env();
        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    async fn add_inline_command_execute_propagates_create_client_error() {
        let guard = crate::atlassian::auth::test_util::EnvGuard::take();
        let _home = guard.clear_credentials();

        let temp_dir = tempfile::tempdir().unwrap();
        let body_path = temp_dir.path().join("body.md");
        fs::write(&body_path, "Hello").unwrap();

        let cmd = AddInlineCommand {
            id: "12345".to_string(),
            file: Some(body_path.to_str().unwrap().to_string()),
            format: ContentFormat::Jfm,
            anchor_text: "phrase".to_string(),
            match_index: None,
        };
        assert!(cmd.execute().await.is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn add_inline_command_execute_runs_through_dispatch() {
        let _lock = crate::atlassian::auth::test_util::AUTH_ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "12345",
                    "title": "Mock",
                    "status": "current",
                    "spaceId": "98",
                    "version": {"number": 1},
                    "body": {"atlas_doc_format": {
                        "value": "{\"version\":1,\"type\":\"doc\",\"content\":[{\"type\":\"paragraph\",\"content\":[{\"type\":\"text\",\"text\":\"the anchor\"}]}]}"
                    }}
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"key": "ENG"})),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/wiki/api/v2/inline-comments"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "ic1"})),
            )
            .mount(&server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        let body_path = temp_dir.path().join("body.md");
        fs::write(&body_path, "Inline note").unwrap();

        set_atlassian_env(&server.uri());
        let cmd = CommentCommand {
            command: CommentSubcommands::AddInline(AddInlineCommand {
                id: "12345".to_string(),
                file: Some(body_path.to_str().unwrap().to_string()),
                format: ContentFormat::Jfm,
                anchor_text: "the anchor".to_string(),
                match_index: None,
            }),
        };
        let result = cmd.execute().await;
        clear_atlassian_env();
        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    async fn replies_command_execute_propagates_create_client_error() {
        let guard = crate::atlassian::auth::test_util::EnvGuard::take();
        let _home = guard.clear_credentials();

        let cmd = RepliesCommand {
            id: "abc".to_string(),
            kind: CommentKindArg::Inline,
            limit: 25,
            output: OutputFormat::Yaml,
        };
        assert!(cmd.execute().await.is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn replies_command_execute_runs_through_dispatch() {
        let _lock = crate::atlassian::auth::test_util::AUTH_ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/inline-comments/abc/children",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .mount(&server)
            .await;

        set_atlassian_env(&server.uri());
        let cmd = CommentCommand {
            command: CommentSubcommands::Replies(RepliesCommand {
                id: "abc".to_string(),
                kind: CommentKindArg::Inline,
                limit: 25,
                output: OutputFormat::Json,
            }),
        };
        let result = cmd.execute().await;
        clear_atlassian_env();
        assert!(result.is_ok(), "{result:?}");
    }

    // ── print_drifts ───────────────────────────────────────────────

    fn sample_drift(id: &str, status: DriftStatus) -> CommentDrift {
        CommentDrift {
            comment_id: id.to_string(),
            author: "alice".to_string(),
            created: "2026-04-01T10:00:00.000Z".to_string(),
            marker_ref: "marker-1".to_string(),
            status,
            original_selection: "original text".to_string(),
            current_anchored_text: Some("current text".to_string()),
            suggested_new_anchor: None,
            suggested_match_count: None,
        }
    }

    #[test]
    fn print_drifts_empty() {
        print_drifts(&[]);
    }

    #[test]
    fn print_drifts_all_statuses() {
        let mut lost = sample_drift("c3", DriftStatus::MarkLost);
        lost.current_anchored_text = None;
        lost.suggested_new_anchor = Some("original text".to_string());
        lost.suggested_match_count = Some(2);
        let mut drifted = sample_drift("c4", DriftStatus::Drifted);
        // Suggestion without a count exercises the `unwrap_or(0)` fallback.
        drifted.suggested_new_anchor = Some("original text".to_string());
        let drifts = vec![
            sample_drift("c1", DriftStatus::Ok),
            sample_drift("c2", DriftStatus::Torn),
            lost,
            drifted,
        ];
        print_drifts(&drifts);
    }

    // ── audit / reanchor command execution ──────────────────────────

    /// Mounts the endpoints a reanchor (and audit) needs: the page (whose ADF
    /// contains "the anchor"), its space, one inline comment `ic1` bearing
    /// marker `aaa`, and the page-update PUT.
    async fn mount_reanchor_fixture(server: &wiremock::MockServer) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "12345",
                    "title": "Mock",
                    "status": "current",
                    "spaceId": "98",
                    "version": {"number": 1},
                    "body": {"atlas_doc_format": {
                        "value": "{\"version\":1,\"type\":\"doc\",\"content\":[{\"type\":\"paragraph\",\"content\":[{\"type\":\"text\",\"text\":\"the anchor\"}]}]}"
                    }}
                })),
            )
            .mount(server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"key": "ENG"})),
            )
            .mount(server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/pages/12345/inline-comments",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"id": "ic1", "version": {"authorId": "a", "createdAt": "t"},
                         "properties": {"inlineMarkerRef": "aaa", "inlineOriginalSelection": "old"}}
                    ]
                })),
            )
            .mount(server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn audit_command_execute_propagates_create_client_error() {
        let guard = crate::atlassian::auth::test_util::EnvGuard::take();
        let _home = guard.clear_credentials();

        let cmd = AuditCommand {
            id: "12345".to_string(),
            output: OutputFormat::Yaml,
        };
        assert!(cmd.execute().await.is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn audit_command_execute_runs_through_dispatch() {
        let _lock = crate::atlassian::auth::test_util::AUTH_ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let server = wiremock::MockServer::start().await;
        mount_reanchor_fixture(&server).await;

        set_atlassian_env(&server.uri());
        let cmd = CommentCommand {
            command: CommentSubcommands::Audit(AuditCommand {
                id: "12345".to_string(),
                output: OutputFormat::Json,
            }),
        };
        let result = cmd.execute().await;
        clear_atlassian_env();
        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn audit_command_execute_table_output_prints_drifts() {
        let _lock = crate::atlassian::auth::test_util::AUTH_ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let server = wiremock::MockServer::start().await;
        mount_reanchor_fixture(&server).await;

        set_atlassian_env(&server.uri());
        let cmd = AuditCommand {
            id: "12345".to_string(),
            output: OutputFormat::Table,
        };
        let result = cmd.execute().await;
        clear_atlassian_env();
        assert!(result.is_ok(), "{result:?}");
    }

    fn reanchor_command(force: bool, dry_run: bool) -> ReanchorCommand {
        ReanchorCommand {
            id: "12345".to_string(),
            comment: "ic1".to_string(),
            anchor_text: "the anchor".to_string(),
            match_index: None,
            force,
            dry_run,
        }
    }

    #[tokio::test]
    async fn reanchor_command_execute_propagates_create_client_error() {
        let guard = crate::atlassian::auth::test_util::EnvGuard::take();
        let _home = guard.clear_credentials();

        let cmd = reanchor_command(true, false);
        assert!(cmd.execute().await.is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reanchor_command_execute_runs_through_dispatch() {
        let _lock = crate::atlassian::auth::test_util::AUTH_ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let server = wiremock::MockServer::start().await;
        mount_reanchor_fixture(&server).await;

        set_atlassian_env(&server.uri());
        let cmd = CommentCommand {
            command: CommentSubcommands::Reanchor(reanchor_command(true, false)),
        };
        let result = cmd.execute().await;
        clear_atlassian_env();
        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    async fn reanchor_execute_with_io_cancelled_makes_no_api_calls() {
        let server = wiremock::MockServer::start().await;
        let api = mock_api(&server);

        let cmd = reanchor_command(false, false);
        let mut reader = io::Cursor::new(b"n\n".to_vec());
        let mut writer: Vec<u8> = Vec::new();
        cmd.execute_with_io(&api, &mut reader, &mut writer)
            .await
            .unwrap();

        let out = String::from_utf8(writer).unwrap();
        assert!(out.contains("Re-anchor inline comment ic1"));
        assert!(out.contains("Cancelled."));
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn reanchor_execute_with_io_dry_run_makes_no_api_calls() {
        let server = wiremock::MockServer::start().await;
        let api = mock_api(&server);

        let cmd = reanchor_command(false, true);
        let mut reader = io::Cursor::new(Vec::new());
        let mut writer: Vec<u8> = Vec::new();
        cmd.execute_with_io(&api, &mut reader, &mut writer)
            .await
            .unwrap();

        let out = String::from_utf8(writer).unwrap();
        assert!(out.contains("Would re-anchor inline comment ic1"));
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn reanchor_execute_with_io_confirmed_proceeds() {
        let server = wiremock::MockServer::start().await;
        mount_reanchor_fixture(&server).await;
        let api = mock_api(&server);

        let cmd = reanchor_command(false, false);
        let mut reader = io::Cursor::new(b"y\n".to_vec());
        let mut writer: Vec<u8> = Vec::new();
        cmd.execute_with_io(&api, &mut reader, &mut writer)
            .await
            .unwrap();

        let out = String::from_utf8(writer).unwrap();
        assert!(out.contains("Re-anchored inline comment ic1"));
        let requests = server.received_requests().await.unwrap();
        assert!(requests
            .iter()
            .any(|r| r.method == wiremock::http::Method::PUT));
    }

    /// A writer that fails at a chosen point (`write` or, when writes are
    /// allowed through, `flush`), for exercising IO-error propagation through
    /// `execute_with_io`.
    struct FailingWriter {
        fail_write: bool,
    }

    impl Write for FailingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.fail_write {
                Err(io::Error::other("writer failed"))
            } else {
                Ok(buf.len())
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("writer failed"))
        }
    }

    #[tokio::test]
    async fn reanchor_execute_with_io_propagates_prompt_write_error() {
        let server = wiremock::MockServer::start().await;
        let api = mock_api(&server);

        // Without --force the guard writes the prompt first; the failing
        // writer surfaces that IO error before any API call.
        let cmd = reanchor_command(false, false);
        let mut reader = io::Cursor::new(b"y\n".to_vec());
        let mut writer = FailingWriter { fail_write: true };
        assert!(cmd
            .execute_with_io(&api, &mut reader, &mut writer)
            .await
            .is_err());
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn reanchor_execute_with_io_propagates_prompt_flush_error() {
        let server = wiremock::MockServer::start().await;
        let api = mock_api(&server);

        // Writes go through but the guard's flush after the prompt fails.
        let cmd = reanchor_command(false, false);
        let mut reader = io::Cursor::new(b"y\n".to_vec());
        let mut writer = FailingWriter { fail_write: false };
        assert!(cmd
            .execute_with_io(&api, &mut reader, &mut writer)
            .await
            .is_err());
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn reanchor_execute_with_io_propagates_result_write_error() {
        let server = wiremock::MockServer::start().await;
        mount_reanchor_fixture(&server).await;
        let api = mock_api(&server);

        // With --force the guard writes nothing; the re-anchor succeeds and
        // the failure comes from writing the success message.
        let cmd = reanchor_command(true, false);
        let mut reader = io::Cursor::new(Vec::new());
        let mut writer = FailingWriter { fail_write: true };
        assert!(cmd
            .execute_with_io(&api, &mut reader, &mut writer)
            .await
            .is_err());
        let requests = server.received_requests().await.unwrap();
        assert!(requests
            .iter()
            .any(|r| r.method == wiremock::http::Method::PUT));
    }
}
