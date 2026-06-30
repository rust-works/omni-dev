//! CLI commands for Confluence space operations.

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::atlassian::confluence_api::{ConfluenceApi, ConfluenceSpacePage, PageSummaryPage};
use crate::cli::atlassian::format::{output_as, OutputFormat};
use crate::cli::atlassian::helpers::create_client;

/// Confluence space operations.
#[derive(Parser)]
pub struct SpaceCommand {
    /// The space subcommand to execute.
    #[command(subcommand)]
    pub command: SpaceSubcommands,
}

/// Space subcommands.
#[derive(Subcommand)]
pub enum SpaceSubcommands {
    /// Lists Confluence spaces (mirrors the `confluence_space_list` MCP tool).
    List(ListCommand),
    /// Enumerates pages within a Confluence space (mirrors the `confluence_space_pages` MCP tool).
    Pages(PagesCommand),
}

impl SpaceCommand {
    /// Executes the space command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            SpaceSubcommands::List(cmd) => cmd.execute().await,
            SpaceSubcommands::Pages(cmd) => cmd.execute().await,
        }
    }
}

/// Lists Confluence spaces.
#[derive(Parser)]
pub struct ListCommand {
    /// Filter to specific space keys. Combined with `--type`/`--status` as AND.
    #[arg(long, value_delimiter = ',')]
    pub keys: Vec<String>,

    /// Filter by space type. Common values: `global`, `personal`,
    /// `collaboration`, `knowledge_base`, `onboarding`. Passed through to the
    /// Confluence v2 API verbatim — Atlassian adds template-derived types
    /// over time, so any string the server accepts is accepted here.
    #[arg(long)]
    pub r#type: Option<String>,

    /// Filter by space status. Common values: `current`, `archived`. Passed
    /// through to the Confluence v2 API verbatim.
    #[arg(long)]
    pub status: Option<String>,

    /// Pagination cursor (returned as `next_cursor` from a previous call).
    #[arg(long)]
    pub cursor: Option<String>,

    /// Maximum number of spaces to return per page.
    #[arg(long, default_value_t = 25)]
    pub limit: u32,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ListCommand {
    /// Executes the list command.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);
        let keys_refs: Vec<&str> = self.keys.iter().map(String::as_str).collect();
        run_list(
            &api,
            &keys_refs,
            self.r#type.as_deref(),
            self.status.as_deref(),
            self.cursor.as_deref(),
            self.limit,
            &self.output,
        )
        .await
    }
}

/// Fetches and displays a page of spaces.
async fn run_list(
    api: &ConfluenceApi,
    keys: &[&str],
    type_: Option<&str>,
    status: Option<&str>,
    cursor: Option<&str>,
    limit: u32,
    output: &OutputFormat,
) -> Result<()> {
    let page = api.list_spaces(keys, type_, status, cursor, limit).await?;
    display_spaces(&page, output)
}

/// Formats and displays spaces in the requested output format.
fn display_spaces(page: &ConfluenceSpacePage, output: &OutputFormat) -> Result<()> {
    if output_as(page, output)? {
        return Ok(());
    }
    print_spaces(page);
    Ok(())
}

