//! CLI command for `omni-dev drive read`.

use anyhow::{Context, Result};
use clap::Parser;

use crate::cli::drive::format::{output_as, OutputFormat};
use crate::drive::client::DriveClient;
use crate::drive::files_api::FilesApi;
use crate::drive::types::DriveFile;

const GOOGLE_DOC: &str = "application/vnd.google-apps.document";
const GOOGLE_SHEET: &str = "application/vnd.google-apps.spreadsheet";
const GOOGLE_SLIDES: &str = "application/vnd.google-apps.presentation";
/// Reused by the `drive_file_read` MCP tool's content-mode folder rejection.
pub(crate) const GOOGLE_FOLDER: &str = "application/vnd.google-apps.folder";
/// Reused by the `drive_file_read` MCP tool's content-mode shortcut rejection.
pub(crate) const GOOGLE_SHORTCUT: &str = "application/vnd.google-apps.shortcut";

/// Reads a single Drive file's metadata or content.
#[derive(Parser)]
pub struct ReadCommand {
    /// Drive file id (from `drive search`, or the `id` segment of a Drive
    /// URL).
    pub file_id: String,

    /// Fetches the file's actual content instead of its metadata.
    /// Google-native files (Docs/Sheets/Slides/...) are exported (see
    /// `--export-mime-type`); every other file is downloaded as-is.
    #[arg(long)]
    pub content: bool,

    /// Export MIME type for a Google-native file's content (only relevant
    /// with `--content`; ignored for non-Google-native files, which are
    /// downloaded as-is regardless). Defaults: Google Docs -> text/markdown,
    /// Google Sheets -> text/csv (first sheet only — Drive's export API has
    /// no multi-sheet CSV format), Google Slides -> text/plain. Required for
    /// every other Google-native type (Forms, Drawings, Apps Script, Sites,
    /// ...); the error names the file's actually supported export MIME
    /// types.
    #[arg(long = "export-mime-type", value_name = "MIME_TYPE")]
    pub export_mime_type: Option<String>,

    /// Writes fetched content to this path instead of stdout. Only valid
    /// with `--content` — metadata always renders via `-o/--output`.
    #[arg(long = "out-file", value_name = "PATH")]
    pub out_file: Option<String>,

    /// Output format (metadata only).
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ReadCommand {
    /// Runs the command against the shared client resolved by the parent
    /// `DriveCommand::execute`.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        run_read(
            client,
            &self.file_id,
            self.content,
            self.export_mime_type.as_deref(),
            self.out_file.as_deref(),
            &self.output,
        )
        .await
    }
}

/// Fetches the file (metadata or content) and emits it in the requested
/// format.
///
/// Split from [`ReadCommand::execute`] so tests can inject a wiremock
/// client without going through the credential-loading path.
async fn run_read(
    client: &DriveClient,
    file_id: &str,
    content: bool,
    export_mime_type: Option<&str>,
    out_file: Option<&str>,
    output: &OutputFormat,
) -> Result<()> {
    if content {
        run_read_content(client, file_id, export_mime_type, out_file).await
    } else {
        anyhow::ensure!(
            out_file.is_none(),
            "--out-file requires --content; `drive read` without --content always renders \
             metadata via -o/--output"
        );
        run_read_metadata(client, file_id, output).await
    }
}

async fn run_read_metadata(
    client: &DriveClient,
    file_id: &str,
    output: &OutputFormat,
) -> Result<()> {
    let meta = FilesApi::new(client).get_metadata(file_id).await?;
    if output_as(&meta, output)? {
        return Ok(());
    }
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    render_metadata_table(&meta, &mut handle)
}

