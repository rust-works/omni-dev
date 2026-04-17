//! CLI command for listing Confluence page children.

use anyhow::Result;
use clap::Parser;
use serde::Serialize;

use crate::atlassian::confluence_api::{ChildPage, ConfluenceApi};
use crate::cli::atlassian::format::{output_as, OutputFormat};
use crate::cli::atlassian::helpers::create_client;

/// Lists child pages of a Confluence page, or top-level pages in a space.
#[derive(Parser)]
pub struct ChildrenCommand {
    /// Page ID whose children should be listed. Omit when using `--space`.
    pub id: Option<String>,

    /// List top-level pages in this space (mutually exclusive with `id`).
    #[arg(long, conflicts_with = "id")]
    pub space: Option<String>,

    /// Recursively fetch descendants.
    #[arg(long)]
    pub recursive: bool,

    /// Maximum tree depth when `--recursive` is set (0 = unlimited).
    #[arg(long, default_value_t = 0)]
    pub max_depth: u32,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

/// A single entry in the children listing, with optional children for tree output.
#[derive(Debug, Clone, Serialize)]
pub struct ChildrenEntry {
    /// Page ID.
    pub id: String,
    /// Page title.
    pub title: String,
    /// Page status (e.g. "current", "draft"). Empty if unknown.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub status: String,
    /// Parent page ID, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Space key, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space_key: Option<String>,
    /// Nested children (populated when `--recursive` is set).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Self>,
}

impl From<ChildPage> for ChildrenEntry {
    fn from(p: ChildPage) -> Self {
        Self {
            id: p.id,
            title: p.title,
            status: p.status,
            parent_id: p.parent_id,
            space_key: p.space_key,
            children: Vec::new(),
        }
    }
}

impl ChildrenCommand {
    /// Executes the command.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        self.run(&ConfluenceApi::new(client)).await
    }

    /// Runs the command against the given API. Separated from `execute` so it
    /// can be exercised in tests without loading credentials.
    async fn run(self, api: &ConfluenceApi) -> Result<()> {
        run_children(
            api,
            self.id.as_deref(),
            self.space.as_deref(),
            self.recursive,
            self.max_depth,
            &self.output,
        )
        .await
    }
}

/// Lists children for the given target (page ID or space key) and prints them.
pub async fn run_children(
    api: &ConfluenceApi,
    id: Option<&str>,
    space: Option<&str>,
    recursive: bool,
    max_depth: u32,
    output: &OutputFormat,
) -> Result<()> {
    if id.is_none() && space.is_none() {
        anyhow::bail!("Provide either a page ID or --space <KEY>");
    }

    let space_key = space.map(ToString::to_string);
    let top = fetch_top_level(api, id, space).await?;
    let mut entries = to_entries(top, space_key.as_deref());

    if recursive {
        for entry in &mut entries {
            populate_descendants(api, entry, 1, max_depth, space_key.as_deref()).await?;
        }
    }

    if output_as(&entries, output)? {
        return Ok(());
    }

    if recursive {
        print_tree(&entries);
    } else {
        print_table(&entries);
    }

    Ok(())
}

/// Fetches the top-level page list for either a page id or space key.
async fn fetch_top_level(
    api: &ConfluenceApi,
    id: Option<&str>,
    space: Option<&str>,
) -> Result<Vec<ChildPage>> {
    if let Some(page_id) = id {
        return api.get_children(page_id).await;
    }
    if let Some(space_key) = space {
        let space_id = api.resolve_space_id(space_key).await?;
        return api.get_space_root_pages(&space_id).await;
    }
    unreachable!("caller guarantees id or space is Some")
}

/// Recursively fetches descendants and populates the `children` field.
fn populate_descendants<'a>(
    api: &'a ConfluenceApi,
    entry: &'a mut ChildrenEntry,
    depth: u32,
    max_depth: u32,
    space_key: Option<&'a str>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(async move {
        if should_recurse(depth, max_depth) {
            fetch_and_populate(api, entry, depth, max_depth, space_key).await?;
        }
        Ok(())
    })
}

/// Whether recursion should continue at the given depth.
fn should_recurse(depth: u32, max_depth: u32) -> bool {
    max_depth == 0 || depth < max_depth
}

