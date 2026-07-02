//! CLI command for listing Confluence page version history.
//!
//! Returns metadata only (no body) for each version of a page, matching the
//! `get_page_history` semantics requested in issue #708.

use anyhow::{Context, Result};
use clap::Parser;
use serde::Serialize;

use crate::atlassian::confluence_api::{ConfluenceApi, PageMetadata, PageVersion, SinceFilter};
use crate::cli::atlassian::format::{output_as, JsonlSerialize, OutputFormat};
use crate::cli::atlassian::helpers::create_client;

/// Lists version history (metadata only) for a Confluence page.
///
/// Authors are returned as Atlassian account IDs — resolve them to display
/// names with `omni-dev atlassian confluence user get`.
#[derive(Parser)]
pub struct HistoryCommand {
    /// Confluence page ID.
    pub id: String,

    /// Filter to versions at or after this point. Accepts a numeric version
    /// number (e.g. `5`) or an ISO 8601 date (e.g. `2026-01-01T00:00:00Z`).
    #[arg(long)]
    pub since: Option<String>,

    /// Maximum number of versions to return. `0` means unlimited.
    #[arg(long, default_value_t = 20)]
    pub limit: u32,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

/// Page metadata header included in the structured output.
#[derive(Debug, Clone, Serialize)]
pub struct PageInfo {
    /// Page ID.
    pub id: String,
    /// Page title.
    pub title: String,
    /// Current (latest) version number, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_version: Option<u32>,
}

impl From<PageMetadata> for PageInfo {
    fn from(m: PageMetadata) -> Self {
        Self {
            id: m.id,
            title: m.title,
            current_version: m.current_version,
        }
    }
}

/// YAML/JSON output shape for `confluence history`.
#[derive(Debug, Clone, Serialize)]
pub struct HistoryOutput {
    /// Page metadata (id, title, current version).
    pub page: PageInfo,
    /// Versions matching the request, newest-first.
    pub versions: Vec<PageVersion>,
    /// Whether `limit` cut the listing short. `false` when the listing was
    /// fully exhausted or stopped at the `since` cutoff.
    pub truncated: bool,
}

impl JsonlSerialize for HistoryOutput {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> Result<()> {
        crate::cli::atlassian::format::write_items_jsonl(self.versions.iter(), out)
    }
}

impl HistoryCommand {
    /// Executes the command.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        self.run(&ConfluenceApi::new(client)).await
    }

    /// Runs the command against the given API. Separated from `execute` so it
    /// can be exercised in tests without loading credentials.
    async fn run(self, api: &ConfluenceApi) -> Result<()> {
        run_history(
            api,
            &self.id,
            self.since.as_deref(),
            self.limit,
            &self.output,
        )
        .await
    }
}

/// Fetches and prints the version history for a page.
pub async fn run_history(
    api: &ConfluenceApi,
    page_id: &str,
    since: Option<&str>,
    limit: u32,
    output: &OutputFormat,
) -> Result<()> {
    let history = fetch_history(api, page_id, since, limit).await?;

    if output_as(&history, output)? {
        return Ok(());
    }

    print_table(&history);
    Ok(())
}

/// Builds the [`HistoryOutput`] for the given page, filter, and limit. Used
/// by both the CLI and the MCP tool so the schema stays in lockstep.
pub async fn fetch_history(
    api: &ConfluenceApi,
    page_id: &str,
    since: Option<&str>,
    limit: u32,
) -> Result<HistoryOutput> {
    let parsed_since = since
        .map(SinceFilter::parse)
        .transpose()
        .context("Invalid `since` value")?;

    let metadata = api.get_page_metadata(page_id).await?;
    let (versions, truncated) = api
        .list_page_versions(page_id, parsed_since.as_ref(), limit)
        .await?;

    Ok(HistoryOutput {
        page: PageInfo::from(metadata),
        versions,
        truncated,
    })
}

