//! CLI command for `omni-dev drive search`.

use std::io::Write;

use anyhow::{Context, Result};
use clap::Parser;

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::drive::client::DriveClient;
use crate::drive::files_api::{FilesApi, DEFAULT_SEARCH_LIMIT};
use crate::drive::types::DriveFile;

/// Searches Drive files.
///
/// Unlike Gmail's `messages.list`, `files.list` returns full metadata per
/// hit in one call via the `fields` parameter — there is no ids-then-enrich
/// split and no `--enrich` flag.
#[derive(Parser)]
pub struct SearchCommand {
    /// Drive search query, passed verbatim to `files.list`'s `q` parameter
    /// (e.g. `name contains 'report' and mimeType = 'application/pdf'`).
    pub query: String,

    /// Maximum results to return. `0` means "fetch every match" (capped at
    /// a hard ceiling to bound run time).
    #[arg(long, default_value_t = DEFAULT_SEARCH_LIMIT)]
    pub limit: usize,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl SearchCommand {
    /// Runs the command against the shared client resolved by the parent
    /// `DriveCommand::execute`.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        run_search(client, &self.query, self.limit, &self.output).await
    }
}

/// Fetches search results and emits them in the requested format.
///
/// Split from [`SearchCommand::execute`] so tests can inject a wiremock
/// client without going through the credential-loading path.
async fn run_search(
    client: &DriveClient,
    query: &str,
    limit: usize,
    output: &OutputFormat,
) -> Result<()> {
    let list = FilesApi::new(client).search_all(Some(query), limit).await?;
    if output_as(&list.files, output)? {
        return Ok(());
    }
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    render_search_table(&list.files, &mut handle)
}

/// Renders search results as an aligned text table.
///
/// Column layout: `ID | NAME | MIMETYPE | MODIFIED | SIZE`. An empty input
/// prints `No files returned.`.
fn render_search_table(files: &[DriveFile], out: &mut dyn Write) -> Result<()> {
    if files.is_empty() {
        writeln!(out, "No files returned.").context("Failed to write empty-table message")?;
        return Ok(());
    }

    // Sanitize server-supplied strings *before* computing column widths, so
    // a stripped control byte can't leave the width one character too wide
    // for what's actually written (#1537).
    let rows: Vec<[String; 5]> = files
        .iter()
        .map(|f| {
            [
                sanitize_for_terminal(&f.id),
                sanitize_for_terminal(&f.name),
                sanitize_for_terminal(&f.mime_type),
                sanitize_for_terminal(f.modified_time.as_deref().unwrap_or("-")),
                sanitize_for_terminal(f.size.as_deref().unwrap_or("-")),
            ]
        })
        .collect();

    let id_width = "ID"
        .len()
        .max(rows.iter().map(|r| r[0].len()).max().unwrap_or(0));
    let name_width = "NAME"
        .len()
        .max(rows.iter().map(|r| r[1].len()).max().unwrap_or(0));
    let mime_width = "MIMETYPE"
        .len()
        .max(rows.iter().map(|r| r[2].len()).max().unwrap_or(0));
    let modified_width = "MODIFIED"
        .len()
        .max(rows.iter().map(|r| r[3].len()).max().unwrap_or(0));
    let size_width = "SIZE"
        .len()
        .max(rows.iter().map(|r| r[4].len()).max().unwrap_or(0));

    write_row(
        out,
        "ID",
        "NAME",
        "MIMETYPE",
        "MODIFIED",
        "SIZE",
        id_width,
        name_width,
        mime_width,
        modified_width,
        size_width,
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
            mime_width,
            modified_width,
            size_width,
        )?;
    }
    Ok(())
}

