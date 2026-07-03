//! CLI commands for managing Confluence page attachments.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::atlassian::confluence_api::{
    ConfluenceApi, ConfluenceAttachment, ConfluenceAttachmentPage,
};
use crate::cli::atlassian::format::{output_as, OutputFormat};
use crate::cli::atlassian::helpers::create_client;
use crate::utils::path::attachment_filename;

/// Manages attachments on Confluence pages.
#[derive(Parser)]
pub struct AttachmentCommand {
    /// The attachment subcommand to execute.
    #[command(subcommand)]
    pub command: AttachmentSubcommands,
}

/// Attachment subcommands.
#[derive(Subcommand)]
pub enum AttachmentSubcommands {
    /// Uploads a file as an attachment to a Confluence page (mirrors the `confluence_attachment_upload` MCP tool).
    Upload(UploadCommand),
    /// Lists attachments on a Confluence page (mirrors the `confluence_attachment_list` MCP tool).
    List(ListCommand),
    /// Downloads an attachment binary by ID (mirrors the `confluence_attachment_download` MCP tool).
    Download(DownloadCommand),
    /// Deletes an attachment by ID (mirrors the `confluence_attachment_delete` MCP tool).
    Delete(DeleteCommand),
}

impl AttachmentCommand {
    /// Executes the attachment command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            AttachmentSubcommands::Upload(cmd) => cmd.execute().await,
            AttachmentSubcommands::List(cmd) => cmd.execute().await,
            AttachmentSubcommands::Download(cmd) => cmd.execute().await,
            AttachmentSubcommands::Delete(cmd) => cmd.execute().await,
        }
    }
}

/// Uploads a file as an attachment to a Confluence page.
#[derive(Parser)]
pub struct UploadCommand {
    /// Confluence page ID (e.g., 12345678).
    pub page_id: String,

    /// Path to the local file to upload.
    pub file: PathBuf,

    /// Override the filename used in Confluence (defaults to the local basename).
    #[arg(long)]
    pub filename: Option<String>,

    /// Optional version comment recorded with the upload.
    #[arg(long)]
    pub comment: Option<String>,

    /// Marks the upload as a minor edit.
    #[arg(long)]
    pub minor_edit: bool,
}

impl UploadCommand {
    /// Executes the upload command.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);
        run_upload(
            &api,
            &self.page_id,
            &self.file,
            self.filename.as_deref(),
            self.comment.as_deref(),
            self.minor_edit,
        )
        .await
    }
}

/// Uploads a file to a page and prints the resulting attachment.
async fn run_upload(
    api: &ConfluenceApi,
    page_id: &str,
    file: &std::path::Path,
    filename: Option<&str>,
    comment: Option<&str>,
    minor_edit: bool,
) -> Result<()> {
    let attachment = api
        .upload_attachment(page_id, file, filename, comment, minor_edit)
        .await?;
    print_upload_confirmation(&attachment, page_id);
    Ok(())
}

/// Prints confirmation after a successful upload.
fn print_upload_confirmation(attachment: &ConfluenceAttachment, page_id: &str) {
    println!(
        "Uploaded {} (id={}) to page {}.",
        attachment.title, attachment.id, page_id,
    );
}

/// Lists attachments on a Confluence page.
#[derive(Parser)]
pub struct ListCommand {
    /// Confluence page ID (e.g., 12345678).
    pub page_id: String,

    /// Pagination cursor (returned as `next_cursor` from a previous call).
    #[arg(long)]
    pub cursor: Option<String>,

    /// Maximum number of attachments to return per page.
    #[arg(long, default_value_t = 25)]
    pub limit: u32,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ListCommand {
    /// Executes the list command.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);
        run_list(
            &api,
            &self.page_id,
            self.cursor.as_deref(),
            self.limit,
            &self.output,
        )
        .await
    }
}

/// Fetches and displays a page of attachments.
async fn run_list(
    api: &ConfluenceApi,
    page_id: &str,
    cursor: Option<&str>,
    limit: u32,
    output: &OutputFormat,
) -> Result<()> {
    let page = api.list_attachments(page_id, cursor, limit).await?;
    display_attachments(&page, output)
}

/// Formats and displays attachments in the requested output format.
fn display_attachments(page: &ConfluenceAttachmentPage, output: &OutputFormat) -> Result<()> {
    if output_as(page, output)? {
        return Ok(());
    }
    print_attachments(page);
    Ok(())
}