/// Renders a table view of the version history.
fn print_table(history: &HistoryOutput) {
    println!(
        "PAGE: {} ({}){}",
        history.page.title,
        history.page.id,
        match history.page.current_version {
            Some(v) => format!(" — current version {v}"),
            None => String::new(),
        }
    );

    if history.versions.is_empty() {
        println!("No versions found.");
        return;
    }

    let num_width = history
        .versions
        .iter()
        .map(|v| v.number.to_string().len())
        .max()
        .unwrap_or(1)
        .max(3);
    let date_width = history
        .versions
        .iter()
        .map(|v| v.created_at.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let author_width = history
        .versions
        .iter()
        .map(|v| v.author_id.len())
        .max()
        .unwrap_or(6)
        .max(6);

    println!(
        "{:>num_width$}  {:<date_width$}  {:<author_width$}  MIN  MESSAGE",
        "VER", "DATE", "AUTHOR"
    );
    println!(
        "{:>num_width$}  {:<date_width$}  {:<author_width$}  ---  -------",
        "-".repeat(num_width),
        "-".repeat(date_width),
        "-".repeat(author_width),
    );
    for v in &history.versions {
        let minor = if v.minor_edit { "yes" } else { "no" };
        println!(
            "{:>num_width$}  {:<date_width$}  {:<author_width$}  {minor:<3}  {}",
            v.number, v.created_at, v.author_id, v.message
        );
    }

    if history.truncated {
        println!("(truncated — increase --limit or pass --since to scope further)");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::atlassian::client::AtlassianClient;

    async fn setup_api() -> (wiremock::MockServer, ConfluenceApi) {
        let server = wiremock::MockServer::start().await;
        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        (server, api)
    }

    async fn mount_metadata(server: &wiremock::MockServer, page_id: &str, version: u32) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!(
                "/wiki/api/v2/pages/{page_id}"
            )))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": page_id,
                    "title": format!("Page {page_id}"),
                    "status": "current",
                    "spaceId": "1",
                    "version": {"number": version}
                })),
            )
            .mount(server)
            .await;
    }

    async fn mount_versions(
        server: &wiremock::MockServer,
        page_id: &str,
        results: serde_json::Value,
    ) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!(
                "/wiki/api/v2/pages/{page_id}/versions"
            )))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": results
                })),
            )
            .mount(server)
            .await;
    }

    #[test]
    fn page_info_from_metadata_copies_fields() {
        let info = PageInfo::from(PageMetadata {
            id: "1".to_string(),
            title: "T".to_string(),
            current_version: Some(5),
        });
        assert_eq!(info.id, "1");
        assert_eq!(info.title, "T");
        assert_eq!(info.current_version, Some(5));
    }

    #[test]
    fn page_info_serialize_skips_missing_version() {
        let info = PageInfo {
            id: "1".to_string(),
            title: "T".to_string(),
            current_version: None,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(!json.contains("current_version"));
    }

    #[test]
    fn history_command_defaults() {
        let cmd = HistoryCommand {
            id: "12345".to_string(),
            since: None,
            limit: 20,
            output: OutputFormat::Table,
        };
        assert_eq!(cmd.id, "12345");
        assert_eq!(cmd.limit, 20);
        assert!(cmd.since.is_none());
    }

    #[tokio::test]
    async fn run_history_table() {
        let (server, api) = setup_api().await;
        mount_metadata(&server, "12", 3).await;
        mount_versions(
            &server,
            "12",
            serde_json::json!([
                {"number": 3, "createdAt": "2026-05-08T10:00:00Z", "authorId": "a", "message": "third", "minorEdit": false},
                {"number": 2, "createdAt": "2026-05-07T10:00:00Z", "authorId": "b", "message": "", "minorEdit": true},
                {"number": 1, "createdAt": "2026-05-06T10:00:00Z", "authorId": "", "message": "first", "minorEdit": false},
            ]),
        )
        .await;
        run_history(&api, "12", None, 20, &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_history_json() {
        let (server, api) = setup_api().await;
        mount_metadata(&server, "12", 1).await;
        mount_versions(
            &server,
            "12",
            serde_json::json!([
                {"number": 1, "createdAt": "2026-05-06T10:00:00Z", "authorId": "", "message": "first", "minorEdit": false},
            ]),
        )
        .await;
        run_history(&api, "12", None, 20, &OutputFormat::Json)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_history_yaml() {
        let (server, api) = setup_api().await;
        mount_metadata(&server, "12", 1).await;
        mount_versions(&server, "12", serde_json::json!([])).await;
        run_history(&api, "12", None, 20, &OutputFormat::Yaml)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_history_jsonl() {
        let (server, api) = setup_api().await;
        mount_metadata(&server, "12", 1).await;
        mount_versions(
            &server,
            "12",
            serde_json::json!([
                {"number": 1, "createdAt": "2026-05-06T10:00:00Z", "authorId": "", "message": "first", "minorEdit": false},
            ]),
        )
        .await;
        run_history(&api, "12", None, 20, &OutputFormat::Jsonl)
            .await
            .unwrap();
    }

    #[test]
    fn print_table_with_no_current_version_falls_back_to_empty_suffix() {
        // Exercises the `None` arm of the `match history.page.current_version`
        // (line 146): the table header should still render without the
        // "current version N" suffix when the page metadata lacks one.
        let history = HistoryOutput {
            page: PageInfo {
                id: "1".to_string(),
                title: "T".to_string(),
                current_version: None,
            },
            versions: vec![PageVersion {
                number: 1,
                created_at: "2026-05-06T10:00:00Z".to_string(),
                author_id: "a".to_string(),
                message: "first".to_string(),
                minor_edit: false,
            }],
            truncated: false,
        };
        print_table(&history);
    }

    #[tokio::test]
    async fn run_history_empty_table() {
        let (server, api) = setup_api().await;
        mount_metadata(&server, "12", 1).await;
        mount_versions(&server, "12", serde_json::json!([])).await;
        run_history(&api, "12", None, 20, &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_history_truncates_with_marker() {
        let (server, api) = setup_api().await;
        mount_metadata(&server, "12", 5).await;
        mount_versions(
            &server,
            "12",
            serde_json::json!([
                {"number": 5, "createdAt": "2026-05-08T10:00:00Z", "authorId": "a", "message": "5", "minorEdit": false},
                {"number": 4, "createdAt": "2026-05-07T10:00:00Z", "authorId": "b", "message": "4", "minorEdit": false},
                {"number": 3, "createdAt": "2026-05-06T10:00:00Z", "authorId": "c", "message": "3", "minorEdit": false},
            ]),
        )
        .await;
        let history = fetch_history(&api, "12", None, 2).await.unwrap();
        assert!(history.truncated);
        assert_eq!(history.versions.len(), 2);
        // Render — exercises the table truncation branch.
        run_history(&api, "12", None, 2, &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_history_invalid_since_rejected() {
        let (_server, api) = setup_api().await;
        let err = run_history(&api, "12", Some("nope"), 20, &OutputFormat::Json)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Invalid `since`"));
    }

    #[tokio::test]
    async fn fetch_history_applies_numeric_since() {
        let (server, api) = setup_api().await;
        mount_metadata(&server, "12", 5).await;
        mount_versions(
            &server,
            "12",
            serde_json::json!([
                {"number": 5, "createdAt": "2026-05-09T10:00:00Z", "authorId": "a", "message": "", "minorEdit": false},
                {"number": 4, "createdAt": "2026-05-08T10:00:00Z", "authorId": "b", "message": "", "minorEdit": false},
                {"number": 3, "createdAt": "2026-05-07T10:00:00Z", "authorId": "c", "message": "", "minorEdit": false},
            ]),
        )
        .await;
        let history = fetch_history(&api, "12", Some("4"), 0).await.unwrap();
        assert_eq!(
            history
                .versions
                .iter()
                .map(|v| v.number)
                .collect::<Vec<_>>(),
            vec![5, 4]
        );
        assert!(!history.truncated);
    }

    #[tokio::test]
    async fn fetch_history_applies_iso_since() {
        let (server, api) = setup_api().await;
        mount_metadata(&server, "12", 3).await;
        mount_versions(
            &server,
            "12",
            serde_json::json!([
                {"number": 3, "createdAt": "2026-05-08T10:00:00Z", "authorId": "a", "message": "", "minorEdit": false},
                {"number": 2, "createdAt": "2026-04-01T10:00:00Z", "authorId": "b", "message": "", "minorEdit": false},
            ]),
        )
        .await;
        let history = fetch_history(&api, "12", Some("2026-05-01"), 0)
            .await
            .unwrap();
        assert_eq!(history.versions.len(), 1);
        assert_eq!(history.versions[0].number, 3);
    }

    #[tokio::test]
    async fn fetch_history_metadata_error_propagates() {
        let (server, api) = setup_api().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/99"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;
        let err = fetch_history(&api, "99", None, 20).await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[test]
    fn history_output_jsonl_emits_versions() {
        let history = HistoryOutput {
            page: PageInfo {
                id: "1".to_string(),
                title: "T".to_string(),
                current_version: Some(2),
            },
            versions: vec![
                PageVersion {
                    number: 2,
                    created_at: "2026-05-08T10:00:00Z".to_string(),
                    author_id: "a".to_string(),
                    message: "two".to_string(),
                    minor_edit: false,
                },
                PageVersion {
                    number: 1,
                    created_at: "2026-05-07T10:00:00Z".to_string(),
                    author_id: "b".to_string(),
                    message: String::new(),
                    minor_edit: true,
                },
            ],
            truncated: false,
        };
        let mut buf: Vec<u8> = Vec::new();
        history.write_jsonl(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"number\":2"));
        assert!(lines[1].contains("\"number\":1"));
    }
}
