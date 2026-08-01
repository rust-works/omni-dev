//! CLI command for `omni-dev gmail search`.

use std::io::Write;

use anyhow::{Context, Result};
use clap::Parser;

use crate::cli::gmail::format::{output_as, OutputFormat};
use crate::gmail::client::GmailClient;
use crate::gmail::messages_api::{MessageSummary, MessagesApi};

/// Maximum snippet length shown in the table view before truncation.
const SNIPPET_TRUNCATE_AT: usize = 60;

/// Searches Gmail messages (mirrors the `gmail_search` MCP tool).
#[derive(Parser)]
pub struct SearchCommand {
    /// Gmail search query (same syntax as the Gmail search box, e.g.
    /// `label:finance after:2026/01/01`).
    #[arg(long)]
    pub query: String,

    /// Maximum results to return. `0` means "fetch every match" (capped, like
    /// Datadog's log/event search, at a hard ceiling to bound run time and quota).
    #[arg(long, default_value_t = 50)]
    pub limit: usize,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl SearchCommand {
    /// Runs the command against the shared client resolved by the parent
    /// `GmailCommand::execute`.
    pub async fn execute(self, client: &GmailClient) -> Result<()> {
        run_search(client, &self.query, self.limit, &self.output).await
    }
}

/// Fetches enriched search results and emits them in the requested format.
///
/// Split from [`SearchCommand::execute`] so tests can inject a wiremock
/// client without going through the credential-loading path.
async fn run_search(
    client: &GmailClient,
    query: &str,
    limit: usize,
    output: &OutputFormat,
) -> Result<()> {
    let summaries = MessagesApi::new(client)
        .search_summaries(Some(query), &[], limit)
        .await?;
    if output_as(&summaries, output)? {
        return Ok(());
    }
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    render_search_table(&summaries, &mut handle)
}

/// Renders enriched search results as an aligned text table.
///
/// Column layout: `ID | FROM | SUBJECT | DATE | SNIPPET`. The snippet is
/// truncated to [`SNIPPET_TRUNCATE_AT`] chars — Gmail's own snippet is
/// already short, but even that is often too wide for a terminal table. An
/// empty input prints `No messages returned.`.
pub(crate) fn render_search_table(summaries: &[MessageSummary], out: &mut dyn Write) -> Result<()> {
    if summaries.is_empty() {
        writeln!(out, "No messages returned.").context("Failed to write empty-table message")?;
        return Ok(());
    }

    let snippets: Vec<String> = summaries.iter().map(|s| truncate(&s.snippet)).collect();

    let id_width = "ID"
        .len()
        .max(summaries.iter().map(|s| s.id.len()).max().unwrap_or(0));
    let from_width = "FROM"
        .len()
        .max(summaries.iter().map(|s| s.from.len()).max().unwrap_or(0));
    let subject_width = "SUBJECT"
        .len()
        .max(summaries.iter().map(|s| s.subject.len()).max().unwrap_or(0));
    let date_width = "DATE"
        .len()
        .max(summaries.iter().map(|s| s.date.len()).max().unwrap_or(0));
    let snippet_width = "SNIPPET"
        .len()
        .max(snippets.iter().map(String::len).max().unwrap_or(0));

    write_row(
        out,
        "ID",
        "FROM",
        "SUBJECT",
        "DATE",
        "SNIPPET",
        id_width,
        from_width,
        subject_width,
        date_width,
        snippet_width,
    )?;
    write_row(
        out,
        &"-".repeat(id_width),
        &"-".repeat(from_width),
        &"-".repeat(subject_width),
        &"-".repeat(date_width),
        &"-".repeat(snippet_width),
        id_width,
        from_width,
        subject_width,
        date_width,
        snippet_width,
    )?;
    for (i, summary) in summaries.iter().enumerate() {
        write_row(
            out,
            &summary.id,
            &summary.from,
            &summary.subject,
            &summary.date,
            &snippets[i],
            id_width,
            from_width,
            subject_width,
            date_width,
            snippet_width,
        )?;
    }
    Ok(())
}

fn truncate(text: &str) -> String {
    if text.chars().count() <= SNIPPET_TRUNCATE_AT {
        text.to_string()
    } else {
        let mut truncated: String = text.chars().take(SNIPPET_TRUNCATE_AT).collect();
        truncated.push('…');
        truncated
    }
}

/// Writes a single row of the bespoke search table with consistent 2-space
/// gutters between cells.
#[allow(clippy::too_many_arguments)]
fn write_row(
    out: &mut dyn Write,
    id: &str,
    from: &str,
    subject: &str,
    date: &str,
    snippet: &str,
    id_w: usize,
    from_w: usize,
    subject_w: usize,
    date_w: usize,
    snippet_w: usize,
) -> Result<()> {
    writeln!(
        out,
        "{id:<id_w$}  {from:<from_w$}  {subject:<subject_w$}  {date:<date_w$}  {snippet:<snippet_w$}"
    )
    .context("Failed to write search row")?;
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

    fn sample_summary(id: &str) -> MessageSummary {
        MessageSummary {
            id: id.to_string(),
            thread_id: "t1".to_string(),
            from: "a@example.com".to_string(),
            subject: "Hello".to_string(),
            date: "Mon, 1 Jan 2026".to_string(),
            snippet: "Hi there".to_string(),
        }
    }

    // ── truncate ─────────────────────────────────────────────────────

    #[test]
    fn truncate_leaves_short_snippet_unchanged() {
        assert_eq!(truncate("short"), "short");
    }

    #[test]
    fn truncate_shortens_long_snippet_with_ellipsis() {
        let long = "a".repeat(SNIPPET_TRUNCATE_AT + 20);
        let truncated = truncate(&long);
        assert_eq!(truncated.chars().count(), SNIPPET_TRUNCATE_AT + 1);
        assert!(truncated.ends_with('…'));
    }

    // ── render_search_table ──────────────────────────────────────────

    #[test]
    fn render_table_empty_prints_message() {
        let mut buf = Vec::new();
        render_search_table(&[], &mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "No messages returned.\n");
    }

    #[test]
    fn render_table_writes_header_and_rows() {
        let summaries = [sample_summary("m1"), sample_summary("m2")];
        let mut buf = Vec::new();
        render_search_table(&summaries, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("ID"));
        assert!(out.contains("FROM"));
        assert!(out.contains("SUBJECT"));
        assert!(out.contains("DATE"));
        assert!(out.contains("SNIPPET"));
        assert!(out.contains("m1"));
        assert!(out.contains("m2"));
        // Header + separator + 2 data rows = 4 lines.
        assert_eq!(out.lines().count(), 4);
    }

    struct FailAfter {
        successes_remaining: usize,
    }
    impl Write for FailAfter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.successes_remaining == 0 {
                return Err(std::io::Error::other("test forced write failure"));
            }
            if buf.contains(&b'\n') {
                self.successes_remaining -= 1;
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn render_table_propagates_header_write_errors() {
        let summaries = [sample_summary("m1")];
        let err = render_search_table(
            &summaries,
            &mut FailAfter {
                successes_remaining: 0,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    #[test]
    fn render_table_empty_propagates_write_errors() {
        let err = render_search_table(
            &[],
            &mut FailAfter {
                successes_remaining: 0,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("empty-table message"));
    }

    // ── run_search ───────────────────────────────────────────────────

    #[tokio::test]
    async fn run_search_table_path_writes_to_stdout() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "messages": [{"id": "m1", "threadId": "t1"}]
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages/m1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "m1"})),
            )
            .mount(&server)
            .await;

        run_search(&client, "label:finance", 5, &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_search_json_path_returns_ok() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "messages": []
                })),
            )
            .mount(&server)
            .await;

        run_search(&client, "*", 5, &OutputFormat::Json)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_search_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let err = run_search(&client, "*", 5, &OutputFormat::Table)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    // ── SearchCommand::execute glue ────────────────────────────────

    #[tokio::test]
    async fn execute_passes_query_through() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages"))
            .and(wiremock::matchers::query_param("q", "label:finance"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "messages": []
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let cmd = SearchCommand {
            query: "label:finance".to_string(),
            limit: 5,
            output: OutputFormat::Json,
        };
        cmd.execute(&client).await.unwrap();
    }
}
