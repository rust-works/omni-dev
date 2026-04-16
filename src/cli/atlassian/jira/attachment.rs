//! CLI commands for JIRA issue attachments.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::atlassian::client::{AtlassianClient, JiraAttachment};
use crate::cli::atlassian::helpers::create_client;

/// Image MIME types for filtering.
const IMAGE_MIME_TYPES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/svg+xml",
    "image/webp",
];

/// Manages JIRA issue attachments.
#[derive(Parser)]
pub struct AttachmentCommand {
    /// The attachment subcommand to execute.
    #[command(subcommand)]
    pub command: AttachmentSubcommands,
}

/// Attachment subcommands.
#[derive(Subcommand)]
pub enum AttachmentSubcommands {
    /// Downloads all attachments (or filtered by pattern).
    Download(DownloadCommand),
    /// Downloads only image attachments.
    Images(ImagesCommand),
}

impl AttachmentCommand {
    /// Executes the attachment command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            AttachmentSubcommands::Download(cmd) => cmd.execute().await,
            AttachmentSubcommands::Images(cmd) => cmd.execute().await,
        }
    }
}

/// Downloads all attachments for an issue.
#[derive(Parser)]
pub struct DownloadCommand {
    /// JIRA issue key (e.g., PROJ-123).
    pub key: String,

    /// Output directory (defaults to current directory).
    #[arg(long, default_value = ".")]
    pub output_dir: String,

    /// Filter filenames by substring (case-insensitive).
    #[arg(long)]
    pub filter: Option<String>,
}

impl DownloadCommand {
    /// Fetches attachment metadata and downloads matching files.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        run_download(&client, &self.key, &self.output_dir, self.filter.as_deref()).await
    }
}

/// Fetches, filters, and downloads attachments for an issue.
async fn run_download(
    client: &AtlassianClient,
    key: &str,
    output_dir: &str,
    filter: Option<&str>,
) -> Result<()> {
    let attachments = client.get_attachments(key).await?;
    let filtered = filter_attachments(&attachments, filter);

    if filtered.is_empty() {
        println!("No attachments found.");
        return Ok(());
    }

    ensure_dir(output_dir)?;

    for attachment in &filtered {
        download_file(client, attachment, output_dir).await?;
    }

    println!("Downloaded {} file(s) to {output_dir}.", filtered.len());
    Ok(())
}

/// Downloads only image attachments for an issue.
#[derive(Parser)]
pub struct ImagesCommand {
    /// JIRA issue key (e.g., PROJ-123).
    pub key: String,

    /// Output directory (defaults to current directory).
    #[arg(long, default_value = ".")]
    pub output_dir: String,
}

impl ImagesCommand {
    /// Fetches attachment metadata and downloads image files.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        run_images(&client, &self.key, &self.output_dir).await
    }
}

/// Fetches and downloads image attachments for an issue.
async fn run_images(client: &AtlassianClient, key: &str, output_dir: &str) -> Result<()> {
    let attachments = client.get_attachments(key).await?;
    let images = filter_images(&attachments);

    if images.is_empty() {
        println!("No image attachments found.");
        return Ok(());
    }

    ensure_dir(output_dir)?;

    for attachment in &images {
        download_file(client, attachment, output_dir).await?;
    }

    println!("Downloaded {} image(s) to {output_dir}.", images.len());
    Ok(())
}

/// Filters attachments by a case-insensitive substring match on filename.
fn filter_attachments<'a>(
    attachments: &'a [JiraAttachment],
    filter: Option<&str>,
) -> Vec<&'a JiraAttachment> {
    match filter {
        Some(pattern) => {
            let pattern_lower = pattern.to_lowercase();
            attachments
                .iter()
                .filter(|a| a.filename.to_lowercase().contains(&pattern_lower))
                .collect()
        }
        None => attachments.iter().collect(),
    }
}

/// Filters attachments to only images by MIME type.
fn filter_images(attachments: &[JiraAttachment]) -> Vec<&JiraAttachment> {
    attachments
        .iter()
        .filter(|a| IMAGE_MIME_TYPES.contains(&a.mime_type.as_str()))
        .collect()
}

