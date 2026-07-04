//! CLI commands for managing Confluence page attachments.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::atlassian::confluence_api::ConfluenceApi;
use crate::atlassian::confluence_types::{ConfluenceAttachment, ConfluenceAttachmentPage};
use crate::cli::atlassian::confirm::{guard_destructive_with_io, GuardOptions, GuardOutcome};
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

    /// Prints what would be deleted without making any API calls.
    #[arg(long)]
    pub dry_run: bool,

    /// Permanently purges the attachment instead of moving to trash (requires space admin).
    #[arg(long)]
    pub purge: bool,
}

impl DeleteCommand {
    /// Executes the delete command.
    pub async fn execute(self) -> Result<()> {
        let (client, instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);
        let mut reader = io::BufReader::new(io::stdin());
        let mut writer = io::stdout();
        self.execute_with_io(&api, &instance_url, &mut reader, &mut writer)
            .await
    }

    /// Inner form taking explicit API, instance URL, and IO handles, for unit tests.
    async fn execute_with_io(
        self,
        api: &ConfluenceApi,
        instance_url: &str,
        reader: &mut (dyn BufRead + Send),
        writer: &mut (dyn Write + Send),
    ) -> Result<()> {
        if !self.force || self.dry_run {
            let prompt = format_delete_prompt(&self.attachment_id, self.purge);
            let dry_run_message = if self.purge {
                format!("Would permanently purge attachment {}.", self.attachment_id)
            } else {
                format!("Would delete attachment {}.", self.attachment_id)
            };

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
        }

        api.delete_attachment(&self.attachment_id, self.purge)
            .await?;
        writeln!(
            writer,
            "Deleted attachment {} from {}.",
            self.attachment_id, instance_url
        )?;

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
                dry_run: false,
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

    // ── DeleteCommand::execute_with_io (wiremock, injected IO) ────

    fn delete_test_api(server: &wiremock::MockServer) -> ConfluenceApi {
        let client =
            crate::atlassian::client::AtlassianClient::new(&server.uri(), "u@t.com", "tok")
                .unwrap();
        ConfluenceApi::new(client)
    }

    async fn mount_delete_mock(server: &wiremock::MockServer, attachment_id: &str) {
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path(format!(
                "/wiki/api/v2/attachments/{attachment_id}"
            )))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(server)
            .await;
    }

    async fn run_delete_with_io(
        cmd: DeleteCommand,
        api: &ConfluenceApi,
        input: &[u8],
    ) -> (Result<()>, String) {
        let mut reader = Cursor::new(input.to_vec());
        let mut output = Vec::<u8>::new();
        let result = cmd
            .execute_with_io(
                api,
                "https://example.atlassian.net",
                &mut reader,
                &mut output,
            )
            .await;
        (result, String::from_utf8(output).unwrap())
    }

    #[tokio::test]
    async fn delete_execute_with_force_calls_delete() {
        let server = wiremock::MockServer::start().await;
        mount_delete_mock(&server, "att-1").await;

        let cmd = DeleteCommand {
            attachment_id: "att-1".to_string(),
            force: true,
            dry_run: false,
            purge: true,
        };
        let (result, out) = run_delete_with_io(cmd, &delete_test_api(&server), b"").await;
        assert!(result.is_ok(), "{result:?}");
        assert!(out.contains("Deleted attachment att-1 from https://example.atlassian.net."));
    }

    #[tokio::test]
    async fn delete_execute_with_dry_run_does_not_call_delete() {
        // No DELETE mock — confirms the API is *not* called on dry-run.
        let server = wiremock::MockServer::start().await;

        let cmd = DeleteCommand {
            attachment_id: "att-1".to_string(),
            force: false,
            dry_run: true,
            purge: false,
        };
        let (result, out) = run_delete_with_io(cmd, &delete_test_api(&server), b"").await;
        assert!(result.is_ok(), "{result:?}");
        assert!(out.contains("Would delete attachment att-1."));
        assert!(!out.contains("Deleted attachment"));
    }

    #[tokio::test]
    async fn delete_execute_with_dry_run_and_purge_adjusts_wording() {
        let server = wiremock::MockServer::start().await;

        let cmd = DeleteCommand {
            attachment_id: "att-1".to_string(),
            force: false,
            dry_run: true,
            purge: true,
        };
        let (result, out) = run_delete_with_io(cmd, &delete_test_api(&server), b"").await;
        assert!(result.is_ok(), "{result:?}");
        assert!(out.contains("Would permanently purge attachment att-1."));
    }