/// Writes a single row of the search table with consistent 2-space gutters
/// between cells.
#[allow(clippy::too_many_arguments)]
fn write_row(
    out: &mut dyn Write,
    id: &str,
    name: &str,
    mime_type: &str,
    modified: &str,
    size: &str,
    id_w: usize,
    name_w: usize,
    mime_w: usize,
    modified_w: usize,
    size_w: usize,
) -> Result<()> {
    writeln!(
        out,
        "{id:<id_w$}  {name:<name_w$}  {mime_type:<mime_w$}  {modified:<modified_w$}  {size:<size_w$}"
    )
    .context("Failed to write search row")?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::utils::secret::Secret;

    fn test_credentials() -> DriveCredentials {
        DriveCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: DriveGrantedScopes::READONLY,
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

    fn sample_file(id: &str) -> DriveFile {
        DriveFile {
            id: id.to_string(),
            name: "report.pdf".to_string(),
            mime_type: "application/pdf".to_string(),
            size: Some("1024".to_string()),
            modified_time: Some("2026-01-01T00:00:00.000Z".to_string()),
            ..Default::default()
        }
    }

    // ── render_search_table ──────────────────────────────────────────

    #[test]
    fn render_table_empty_prints_message() {
        let mut buf = Vec::new();
        render_search_table(&[], &mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "No files returned.\n");
    }

    #[test]
    fn render_table_writes_header_and_rows() {
        let files = [sample_file("f1"), sample_file("f2")];
        let mut buf = Vec::new();
        render_search_table(&files, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("ID"));
        assert!(out.contains("NAME"));
        assert!(out.contains("MIMETYPE"));
        assert!(out.contains("MODIFIED"));
        assert!(out.contains("SIZE"));
        assert!(out.contains("f1"));
        assert!(out.contains("f2"));
        assert_eq!(out.lines().count(), 3);
    }

    #[test]
    fn render_table_strips_control_bytes_and_keeps_columns_aligned() {
        let files = [
            DriveFile {
                id: "f1".to_string(),
                name: "evil\x1b[31mname".to_string(),
                mime_type: "text/plain\r\x07".to_string(),
                ..Default::default()
            },
            sample_file("f2"),
        ];
        let mut buf = Vec::new();
        render_search_table(&files, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(
            !out.contains(|c: char| c.is_control() && c != '\n'),
            "{out:?}"
        );
        assert!(out.contains("evil[31mname"), "{out:?}");
        // Header + 2 data rows, every line the same length (alignment
        // survived sanitizing before width calculation).
        let lengths: Vec<usize> = out.lines().map(str::len).collect();
        assert_eq!(lengths.len(), 3);
        assert!(lengths.windows(2).all(|w| w[0] == w[1]), "{out:?}");
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

    #[test]
    fn render_table_propagates_header_write_errors() {
        let files = [sample_file("f1")];
        let err = render_search_table(
            &files,
            &mut FailAfter {
                successes_remaining: 0,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    #[test]
    fn render_table_propagates_data_row_write_errors() {
        let files = [sample_file("f1")];
        let err = render_search_table(
            &files,
            &mut FailAfter {
                successes_remaining: 1,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    // ── run_search ───────────────────────────────────────────────────

    #[tokio::test]
    async fn run_search_table_path_writes_to_stdout() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "files": [{"id": "f1", "name": "a"}],
                })),
            )
            .mount(&server)
            .await;

        run_search(&client, "name contains 'a'", 10, &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_search_json_path_returns_ok() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "files": [],
                })),
            )
            .mount(&server)
            .await;

        run_search(&client, "*", 10, &OutputFormat::Json)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_search_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let err = run_search(&client, "*", 10, &OutputFormat::Table)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    #[tokio::test]
    async fn run_search_passes_query_through_as_q_param() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files"))
            .and(wiremock::matchers::query_param(
                "q",
                "name contains 'report'",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "files": [],
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        run_search(&client, "name contains 'report'", 10, &OutputFormat::Json)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_search_sends_supports_all_drives_and_include_items_from_all_drives() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files"))
            .and(wiremock::matchers::query_param("supportsAllDrives", "true"))
            .and(wiremock::matchers::query_param(
                "includeItemsFromAllDrives",
                "true",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "files": [],
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        run_search(&client, "*", 10, &OutputFormat::Json)
            .await
            .unwrap();
    }

    // ── SearchCommand::execute glue ────────────────────────────────

    #[tokio::test]
    async fn execute_passes_query_through() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files"))
            .and(wiremock::matchers::query_param("q", "name contains 'x'"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "files": [],
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let cmd = SearchCommand {
            query: "name contains 'x'".to_string(),
            limit: 10,
            output: OutputFormat::Json,
        };
        cmd.execute(&client).await.unwrap();
    }
}