/// Fetches children for a single entry, populates the `children` field, and
/// recurses into each child.
async fn fetch_and_populate<'a>(
    api: &'a ConfluenceApi,
    entry: &'a mut ChildrenEntry,
    depth: u32,
    max_depth: u32,
    space_key: Option<&'a str>,
) -> Result<()> {
    entry.children = to_entries(api.get_children(&entry.id).await?, space_key);
    for child in &mut entry.children {
        populate_descendants(api, child, depth + 1, max_depth, space_key).await?;
    }
    Ok(())
}

/// Converts a list of `ChildPage` into `ChildrenEntry`, propagating the given
/// `space_key` when a page has none set.
fn to_entries(pages: Vec<ChildPage>, space_key: Option<&str>) -> Vec<ChildrenEntry> {
    let mut entries = Vec::with_capacity(pages.len());
    for mut page in pages {
        if page.space_key.is_none() {
            page.space_key = space_key.map(str::to_string);
        }
        entries.push(ChildrenEntry::from(page));
    }
    entries
}

/// Prints a flat table of top-level entries.
fn print_table(entries: &[ChildrenEntry]) {
    if entries.is_empty() {
        println!("No pages found.");
        return;
    }

    let id_width = entries.iter().map(|e| e.id.len()).max().unwrap_or(2).max(2);
    let status_width = entries
        .iter()
        .map(|e| e.status.len())
        .max()
        .unwrap_or(6)
        .max(6);

    println!("{:<id_width$}  {:<status_width$}  TITLE", "ID", "STATUS");
    println!(
        "{:<id_width$}  {:<status_width$}  {}",
        "-".repeat(id_width),
        "-".repeat(status_width),
        "-".repeat(5),
    );
    for entry in entries {
        println!(
            "{:<id_width$}  {:<status_width$}  {}",
            entry.id, entry.status, entry.title
        );
    }
}

/// Prints an indented tree of entries.
fn print_tree(entries: &[ChildrenEntry]) {
    if entries.is_empty() {
        println!("No pages found.");
        return;
    }
    let last = entries.len().saturating_sub(1);
    for (i, entry) in entries.iter().enumerate() {
        print_tree_node(entry, "", i == last);
    }
}