/// Renders a single file's metadata as a bespoke header block — a "table"
/// in the sense of "one command, one rendering," not a literal grid,
/// matching `crate::cli::gmail::read::render_read_table`'s precedent.
fn render_metadata_table(file: &DriveFile, out: &mut dyn std::io::Write) -> Result<()> {
    writeln!(out, "Id: {}", file.id).context("Failed to write read row")?;
    writeln!(out, "Name: {}", file.name).context("Failed to write read row")?;
    writeln!(out, "MimeType: {}", file.mime_type).context("Failed to write read row")?;
    if let Some(size) = &file.size {
        writeln!(out, "Size: {size}").context("Failed to write read row")?;
    }
    if let Some(modified) = &file.modified_time {
        writeln!(out, "Modified: {modified}").context("Failed to write read row")?;
    }
    if !file.parents.is_empty() {
        writeln!(out, "Parents: {}", file.parents.join(", "))
            .context("Failed to write read row")?;
    }
    if let Some(link) = &file.web_view_link {
        writeln!(out, "WebViewLink: {link}").context("Failed to write read row")?;
    }
    Ok(())
}

/// Content path: always fetches metadata first (to learn `mimeType` and
/// `exportLinks`), then either exports (Google-native) or downloads
/// (everything else).
async fn run_read_content(
    client: &DriveClient,
    file_id: &str,
    export_mime_type: Option<&str>,
    out_file: Option<&str>,
) -> Result<()> {
    let api = FilesApi::new(client);
    let meta = api.get_metadata(file_id).await?;

    if meta.mime_type == GOOGLE_FOLDER {
        anyhow::bail!(
            "'{}' is a folder; folders have no content to read — use `drive search` to list \
             what it contains",
            meta.name
        );
    }
    if meta.mime_type == GOOGLE_SHORTCUT {
        anyhow::bail!(
            "'{}' is a shortcut; `drive read --content` doesn't follow shortcuts to their \
             target file — resolve the target file's id and read that instead",
            meta.name
        );
    }

    let (bytes, content_mime_type) = if meta.is_google_native() {
        let export_mime = resolve_export_mime_type(&meta, export_mime_type)?;
        let bytes = api.export(file_id, &export_mime).await?;
        (bytes, export_mime)
    } else {
        let bytes = api.download(file_id).await?;
        (bytes, meta.mime_type.clone())
    };

    if let Some(path) = out_file {
        std::fs::write(path, &bytes).with_context(|| format!("Failed to write to {path}"))?;
        println!(
            "Saved {} bytes to {path} (mimeType: {content_mime_type}).",
            bytes.len()
        );
        return Ok(());
    }

    // No --out-file: print texty content directly; refuse to garble the
    // terminal with binary content.
    if is_texty(&content_mime_type) {
        if let Ok(text) = String::from_utf8(bytes) {
            print!("{text}");
            return Ok(());
        }
        anyhow::bail!(
            "content (mimeType: {content_mime_type}) claims to be text but is not valid \
             UTF-8; use --out-file to save it"
        );
    }
    anyhow::bail!(
        "refusing to print binary content (mimeType: {content_mime_type}) to the terminal; \
         use --out-file <PATH> to save it"
    );
}

/// Resolves the export MIME type: an explicit `--export-mime-type` always
/// wins (never validated against `exportLinks` client-side — the server is
/// authoritative and already returns a clear error for an unsupported
/// value); otherwise Docs/Sheets/Slides fall back to a documented default;
/// every other Google-native type has no safe default and requires
/// `--export-mime-type`, so the error lists the file's actually supported
/// export MIME types from `exportLinks`.
///
/// `pub(crate)`: also reused by the `drive_file_read` MCP tool
/// (`src/mcp/drive_tools.rs`, issue #1525) for its `format: "content"` path.
pub(crate) fn resolve_export_mime_type(meta: &DriveFile, explicit: Option<&str>) -> Result<String> {
    if let Some(mime) = explicit {
        return Ok(mime.to_string());
    }
    if let Some(default) = default_export_mime_type(&meta.mime_type) {
        return Ok(default.to_string());
    }
    let available = meta.export_links.as_ref().map_or_else(
        || "(none)".to_string(),
        |links| {
            let mut keys: Vec<&str> = links.keys().map(String::as_str).collect();
            keys.sort_unstable();
            keys.join(", ")
        },
    );
    Err(anyhow::anyhow!(
        "'{}' (mimeType: {}) has no default export format; pass --export-mime-type. \
         Supported export MIME types: {available}",
        meta.name,
        meta.mime_type,
    ))
}

