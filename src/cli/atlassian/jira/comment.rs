//! CLI commands for JIRA issue comments.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

use crate::atlassian::adf::AdfDocument;
use crate::atlassian::adf_validated::{markdown_to_validated_adf, ValidatedAdfDocument};
use crate::atlassian::client::AtlassianClient;
use crate::atlassian::convert::adf_to_markdown;
use crate::atlassian::document::JfmDocument;
use crate::atlassian::jira_types::{JiraComment, JiraVisibility, JiraVisibilityType};
use crate::cli::atlassian::format::{output_as, ContentFormat, OutputFormat};
use crate::cli::atlassian::helpers::{create_client, read_input};

/// Manages comments on a JIRA issue.
#[derive(Parser)]
pub struct CommentCommand {
    /// The comment subcommand to execute.
    #[command(subcommand)]
    pub command: CommentSubcommands,
}

/// Comment subcommands.
#[derive(Subcommand)]
pub enum CommentSubcommands {
    /// Lists comments on a JIRA issue (mirrors the `jira_comment` MCP tool with `action: list`).
    List(ListCommand),
    /// Adds a comment to a JIRA issue (mirrors the `jira_comment` MCP tool with `action: add`).
    Add(AddCommand),
    /// Edits an existing comment on a JIRA issue (mirrors the `jira_comment_edit` MCP tool).
    Edit(EditCommand),
}

impl CommentCommand {
    /// Executes the comment command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            CommentSubcommands::List(cmd) => cmd.execute().await,
            CommentSubcommands::Add(cmd) => cmd.execute().await,
            CommentSubcommands::Edit(cmd) => cmd.execute().await,
        }
    }
}

/// Lists comments on a JIRA issue.
///
/// Comment authors are returned as Atlassian account IDs — resolve them to
/// display names with `omni-dev atlassian jira user get`.
#[derive(Parser)]
pub struct ListCommand {
    /// JIRA issue key (e.g., PROJ-123).
    pub key: String,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,

    /// Maximum number of comments to return. Use 0 for unlimited.
    #[arg(long, default_value_t = 0)]
    pub limit: u32,
}

impl ListCommand {
    /// Fetches and displays comments.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        run_list_comments(&client, &self.key, self.limit, &self.output).await
    }
}

/// Adds a comment to a JIRA issue.
#[derive(Parser)]
pub struct AddCommand {
    /// JIRA issue key (e.g., PROJ-123).
    pub key: String,

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
        run_add_comment(&client, &self.key, &adf).await
    }
}

/// Edits an existing comment on a JIRA issue.
#[derive(Parser)]
pub struct EditCommand {
    /// JIRA issue key (e.g., PROJ-123).
    pub key: String,

    /// Comment ID to edit.
    pub comment_id: String,

    /// Input file (reads from stdin if omitted or "-").
    pub file: Option<String>,

    /// Input format.
    #[arg(long, value_enum, default_value_t = ContentFormat::Jfm)]
    pub format: ContentFormat,

    /// Visibility restriction kind. Pair with `--visibility-value`.
    #[arg(long, value_enum, requires = "visibility_value")]
    pub visibility_type: Option<CliVisibilityType>,

    /// Visibility group or role name. Pair with `--visibility-type`.
    #[arg(long, requires = "visibility_type")]
    pub visibility_value: Option<String>,
}

/// Visibility restriction kind for the CLI.
#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum CliVisibilityType {
    /// Restrict to a JIRA group.
    Group,
    /// Restrict to a project role.
    Role,
}

impl From<CliVisibilityType> for JiraVisibilityType {
    fn from(value: CliVisibilityType) -> Self {
        match value {
            CliVisibilityType::Group => Self::Group,
            CliVisibilityType::Role => Self::Role,
        }
    }
}

impl EditCommand {
    /// Reads input, converts to ADF, and updates the comment.
    pub async fn execute(self) -> Result<()> {
        let adf = parse_comment_input(self.file.as_deref(), self.format)?;
        let visibility = match (self.visibility_type, self.visibility_value) {
            (Some(ty), Some(value)) => Some(JiraVisibility {
                ty: ty.into(),
                value,
            }),
            _ => None,
        };

        let (client, _instance_url) = create_client()?;
        run_edit_comment(
            &client,
            &self.key,
            &self.comment_id,
            &adf,
            visibility.as_ref(),
        )
        .await
    }
}

