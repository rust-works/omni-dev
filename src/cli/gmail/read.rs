//! CLI command for `omni-dev gmail read`.

use std::fs;
use std::io::Write;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};

use crate::cli::gmail::format::{output_as, OutputFormat};
use crate::gmail::client::GmailClient;
use crate::gmail::messages_api::{MessageFormat, MessagesApi};
use crate::gmail::raw_message::decode_raw_message;
use crate::gmail::render::render_markdown;
use crate::gmail::types::Message;

/// How much of the message to fetch.
///
/// Named `--detail`, not `--format`: ADR-0046 retired `--format` project-wide
/// (every surviving `--format` in the codebase is a hidden deprecated alias
/// for `-o/--output`), so a new *visible* `--format` would be the only one
/// left and would collide in spirit with that migration — Gmail's `format`
/// is a request-side projection, not an output format, which is a different
/// axis from `-o` entirely (the same reasoning ADR-0046 applies to
/// `--out-file`). Variant names match Gmail's own wire values verbatim
/// (`minimal`/`metadata`/`full`/`raw`) rather than an invented shorthand.
#[derive(Clone, Copy, Debug, Default, ValueEnum)]
pub enum ReadDetail {
    /// Only `id`/`threadId`/`labelIds`/`sizeEstimate` — no headers or body.
    Minimal,
    /// Headers and snippet only, no body.
    Metadata,
    /// The full parsed MIME structure. Default.
    #[default]
    Full,
    /// The full RFC 2822 message, base64url-encoded.
    Raw,
}

impl ReadDetail {
    fn as_message_format(self) -> MessageFormat {
        match self {
            Self::Minimal => MessageFormat::Minimal,
            Self::Metadata => MessageFormat::Metadata,
            Self::Full => MessageFormat::Full,
            Self::Raw => MessageFormat::Raw,
        }
    }
}

/// Output format for `gmail read`, extending the shared [`OutputFormat`]
/// with `Markdown` — a human-readable rendering of the message headers
/// (RFC 2047-decoded) and body via
/// [`render_markdown`](crate::gmail::render::render_markdown), the same
/// function `gmail render` uses for archived `.eml` files (#1513). Kept
/// local to `read` rather than added to the shared `OutputFormat` used
/// crate-wide: every other CLI surface's `-o` renders arbitrary
/// `Serialize` data generically, and a `Markdown` variant only makes sense
/// for a MIME message.
#[derive(Clone, Debug, Default, ValueEnum)]
pub enum ReadOutputFormat {
    /// Human-readable table (id/thread-id/labels/snippet). Default.
    #[default]
    Table,
    /// JSON.
    Json,
    /// YAML (single document).
    Yaml,
    /// YAML stream (`---`-separated multi-document).
    Yamls,
    /// JSON Lines.
    Jsonl,
    /// Human-readable Markdown rendering of the full message (headers +
    /// body). Always fetches the complete raw MIME message regardless of
    /// `--detail`, since rendering needs the full message structure.
    Markdown,
}

impl ReadOutputFormat {
    /// Converts to the shared [`OutputFormat`] for the non-`Markdown`
    /// variants. Never called for `Markdown`, which `run_read` handles
    /// before this conversion is needed.
    fn as_shared(&self) -> OutputFormat {
        match self {
            Self::Table => OutputFormat::Table,
            Self::Json => OutputFormat::Json,
            Self::Yaml => OutputFormat::Yaml,
            Self::Yamls => OutputFormat::Yamls,
            Self::Jsonl => OutputFormat::Jsonl,
            Self::Markdown => unreachable!("Markdown is handled before this point in run_read"),
        }
    }
}

/// Reads a single Gmail message.
///
/// (mirrors the `gmail_message_read` MCP tool)
#[derive(Parser)]
pub struct ReadCommand {
    /// Gmail message id.
    pub message_id: String,

    /// Output file (writes to stdout if omitted).
    #[arg(long = "out-file", value_name = "PATH")]
    pub out_file: Option<String>,

