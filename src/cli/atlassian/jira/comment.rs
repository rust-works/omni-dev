//! CLI commands for JIRA issue comments.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::atlassian::adf::AdfDocument;
use crate::atlassian::client::{AtlassianClient, JiraComment};
use crate::atlassian::convert::{adf_to_markdown, markdown_to_adf};
use crate::atlassian::document::JfmDocument;
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
    /// Lists comments on a JIRA issue.
    List(ListCommand),
    /// Adds a comment to a JIRA issue.
    Add(AddCommand),
}

impl CommentCommand {
    /// Executes the comment command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            CommentSubcommands::List(cmd) => cmd.execute().await,
            CommentSubcommands::Add(cmd) => cmd.execute().await,
        }
    }
}

/// Lists comments on a JIRA issue.
#[derive(Parser)]
pub struct ListCommand {
    /// JIRA issue key (e.g., PROJ-123).
    pub key: String,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ListCommand {
    /// Fetches and displays comments.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        run_list_comments(&client, &self.key, &self.output).await
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
        let adf = self.parse_input()?;

        let (client, _instance_url) = create_client()?;
        run_add_comment(&client, &self.key, &adf).await
    }

    /// Parses the input file into an ADF document.
    fn parse_input(&self) -> Result<AdfDocument> {
        let input = read_input(self.file.as_deref())?;

        match self.format {
            ContentFormat::Jfm => {
                // Try parsing as JFM document (with frontmatter) first,
                // fall back to raw markdown
                if input.starts_with("---\n") {
                    let doc = JfmDocument::parse(&input)?;
                    markdown_to_adf(&doc.body)
                } else {
                    markdown_to_adf(&input)
                }
            }
            ContentFormat::Adf => {
                serde_json::from_str(&input).context("Failed to parse ADF JSON input")
            }
        }
    }
}

/// Fetches and displays comments for an issue.
async fn run_list_comments(
    client: &AtlassianClient,
    key: &str,
    output: &OutputFormat,
) -> Result<()> {
    let comments = client.get_comments(key).await?;
    if output_as(&comments, output)? {
        return Ok(());
    }
    print_comments(&comments);
    Ok(())
}

/// Posts a comment to an issue.
async fn run_add_comment(client: &AtlassianClient, key: &str, adf: &AdfDocument) -> Result<()> {
    client.add_comment(key, adf).await?;
    println!("Comment added to {key}.");
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

    // ── AddCommand::parse_input ────────────────────────────────────

    #[test]
    fn parse_input_raw_markdown() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("comment.md");
        fs::write(&file_path, "Hello **world**\n").unwrap();

        let cmd = AddCommand {
            key: "PROJ-1".to_string(),
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Jfm,
        };

        let adf = cmd.parse_input().unwrap();
        assert!(!adf.content.is_empty());
    }

    #[test]
    fn parse_input_jfm_with_frontmatter() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("comment.md");
        let content =
            "---\ntype: jira\ninstance: https://org.atlassian.net\nkey: PROJ-1\nsummary: Test\n---\n\nComment body\n";
        fs::write(&file_path, content).unwrap();

        let cmd = AddCommand {
            key: "PROJ-1".to_string(),
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Jfm,
        };

        let adf = cmd.parse_input().unwrap();
        assert!(!adf.content.is_empty());
    }

    #[test]
    fn parse_input_adf_format() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("comment.json");
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"Hello"}]}]}"#;
        fs::write(&file_path, adf_json).unwrap();

        let cmd = AddCommand {
            key: "PROJ-1".to_string(),
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Adf,
        };

        let adf = cmd.parse_input().unwrap();
        assert_eq!(adf.content.len(), 1);
    }

    #[test]
    fn parse_input_invalid_adf() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("bad.json");
        fs::write(&file_path, "not json").unwrap();

        let cmd = AddCommand {
            key: "PROJ-1".to_string(),
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Adf,
        };

        assert!(cmd.parse_input().is_err());
    }

    // ── CommentCommand dispatch ────────────────────────────────────

    #[test]
    fn comment_command_list_variant() {
        let cmd = CommentCommand {
            command: CommentSubcommands::List(ListCommand {
                key: "PROJ-1".to_string(),
                output: OutputFormat::Table,
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
}