/// Parses the input file into a validated ADF document.
fn parse_comment_input(file: Option<&str>, format: ContentFormat) -> Result<ValidatedAdfDocument> {
    let input = read_input(file)?;

    let validated: ValidatedAdfDocument = match format {
        ContentFormat::Jfm => {
            // Try parsing as JFM document (with frontmatter) first,
            // fall back to raw markdown
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

/// Fetches and displays comments for an issue.
async fn run_list_comments(
    client: &AtlassianClient,
    key: &str,
    limit: u32,
    output: &OutputFormat,
) -> Result<()> {
    let comments = client.get_comments(key, limit).await?;
    if output_as(&comments, output)? {
        return Ok(());
    }
    print_comments(&comments);
    Ok(())
}

/// Posts a comment to an issue.
async fn run_add_comment(
    client: &AtlassianClient,
    key: &str,
    adf: &ValidatedAdfDocument,
) -> Result<()> {
    client.add_comment(key, adf).await?;
    println!("Comment added to {key}.");
    Ok(())
}

/// Updates an existing comment on an issue.
async fn run_edit_comment(
    client: &AtlassianClient,
    key: &str,
    comment_id: &str,
    adf: &ValidatedAdfDocument,
    visibility: Option<&JiraVisibility>,
) -> Result<()> {
    let updated = client
        .update_comment(key, comment_id, adf, visibility)
        .await?;
    println!("Comment {comment_id} updated on {key}.");
    let yaml =
        serde_yaml::to_string(&updated).context("Failed to serialize updated comment as YAML")?;
    print!("{yaml}");
    Ok(())
}

/// Prints comments in a readable format.
fn print_comments(comments: &[JiraComment]) {
    if comments.is_empty() {
        println!("No comments.");
        return;
    }

    for (i, comment) in comments.iter().enumerate() {
        if i > 0 {
            println!();
        }

        let timestamp = format_timestamp(&comment.created);
        println!("--- {} | {} ---", comment.author, timestamp);
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
    // e.g., "2026-04-01T10:00:00.000+0000" -> "2026-04-01T10:00:00"
    ts.split('.').next().unwrap_or(ts)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::fs;

    fn sample_comment(id: &str, author: &str, body_adf: Option<serde_json::Value>) -> JiraComment {
        JiraComment {
            id: id.to_string(),
            author: author.to_string(),
            body_adf,
            created: "2026-04-01T10:30:00.000+0000".to_string(),
            updated: None,
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
            sample_comment("2", "Bob", None),
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
            format_timestamp("2026-04-01T10:30:00.000+0000"),
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
            "---\ntype: jira\ninstance: https://org.atlassian.net\nkey: PROJ-1\nsummary: Test\n---\n\nComment body\n";
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
                key: "PROJ-1".to_string(),
                output: OutputFormat::Table,
                limit: 0,
            }),
        };
        assert!(matches!(cmd.command, CommentSubcommands::List(_)));
    }

    #[test]
    fn comment_command_add_variant() {
        let cmd = CommentCommand {
            command: CommentSubcommands::Add(AddCommand {
                key: "PROJ-1".to_string(),
                file: None,
                format: ContentFormat::Jfm,
            }),
        };
        assert!(matches!(cmd.command, CommentSubcommands::Add(_)));
    }

    #[test]
    fn comment_command_edit_variant() {
        let cmd = CommentCommand {
            command: CommentSubcommands::Edit(EditCommand {
                key: "PROJ-1".to_string(),
                comment_id: "100".to_string(),
                file: None,
                format: ContentFormat::Jfm,
                visibility_type: None,
                visibility_value: None,
            }),
        };
        assert!(matches!(cmd.command, CommentSubcommands::Edit(_)));
    }

    #[test]
    fn cli_visibility_type_into_group() {
        let mapped: JiraVisibilityType = CliVisibilityType::Group.into();
        assert!(matches!(mapped, JiraVisibilityType::Group));
    }

    #[test]
    fn cli_visibility_type_into_role() {
        let mapped: JiraVisibilityType = CliVisibilityType::Role.into();
        assert!(matches!(mapped, JiraVisibilityType::Role));
    }

    // ── run_list_comments / run_add_comment ────────────────────────

    fn mock_client(base_url: &str) -> AtlassianClient {
        AtlassianClient::new(base_url, "user@test.com", "token").unwrap()
    }

    #[tokio::test]
    async fn run_list_comments_success() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1/comment"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "startAt": 0,
                    "maxResults": 100,
                    "total": 1,
                    "comments": [{
                        "id": "1",
                        "author": {"displayName": "Alice"},
                        "created": "2026-04-01T10:00:00.000+0000",
                        "body": null
                    }]
                })),
            )
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        assert!(
            run_list_comments(&client, "PROJ-1", 0, &OutputFormat::Table)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn run_list_comments_json_output() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1/comment"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"startAt": 0, "maxResults": 100, "total": 0, "comments": []}),
            ))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        assert!(run_list_comments(&client, "PROJ-1", 0, &OutputFormat::Json)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn run_list_comments_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/NOPE-1/comment"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let err = run_list_comments(&client, "NOPE-1", 0, &OutputFormat::Table)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn run_list_comments_respects_limit() {
        let server = wiremock::MockServer::start().await;
        // Only a single page request is expected when limit=2
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1/comment"))
            .and(wiremock::matchers::query_param("maxResults", "2"))
            .and(wiremock::matchers::query_param("startAt", "0"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "startAt": 0,
                    "maxResults": 2,
                    "total": 10,
                    "comments": [
                        {"id": "1", "author": {"displayName": "A"}, "body": null, "created": "2026-04-01T10:00:00.000+0000"},
                        {"id": "2", "author": {"displayName": "B"}, "body": null, "created": "2026-04-02T10:00:00.000+0000"}
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        assert!(run_list_comments(&client, "PROJ-1", 2, &OutputFormat::Json)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn run_add_comment_success() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1/comment"))
            .respond_with(
                wiremock::ResponseTemplate::new(201)
                    .set_body_json(serde_json::json!({"id": "100"})),
            )
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let adf = ValidatedAdfDocument::empty();
        assert!(run_add_comment(&client, "PROJ-1", &adf).await.is_ok());
    }

    #[tokio::test]
    async fn run_add_comment_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1/comment"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let adf = ValidatedAdfDocument::empty();
        let err = run_add_comment(&client, "PROJ-1", &adf).await.unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    // ── run_edit_comment ────────────────────────────────────────────

    #[tokio::test]
    async fn run_edit_comment_success() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/comment/100",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "100",
                    "author": {"displayName": "Me"},
                    "created": "2026-04-01T10:00:00.000+0000",
                    "updated": "2026-05-10T12:00:00.000+0000",
                    "body": null
                })),
            )
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let adf = ValidatedAdfDocument::empty();
        assert!(run_edit_comment(&client, "PROJ-1", "100", &adf, None)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn run_edit_comment_with_visibility_sends_payload() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/comment/100",
            ))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "visibility": {"type": "role", "identifier": "Administrators"}
            })))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "100",
                    "author": {"displayName": "Me"},
                    "created": "2026-04-01T10:00:00.000+0000",
                    "updated": "2026-05-10T12:00:00.000+0000",
                    "body": null
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let adf = ValidatedAdfDocument::empty();
        let visibility = JiraVisibility {
            ty: JiraVisibilityType::Role,
            value: "Administrators".to_string(),
        };
        run_edit_comment(&client, "PROJ-1", "100", &adf, Some(&visibility))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_edit_comment_forbidden() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/comment/100",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(403).set_body_json(serde_json::json!({
                    "errorMessages": ["You do not have permission to edit this comment"]
                })),
            )
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let adf = ValidatedAdfDocument::empty();
        let err = run_edit_comment(&client, "PROJ-1", "100", &adf, None)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("403"));
        assert!(msg.contains("permission to edit"));
    }

    #[tokio::test]
    async fn run_edit_comment_not_found() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/comment/9999",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(404).set_body_json(serde_json::json!({
                    "errorMessages": ["Comment not found"]
                })),
            )
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let adf = ValidatedAdfDocument::empty();
        let err = run_edit_comment(&client, "PROJ-1", "9999", &adf, None)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("404"));
        assert!(msg.contains("Comment not found"));
    }
}
