//! CLI command for `omni-dev gmail label list`.

use std::io::Write;

use anyhow::{Context, Result};
use clap::Parser;

use crate::cli::gmail::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::gmail::client::GmailClient;
use crate::gmail::labels_api::LabelsApi;
use crate::gmail::types::Label;

/// Lists Gmail labels.
#[derive(Parser)]
pub struct ListCommand {
    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ListCommand {
    /// Runs the command against the shared client resolved by the parent
    /// `GmailCommand::execute`.
    pub async fn execute(self, client: &GmailClient) -> Result<()> {
        run_list(client, &self.output).await
    }
}

/// Fetches every label and emits it in the requested format.
///
/// Split from [`ListCommand::execute`] so tests can inject a wiremock
/// client without going through the credential-loading path.
async fn run_list(client: &GmailClient, output: &OutputFormat) -> Result<()> {
    let response = LabelsApi::new(client).list().await?;
    if output_as(&response.labels, output)? {
        return Ok(());
    }
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    render_label_table(&response.labels, &mut handle)
}

/// Renders labels as an aligned text table.
///
/// Column layout: `ID | NAME | TYPE | UNREAD | TOTAL`. An empty input
/// prints `No labels returned.`.
fn render_label_table(labels: &[Label], out: &mut dyn Write) -> Result<()> {
    if labels.is_empty() {
        writeln!(out, "No labels returned.").context("Failed to write empty-table message")?;
        return Ok(());
    }

    // Sanitize server-supplied strings *before* computing column widths, so
    // a stripped control byte can't leave a column one character too wide
    // for what's actually written (#1537). `messages_unread`/`messages_total`
    // are numeric, locally formatted, and not attacker-controlled.
    let rows: Vec<[String; 5]> = labels
        .iter()
        .map(|l| {
            [
                sanitize_for_terminal(&l.id),
                sanitize_for_terminal(&l.name),
                sanitize_for_terminal(l.label_type.as_deref().unwrap_or("-")),
                l.messages_unread
                    .map_or_else(|| "-".to_string(), |n| n.to_string()),
                l.messages_total
                    .map_or_else(|| "-".to_string(), |n| n.to_string()),
            ]
        })
        .collect();

    let id_width = "ID"
        .len()
        .max(rows.iter().map(|r| r[0].len()).max().unwrap_or(0));
    let name_width = "NAME"
        .len()
        .max(rows.iter().map(|r| r[1].len()).max().unwrap_or(0));
    let type_width = "TYPE"
        .len()
        .max(rows.iter().map(|r| r[2].len()).max().unwrap_or(0));
    let unread_width = "UNREAD"
        .len()
        .max(rows.iter().map(|r| r[3].len()).max().unwrap_or(0));
    let total_width = "TOTAL"
        .len()
        .max(rows.iter().map(|r| r[4].len()).max().unwrap_or(0));

    write_row(
        out,
        "ID",
        "NAME",
        "TYPE",
        "UNREAD",
        "TOTAL",
        id_width,
        name_width,
        type_width,
        unread_width,
        total_width,
    )?;
    write_row(
        out,
        &"-".repeat(id_width),
        &"-".repeat(name_width),
        &"-".repeat(type_width),
        &"-".repeat(unread_width),
        &"-".repeat(total_width),
        id_width,
        name_width,
        type_width,
        unread_width,
        total_width,
    )?;
    for row in &rows {
        write_row(
            out,
            &row[0],
            &row[1],
            &row[2],
            &row[3],
            &row[4],
            id_width,
            name_width,
            type_width,
            unread_width,
            total_width,
        )?;
    }
    Ok(())
}

/// Writes a single row of the bespoke label table with consistent 2-space
/// gutters between cells.
#[allow(clippy::too_many_arguments)]
fn write_row(
    out: &mut dyn Write,
    id: &str,
    name: &str,
    label_type: &str,
    unread: &str,
    total: &str,
    id_w: usize,
    name_w: usize,
    type_w: usize,
    unread_w: usize,
    total_w: usize,
) -> Result<()> {
    writeln!(
        out,
        "{id:<id_w$}  {name:<name_w$}  {label_type:<type_w$}  {unread:<unread_w$}  {total:<total_w$}"
    )
    .context("Failed to write label row")?;
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

    fn sample_label(id: &str) -> Label {
        Label {
            id: id.to_string(),
            name: id.to_string(),
            label_type: Some("user".to_string()),
            messages_unread: Some(2),
            messages_total: Some(10),
            ..Default::default()
        }
    }

    #[test]
    fn render_table_empty_prints_message() {
        let mut buf = Vec::new();
        render_label_table(&[], &mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "No labels returned.\n");
    }

    #[test]
    fn render_table_writes_header_and_rows() {
        let labels = [sample_label("L1"), sample_label("L2")];
        let mut buf = Vec::new();
        render_label_table(&labels, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("ID"));
        assert!(out.contains("NAME"));
        assert!(out.contains("TYPE"));
        assert!(out.contains("UNREAD"));
        assert!(out.contains("TOTAL"));
        assert!(out.contains("L1"));
        assert_eq!(out.lines().count(), 4);
    }

    #[test]
    fn render_table_strips_control_bytes_and_keeps_columns_aligned() {
        let labels = [
            Label {
                id: "evil\x1b[31mid".to_string(),
                name: "na\rme".to_string(),
                label_type: Some("us\x07er".to_string()),
                messages_unread: Some(2),
                messages_total: Some(10),
                ..Default::default()
            },
            sample_label("L2"),
        ];
        let mut buf = Vec::new();
        render_label_table(&labels, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(
            !out.contains(|c: char| c.is_control() && c != '\n'),
            "{out:?}"
        );
        assert!(out.contains("evil[31mid"), "{out:?}");
        let lengths: Vec<usize> = out.lines().map(str::len).collect();
        assert!(lengths.windows(2).all(|w| w[0] == w[1]), "{out:?}");
    }

    #[test]
    fn render_table_uses_dash_for_missing_counts() {
        let label = Label {
            id: "L1".to_string(),
            name: "L1".to_string(),
            ..Default::default()
        };
        let mut buf = Vec::new();
        render_label_table(&[label], &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains('-'));
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
        let labels = [sample_label("L1")];
        let err = render_label_table(
            &labels,
            &mut FailAfter {
                successes_remaining: 0,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    #[test]
    fn render_table_empty_propagates_write_errors() {
        let err = render_label_table(
            &[],
            &mut FailAfter {
                successes_remaining: 0,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("empty-table message"));
    }

    #[test]
    fn render_table_propagates_separator_row_write_errors() {
        let labels = [sample_label("L1")];
        let err = render_label_table(
            &labels,
            &mut FailAfter {
                successes_remaining: 1,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    #[test]
    fn render_table_propagates_data_row_write_errors() {
        let labels = [sample_label("L1")];
        let err = render_label_table(
            &labels,
            &mut FailAfter {
                successes_remaining: 2,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    // ── run_list ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn run_list_table_path_writes_to_stdout() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/labels"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "labels": [{"id": "INBOX", "name": "INBOX", "type": "system"}]
                })),
            )
            .mount(&server)
            .await;

        run_list(&client, &OutputFormat::Table).await.unwrap();
    }

    #[tokio::test]
    async fn run_list_json_path_returns_ok() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/labels"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "labels": []
                })),
            )
            .mount(&server)
            .await;

        run_list(&client, &OutputFormat::Json).await.unwrap();
    }

    #[tokio::test]
    async fn run_list_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/labels"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let err = run_list(&client, &OutputFormat::Table).await.unwrap_err();
        assert!(err.to_string().contains("500"));
    }
}
