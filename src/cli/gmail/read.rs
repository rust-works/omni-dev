//! CLI command for `omni-dev gmail read`.

use std::fs;
use std::io::Write;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};

use crate::cli::gmail::format::{output_as, OutputFormat};
use crate::gmail::client::GmailClient;
use crate::gmail::messages_api::{MessageFormat, MessagesApi};
use crate::gmail::types::Message;

/// How much of the message to fetch.
#[derive(Clone, Copy, Debug, Default, ValueEnum)]
pub enum ReadFormat {
    /// Headers and snippet only, no body.
    Meta,
    /// The full parsed MIME structure. Default.
    #[default]
    Full,
    /// The full RFC 2822 message, base64url-encoded.
    Raw,
}

impl ReadFormat {
    fn as_message_format(self) -> MessageFormat {
        match self {
            Self::Meta => MessageFormat::Metadata,
            Self::Full => MessageFormat::Full,
            Self::Raw => MessageFormat::Raw,
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
    #[arg(long, value_enum, default_value_t = ReadFormat::Full)]
    pub format: ReadFormat,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ReadCommand {
    /// Runs the command against the shared client resolved by the parent
    /// `GmailCommand::execute`.
    pub async fn execute(self, client: &GmailClient) -> Result<()> {
        run_read(
            client,
            &self.message_id,
            self.format,
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
    format: ReadFormat,
    out_file: Option<&str>,
    output: &OutputFormat,
) -> Result<()> {
    let message = MessagesApi::new(client)
        .get(message_id, format.as_message_format(), &[])
        .await?;

    if let Some(path) = out_file {
        let rendered = render_plain_text(&message);
        fs::write(path, &rendered).with_context(|| format!("Failed to write to {path}"))?;
        println!("Saved to: {path}");
        return Ok(());
    }

    if output_as(&message, output)? {
        return Ok(());
    }
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    render_read_table(&message, &mut handle)
}

/// Renders a message as a flat `key: value` header block followed by its
/// snippet — an `.eml`-ish preview for `--out-file`, not a markdown dialect.
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
    if let Some(raw) = &message.raw {
        lines.push(raw.clone());
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
    fn read_format_maps_to_message_format() {
        assert!(matches!(
            ReadFormat::Meta.as_message_format(),
            MessageFormat::Metadata
        ));
        assert!(matches!(
            ReadFormat::Full.as_message_format(),
            MessageFormat::Full
        ));
        assert!(matches!(
            ReadFormat::Raw.as_message_format(),
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

    #[test]
    fn render_plain_text_includes_raw_source_when_present() {
        let message = Message {
            id: "m1".to_string(),
            raw: Some("raw-rfc2822-bytes".to_string()),
            ..Default::default()
        };
        let text = render_plain_text(&message);
        assert!(text.contains("raw-rfc2822-bytes"));
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
            ReadFormat::Full,
            Some(path.to_str().unwrap()),
            &OutputFormat::Table,
        )
        .await
        .unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("Hi there"));
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

        run_read(&client, "m1", ReadFormat::Full, None, &OutputFormat::Table)
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

        run_read(&client, "m1", ReadFormat::Full, None, &OutputFormat::Json)
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

        let err = run_read(&client, "m1", ReadFormat::Full, None, &OutputFormat::Table)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn run_read_uses_metadata_format_for_meta() {
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

        run_read(&client, "m1", ReadFormat::Meta, None, &OutputFormat::Table)
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
            format: ReadFormat::Full,
            output: OutputFormat::Json,
        };
        cmd.execute(&client).await.unwrap();
    }
}
