//! CLI command for `omni-dev gmail draft list`.

use std::io::Write;

use anyhow::{Context, Result};
use clap::Parser;

use crate::cli::gmail::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::cli::gmail::search::truncate;
use crate::gmail::client::GmailClient;
use crate::gmail::drafts_api::{DraftSummary, DraftsApi};
use crate::gmail::messages_api::{DEFAULT_ENRICH_CONCURRENCY, DEFAULT_SEARCH_LIMIT};

/// Column headers of the table view, in order.
const HEADERS: [&str; 7] = [
    "DRAFT_ID",
    "MESSAGE_ID",
    "THREAD_ID",
    "TO",
    "SUBJECT",
    "DATE",
    "SNIPPET",
];

/// Lists Gmail drafts, showing each one's draft id.
///
/// The draft id is what `gmail draft show`/`update` take. It is not the
/// message id: `gmail search in:drafts` finds the messages but can't
/// return their draft ids, which is why this command exists.
///
/// Each row costs one `messages.get` on top of the listing, since
/// `drafts.list` returns only ids. They run at `gmail search --enrich`'s
/// default concurrency. Works with a `gmail.readonly` account.
#[derive(Parser)]
pub struct ListCommand {
    /// Only list drafts matching this Gmail search query (same syntax as
    /// the Gmail search box, e.g. `to:alice subject:report`).
    #[arg(long)]
    pub query: Option<String>,