    #[tokio::test]
    async fn delete_execute_dry_run_wins_over_force() {
        // No DELETE mock — dry-run takes precedence and skips the API.
        let server = wiremock::MockServer::start().await;

        let cmd = DeleteCommand {
            attachment_id: "att-1".to_string(),
            force: true,
            dry_run: true,
            purge: false,
        };
        let (result, out) = run_delete_with_io(cmd, &delete_test_api(&server), b"").await;
        assert!(result.is_ok(), "{result:?}");
        assert!(out.contains("Would delete attachment att-1."));
        assert!(!out.contains("Deleted attachment"));
    }

    #[tokio::test]
    async fn delete_execute_with_prompt_yes_calls_delete() {
        let server = wiremock::MockServer::start().await;
        mount_delete_mock(&server, "att-1").await;

        let cmd = DeleteCommand {
            attachment_id: "att-1".to_string(),
            force: false,
            dry_run: false,
            purge: false,
        };
        let (result, out) = run_delete_with_io(cmd, &delete_test_api(&server), b"y\n").await;
        assert!(result.is_ok(), "{result:?}");
        assert!(out.contains("Delete attachment att-1? [y/N]"));
        assert!(out.contains("Deleted attachment att-1"));
    }

    #[tokio::test]
    async fn delete_execute_with_prompt_no_skips_api() {
        // No DELETE mock — confirms the API is *not* called when declined.
        let server = wiremock::MockServer::start().await;

        let cmd = DeleteCommand {
            attachment_id: "att-1".to_string(),
            force: false,
            dry_run: false,
            purge: true,
        };
        let (result, out) = run_delete_with_io(cmd, &delete_test_api(&server), b"n\n").await;
        assert!(result.is_ok(), "{result:?}");
        assert!(out.contains("Permanently purge attachment att-1? [y/N]"));
        assert!(out.contains("Cancelled."));
        assert!(!out.contains("Deleted attachment"));
    }

    #[tokio::test]
    async fn delete_execute_propagates_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/wiki/api/v2/attachments/missing"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;

        let cmd = DeleteCommand {
            attachment_id: "missing".to_string(),
            force: true,
            dry_run: false,
            purge: false,
        };
        let (result, _out) = run_delete_with_io(cmd, &delete_test_api(&server), b"").await;
        let err = result.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    /// Dry-run with a failing writer covers `?` on guard_destructive_with_io.
    #[tokio::test]
    async fn delete_execute_dry_run_propagates_guard_error() {
        use crate::test_support::failing_io::FailingWriter;
        let server = wiremock::MockServer::start().await;
        let cmd = DeleteCommand {
            attachment_id: "att-1".to_string(),
            force: false,
            dry_run: true,
            purge: false,
        };
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut writer = FailingWriter;
        let err = cmd
            .execute_with_io(
                &delete_test_api(&server),
                "https://example.atlassian.net",
                &mut input,
                &mut writer,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("simulated write failure"));
    }

    /// Force-mode + failing writer covers `?` on the post-API writeln.
    #[tokio::test]
    async fn delete_execute_force_propagates_writeln_error() {
        use crate::test_support::failing_io::FailingWriter;
        let server = wiremock::MockServer::start().await;
        mount_delete_mock(&server, "att-1").await;
        let cmd = DeleteCommand {
            attachment_id: "att-1".to_string(),
            force: true,
            dry_run: false,
            purge: false,
        };
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut writer = FailingWriter;
        let err = cmd
            .execute_with_io(
                &delete_test_api(&server),
                "https://example.atlassian.net",
                &mut input,
                &mut writer,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("simulated write failure"));
    }

    /// End-to-end exercise of the public `execute()` wrapper through the
    /// `AttachmentCommand` dispatch arm.
    #[tokio::test]
    async fn delete_execute_with_force_drives_create_client_and_calls_delete() {
        use crate::test_support::atlassian_env::AtlassianEnvGuard;
        let server = wiremock::MockServer::start().await;
        mount_delete_mock(&server, "att-1").await;

        let _env = AtlassianEnvGuard::new(&server.uri(), "u@t.com", "tok");
        let cmd = AttachmentCommand {
            command: AttachmentSubcommands::Delete(DeleteCommand {
                attachment_id: "att-1".to_string(),
                force: true,
                dry_run: false,
                purge: false,
            }),
        };
        cmd.execute().await.unwrap();
    }

    // ── extra coverage for print_upload ────────────────────────────

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