/// Prints attachments as a formatted table.
fn print_attachments(page: &ConfluenceAttachmentPage) {
    if page.results.is_empty() {
        println!("No attachments found.");
        return;
    }

    let id_width = page
        .results
        .iter()
        .map(|a| a.id.len())
        .max()
        .unwrap_or(2)
        .max(2);
    let title_width = page
        .results
        .iter()
        .map(|a| a.title.len())
        .max()
        .unwrap_or(5)
        .max(5);
    let media_width = page
        .results
        .iter()
        .map(|a| a.media_type.as_deref().unwrap_or("-").len())
        .max()
        .unwrap_or(10)
        .max(10);

    println!(
        "{:<id_width$}  {:<title_width$}  {:<media_width$}  {:>10}",
        "ID", "TITLE", "MEDIA-TYPE", "SIZE"
    );
    println!(
        "{:<id_width$}  {:<title_width$}  {:<media_width$}  {:>10}",
        "-".repeat(id_width),
        "-".repeat(title_width),
        "-".repeat(media_width),
        "-".repeat(10),
    );

    for a in &page.results {
        let media = a.media_type.as_deref().unwrap_or("-");
        let size = a
            .file_size
            .map_or_else(|| "-".to_string(), |s| s.to_string());
        println!(
            "{:<id_width$}  {:<title_width$}  {:<media_width$}  {:>10}",
            a.id, a.title, media, size,
        );
    }

    if let Some(cursor) = &page.next_cursor {
        println!();
        println!("Next page: --cursor {cursor}");
    }
}

/// Downloads an attachment binary by ID.
#[derive(Parser)]
pub struct DownloadCommand {
    /// Attachment ID (from `attachment list`).
    pub attachment_id: String,

    /// Destination path. If omitted, the file is written to the attachment's
    /// filename in the current directory. If this names an existing
    /// directory, the file is written inside it under the attachment's
    /// filename.
    #[arg(short = 'o', long)]
    pub output: Option<PathBuf>,
}

impl DownloadCommand {
    /// Executes the download command.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);
        run_download(&api, &self.attachment_id, self.output.as_deref()).await
    }
}

/// Downloads an attachment to disk and prints a confirmation.
async fn run_download(
    api: &ConfluenceApi,
    attachment_id: &str,
    output: Option<&Path>,
) -> Result<()> {
    let (attachment, bytes) = api.download_attachment(attachment_id).await?;
    let path = resolve_output_path(output, &attachment.title, &attachment.id);
    std::fs::write(&path, &bytes).with_context(|| format!("Failed to write {}", path.display()))?;
    println!(
        "Downloaded {} ({} bytes) to {}.",
        attachment.title,
        bytes.len(),
        path.display()
    );
    Ok(())
}

/// Resolves the on-disk destination for a downloaded attachment.
///
/// An explicit `--output` that points at an existing directory is joined
/// with the attachment's title; otherwise it is used verbatim. With no
/// `--output`, the attachment's title is written to the current directory.
/// The title is remote-controlled, so it is sanitized to its final path
/// component (falling back to the attachment ID) before use.
fn resolve_output_path(output: Option<&Path>, title: &str, attachment_id: &str) -> PathBuf {
    match output {
        Some(p) if p.is_dir() => p.join(attachment_filename(title, attachment_id)),
        Some(p) => p.to_path_buf(),
        None => PathBuf::from(attachment_filename(title, attachment_id)),
    }
}

/// Deletes an attachment by ID.
#[derive(Parser)]
pub struct DeleteCommand {
    /// Attachment ID.
    pub attachment_id: String,

    /// Skips the confirmation prompt.
    #[arg(long)]
    pub force: bool,

    /// Permanently purges the attachment instead of moving to trash (requires space admin).
    #[arg(long)]
    pub purge: bool,
}

impl DeleteCommand {
    /// Executes the delete command using stdin for confirmation.
    pub async fn execute(self) -> Result<()> {
        let confirmed = self.force || self.prompt(&mut io::stdin().lock())?;
        self.run_delete(confirmed).await
    }

    /// Reads the confirmation answer from `reader`. Synchronous so the
    /// borrow ends before any `.await` in the async caller.
    fn prompt(&self, reader: &mut dyn BufRead) -> Result<bool> {
        let prompt = format_delete_prompt(&self.attachment_id, self.purge);
        confirm_with_reader(&prompt, reader)
    }