/// Creates the output directory if it doesn't exist.
fn ensure_dir(dir: &str) -> Result<()> {
    if !Path::new(dir).exists() {
        fs::create_dir_all(dir)
            .with_context(|| format!("Failed to create output directory: {dir}"))?;
    }
    Ok(())
}

/// Downloads a single attachment to the output directory.
async fn download_file(
    client: &crate::atlassian::client::AtlassianClient,
    attachment: &JiraAttachment,
    output_dir: &str,
) -> Result<()> {
    eprintln!(
        "Downloading {} ({})...",
        attachment.filename,
        format_size(attachment.size)
    );
    let data = client.get_bytes(&attachment.content_url).await?;
    let path = Path::new(output_dir).join(&attachment.filename);
    fs::write(&path, &data).with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
}

/// Formats a file size for display.
fn format_size(size: u64) -> String {
    if size < 1024 {
        format!("{size} B")
    } else if size < 1024 * 1024 {
        format!("{:.1} KB", size as f64 / 1024.0)
    } else {
        format!("{:.1} MB", size as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn sample_attachment(id: &str, filename: &str, mime_type: &str, size: u64) -> JiraAttachment {
        JiraAttachment {
            id: id.to_string(),
            filename: filename.to_string(),
            mime_type: mime_type.to_string(),
            size,
            content_url: format!("https://org.atlassian.net/attachment/{id}"),
        }
    }

    // ── filter_attachments ─────────────────────────────────────────

    #[test]
    fn filter_no_pattern_returns_all() {
        let attachments = vec![
            sample_attachment("1", "file.txt", "text/plain", 100),
            sample_attachment("2", "image.png", "image/png", 200),
        ];
        let result = filter_attachments(&attachments, None);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn filter_by_pattern() {
        let attachments = vec![
            sample_attachment("1", "screenshot.png", "image/png", 100),
            sample_attachment("2", "report.pdf", "application/pdf", 200),
            sample_attachment("3", "Screenshot_2.png", "image/png", 300),
        ];
        let result = filter_attachments(&attachments, Some("screenshot"));
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn filter_no_match() {
        let attachments = vec![sample_attachment("1", "file.txt", "text/plain", 100)];
        let result = filter_attachments(&attachments, Some("nonexistent"));
        assert!(result.is_empty());
    }

    // ── filter_images ──────────────────────────────────────────────

    #[test]
    fn filter_images_mixed() {
        let attachments = vec![
            sample_attachment("1", "photo.png", "image/png", 100),
            sample_attachment("2", "doc.pdf", "application/pdf", 200),
            sample_attachment("3", "icon.gif", "image/gif", 50),
            sample_attachment("4", "page.svg", "image/svg+xml", 75),
            sample_attachment("5", "hero.webp", "image/webp", 150),
            sample_attachment("6", "photo.jpg", "image/jpeg", 300),
        ];
        let result = filter_images(&attachments);
        assert_eq!(result.len(), 5);
    }

    #[test]
    fn filter_images_none() {
        let attachments = vec![
            sample_attachment("1", "doc.pdf", "application/pdf", 200),
            sample_attachment("2", "data.json", "application/json", 100),
        ];
        let result = filter_images(&attachments);
        assert!(result.is_empty());
    }

    // ── format_size ────────────────────────────────────────────────

    #[test]
    fn format_size_bytes() {
        assert_eq!(format_size(500), "500 B");
    }

    #[test]
    fn format_size_kilobytes() {
        assert_eq!(format_size(2048), "2.0 KB");
    }

    #[test]
    fn format_size_megabytes() {
        assert_eq!(format_size(5_242_880), "5.0 MB");
    }

    #[test]
    fn format_size_zero() {
        assert_eq!(format_size(0), "0 B");
    }

    // ── ensure_dir ─────────────────────────────────────────────────

    #[test]
    fn ensure_dir_creates_directory() {
        let temp = tempfile::tempdir().unwrap();
        let new_dir = temp.path().join("subdir");
        ensure_dir(new_dir.to_str().unwrap()).unwrap();
        assert!(new_dir.exists());
    }

    #[test]
    fn ensure_dir_existing_is_ok() {
        let temp = tempfile::tempdir().unwrap();
        ensure_dir(temp.path().to_str().unwrap()).unwrap();
    }

    // ── run_download (wiremock) ──────────────────────────────────────

    #[tokio::test]
    async fn run_download_success() {
        let server = wiremock::MockServer::start().await;
        let content_url = format!("{}/attachment/1", server.uri());

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "fields": {
                        "attachment": [{
                            "id": "1",
                            "filename": "test.txt",
                            "mimeType": "text/plain",
                            "size": 5,
                            "content": content_url
                        }]
                    }
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/attachment/1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"hello".as_slice()))
            .expect(1)
            .mount(&server)
            .await;

        let client =
            crate::atlassian::client::AtlassianClient::new(&server.uri(), "u@t.com", "tok")
                .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let result = run_download(&client, "PROJ-1", temp.path().to_str().unwrap(), None).await;
        assert!(result.is_ok());
        assert!(temp.path().join("test.txt").exists());
    }

    #[tokio::test]
    async fn run_download_empty() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"fields": {"attachment": []}})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client =
            crate::atlassian::client::AtlassianClient::new(&server.uri(), "u@t.com", "tok")
                .unwrap();
        let result = run_download(&client, "PROJ-1", ".", None).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn run_download_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/NOPE-1"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client =
            crate::atlassian::client::AtlassianClient::new(&server.uri(), "u@t.com", "tok")
                .unwrap();
        let err = run_download(&client, "NOPE-1", ".", None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    // ── run_images (wiremock) ─────────────────────────────────────────

    #[tokio::test]
    async fn run_images_success() {
        let server = wiremock::MockServer::start().await;
        let content_url = format!("{}/attachment/1", server.uri());

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({
                    "fields": {
                        "attachment": [
                            {"id": "1", "filename": "photo.png", "mimeType": "image/png", "size": 100, "content": content_url},
                            {"id": "2", "filename": "doc.pdf", "mimeType": "application/pdf", "size": 200, "content": "https://example.com/2"}
                        ]
                    }
                }),
            ))
            .expect(1)
            .mount(&server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/attachment/1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_bytes(b"\x89PNG".as_slice()),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client =
            crate::atlassian::client::AtlassianClient::new(&server.uri(), "u@t.com", "tok")
                .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let result = run_images(&client, "PROJ-1", temp.path().to_str().unwrap()).await;
        assert!(result.is_ok());
        assert!(temp.path().join("photo.png").exists());
    }

    #[tokio::test]
    async fn run_images_no_images() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({
                    "fields": {
                        "attachment": [
                            {"id": "1", "filename": "doc.pdf", "mimeType": "application/pdf", "size": 200, "content": "https://example.com/1"}
                        ]
                    }
                }),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let client =
            crate::atlassian::client::AtlassianClient::new(&server.uri(), "u@t.com", "tok")
                .unwrap();
        let result = run_images(&client, "PROJ-1", ".").await;
        assert!(result.is_ok());
    }

    // ── dispatch ───────────────────────────────────────────────────

    #[test]
    fn attachment_command_download_variant() {
        let cmd = AttachmentCommand {
            command: AttachmentSubcommands::Download(DownloadCommand {
                key: "PROJ-1".to_string(),
                output_dir: ".".to_string(),
                filter: None,
            }),
        };
        assert!(matches!(cmd.command, AttachmentSubcommands::Download(_)));
    }

    #[test]
    fn attachment_command_images_variant() {
        let cmd = AttachmentCommand {
            command: AttachmentSubcommands::Images(ImagesCommand {
                key: "PROJ-1".to_string(),
                output_dir: ".".to_string(),
            }),
        };
        assert!(matches!(cmd.command, AttachmentSubcommands::Images(_)));
    }

    #[test]
    fn download_command_with_filter() {
        let cmd = DownloadCommand {
            key: "PROJ-1".to_string(),
            output_dir: "/tmp/out".to_string(),
            filter: Some("screenshot".to_string()),
        };
        assert_eq!(cmd.filter.as_deref(), Some("screenshot"));
    }
}