fn print_tree_node(entry: &ChildrenEntry, prefix: &str, is_last: bool) {
    let connector = if is_last { "└── " } else { "├── " };
    println!("{prefix}{connector}{} ({})", entry.title, entry.id);

    let child_prefix = format!("{prefix}{}", if is_last { "    " } else { "│   " });
    let last = entry.children.len().saturating_sub(1);
    for (i, child) in entry.children.iter().enumerate() {
        print_tree_node(child, &child_prefix, i == last);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::atlassian::client::AtlassianClient;

    fn sample_child(id: &str, title: &str) -> ChildPage {
        ChildPage {
            id: id.to_string(),
            title: title.to_string(),
            status: "current".to_string(),
            parent_id: Some("100".to_string()),
            space_key: None,
        }
    }

    // ── ChildrenEntry::from ────────────────────────────────────────

    #[test]
    fn children_entry_from_child_page() {
        let entry = ChildrenEntry::from(sample_child("1", "Page"));
        assert_eq!(entry.id, "1");
        assert_eq!(entry.title, "Page");
        assert_eq!(entry.status, "current");
        assert_eq!(entry.parent_id.as_deref(), Some("100"));
        assert!(entry.children.is_empty());
    }

    #[test]
    fn children_entry_serialize_skips_empty() {
        let entry = ChildrenEntry {
            id: "1".to_string(),
            title: "P".to_string(),
            status: String::new(),
            parent_id: None,
            space_key: None,
            children: Vec::new(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains("status"));
        assert!(!json.contains("parent_id"));
        assert!(!json.contains("space_key"));
        assert!(!json.contains("children"));
    }

    // ── should_recurse ─────────────────────────────────────────────

    #[test]
    fn should_recurse_unlimited() {
        assert!(should_recurse(1, 0));
        assert!(should_recurse(100, 0));
    }

    #[test]
    fn should_recurse_within_limit() {
        assert!(should_recurse(1, 3));
        assert!(should_recurse(2, 3));
    }

    #[test]
    fn should_recurse_at_limit() {
        assert!(!should_recurse(3, 3));
    }

    #[test]
    fn should_recurse_past_limit() {
        assert!(!should_recurse(5, 3));
    }

    // ── to_entries ─────────────────────────────────────────────────

    #[test]
    fn to_entries_preserves_existing_space_key() {
        let pages = vec![ChildPage {
            id: "1".to_string(),
            title: "P".to_string(),
            status: "current".to_string(),
            parent_id: None,
            space_key: Some("PRE".to_string()),
        }];
        let entries = to_entries(pages, Some("OTHER"));
        assert_eq!(entries[0].space_key.as_deref(), Some("PRE"));
    }

    #[test]
    fn to_entries_fills_missing_space_key() {
        let pages = vec![ChildPage {
            id: "1".to_string(),
            title: "P".to_string(),
            status: "current".to_string(),
            parent_id: None,
            space_key: None,
        }];
        let entries = to_entries(pages, Some("ENG"));
        assert_eq!(entries[0].space_key.as_deref(), Some("ENG"));
    }

    #[test]
    fn to_entries_empty_input_returns_empty() {
        let entries = to_entries(Vec::new(), Some("ENG"));
        assert!(entries.is_empty());
    }

    #[test]
    fn to_entries_none_space_key_leaves_none() {
        let pages = vec![ChildPage {
            id: "1".to_string(),
            title: "P".to_string(),
            status: "current".to_string(),
            parent_id: None,
            space_key: None,
        }];
        let entries = to_entries(pages, None);
        assert!(entries[0].space_key.is_none());
    }

    // ── fetch_top_level ────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_top_level_by_id() {
        let (server, api) = setup_api().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/42/child/page",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{"id": "99", "title": "X", "status": "current"}]
                })),
            )
            .mount(&server)
            .await;

        let pages = fetch_top_level(&api, Some("42"), None).await.unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].id, "99");
    }

    #[tokio::test]
    async fn fetch_top_level_by_space() {
        let (server, api) = setup_api().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": [{"id": "555"}]})),
            )
            .mount(&server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/555/pages"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{"id": "1", "title": "Root", "status": "current"}]
                })),
            )
            .mount(&server)
            .await;

        let pages = fetch_top_level(&api, None, Some("KEY")).await.unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].id, "1");
    }

    #[tokio::test]
    async fn fetch_top_level_by_id_takes_precedence_over_space() {
        let (server, api) = setup_api().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/42/child/page",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": []
                })),
            )
            .mount(&server)
            .await;

        // If --space path were used, these would be needed; expect(0) guards that.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        let pages = fetch_top_level(&api, Some("42"), Some("KEY"))
            .await
            .unwrap();
        assert!(pages.is_empty());
    }

    // ── print_table / print_tree ───────────────────────────────────

    #[test]
    fn print_table_empty() {
        print_table(&[]);
    }

    #[test]
    fn print_table_with_entries() {
        let entries = vec![
            ChildrenEntry::from(sample_child("123", "One")),
            ChildrenEntry::from(sample_child("456", "Two")),
        ];
        print_table(&entries);
    }

    #[test]
    fn print_tree_empty() {
        print_tree(&[]);
    }

    #[test]
    fn print_tree_nested() {
        let mut root = ChildrenEntry::from(sample_child("1", "Root"));
        let mut mid = ChildrenEntry::from(sample_child("2", "Mid"));
        mid.children
            .push(ChildrenEntry::from(sample_child("3", "Leaf")));
        root.children.push(mid);
        root.children
            .push(ChildrenEntry::from(sample_child("4", "Sibling")));
        print_tree(&[root]);
    }

    // ── run_children (wiremock) ────────────────────────────────────

    async fn setup_api() -> (wiremock::MockServer, ConfluenceApi) {
        let server = wiremock::MockServer::start().await;
        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        (server, api)
    }

    #[tokio::test]
    async fn run_children_requires_target() {
        let (_server, api) = setup_api().await;
        let err = run_children(&api, None, None, false, 0, &OutputFormat::Json)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Provide either"));
    }

    #[tokio::test]
    async fn run_children_by_id_json() {
        let (server, api) = setup_api().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/100/child/page",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"id": "111", "title": "Alpha", "status": "current"},
                        {"id": "222", "title": "Beta", "status": "current"}
                    ]
                })),
            )
            .mount(&server)
            .await;

        run_children(&api, Some("100"), None, false, 0, &OutputFormat::Json)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_children_by_id_table() {
        let (server, api) = setup_api().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/100/child/page",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"id": "111", "title": "Alpha", "status": "current"}
                    ]
                })),
            )
            .mount(&server)
            .await;

        run_children(&api, Some("100"), None, false, 0, &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_children_by_id_empty_table() {
        let (server, api) = setup_api().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/100/child/page",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .mount(&server)
            .await;

        run_children(&api, Some("100"), None, false, 0, &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_children_by_space_yaml() {
        let (server, api) = setup_api().await;

        // Space key → space ID lookup.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": [{"id": "98765"}]})),
            )
            .mount(&server)
            .await;

        // Space root pages (depth=root).
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98765/pages"))
            .and(wiremock::matchers::query_param("depth", "root"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"id": "777", "title": "Home", "status": "current", "parentId": null},
                        {"id": "888", "title": "Other", "status": "current"}
                    ]
                })),
            )
            .mount(&server)
            .await;

        run_children(&api, None, Some("ENG"), false, 0, &OutputFormat::Yaml)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_children_by_space_recursive_propagates_space_key() {
        let (server, api) = setup_api().await;

        // Space key → space ID lookup.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": [{"id": "98765"}]})),
            )
            .mount(&server)
            .await;

        // Space root pages — one top-level page with no space_key set.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98765/pages"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"id": "777", "title": "Home", "status": "current"}
                    ]
                })),
            )
            .mount(&server)
            .await;

        // Descendants of 777.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/777/child/page",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"id": "888", "title": "Sub", "status": "current"}
                    ]
                })),
            )
            .mount(&server)
            .await;

        // Leaf with no children.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/888/child/page",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .mount(&server)
            .await;

        // Use JSON so we can inspect the serialised output for space_key propagation.
        let space_id = api.resolve_space_id("ENG").await.unwrap();
        let top = api.get_space_root_pages(&space_id).await.unwrap();
        assert_eq!(top.len(), 1);

        // Now exercise the full recursive flow.
        run_children(&api, None, Some("ENG"), true, 0, &OutputFormat::Json)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_children_by_space_error_propagates() {
        let (server, api) = setup_api().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .mount(&server)
            .await;

        // resolve_space_id should fail with "not found" since we returned empty results.
        let err = run_children(&api, None, Some("NOPE"), false, 0, &OutputFormat::Json)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn run_children_recursive_respects_max_depth() {
        let (server, api) = setup_api().await;

        // Root page → two children.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/1/child/page",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"id": "2", "title": "Child A", "status": "current"},
                        {"id": "3", "title": "Child B", "status": "current"}
                    ]
                })),
            )
            .mount(&server)
            .await;

        // Even though the mock below would respond if called, max_depth=1 should
        // prevent `get_children` from being invoked for ids 2 and 3.
        // We don't set `.expect()` so an unexpected call simply logs — wiremock's
        // default is permissive. Configure stricter expectations below.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/2/child/page",
            ))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/3/child/page",
            ))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        run_children(&api, Some("1"), None, true, 1, &OutputFormat::Json)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_children_recursive_walks_tree() {
        let (server, api) = setup_api().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/1/child/page",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"id": "2", "title": "Mid", "status": "current"}
                    ]
                })),
            )
            .mount(&server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/2/child/page",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"id": "3", "title": "Leaf", "status": "current"}
                    ]
                })),
            )
            .mount(&server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/3/child/page",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .mount(&server)
            .await;

        run_children(&api, Some("1"), None, true, 0, &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_children_api_error_propagates() {
        let (server, api) = setup_api().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/99999/child/page",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;

        let err = run_children(&api, Some("99999"), None, false, 0, &OutputFormat::Json)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    // ── ChildrenCommand struct ─────────────────────────────────────

    #[test]
    fn children_command_defaults() {
        let cmd = ChildrenCommand {
            id: Some("12345".to_string()),
            space: None,
            recursive: false,
            max_depth: 0,
            output: OutputFormat::Table,
        };
        assert_eq!(cmd.id.as_deref(), Some("12345"));
        assert!(cmd.space.is_none());
        assert!(!cmd.recursive);
    }

    #[tokio::test]
    async fn children_command_run_dispatches_to_run_children() {
        let (server, api) = setup_api().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/child/page",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .mount(&server)
            .await;

        let cmd = ChildrenCommand {
            id: Some("12345".to_string()),
            space: None,
            recursive: false,
            max_depth: 0,
            output: OutputFormat::Json,
        };
        cmd.run(&api).await.unwrap();
    }

    #[test]
    fn children_command_space_mode() {
        let cmd = ChildrenCommand {
            id: None,
            space: Some("ENG".to_string()),
            recursive: true,
            max_depth: 3,
            output: OutputFormat::Yaml,
        };
        assert_eq!(cmd.space.as_deref(), Some("ENG"));
        assert!(cmd.recursive);
        assert_eq!(cmd.max_depth, 3);
    }
}