    /// Performs the delete after the user has (or hasn't) confirmed.
    async fn run_delete(self, confirmed: bool) -> Result<()> {
        if !confirmed {
            println!("Cancelled.");
            return Ok(());
        }

        let (client, instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);
        api.delete_attachment(&self.attachment_id, self.purge)
            .await?;
        println!(
            "Deleted attachment {} from {}.",
            self.attachment_id, instance_url
        );

        Ok(())
    }
}

/// Formats the deletion confirmation prompt.
fn format_delete_prompt(attachment_id: &str, purge: bool) -> String {
    if purge {
        format!("Permanently purge attachment {attachment_id}? [y/N] ")
    } else {
        format!("Delete attachment {attachment_id}? [y/N] ")
    }
}

/// Prompts the user for confirmation using the given reader for input.
fn confirm_with_reader(prompt: &str, reader: &mut dyn BufRead) -> Result<bool> {
    print!("{prompt}");
    io::stdout().flush()?;

    let mut answer = String::new();
    reader.read_line(&mut answer)?;
    Ok(answer.trim().eq_ignore_ascii_case("y"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::await_holding_lock)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn sample_attachment(id: &str, title: &str) -> ConfluenceAttachment {
        ConfluenceAttachment {
            id: id.to_string(),
            title: title.to_string(),
            media_type: Some("text/plain".to_string()),
            file_size: Some(42),
            download_url: Some("/dl".to_string()),
            version: Some(1),
            page_id: Some("12345".to_string()),
            file_id: Some("f-1".to_string()),
        }
    }

    fn sample_page(
        items: Vec<ConfluenceAttachment>,
        cursor: Option<&str>,
    ) -> ConfluenceAttachmentPage {
        ConfluenceAttachmentPage {
            results: items,
            next_cursor: cursor.map(str::to_string),
        }
    }

    // ── AttachmentCommand variants ────────────────────────────────

    #[test]
    fn attachment_subcommands_upload_variant() {
        let cmd = AttachmentCommand {
            command: AttachmentSubcommands::Upload(UploadCommand {
                page_id: "12345".to_string(),
                file: PathBuf::from("/tmp/x"),
                filename: None,
                comment: None,
                minor_edit: false,
            }),
        };
        assert!(matches!(cmd.command, AttachmentSubcommands::Upload(_)));
    }

    #[test]
    fn attachment_subcommands_list_variant() {
        let cmd = AttachmentCommand {
            command: AttachmentSubcommands::List(ListCommand {
                page_id: "12345".to_string(),
                cursor: None,
                limit: 25,
                output: OutputFormat::Table,
            }),
        };
        assert!(matches!(cmd.command, AttachmentSubcommands::List(_)));
    }

    #[test]
    fn attachment_subcommands_download_variant() {
        let cmd = AttachmentCommand {
            command: AttachmentSubcommands::Download(DownloadCommand {
                attachment_id: "att-1".to_string(),
                output: None,
            }),
        };
        assert!(matches!(cmd.command, AttachmentSubcommands::Download(_)));
    }

    #[test]
    fn attachment_subcommands_delete_variant() {
        let cmd = AttachmentCommand {
            command: AttachmentSubcommands::Delete(DeleteCommand {
                attachment_id: "att-1".to_string(),
                force: true,
                purge: false,
            }),
        };
        assert!(matches!(cmd.command, AttachmentSubcommands::Delete(_)));
    }

    // ── display_attachments ────────────────────────────────────────

    #[test]
    fn display_attachments_table() {
        let page = sample_page(vec![sample_attachment("a", "x.txt")], None);
        assert!(display_attachments(&page, &OutputFormat::Table).is_ok());
    }

    #[test]
    fn display_attachments_json() {
        let page = sample_page(vec![sample_attachment("a", "x.txt")], None);
        assert!(display_attachments(&page, &OutputFormat::Json).is_ok());
    }

    #[test]
    fn display_attachments_yaml() {
        let page = sample_page(vec![sample_attachment("a", "x.txt")], None);
        assert!(display_attachments(&page, &OutputFormat::Yaml).is_ok());
    }

    #[test]
    fn display_attachments_empty_table() {
        let page = sample_page(vec![], None);
        assert!(display_attachments(&page, &OutputFormat::Table).is_ok());
    }

    #[test]
    fn print_attachments_with_cursor() {
        let page = sample_page(vec![sample_attachment("a", "x.txt")], Some("NEXT"));
        print_attachments(&page);
    }

    // ── format_delete_prompt ───────────────────────────────────────

    #[test]
    fn format_delete_prompt_default() {
        assert_eq!(
            format_delete_prompt("att-1", false),
            "Delete attachment att-1? [y/N] "
        );
    }

    #[test]
    fn format_delete_prompt_purge() {
        assert_eq!(
            format_delete_prompt("att-1", true),
            "Permanently purge attachment att-1? [y/N] "
        );
    }

    // ── confirm_with_reader ────────────────────────────────────────

    #[test]
    fn confirm_yes_lowercase() {
        let mut input = Cursor::new(b"y\n");
        assert!(confirm_with_reader("Delete? ", &mut input).unwrap());
    }

    #[test]
    fn confirm_no() {
        let mut input = Cursor::new(b"n\n");
        assert!(!confirm_with_reader("Delete? ", &mut input).unwrap());
    }

    #[test]
    fn confirm_empty_is_no() {
        let mut input = Cursor::new(b"\n");
        assert!(!confirm_with_reader("Delete? ", &mut input).unwrap());
    }

    // ── run_upload (wiremock) ─────────────────────────────────

    #[tokio::test]
    async fn run_upload_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/child/attachment",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{"id": "att-1", "title": "hello.txt"}]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hello.txt");
        tokio::fs::write(&path, b"hi").await.unwrap();

        let client =
            crate::atlassian::client::AtlassianClient::new(&server.uri(), "u@t.com", "tok")
                .unwrap();
        let api = ConfluenceApi::new(client);
        assert!(run_upload(&api, "12345", &path, None, None, false)
            .await
            .is_ok());
    }

    // ── run_list (wiremock) ───────────────────────────────────

    #[tokio::test]
    async fn run_list_table_output() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/pages/12345/attachments",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"id": "a", "title": "x.txt", "mediaType": "text/plain", "fileSize": 1}
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client =
            crate::atlassian::client::AtlassianClient::new(&server.uri(), "u@t.com", "tok")
                .unwrap();
        let api = ConfluenceApi::new(client);
        assert!(run_list(&api, "12345", None, 25, &OutputFormat::Table)
            .await
            .is_ok());
    }

    // ── resolve_output_path ───────────────────────────────────────

    #[test]
    fn resolve_output_path_defaults_to_title_in_cwd() {
        assert_eq!(
            resolve_output_path(None, "diagram.png", "att-1"),
            PathBuf::from("diagram.png")
        );
    }

    #[test]
    fn resolve_output_path_explicit_file() {
        assert_eq!(
            resolve_output_path(Some(Path::new("/tmp/out.png")), "diagram.png", "att-1"),
            PathBuf::from("/tmp/out.png")
        );
    }

    #[test]
    fn resolve_output_path_existing_dir_joins_title() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_output_path(Some(dir.path()), "diagram.png", "att-1"),
            dir.path().join("diagram.png")
        );
    }

    #[test]
    fn resolve_output_path_sanitizes_traversal_title_in_cwd() {
        assert_eq!(
            resolve_output_path(None, "../../evil.txt", "att-1"),
            PathBuf::from("evil.txt")
        );
    }

    #[test]
    fn resolve_output_path_existing_dir_sanitizes_traversal() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_output_path(Some(dir.path()), "../x.png", "att-1"),
            dir.path().join("x.png")
        );
    }

    #[test]
    fn resolve_output_path_dotdot_title_falls_back_to_id() {
        assert_eq!(
            resolve_output_path(None, "..", "att-1"),
            PathBuf::from("attachment-att-1")
        );
    }

    // ── run_download (wiremock) ───────────────────────────────────

    #[tokio::test]
    async fn run_download_writes_file() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/attachments/att-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "att-1",
                    "title": "notes.txt",
                    "downloadLink": "/download/attachments/12345/notes.txt"
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/download/attachments/12345/notes.txt",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("downloaded.txt");
        let client =
            crate::atlassian::client::AtlassianClient::new(&server.uri(), "u@t.com", "tok")
                .unwrap();
        let api = ConfluenceApi::new(client);
        assert!(run_download(&api, "att-1", Some(&out)).await.is_ok());
        assert_eq!(std::fs::read(&out).unwrap(), b"hello");
    }

    // ── *Command::execute (env-mutex serialised) ──────────────────

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

    // Serialise on the one canonical env mutex (issue #950) — an independent
    // lock over the same process-global `ATLASSIAN_*` vars provides no mutual
    // exclusion against the other Atlassian credential tests.
    use crate::atlassian::auth::test_util::AUTH_ENV_MUTEX as ENV_MUTEX;

    #[tokio::test(flavor = "current_thread")]
    async fn upload_command_execute_runs_through_dispatch() {
        let _lock = ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/child/attachment",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{"id": "att-1", "title": "x.txt"}]
                })),
            )
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.txt");
        tokio::fs::write(&path, b"hi").await.unwrap();

        set_atlassian_env(&server.uri());
        let cmd = AttachmentCommand {
            command: AttachmentSubcommands::Upload(UploadCommand {
                page_id: "12345".to_string(),
                file: path,
                filename: None,
                comment: Some("v1".to_string()),
                minor_edit: true,
            }),
        };
        let result = cmd.execute().await;
        clear_atlassian_env();
        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn list_command_execute_runs_through_dispatch() {
        let _lock = ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/api/v2/pages/12345/attachments",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": []
                })),
            )
            .mount(&server)
            .await;

        set_atlassian_env(&server.uri());
        let cmd = AttachmentCommand {
            command: AttachmentSubcommands::List(ListCommand {
                page_id: "12345".to_string(),
                cursor: Some("opaque".to_string()),
                limit: 5,
                output: OutputFormat::Json,
            }),
        };
        let result = cmd.execute().await;
        clear_atlassian_env();
        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn download_command_execute_runs_through_dispatch() {
        let _lock = ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/attachments/att-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "att-1",
                    "title": "x.txt",
                    "downloadLink": "/download/attachments/12345/x.txt"
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/download/attachments/12345/x.txt",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"hi".to_vec()))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("x.txt");
        set_atlassian_env(&server.uri());
        let cmd = AttachmentCommand {
            command: AttachmentSubcommands::Download(DownloadCommand {
                attachment_id: "att-1".to_string(),
                output: Some(out.clone()),
            }),
        };
        let result = cmd.execute().await;
        clear_atlassian_env();
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(std::fs::read(&out).unwrap(), b"hi");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn delete_command_execute_force_runs_through_dispatch() {
        let _lock = ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/wiki/api/v2/attachments/att-1"))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .mount(&server)
            .await;

        set_atlassian_env(&server.uri());
        let cmd = AttachmentCommand {
            command: AttachmentSubcommands::Delete(DeleteCommand {
                attachment_id: "att-1".to_string(),
                force: true,
                purge: true,
            }),
        };
        let result = cmd.execute().await;
        clear_atlassian_env();
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn delete_command_prompt_yes_returns_true() {
        let cmd = DeleteCommand {
            attachment_id: "att-1".to_string(),
            force: false,
            purge: false,
        };
        let mut input = Cursor::new(b"y\n");
        assert!(cmd.prompt(&mut input).unwrap());
    }

    #[test]
    fn delete_command_prompt_no_returns_false() {
        let cmd = DeleteCommand {
            attachment_id: "att-1".to_string(),
            force: false,
            purge: true,
        };
        let mut input = Cursor::new(b"n\n");
        assert!(!cmd.prompt(&mut input).unwrap());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn delete_command_run_delete_unconfirmed_skips_api() {
        let _lock = ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // No DELETE mock — confirms the API is *not* called when not confirmed.
        let server = wiremock::MockServer::start().await;
        set_atlassian_env(&server.uri());
        let cmd = DeleteCommand {
            attachment_id: "att-1".to_string(),
            force: false,
            purge: false,
        };
        let result = cmd.run_delete(false).await;
        clear_atlassian_env();
        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn delete_command_execute_propagates_api_error() {
        let _lock = ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/wiki/api/v2/attachments/missing"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;

        set_atlassian_env(&server.uri());
        let cmd = DeleteCommand {
            attachment_id: "missing".to_string(),
            force: true,
            purge: false,
        };
        let result = cmd.execute().await;
        clear_atlassian_env();
        let err = result.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    // ── extra coverage for confirm_with_reader & print_upload ─────

    #[test]
    fn confirm_yes_uppercase() {
        let mut input = Cursor::new(b"Y\n");
        assert!(confirm_with_reader("Delete? ", &mut input).unwrap());
    }

    #[test]
    fn confirm_random_text_is_no() {
        let mut input = Cursor::new(b"maybe\n");
        assert!(!confirm_with_reader("Delete? ", &mut input).unwrap());
    }

    #[test]
    fn print_upload_confirmation_prints() {
        let attachment = sample_attachment("att-1", "x.txt");
        print_upload_confirmation(&attachment, "12345");
    }

    #[test]
    fn print_attachments_without_cursor() {
        let page = sample_page(vec![sample_attachment("a", "x.txt")], None);
        print_attachments(&page);
    }
}