    /// How much of the message to fetch.
    #[arg(long, value_enum, default_value_t = ReadDetail::Full)]
    pub detail: ReadDetail,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = ReadOutputFormat::Table)]
    pub output: ReadOutputFormat,
}

impl ReadCommand {
    /// Runs the command against the shared client resolved by the parent
    /// `GmailCommand::execute`.
    pub async fn execute(self, client: &GmailClient) -> Result<()> {
        run_read(
            client,
            &self.message_id,
            self.detail,
            self.out_file.as_deref(),
            &self.output,
        )
        .await
    }
}

/// Fetches the message and emits it in the requested format.
///
/// Split from [`ReadCommand::execute`] so tests can inject a wiremock
/// client without going through the credential-loading path.
async fn run_read(
    client: &GmailClient,
    message_id: &str,
    detail: ReadDetail,
    out_file: Option<&str>,
    output: &ReadOutputFormat,
) -> Result<()> {
    if matches!(output, ReadOutputFormat::Markdown) {
        // Rendering needs the full raw MIME message regardless of
        // `--detail`, so this fetches with `format=raw` directly rather
        // than honouring `detail.as_message_format()`.
        let message = MessagesApi::new(client)
            .get(message_id, MessageFormat::Raw, &[])
            .await?;
        let bytes = decode_raw_message(&message)?;
        let markdown = render_markdown(&bytes);

        if let Some(path) = out_file {
            fs::write(path, &markdown).with_context(|| format!("Failed to write to {path}"))?;
            println!("Saved to: {path}");
            return Ok(());
        }
        print!("{markdown}");
        return Ok(());
    }

    let message = MessagesApi::new(client)
        .get(message_id, detail.as_message_format(), &[])
        .await?;

    if let Some(path) = out_file {
        if matches!(detail, ReadDetail::Raw) {
            let bytes = decode_raw_message(&message)?;
            fs::write(path, &bytes).with_context(|| format!("Failed to write to {path}"))?;
        } else {
            let rendered = render_plain_text(&message);
            fs::write(path, &rendered).with_context(|| format!("Failed to write to {path}"))?;
        }
        println!("Saved to: {path}");
        return Ok(());
    }

    if output_as(&message, &output.as_shared())? {
        return Ok(());
    }
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    render_read_table(&message, &mut handle)
}

/// Renders a message as a flat `key: value` header block followed by its
/// snippet — an `.eml`-ish preview for `--out-file` on non-`raw` details,
/// not a markdown dialect. `--detail raw` never reaches this: it writes the
/// decoded bytes from [`decode_raw_message`] instead, since Gmail's `raw`
/// field only comes back populated for that format.
fn render_plain_text(message: &Message) -> String {
    let mut lines = Vec::new();
    lines.push(format!("Id: {}", message.id));
    if let Some(thread_id) = &message.thread_id {
        lines.push(format!("Thread-Id: {thread_id}"));
    }
    if !message.label_ids.is_empty() {
        lines.push(format!("Labels: {}", message.label_ids.join(", ")));
    }
    lines.push(String::new());
    if let Some(snippet) = &message.snippet {
        lines.push(snippet.clone());
    }
    lines.join("\n")
}

