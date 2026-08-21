//! CLI command for `omni-dev drive dedupe`.

use std::collections::BTreeMap;
use std::io::Write;

use anyhow::{Context, Result};
use clap::Parser;
use serde::Serialize;

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::drive::client::DriveClient;
use crate::drive::files_api::{FilesApi, DEFAULT_SEARCH_LIMIT};
use crate::drive::types::DriveFile;

/// Finds Drive files sharing the same content hash.
///
/// Reuses the same bulk search path as `drive search` — `files.list`
/// already returns full metadata (including `md5Checksum`) per hit, so
/// duplicate detection needs no per-file follow-up call.
#[derive(Parser)]
pub struct DedupeCommand {
    /// Drive search query, passed verbatim to `files.list`'s `q` parameter
    /// (e.g. `'<folder-id>' in parents` to dedupe within one folder).
    pub query: String,

    /// Maximum results to scan. `0` means "scan every match" (capped at a
    /// hard ceiling to bound run time).
    #[arg(long, default_value_t = DEFAULT_SEARCH_LIMIT)]
    pub limit: usize,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl DedupeCommand {
    /// Runs the command against the shared client resolved by the parent
    /// `DriveCommand::execute`.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        run_dedupe(client, &self.query, self.limit, &self.output).await
    }
}

/// A group of files sharing the same `md5Checksum`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct DuplicateGroup {
    /// The shared MD5 checksum.
    pub(crate) checksum: String,
    /// The files sharing it (always 2 or more).
    pub(crate) files: Vec<DriveFile>,
}

/// Groups `files` by `md5_checksum`, keeping only groups with 2 or more
/// members. Files with no checksum (folders, Google-native documents) are
/// skipped — md5 is used rather than sha1/sha256 since it has the broadest
/// coverage across Drive files.
///
/// Grouped via a `BTreeMap` rather than a `HashMap` so both table output
/// and callers get deterministic, checksum-sorted ordering.
pub(crate) fn group_duplicates(files: &[DriveFile]) -> Vec<DuplicateGroup> {
    let mut groups: BTreeMap<String, Vec<DriveFile>> = BTreeMap::new();
    for file in files {
        if let Some(checksum) = &file.md5_checksum {
            groups
                .entry(checksum.clone())
                .or_default()
                .push(file.clone());
        }
    }
    groups
        .into_iter()
        .filter(|(_, files)| files.len() >= 2)
        .map(|(checksum, files)| DuplicateGroup { checksum, files })
        .collect()
}

/// Fetches search results, groups them by content hash, and emits
/// duplicates in the requested format.
///
/// Split from [`DedupeCommand::execute`] so tests can inject a wiremock
/// client without going through the credential-loading path.
async fn run_dedupe(
    client: &DriveClient,
    query: &str,
    limit: usize,
    output: &OutputFormat,
) -> Result<()> {
    let list = FilesApi::new(client).search_all(Some(query), limit).await?;
    let groups = group_duplicates(&list.files);
    if output_as(&groups, output)? {
        return Ok(());
    }
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    render_dedupe_table(&groups, &mut handle)
}