    /// Maximum drafts to return. `0` means "every draft" (capped at the
    /// same hard ceiling as `gmail search`).
    #[arg(long, default_value_t = DEFAULT_SEARCH_LIMIT)]
    pub limit: usize,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ListCommand {
    /// Runs the command against the shared client resolved by the parent
    /// `GmailCommand::execute`.
    pub async fn execute(self, client: &GmailClient) -> Result<()> {
        run_list(client, self.query.as_deref(), self.limit, &self.output).await
    }
}

/// Lists and hydrates drafts, then emits them in the requested format.
///
/// Split from [`ListCommand::execute`] so tests can inject a wiremock client
/// without going through the credential-loading path.
async fn run_list(
    client: &GmailClient,
    query: Option<&str>,
    limit: usize,
    output: &OutputFormat,
) -> Result<()> {
    let summaries = DraftsApi::new(client)
        .list_summaries(query, limit, DEFAULT_ENRICH_CONCURRENCY)
        .await?;
    if output_as(&summaries, output)? {
        return Ok(());
    }
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    render_draft_table(&summaries, &mut handle)
}

/// Renders drafts as an aligned text table with the columns in [`HEADERS`].
///
/// `TO` shows the `To` header only; `-o yaml`/`json` also carry `cc`/`bcc`.
/// The snippet is truncated like `gmail search`'s. An empty input prints
/// `No drafts returned.`.
fn render_draft_table(summaries: &[DraftSummary], out: &mut dyn Write) -> Result<()> {
    if summaries.is_empty() {
        writeln!(out, "No drafts returned.").context("Failed to write empty-table message")?;
        return Ok(());
    }

    // Sanitize server-supplied strings before computing column widths, so
    // a stripped control byte can't leave a column wider than what's
    // written (#1537).
    let rows: Vec<[String; 7]> = summaries
        .iter()
        .map(|s| {
            [
                sanitize_for_terminal(&s.draft_id),
                sanitize_for_terminal(&s.message_id),
                sanitize_for_terminal(&s.thread_id),
                sanitize_for_terminal(&s.to),
                sanitize_for_terminal(&s.subject),
                sanitize_for_terminal(&s.date),
                truncate(&sanitize_for_terminal(&s.snippet)),
            ]
        })
        .collect();

    let widths: [usize; 7] = std::array::from_fn(|column| {
        rows.iter()
            .map(|row| row[column].chars().count())
            .max()
            .unwrap_or(0)
            .max(HEADERS[column].len())
    });
    let separators = widths.map(|width| "-".repeat(width));

    write_row(out, &HEADERS, &widths)?;
    write_row(out, &separators, &widths)?;
    for row in &rows {
        write_row(out, row, &widths)?;
    }
    Ok(())
}

/// Writes one row with 2-space gutters, each cell left-aligned to its
/// column's width.
fn write_row(out: &mut dyn Write, cells: &[impl AsRef<str>], widths: &[usize]) -> Result<()> {
    let line = cells
        .iter()
        .zip(widths)
        .map(|(cell, &width)| format!("{:<width$}", cell.as_ref()))
        .collect::<Vec<_>>()
        .join("  ");
    writeln!(out, "{line}").context("Failed to write draft row")?;
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

    fn sample_summary(n: &str) -> DraftSummary {
        DraftSummary {
            draft_id: format!("r-{n}"),
            message_id: format!("m-{n}"),
            thread_id: format!("t-{n}"),
            to: "a@example.com".to_string(),
            cc: String::new(),
            bcc: String::new(),
            subject: "Hello".to_string(),
            date: "Mon, 1 Jan 2026".to_string(),
            snippet: "Hi there".to_string(),
        }
    }

    // ── render_draft_table ───────────────────────────────────────────

    #[test]
    fn render_draft_table_empty_prints_message() {
        let mut buf = Vec::new();
        render_draft_table(&[], &mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "No drafts returned.\n");
    }

    #[test]
    fn render_draft_table_labels_both_ids_and_every_column() {
        let mut buf = Vec::new();
        render_draft_table(&[sample_summary("1"), sample_summary("2")], &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        let header = out.lines().next().unwrap();
        let columns: Vec<&str> = header.split_whitespace().collect();
        assert_eq!(columns, HEADERS);
        let row = out.lines().nth(2).unwrap();
        for cell in ["r-1", "m-1", "t-1", "a@example.com", "Hello", "Hi there"] {
            assert!(row.contains(cell), "{row:?} lacks {cell}");
        }
        // Header + separator + 2 data rows.
        assert_eq!(out.lines().count(), 4);
    }

    #[test]
    fn render_draft_table_truncates_long_snippets() {
        let mut summary = sample_summary("1");
        summary.snippet = "x".repeat(200);
        let mut buf = Vec::new();
        render_draft_table(&[summary], &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains('…'));
        assert!(!out.contains(&"x".repeat(200)));
    }

    #[test]
    fn render_draft_table_strips_control_bytes_and_keeps_columns_aligned() {
        let mut evil = sample_summary("1");
        evil.subject = "evil\x1b[31msubject".to_string();
        evil.to = "b\r\x07@example.com".to_string();
        let mut buf = Vec::new();
        render_draft_table(&[evil, sample_summary("2")], &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(
            !out.contains(|c: char| c.is_control() && c != '\n'),
            "{out:?}"
        );
        let lengths: Vec<usize> = out.lines().map(|l| l.chars().count()).collect();
        assert!(lengths.windows(2).all(|w| w[0] == w[1]), "{out:?}");
    }

    // ── run_list ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn run_list_lists_and_hydrates_with_a_readonly_client() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/drafts"))
            .and(wiremock::matchers::query_param("q", "to:bob"))
            .and(wiremock::matchers::query_param("maxResults", "5"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "drafts": [{"id": "r1", "message": {"id": "m1", "threadId": "t1"}}],
                })),
            )
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages/m1"))
            .and(wiremock::matchers::query_param("format", "metadata"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "m1", "threadId": "t1"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        run_list(&client, Some("to:bob"), 5, &OutputFormat::Json)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_list_renders_an_empty_mailbox_as_a_table() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/drafts"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(&server)
            .await;

        run_list(&client, None, DEFAULT_SEARCH_LIMIT, &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_list_propagates_a_listing_error() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/drafts"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("nope"))
            .mount(&server)
            .await;

        let err = run_list(&client, None, 10, &OutputFormat::Table)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }
}