/// Renders a single message as a bespoke header block — a "table" in the
/// sense of "one command, one rendering," not a literal grid, matching the
/// Datadog `monitor get` precedent for single-record views.
fn render_read_table(message: &Message, out: &mut dyn Write) -> Result<()> {
    writeln!(out, "Id: {}", message.id).context("Failed to write read row")?;
    if let Some(thread_id) = &message.thread_id {
        writeln!(out, "Thread-Id: {thread_id}").context("Failed to write read row")?;
    }
    if !message.label_ids.is_empty() {
        writeln!(out, "Labels: {}", message.label_ids.join(", "))
            .context("Failed to write read row")?;
    }
    if let Some(snippet) = &message.snippet {
        writeln!(out, "Snippet: {snippet}").context("Failed to write read row")?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::gmail::auth::{GmailCredentials, GmailScope};
    use crate::utils::secret::Secret;
    use base64::Engine as _;

    fn test_credentials() -> GmailCredentials {
        GmailCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: GmailScope::ReadOnly,
        }
    }

    async fn client_with_bootstrapped_token(server: &wiremock::MockServer) -> GmailClient {
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

        let mut client = GmailClient::new(&server.uri(), &test_credentials()).unwrap();
        crate::gmail::client::test_support::replace_session(
            &mut client,
            &test_credentials(),
            &format!("{}/token", server.uri()),
        );
        client
    }

    #[test]
    fn read_detail_maps_to_message_format() {
        assert!(matches!(
            ReadDetail::Minimal.as_message_format(),
            MessageFormat::Minimal
        ));
        assert!(matches!(
            ReadDetail::Metadata.as_message_format(),
            MessageFormat::Metadata
        ));
        assert!(matches!(
            ReadDetail::Full.as_message_format(),
            MessageFormat::Full
        ));
        assert!(matches!(
            ReadDetail::Raw.as_message_format(),
            MessageFormat::Raw
        ));
    }

    #[test]
    fn render_plain_text_includes_id_labels_and_snippet() {
        let message = Message {
            id: "m1".to_string(),
            thread_id: Some("t1".to_string()),
            label_ids: vec!["INBOX".to_string(), "UNREAD".to_string()],
            snippet: Some("Hi there".to_string()),
            ..Default::default()
        };
        let text = render_plain_text(&message);
        assert!(text.contains("Id: m1"));
        assert!(text.contains("Thread-Id: t1"));
        assert!(text.contains("Labels: INBOX, UNREAD"));
        assert!(text.contains("Hi there"));
    }

    // ── render_read_table ────────────────────────────────────────────

    #[test]
    fn render_read_table_writes_id_thread_labels_and_snippet() {
        let message = Message {
            id: "m1".to_string(),
            thread_id: Some("t1".to_string()),
            label_ids: vec!["INBOX".to_string(), "UNREAD".to_string()],
            snippet: Some("Hi there".to_string()),
            ..Default::default()
        };
        let mut buf = Vec::new();
        render_read_table(&message, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("Id: m1"));
        assert!(text.contains("Thread-Id: t1"));
        assert!(text.contains("Labels: INBOX, UNREAD"));
        assert!(text.contains("Snippet: Hi there"));
    }

    #[test]
    fn render_read_table_omits_absent_fields() {
        let message = Message {
            id: "m1".to_string(),
            ..Default::default()
        };
        let mut buf = Vec::new();
        render_read_table(&message, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert_eq!(text, "Id: m1\n");
    }

    // ── run_read ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn run_read_writes_to_out_file() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages/m1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "m1",
                    "snippet": "Hi there",
                })),
            )
            .mount(&server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("message.txt");
        run_read(
            &client,
            "m1",
            ReadDetail::Full,
            Some(path.to_str().unwrap()),
            &ReadOutputFormat::Table,
        )
        .await
        .unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("Hi there"));
    }

    #[tokio::test]
    async fn run_read_detail_raw_out_file_writes_decoded_bytes_not_base64() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let source = "From: a@example.com\r\nSubject: Hi\r\n\r\nBody text.";
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(source);
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages/m1"))
            .and(wiremock::matchers::query_param("format", "raw"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "m1",
                    "raw": encoded,
                })),
            )
            .mount(&server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("message.eml");
        run_read(
            &client,
            "m1",
            ReadDetail::Raw,
            Some(path.to_str().unwrap()),
            &ReadOutputFormat::Table,
        )
        .await
        .unwrap();

        // A genuine byte-exact copy: no Id:/Thread-Id:/Labels: preamble, no
        // duplicated snippet, and definitely not still base64-encoded.
        let bytes = fs::read(&path).unwrap();
        assert_eq!(bytes, source.as_bytes());
    }

    #[tokio::test]
    async fn run_read_detail_raw_out_file_propagates_decode_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages/m1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "m1"})),
            )
            .mount(&server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("message.eml");
        let err = run_read(
            &client,
            "m1",
            ReadDetail::Raw,
            Some(path.to_str().unwrap()),
            &ReadOutputFormat::Table,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no `raw` field"));
        assert!(!path.exists());
    }

    // ── ReadOutputFormat::Markdown ──────────────────────────────────

    #[tokio::test]
    async fn run_read_markdown_writes_rendered_markdown_to_out_file() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let source = "Subject: Hi\r\nFrom: a@example.com\r\n\r\nBody text.";
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(source);
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages/m1"))
            .and(wiremock::matchers::query_param("format", "raw"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "m1",
                    "raw": encoded,
                })),
            )
            .mount(&server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("message.md");
        run_read(
            &client,
            "m1",
            ReadDetail::Full,
            Some(path.to_str().unwrap()),
            &ReadOutputFormat::Markdown,
        )
        .await
        .unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("# Hi"));
        assert!(content.contains("Body text."));
    }

    #[tokio::test]
    async fn run_read_markdown_ignores_detail_and_always_fetches_raw() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let source = "Subject: Hi\r\n\r\nBody.";
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(source);
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages/m1"))
            .and(wiremock::matchers::query_param("format", "raw"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "m1",
                    "raw": encoded,
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        // `--detail minimal` would normally request `format=minimal`; the
        // mock above only matches `format=raw`, so a request for anything
        // else 404s against wiremock's unmatched-request default and this
        // would fail if `-o markdown` didn't override `detail`.
        run_read(
            &client,
            "m1",
            ReadDetail::Minimal,
            None,
            &ReadOutputFormat::Markdown,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_read_markdown_prints_to_stdout_without_out_file() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let source = "Subject: Hi\r\n\r\nBody.";
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(source);
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages/m1"))
            .and(wiremock::matchers::query_param("format", "raw"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "m1",
                    "raw": encoded,
                })),
            )
            .mount(&server)
            .await;

        run_read(
            &client,
            "m1",
            ReadDetail::Full,
            None,
            &ReadOutputFormat::Markdown,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_read_markdown_propagates_decode_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages/m1"))
            .and(wiremock::matchers::query_param("format", "raw"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "m1"})),
            )
            .mount(&server)
            .await;

        let err = run_read(
            &client,
            "m1",
            ReadDetail::Full,
            None,
            &ReadOutputFormat::Markdown,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no `raw` field"));
    }

    #[tokio::test]
    async fn run_read_table_path_writes_to_stdout() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages/m1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "m1"})),
            )
            .mount(&server)
            .await;

        run_read(
            &client,
            "m1",
            ReadDetail::Full,
            None,
            &ReadOutputFormat::Table,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_read_json_path_returns_ok() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages/m1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "m1"})),
            )
            .mount(&server)
            .await;

        run_read(
            &client,
            "m1",
            ReadDetail::Full,
            None,
            &ReadOutputFormat::Json,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_read_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages/m1"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;

        let err = run_read(
            &client,
            "m1",
            ReadDetail::Full,
            None,
            &ReadOutputFormat::Table,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn run_read_uses_metadata_format_for_metadata_detail() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages/m1"))
            .and(wiremock::matchers::query_param("format", "metadata"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "m1"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        run_read(
            &client,
            "m1",
            ReadDetail::Metadata,
            None,
            &ReadOutputFormat::Table,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_read_uses_minimal_format_for_minimal_detail() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages/m1"))
            .and(wiremock::matchers::query_param("format", "minimal"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "m1"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        run_read(
            &client,
            "m1",
            ReadDetail::Minimal,
            None,
            &ReadOutputFormat::Table,
        )
        .await
        .unwrap();
    }

    // ── ReadCommand::execute glue ────────────────────────────────────

    #[tokio::test]
    async fn execute_passes_message_id_through() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages/m42"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "m42"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let cmd = ReadCommand {
            message_id: "m42".to_string(),
            out_file: None,
            detail: ReadDetail::Full,
            output: ReadOutputFormat::Json,
        };
        cmd.execute(&client).await.unwrap();
    }
}