/// The default export MIME type for a Google-native `mime_type`, or `None`
/// when there isn't a safe one (Forms, Drawings, Apps Script, Sites, ...).
fn default_export_mime_type(mime_type: &str) -> Option<&'static str> {
    match mime_type {
        GOOGLE_DOC => Some("text/markdown"),
        GOOGLE_SHEET => Some("text/csv"),
        GOOGLE_SLIDES => Some("text/plain"),
        _ => None,
    }
}

/// Whether `mime_type` is safe to print directly to a terminal (or return
/// inline from the `drive_file_read` MCP tool, its other `pub(crate)`
/// consumer — issue #1525).
pub(crate) fn is_texty(mime_type: &str) -> bool {
    mime_type.starts_with("text/") || mime_type == "application/json"
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, SCOPE_READONLY};
    use crate::utils::secret::Secret;

    fn test_credentials() -> DriveCredentials {
        DriveCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: SCOPE_READONLY.to_string(),
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

    // ── render_metadata_table ────────────────────────────────────────

    #[test]
    fn render_metadata_table_writes_all_present_fields() {
        let file = DriveFile {
            id: "f1".to_string(),
            name: "report.pdf".to_string(),
            mime_type: "application/pdf".to_string(),
            size: Some("1024".to_string()),
            modified_time: Some("2026-01-01T00:00:00.000Z".to_string()),
            parents: vec!["folder1".to_string()],
            web_view_link: Some("https://drive.google.com/file/d/f1/view".to_string()),
            ..Default::default()
        };
        let mut buf = Vec::new();
        render_metadata_table(&file, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("Id: f1"));
        assert!(text.contains("Name: report.pdf"));
        assert!(text.contains("MimeType: application/pdf"));
        assert!(text.contains("Size: 1024"));
        assert!(text.contains("Modified: 2026-01-01T00:00:00.000Z"));
        assert!(text.contains("Parents: folder1"));
        assert!(text.contains("WebViewLink: https://drive.google.com/file/d/f1/view"));
    }

    #[test]
    fn render_metadata_table_omits_absent_fields() {
        let file = DriveFile {
            id: "f1".to_string(),
            name: "n".to_string(),
            ..Default::default()
        };
        let mut buf = Vec::new();
        render_metadata_table(&file, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert_eq!(text, "Id: f1\nName: n\nMimeType: \n");
    }

    // ── default_export_mime_type ──────────────────────────────────────

    #[test]
    fn default_export_mime_type_maps_docs_sheets_slides() {
        assert_eq!(default_export_mime_type(GOOGLE_DOC), Some("text/markdown"));
        assert_eq!(default_export_mime_type(GOOGLE_SHEET), Some("text/csv"));
        assert_eq!(default_export_mime_type(GOOGLE_SLIDES), Some("text/plain"));
    }

    #[test]
    fn default_export_mime_type_none_for_other_google_native_and_non_native() {
        assert_eq!(
            default_export_mime_type("application/vnd.google-apps.form"),
            None
        );
        assert_eq!(default_export_mime_type("application/pdf"), None);
    }

    // ── resolve_export_mime_type ───────────────────────────────────────

    #[test]
    fn resolve_export_mime_type_explicit_wins_over_default() {
        let meta = DriveFile {
            mime_type: GOOGLE_DOC.to_string(),
            ..Default::default()
        };
        let result = resolve_export_mime_type(&meta, Some("application/pdf")).unwrap();
        assert_eq!(result, "application/pdf");
    }

    #[test]
    fn resolve_export_mime_type_falls_back_to_doc_default() {
        let meta = DriveFile {
            mime_type: GOOGLE_DOC.to_string(),
            ..Default::default()
        };
        assert_eq!(
            resolve_export_mime_type(&meta, None).unwrap(),
            "text/markdown"
        );
    }

    #[test]
    fn resolve_export_mime_type_falls_back_to_sheet_default() {
        let meta = DriveFile {
            mime_type: GOOGLE_SHEET.to_string(),
            ..Default::default()
        };
        assert_eq!(resolve_export_mime_type(&meta, None).unwrap(), "text/csv");
    }

    #[test]
    fn resolve_export_mime_type_falls_back_to_slides_default() {
        let meta = DriveFile {
            mime_type: GOOGLE_SLIDES.to_string(),
            ..Default::default()
        };
        assert_eq!(resolve_export_mime_type(&meta, None).unwrap(), "text/plain");
    }

    #[test]
    fn resolve_export_mime_type_errors_listing_export_links_keys_for_unsupported_type() {
        let mut links = std::collections::HashMap::new();
        links.insert("application/pdf".to_string(), "url1".to_string());
        links.insert("text/plain".to_string(), "url2".to_string());
        let meta = DriveFile {
            name: "myform".to_string(),
            mime_type: "application/vnd.google-apps.form".to_string(),
            export_links: Some(links),
            ..Default::default()
        };
        let err = resolve_export_mime_type(&meta, None).unwrap_err();
        assert!(err.to_string().contains("--export-mime-type"));
        assert!(err.to_string().contains("application/pdf"));
        assert!(err.to_string().contains("text/plain"));
    }

    #[test]
    fn resolve_export_mime_type_errors_with_none_when_export_links_absent() {
        let meta = DriveFile {
            name: "myform".to_string(),
            mime_type: "application/vnd.google-apps.form".to_string(),
            ..Default::default()
        };
        let err = resolve_export_mime_type(&meta, None).unwrap_err();
        assert!(err.to_string().contains("(none)"));
    }

    // ── is_texty ────────────────────────────────────────────────────

    #[test]
    fn is_texty_true_for_text_star_and_json() {
        assert!(is_texty("text/plain"));
        assert!(is_texty("text/csv"));
        assert!(is_texty("application/json"));
    }

    #[test]
    fn is_texty_false_for_binary() {
        assert!(!is_texty("application/pdf"));
        assert!(!is_texty("image/png"));
    }

    // ── run_read (metadata) ────────────────────────────────────────

    #[tokio::test]
    async fn run_read_table_path_writes_to_stdout() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "n",
                })),
            )
            .mount(&server)
            .await;

        run_read(&client, "f1", false, None, None, &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_read_json_path_returns_ok() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "n",
                })),
            )
            .mount(&server)
            .await;

        run_read(&client, "f1", false, None, None, &OutputFormat::Json)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_read_metadata_propagates_get_metadata_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/missing"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;

        let err = run_read(&client, "missing", false, None, None, &OutputFormat::Table)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn run_read_rejects_out_file_without_content() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;

        let err = run_read(
            &client,
            "f1",
            false,
            None,
            Some("/tmp/out.txt"),
            &OutputFormat::Table,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("--out-file requires --content"));
    }

    // ── run_read (content) ─────────────────────────────────────────

    #[tokio::test]
    async fn run_read_content_non_google_native_downloads_via_alt_media() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "n", "mimeType": "text/plain",
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param("alt", "media"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()))
            .expect(1)
            .mount(&server)
            .await;

        run_read(&client, "f1", true, None, None, &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_read_content_rejects_folder_with_actionable_error() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "My Folder", "mimeType": GOOGLE_FOLDER,
                })),
            )
            .mount(&server)
            .await;

        let err = run_read(&client, "f1", true, None, None, &OutputFormat::Table)
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("'My Folder' is a folder"), "{message}");
        assert!(message.contains("drive search"), "{message}");
    }

    #[tokio::test]
    async fn run_read_content_rejects_shortcut_with_actionable_error() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "My Shortcut", "mimeType": GOOGLE_SHORTCUT,
                })),
            )
            .mount(&server)
            .await;

        let err = run_read(&client, "f1", true, None, None, &OutputFormat::Table)
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("'My Shortcut' is a shortcut"), "{message}");
    }

    #[tokio::test]
    async fn run_read_content_google_doc_default_exports_text_markdown() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "n", "mimeType": GOOGLE_DOC,
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1/export"))
            .and(wiremock::matchers::query_param("mimeType", "text/markdown"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"# Title".to_vec()))
            .expect(1)
            .mount(&server)
            .await;

        run_read(&client, "f1", true, None, None, &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_read_content_google_sheet_default_exports_text_csv() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "n", "mimeType": GOOGLE_SHEET,
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1/export"))
            .and(wiremock::matchers::query_param("mimeType", "text/csv"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"a,b\n1,2".to_vec()))
            .expect(1)
            .mount(&server)
            .await;

        run_read(&client, "f1", true, None, None, &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_read_content_google_slides_default_exports_text_plain() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "n", "mimeType": GOOGLE_SLIDES,
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1/export"))
            .and(wiremock::matchers::query_param("mimeType", "text/plain"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_bytes(b"slide text".to_vec()),
            )
            .expect(1)
            .mount(&server)
            .await;

        run_read(&client, "f1", true, None, None, &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_read_content_explicit_export_mime_type_overrides_default() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "n", "mimeType": GOOGLE_DOC,
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1/export"))
            .and(wiremock::matchers::query_param(
                "mimeType",
                "application/pdf",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"%PDF".to_vec()))
            .expect(1)
            .mount(&server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("doc.pdf");
        run_read(
            &client,
            "f1",
            true,
            Some("application/pdf"),
            Some(path.to_str().unwrap()),
            &OutputFormat::Table,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"%PDF");
    }

    #[tokio::test]
    async fn run_read_content_other_google_native_without_export_mime_type_errors_listing_export_links(
    ) {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1",
                    "name": "myform",
                    "mimeType": "application/vnd.google-apps.form",
                    "exportLinks": {"application/zip": "url"},
                })),
            )
            .mount(&server)
            .await;

        let err = run_read(&client, "f1", true, None, None, &OutputFormat::Table)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("--export-mime-type"));
        assert!(err.to_string().contains("application/zip"));
    }

    #[tokio::test]
    async fn run_read_content_writes_out_file_and_prints_summary_byte_exact() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "n", "mimeType": "application/pdf",
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param("alt", "media"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"%PDF-1.4".to_vec()))
            .mount(&server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("out.pdf");
        run_read(
            &client,
            "f1",
            true,
            None,
            Some(path.to_str().unwrap()),
            &OutputFormat::Table,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"%PDF-1.4");
    }

    #[tokio::test]
    async fn run_read_content_prints_texty_content_without_out_file() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "n", "mimeType": "text/plain",
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param("alt", "media"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()))
            .mount(&server)
            .await;

        run_read(&client, "f1", true, None, None, &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_read_content_refuses_binary_content_without_out_file() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "n", "mimeType": "application/pdf",
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param("alt", "media"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"%PDF".to_vec()))
            .mount(&server)
            .await;

        let err = run_read(&client, "f1", true, None, None, &OutputFormat::Table)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("refusing to print binary content"));
    }

    #[tokio::test]
    async fn run_read_content_invalid_utf8_texty_content_without_out_file_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "n", "mimeType": "text/plain",
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param("alt", "media"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_bytes(vec![0xff, 0xfe, 0xfd]),
            )
            .mount(&server)
            .await;

        let err = run_read(&client, "f1", true, None, None, &OutputFormat::Table)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not valid UTF-8"));
    }

    #[tokio::test]
    async fn run_read_content_propagates_export_api_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "n", "mimeType": GOOGLE_DOC,
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1/export"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let err = run_read(&client, "f1", true, None, None, &OutputFormat::Table)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    #[tokio::test]
    async fn run_read_content_propagates_download_api_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param_is_missing("alt"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "f1", "name": "n", "mimeType": "text/plain",
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .and(wiremock::matchers::query_param("alt", "media"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let err = run_read(&client, "f1", true, None, None, &OutputFormat::Table)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    // ── ReadCommand::execute glue ────────────────────────────────────

    #[tokio::test]
    async fn execute_passes_file_id_through() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f42"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "f42", "name": "n"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let cmd = ReadCommand {
            file_id: "f42".to_string(),
            content: false,
            export_mime_type: None,
            out_file: None,
            output: OutputFormat::Json,
        };
        cmd.execute(&client).await.unwrap();
    }
}