/// Prints spaces as a formatted table.
fn print_spaces(page: &ConfluenceSpacePage) {
    if page.results.is_empty() {
        println!("No spaces found.");
        return;
    }

    let id_width = page
        .results
        .iter()
        .map(|s| s.id.len())
        .max()
        .unwrap_or(2)
        .max(2);
    let key_width = page
        .results
        .iter()
        .map(|s| s.key.len())
        .max()
        .unwrap_or(3)
        .max(3);
    let name_width = page
        .results
        .iter()
        .map(|s| s.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let type_width = page
        .results
        .iter()
        .map(|s| s.type_.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let status_width = page
        .results
        .iter()
        .map(|s| s.status.len())
        .max()
        .unwrap_or(6)
        .max(6);

    println!(
        "{:<id_width$}  {:<key_width$}  {:<name_width$}  {:<type_width$}  {:<status_width$}  HOMEPAGE",
        "ID", "KEY", "NAME", "TYPE", "STATUS"
    );
    println!(
        "{:<id_width$}  {:<key_width$}  {:<name_width$}  {:<type_width$}  {:<status_width$}  {:<8}",
        "-".repeat(id_width),
        "-".repeat(key_width),
        "-".repeat(name_width),
        "-".repeat(type_width),
        "-".repeat(status_width),
        "-".repeat(8),
    );

    for s in &page.results {
        let homepage = s.homepage_id.as_deref().unwrap_or("-");
        println!(
            "{:<id_width$}  {:<key_width$}  {:<name_width$}  {:<type_width$}  {:<status_width$}  {homepage}",
            s.id, s.key, s.name, s.type_, s.status,
        );
    }

    if let Some(cursor) = &page.next_cursor {
        println!();
        println!("Next page: --cursor {cursor}");
    }
}

/// Enumerates pages within a Confluence space.
#[derive(Parser)]
pub struct PagesCommand {
    /// Space key whose pages to enumerate (e.g. `ENG`).
    pub key: String,

    /// Filter by page status. Common values: `current`, `archived`, `draft`,
    /// `trashed`. Passed through to the Confluence v2 API verbatim.
    #[arg(long)]
    pub status: Option<String>,

    /// Sort order. Common values: `id`, `-id`, `title`, `-title`,
    /// `created-date`, `-created-date`, `modified-date`, `-modified-date`.
    /// Passed through to the Confluence v2 API verbatim.
    // `allow_hyphen_values` keeps clap from misreading descending values like
    // `-created-date` as a short flag.
    #[arg(long, allow_hyphen_values = true)]
    pub sort: Option<String>,

    /// Pagination cursor (returned as `next_cursor` from a previous call).
    #[arg(long)]
    pub cursor: Option<String>,

    /// Maximum number of pages to return per response.
    #[arg(long, default_value_t = 25)]
    pub limit: u32,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl PagesCommand {
    /// Executes the pages command.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);
        run_pages(
            &api,
            &self.key,
            self.status.as_deref(),
            self.sort.as_deref(),
            self.cursor.as_deref(),
            self.limit,
            &self.output,
        )
        .await
    }
}

/// Resolves the space key and displays one page of summary records.
async fn run_pages(
    api: &ConfluenceApi,
    key: &str,
    status: Option<&str>,
    sort: Option<&str>,
    cursor: Option<&str>,
    limit: u32,
    output: &OutputFormat,
) -> Result<()> {
    let space_id = api.resolve_space_id(key).await?;
    let page = api
        .list_space_pages(&space_id, status, sort, cursor, limit)
        .await?;
    display_pages(&page, output)
}

/// Formats and displays page summaries in the requested output format.
fn display_pages(page: &PageSummaryPage, output: &OutputFormat) -> Result<()> {
    if output_as(page, output)? {
        return Ok(());
    }
    print_pages(page);
    Ok(())
}

/// Prints page summaries as a formatted table.
fn print_pages(page: &PageSummaryPage) {
    if page.results.is_empty() {
        println!("No pages found.");
        return;
    }

    let id_width = page
        .results
        .iter()
        .map(|p| p.id.len())
        .max()
        .unwrap_or(2)
        .max(2);
    let status_width = page
        .results
        .iter()
        .map(|p| p.status.len())
        .max()
        .unwrap_or(6)
        .max(6);
    let parent_width = page
        .results
        .iter()
        .map(|p| p.parent_id.as_deref().unwrap_or("-").len())
        .max()
        .unwrap_or(6)
        .max(6);
    let author_width = page
        .results
        .iter()
        .map(|p| p.author_id.as_deref().unwrap_or("-").len())
        .max()
        .unwrap_or(6)
        .max(6);
    let created_width = page
        .results
        .iter()
        .map(|p| p.created_at.as_deref().unwrap_or("-").len())
        .max()
        .unwrap_or(7)
        .max(7);

    println!(
        "{:<id_width$}  {:<status_width$}  {:<parent_width$}  {:<author_width$}  {:<created_width$}  TITLE",
        "ID", "STATUS", "PARENT", "AUTHOR", "CREATED",
    );
    println!(
        "{:<id_width$}  {:<status_width$}  {:<parent_width$}  {:<author_width$}  {:<created_width$}  {:<5}",
        "-".repeat(id_width),
        "-".repeat(status_width),
        "-".repeat(parent_width),
        "-".repeat(author_width),
        "-".repeat(created_width),
        "-".repeat(5),
    );

    for p in &page.results {
        let parent = p.parent_id.as_deref().unwrap_or("-");
        let author = p.author_id.as_deref().unwrap_or("-");
        let created = p.created_at.as_deref().unwrap_or("-");
        println!(
            "{:<id_width$}  {:<status_width$}  {parent:<parent_width$}  {author:<author_width$}  {created:<created_width$}  {}",
            p.id, p.status, p.title,
        );
    }

    if let Some(cursor) = &page.next_cursor {
        println!();
        println!("Next page: --cursor {cursor}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::await_holding_lock)]
mod tests {
    use super::*;
    use crate::atlassian::confluence_api::ConfluenceSpace;

    fn sample_space(id: &str, key: &str, archived: bool) -> ConfluenceSpace {
        ConfluenceSpace {
            id: id.to_string(),
            key: key.to_string(),
            name: format!("{key} space"),
            type_: "global".to_string(),
            status: if archived { "archived" } else { "current" }.to_string(),
            homepage_id: Some(format!("{id}-home")),
        }
    }

    fn sample_page(items: Vec<ConfluenceSpace>, cursor: Option<&str>) -> ConfluenceSpacePage {
        ConfluenceSpacePage {
            results: items,
            next_cursor: cursor.map(str::to_string),
        }
    }

    // ── SpaceCommand variants ────────────────────────────────────────

    #[test]
    fn space_subcommands_list_variant() {
        let cmd = SpaceCommand {
            command: SpaceSubcommands::List(ListCommand {
                keys: vec![],
                r#type: None,
                status: None,
                cursor: None,
                limit: 25,
                output: OutputFormat::Table,
            }),
        };
        assert!(matches!(cmd.command, SpaceSubcommands::List(_)));
    }

    // ── display_spaces ──────────────────────────────────────────────

    #[test]
    fn display_spaces_table() {
        let page = sample_page(vec![sample_space("1", "ENG", false)], None);
        assert!(display_spaces(&page, &OutputFormat::Table).is_ok());
    }

    #[test]
    fn display_spaces_json() {
        let page = sample_page(vec![sample_space("1", "ENG", false)], None);
        assert!(display_spaces(&page, &OutputFormat::Json).is_ok());
    }

    #[test]
    fn display_spaces_yaml() {
        let page = sample_page(vec![sample_space("1", "ENG", false)], None);
        assert!(display_spaces(&page, &OutputFormat::Yaml).is_ok());
    }

    #[test]
    fn display_spaces_yamls() {
        let page = sample_page(vec![sample_space("1", "ENG", false)], None);
        assert!(display_spaces(&page, &OutputFormat::Yamls).is_ok());
    }

    #[test]
    fn display_spaces_jsonl() {
        let page = sample_page(vec![sample_space("1", "ENG", false)], None);
        assert!(display_spaces(&page, &OutputFormat::Jsonl).is_ok());
    }

    #[test]
    fn display_spaces_empty_table() {
        let page = sample_page(vec![], None);
        assert!(display_spaces(&page, &OutputFormat::Table).is_ok());
    }

    #[test]
    fn print_spaces_with_cursor() {
        let page = sample_page(vec![sample_space("1", "ENG", false)], Some("NEXT"));
        print_spaces(&page);
    }

    #[test]
    fn print_spaces_archived_and_missing_homepage() {
        let mut space = sample_space("2", "OPS", true);
        space.homepage_id = None;
        let page = sample_page(vec![space], None);
        print_spaces(&page);
    }

    // ── run_list (wiremock) ─────────────────────────────────────────

    #[tokio::test]
    async fn run_list_table_output() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {
                            "id": "1", "key": "ENG", "name": "Engineering",
                            "type": "global", "status": "current", "homepageId": "h-1"
                        }
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client =
            crate::atlassian::client::AtlassianClient::new(&server.uri(), "u@t.com", "tok")
                .unwrap();
        let api = ConfluenceApi::new(client);
        assert!(
            run_list(&api, &[], None, None, None, 25, &OutputFormat::Table)
                .await
                .is_ok()
        );
    }

    /// Exercises the `?` Err path on `create_client()` in
    /// `ListCommand::execute` by clearing all credential env vars before
    /// calling.
    #[tokio::test]
    async fn list_command_execute_propagates_create_client_error() {
        let guard = crate::atlassian::auth::test_util::EnvGuard::take();
        let _home = guard.clear_credentials();

        let cmd = ListCommand {
            keys: vec![],
            r#type: None,
            status: None,
            cursor: None,
            limit: 25,
            output: OutputFormat::Yaml,
        };
        assert!(cmd.execute().await.is_err());
    }

    #[tokio::test]
    async fn run_list_propagates_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .expect(1)
            .mount(&server)
            .await;

        let client =
            crate::atlassian::client::AtlassianClient::new(&server.uri(), "u@t.com", "tok")
                .unwrap();
        let api = ConfluenceApi::new(client);
        let err = run_list(&api, &[], None, None, None, 25, &OutputFormat::Table)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    // ── *Command::execute (env-mutex serialised) ─────────────────────

    fn set_atlassian_env(uri: &str) {
        std::env::set_var(crate::atlassian::auth::ATLASSIAN_INSTANCE_URL, uri);
        std::env::set_var(crate::atlassian::auth::ATLASSIAN_EMAIL, "user@test.com");
        std::env::set_var(crate::atlassian::auth::ATLASSIAN_API_TOKEN, "t");
    }

    fn clear_atlassian_env() {
        std::env::remove_var(crate::atlassian::auth::ATLASSIAN_INSTANCE_URL);
        std::env::remove_var(crate::atlassian::auth::ATLASSIAN_EMAIL);
        std::env::remove_var(crate::atlassian::auth::ATLASSIAN_API_TOKEN);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn list_command_execute_runs_through_dispatch() {
        let _lock = crate::atlassian::auth::test_util::AUTH_ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .and(wiremock::matchers::query_param("keys", "ENG,DEV"))
            .and(wiremock::matchers::query_param("type", "knowledge_base"))
            .and(wiremock::matchers::query_param("status", "archived"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .mount(&server)
            .await;

        set_atlassian_env(&server.uri());
        let cmd = SpaceCommand {
            command: SpaceSubcommands::List(ListCommand {
                keys: vec!["ENG".to_string(), "DEV".to_string()],
                r#type: Some("knowledge_base".to_string()),
                status: Some("archived".to_string()),
                cursor: Some("opaque".to_string()),
                limit: 5,
                output: OutputFormat::Json,
            }),
        };
        let result = cmd.execute().await;
        clear_atlassian_env();
        assert!(result.is_ok(), "{result:?}");
    }

    // ── PagesCommand: variants, display, run, execute ────────────────

    use crate::atlassian::confluence_api::{PageSummary, PageSummaryPage};

    fn sample_page_summary(id: &str, title: &str) -> PageSummary {
        PageSummary {
            id: id.to_string(),
            title: title.to_string(),
            status: "current".to_string(),
            parent_id: Some("100".to_string()),
            author_id: Some(format!("u-{id}")),
            created_at: Some("2024-05-06T07:08:09Z".to_string()),
        }
    }

    fn sample_pages(items: Vec<PageSummary>, cursor: Option<&str>) -> PageSummaryPage {
        PageSummaryPage {
            results: items,
            next_cursor: cursor.map(str::to_string),
        }
    }

    /// Regression: `--sort -created-date` and the other descending sort
    /// values start with `-`, which clap will otherwise mistake for short
    /// flags. The `allow_hyphen_values = true` attribute on `PagesCommand::sort`
    /// keeps the parse working.
    #[test]
    fn pages_command_accepts_hyphenated_sort_values() {
        use clap::Parser;
        let cmd = PagesCommand::try_parse_from(["pages", "ENG", "--sort", "-created-date"])
            .expect("clap should accept hyphenated sort values");
        assert_eq!(cmd.sort.as_deref(), Some("-created-date"));

        let cmd = PagesCommand::try_parse_from(["pages", "ENG", "--sort", "-title"])
            .expect("clap should accept hyphenated sort values");
        assert_eq!(cmd.sort.as_deref(), Some("-title"));
    }

    #[test]
    fn space_subcommands_pages_variant() {
        let pages = SpaceCommand {
            command: SpaceSubcommands::Pages(PagesCommand {
                key: "ENG".to_string(),
                status: None,
                sort: None,
                cursor: None,
                limit: 25,
                output: OutputFormat::Table,
            }),
        };
        let list = SpaceCommand {
            command: SpaceSubcommands::List(ListCommand {
                keys: vec![],
                r#type: None,
                status: None,
                cursor: None,
                limit: 25,
                output: OutputFormat::Table,
            }),
        };
        // Single `matches!` site exercised against both a matching and
        // non-matching variant so both arms are covered at the same source
        // line (avoids the partial-branch noise of two separate sites).
        for (expected, cmd) in [(true, pages), (false, list)] {
            assert_eq!(matches!(cmd.command, SpaceSubcommands::Pages(_)), expected);
        }
    }

    #[test]
    fn display_pages_table() {
        let page = sample_pages(vec![sample_page_summary("1", "Home")], None);
        assert!(display_pages(&page, &OutputFormat::Table).is_ok());
    }

    #[test]
    fn display_pages_json() {
        let page = sample_pages(vec![sample_page_summary("1", "Home")], None);
        assert!(display_pages(&page, &OutputFormat::Json).is_ok());
    }

    #[test]
    fn display_pages_yaml() {
        let page = sample_pages(vec![sample_page_summary("1", "Home")], None);
        assert!(display_pages(&page, &OutputFormat::Yaml).is_ok());
    }

    #[test]
    fn display_pages_yamls() {
        let page = sample_pages(vec![sample_page_summary("1", "Home")], None);
        assert!(display_pages(&page, &OutputFormat::Yamls).is_ok());
    }

    #[test]
    fn display_pages_jsonl() {
        let page = sample_pages(vec![sample_page_summary("1", "Home")], None);
        assert!(display_pages(&page, &OutputFormat::Jsonl).is_ok());
    }

    #[test]
    fn display_pages_empty_table() {
        let page = sample_pages(vec![], None);
        assert!(display_pages(&page, &OutputFormat::Table).is_ok());
    }

    #[test]
    fn print_pages_with_cursor_and_missing_fields() {
        let bare = PageSummary {
            id: "9".to_string(),
            title: "Bare".to_string(),
            status: String::new(),
            parent_id: None,
            author_id: None,
            created_at: None,
        };
        let page = sample_pages(vec![sample_page_summary("1", "Home"), bare], Some("NEXT"));
        print_pages(&page);
    }

    #[tokio::test]
    async fn run_pages_table_output() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .and(wiremock::matchers::query_param("keys", "ENG"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": [{"id": "98765"}]})),
            )
            .expect(1)
            .mount(&server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98765/pages"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"id": "1", "title": "Home", "status": "current",
                         "authorId": "u1", "createdAt": "2024-01-02T03:04:05Z"}
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client =
            crate::atlassian::client::AtlassianClient::new(&server.uri(), "u@t.com", "tok")
                .unwrap();
        let api = ConfluenceApi::new(client);
        assert!(
            run_pages(&api, "ENG", None, None, None, 25, &OutputFormat::Table)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn run_pages_propagates_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": [{"id": "98765"}]})),
            )
            .mount(&server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98765/pages"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .expect(1)
            .mount(&server)
            .await;

        let client =
            crate::atlassian::client::AtlassianClient::new(&server.uri(), "u@t.com", "tok")
                .unwrap();
        let api = ConfluenceApi::new(client);
        let err = run_pages(&api, "ENG", None, None, None, 25, &OutputFormat::Json)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    /// Exercises the Err branch of `resolve_space_id` in `run_pages` — the
    /// happy-path `run_pages_*` tests cover the success branch, but the `?`
    /// on line 240 needs a failing space lookup to be marked covered.
    #[tokio::test]
    async fn run_pages_propagates_resolve_space_id_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client =
            crate::atlassian::client::AtlassianClient::new(&server.uri(), "u@t.com", "tok")
                .unwrap();
        let api = ConfluenceApi::new(client);
        let err = run_pages(&api, "NOPE", None, None, None, 25, &OutputFormat::Json)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn pages_command_execute_propagates_create_client_error() {
        let guard = crate::atlassian::auth::test_util::EnvGuard::take();
        let _home = guard.clear_credentials();

        let cmd = PagesCommand {
            key: "ENG".to_string(),
            status: None,
            sort: None,
            cursor: None,
            limit: 25,
            output: OutputFormat::Yaml,
        };
        assert!(cmd.execute().await.is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pages_command_execute_runs_through_dispatch() {
        let _lock = crate::atlassian::auth::test_util::AUTH_ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .and(wiremock::matchers::query_param("keys", "ENG"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": [{"id": "55"}]})),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/55/pages"))
            .and(wiremock::matchers::query_param("status", "archived"))
            .and(wiremock::matchers::query_param("sort", "-created-date"))
            .and(wiremock::matchers::query_param("cursor", "opaque"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .mount(&server)
            .await;

        set_atlassian_env(&server.uri());
        let cmd = SpaceCommand {
            command: SpaceSubcommands::Pages(PagesCommand {
                key: "ENG".to_string(),
                status: Some("archived".to_string()),
                sort: Some("-created-date".to_string()),
                cursor: Some("opaque".to_string()),
                limit: 5,
                output: OutputFormat::Json,
            }),
        };
        let result = cmd.execute().await;
        clear_atlassian_env();
        assert!(result.is_ok(), "{result:?}");
    }
}