/// Renders duplicate groups as an aligned text table.
///
/// Column layout: `HASH | COUNT | FILES`, with `FILES` a comma-joined list
/// of `name (id)` (left unpadded — its length is unbounded, unlike the
/// other two columns). An empty input prints `No duplicate files found.`.
fn render_dedupe_table(groups: &[DuplicateGroup], out: &mut dyn Write) -> Result<()> {
    if groups.is_empty() {
        writeln!(out, "No duplicate files found.")
            .context("Failed to write empty-table message")?;
        return Ok(());
    }

    // Sanitize server-supplied strings *before* computing column widths,
    // matching `render_search_table`'s precedent (#1537).
    let rows: Vec<(String, String, String)> = groups
        .iter()
        .map(|g| {
            let files = g
                .files
                .iter()
                .map(|f| {
                    format!(
                        "{} ({})",
                        sanitize_for_terminal(&f.name),
                        sanitize_for_terminal(&f.id)
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            (
                sanitize_for_terminal(&g.checksum),
                g.files.len().to_string(),
                files,
            )
        })
        .collect();

    let hash_width = "HASH"
        .len()
        .max(rows.iter().map(|r| r.0.len()).max().unwrap_or(0));
    let count_width = "COUNT"
        .len()
        .max(rows.iter().map(|r| r.1.len()).max().unwrap_or(0));

    writeln!(
        out,
        "{:<hash_width$}  {:<count_width$}  FILES",
        "HASH", "COUNT"
    )
    .context("Failed to write dedupe row")?;
    for (hash, count, files) in &rows {
        writeln!(out, "{hash:<hash_width$}  {count:<count_width$}  {files}")
            .context("Failed to write dedupe row")?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveScope};
    use crate::utils::secret::Secret;

    fn test_credentials() -> DriveCredentials {
        DriveCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: DriveScope::ReadOnly,
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

    fn file_with_checksum(id: &str, checksum: Option<&str>) -> DriveFile {
        DriveFile {
            id: id.to_string(),
            name: format!("{id}.pdf"),
            mime_type: "application/pdf".to_string(),
            md5_checksum: checksum.map(str::to_string),
            ..Default::default()
        }
    }

    // ── group_duplicates ────────────────────────────────────────────

    #[test]
    fn group_duplicates_returns_empty_for_no_duplicates() {
        let files = [
            file_with_checksum("f1", Some("hash-a")),
            file_with_checksum("f2", Some("hash-b")),
        ];
        assert!(group_duplicates(&files).is_empty());
    }

    #[test]
    fn group_duplicates_groups_files_sharing_checksum() {
        let files = [
            file_with_checksum("f1", Some("hash-a")),
            file_with_checksum("f2", Some("hash-b")),
            file_with_checksum("f3", Some("hash-a")),
        ];
        let groups = group_duplicates(&files);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].checksum, "hash-a");
        assert_eq!(groups[0].files.len(), 2);
        assert_eq!(groups[0].files[0].id, "f1");
        assert_eq!(groups[0].files[1].id, "f3");
    }

    #[test]
    fn group_duplicates_skips_files_with_no_checksum() {
        let files = [
            file_with_checksum("f1", None),
            file_with_checksum("f2", None),
        ];
        assert!(group_duplicates(&files).is_empty());
    }

    #[test]
    fn group_duplicates_orders_groups_by_checksum() {
        let files = [
            file_with_checksum("f1", Some("hash-z")),
            file_with_checksum("f2", Some("hash-z")),
            file_with_checksum("f3", Some("hash-a")),
            file_with_checksum("f4", Some("hash-a")),
        ];
        let groups = group_duplicates(&files);
        let checksums: Vec<&str> = groups.iter().map(|g| g.checksum.as_str()).collect();
        assert_eq!(checksums, vec!["hash-a", "hash-z"]);
    }

    // ── render_dedupe_table ─────────────────────────────────────────

    #[test]
    fn render_table_empty_prints_message() {
        let mut buf = Vec::new();
        render_dedupe_table(&[], &mut buf).unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "No duplicate files found.\n"
        );
    }

    #[test]
    fn render_table_writes_header_and_grouped_files() {
        let groups = group_duplicates(&[
            file_with_checksum("f1", Some("hash-a")),
            file_with_checksum("f2", Some("hash-a")),
        ]);
        let mut buf = Vec::new();
        render_dedupe_table(&groups, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("HASH"));
        assert!(out.contains("COUNT"));
        assert!(out.contains("FILES"));
        assert!(out.contains("hash-a"));
        assert!(out.contains("f1.pdf (f1)"));
        assert!(out.contains("f2.pdf (f2)"));
        assert_eq!(out.lines().count(), 2);
    }

    #[test]
    fn render_table_strips_control_bytes_from_server_strings() {
        let groups = vec![DuplicateGroup {
            checksum: "hash\x1b[31ma".to_string(),
            files: vec![
                DriveFile {
                    id: "f1".to_string(),
                    name: "evil\x1b[31mname".to_string(),
                    ..Default::default()
                },
                file_with_checksum("f2", Some("hash-a")),
            ],
        }];
        let mut buf = Vec::new();
        render_dedupe_table(&groups, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(
            !out.contains(|c: char| c.is_control() && c != '\n'),
            "{out:?}"
        );
        assert!(out.contains("evil[31mname"), "{out:?}");
    }

    // ── run_dedupe ──────────────────────────────────────────────────

    #[tokio::test]
    async fn run_dedupe_table_path_writes_to_stdout() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "files": [
                        {"id": "f1", "name": "a", "md5Checksum": "hash-a"},
                        {"id": "f2", "name": "b", "md5Checksum": "hash-a"},
                    ],
                })),
            )
            .mount(&server)
            .await;

        run_dedupe(&client, "name contains 'a'", 10, &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_dedupe_json_path_returns_ok() {
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

        run_dedupe(&client, "*", 10, &OutputFormat::Json)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_dedupe_excludes_groups_of_one() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "files": [{"id": "f1", "name": "a", "md5Checksum": "hash-a"}],
                })),
            )
            .mount(&server)
            .await;

        run_dedupe(&client, "*", 10, &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_dedupe_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let err = run_dedupe(&client, "*", 10, &OutputFormat::Table)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    // ── DedupeCommand::execute glue ──────────────────────────────────

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

        let cmd = DedupeCommand {
            query: "name contains 'x'".to_string(),
            limit: 10,
            output: OutputFormat::Json,
        };
        cmd.execute(&client).await.unwrap();
    }
}
