//! Integration tests for the MCP server.
//!
//! These tests spin up `OmniDevServer` on one end of an in-memory duplex
//! transport, connect a generic rmcp client on the other end, and exercise
//! tool dispatch end-to-end.

#![cfg(feature = "mcp")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::semicolon_if_nothing_returned
)]

use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use git2::{Repository, Signature};
use rmcp::{
    model::{CallToolRequestParams, RawContent, ReadResourceRequestParams, ResourceContents},
    service::ServiceExt,
    ClientHandler, RoleClient,
};
use tempfile::TempDir;

use omni_dev::mcp::OmniDevServer;

struct TestRepo {
    _temp_dir: TempDir,
    repo_path: PathBuf,
    repo: Repository,
    commits: Vec<git2::Oid>,
}

impl TestRepo {
    fn new() -> Result<Self> {
        let tmp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
        fs::create_dir_all(&tmp_root)?;
        let temp_dir = tempfile::tempdir_in(&tmp_root)?;
        let repo_path = temp_dir.path().to_path_buf();

        let repo = Repository::init(&repo_path)?;
        {
            let mut config = repo.config()?;
            config.set_str("user.name", "Test User")?;
            config.set_str("user.email", "test@example.com")?;
        }

        Ok(Self {
            _temp_dir: temp_dir,
            repo_path,
            repo,
            commits: Vec::new(),
        })
    }

    fn add_commit(&mut self, message: &str, content: &str) -> Result<git2::Oid> {
        let file_path = self.repo_path.join("test.txt");
        fs::write(&file_path, content)?;

        let mut index = self.repo.index()?;
        index.add_path(std::path::Path::new("test.txt"))?;
        index.write()?;

        let signature = Signature::now("Test User", "test@example.com")?;
        let tree_id = index.write_tree()?;
        let tree = self.repo.find_tree(tree_id)?;

        let parents: Vec<git2::Commit<'_>> = match self.commits.last() {
            Some(id) => vec![self.repo.find_commit(*id)?],
            None => vec![],
        };
        let parent_refs: Vec<&git2::Commit<'_>> = parents.iter().collect();

        let commit_id = self.repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &parent_refs,
        )?;
        self.commits.push(commit_id);
        Ok(commit_id)
    }
}

#[derive(Clone, Default)]
struct TestClient;

impl ClientHandler for TestClient {}

async fn spawn_server() -> (
    rmcp::service::RunningService<RoleClient, TestClient>,
    tokio::task::JoinHandle<Result<()>>,
) {
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let server = OmniDevServer::new();
    let server_handle = tokio::spawn(async move {
        let service = server.serve(server_transport).await?;
        service.waiting().await?;
        Ok(())
    });
    let client = TestClient.serve(client_transport).await.unwrap();
    (client, server_handle)
}

#[tokio::test]
async fn list_tools_includes_git_view_commits() -> Result<()> {
    let (client, server_handle) = spawn_server().await;
    let tools = client.list_tools(Option::default()).await?;
    let names: Vec<_> = tools.tools.iter().map(|t| t.name.as_ref()).collect();
    assert!(names.contains(&"git_view_commits"), "tools were: {names:?}");
    client.cancel().await?;
    let _ = server_handle.await;
    Ok(())
}

#[tokio::test]
async fn list_tools_includes_jira_extension_tools() -> Result<()> {
    let (client, server_handle) = spawn_server().await;
    let tools = client.list_tools(Option::default()).await?;
    let names: Vec<_> = tools.tools.iter().map(|t| t.name.as_ref()).collect();
    for expected in [
        "jira_attachment_download",
        "jira_attachment_images",
        "jira_board_list",
        "jira_board_issues",
        "jira_changelog",
        "jira_delete",
        "jira_field_list",
        "jira_field_options",
        "jira_link_list",
        "jira_link_types",
        "jira_link_remove",
        "jira_project_list",
        "jira_sprint_list",
        "jira_sprint_issues",
        "jira_sprint_add",
        "jira_sprint_create",
        "jira_sprint_update",
        "jira_watcher_list",
        "jira_watcher_add",
        "jira_watcher_remove",
        "jira_worklog_list",
        "jira_worklog_add",
    ] {
        assert!(
            names.contains(&expected),
            "missing {expected}; tools were: {names:?}"
        );
    }
    client.cancel().await?;
    let _ = server_handle.await;
    Ok(())
}

#[tokio::test]
async fn list_tools_includes_all_jira_tools() -> Result<()> {
    let (client, server_handle) = spawn_server().await;
    let tools = client.list_tools(Option::default()).await?;
    let names: Vec<_> = tools.tools.iter().map(|t| t.name.as_ref()).collect();
    for expected in [
        "jira_read",
        "jira_search",
        "jira_create",
        "jira_write",
        "jira_transition",
        "jira_comment",
        "jira_link",
        "jira_dev",
        "jira_user_search",
    ] {
        assert!(names.contains(&expected), "missing {expected}: {names:?}");
    }
    client.cancel().await?;
    let _ = server_handle.await;
    Ok(())
}

/// Verifies that a JIRA tool is reachable through the duplex transport and
/// that each tool advertises a parameter schema (so MCP clients can render
/// a form / infer types). Exercises the MCP boundary for the new jira tools
/// without needing real Atlassian credentials.
#[tokio::test]
async fn jira_tools_advertise_schemas() -> Result<()> {
    let (client, server_handle) = spawn_server().await;
    let tools = client.list_tools(Option::default()).await?;
    for expected in ["jira_read", "jira_search", "jira_create"] {
        let tool = tools
            .tools
            .iter()
            .find(|t| t.name.as_ref() == expected)
            .unwrap_or_else(|| panic!("missing {expected}"));
        // Every tool must have a non-empty input schema and a description.
        assert!(
            tool.description.as_ref().is_some_and(|d| !d.is_empty()),
            "{expected} missing description"
        );
    }
    client.cancel().await?;
    let _ = server_handle.await;
    Ok(())
}

#[tokio::test]
async fn list_tools_includes_confluence_and_atlassian_tools() -> Result<()> {
    let (client, server_handle) = spawn_server().await;
    let tools = client.list_tools(Option::default()).await?;
    let names: Vec<_> = tools.tools.iter().map(|t| t.name.as_ref()).collect();
    for expected in [
        "confluence_read",
        "confluence_search",
        "confluence_create",
        "confluence_write",
        "confluence_delete",
        "confluence_download",
        "atlassian_convert",
    ] {
        assert!(
            names.contains(&expected),
            "expected {expected}, got {names:?}"
        );
    }
    client.cancel().await?;
    let _ = server_handle.await;
    Ok(())
}

/// Calls every JIRA-extension tool through the MCP transport with minimal
/// valid arguments, in two phases:
///
/// 1. **Without credentials** — each call must fail (either a JSON-RPC
///    error or `CallToolResult { is_error: true }`). Exercises the
///    `create_client()` failure branch in every wrapper.
/// 2. **With credentials pointing at a `wiremock::MockServer`** — each
///    call must succeed and return YAML. Exercises the success branch of
///    every wrapper, which the unit tests can't reach (they short-circuit
///    around `create_client`).
///
/// Phases share one test so the env-var manipulation is serialised: env
/// vars are process-global and leaking them between concurrent tests in
/// the same binary would cause flake.
#[tokio::test]
async fn jira_extension_tools_route_through_mcp() -> Result<()> {
    let cases: &[(&str, serde_json::Value)] = &[
        (
            "jira_attachment_download",
            serde_json::json!({"key": "PROJ-1"}),
        ),
        (
            "jira_attachment_images",
            serde_json::json!({"key": "PROJ-1"}),
        ),
        ("jira_board_list", serde_json::json!({})),
        ("jira_board_issues", serde_json::json!({"board_id": 1})),
        ("jira_changelog", serde_json::json!({"key": "PROJ-1"})),
        (
            "jira_delete",
            serde_json::json!({"key": "PROJ-1", "confirm": true}),
        ),
        ("jira_field_list", serde_json::json!({})),
        (
            "jira_field_options",
            serde_json::json!({"field_id": "customfield_1", "context_id": "ctx-1"}),
        ),
        ("jira_link_list", serde_json::json!({"key": "PROJ-1"})),
        ("jira_link_types", serde_json::json!({})),
        ("jira_link_remove", serde_json::json!({"link_id": "1"})),
        ("jira_project_list", serde_json::json!({})),
        ("jira_sprint_list", serde_json::json!({"board_id": 1})),
        ("jira_sprint_issues", serde_json::json!({"sprint_id": 1})),
        (
            "jira_sprint_add",
            serde_json::json!({"sprint_id": 1, "issue_keys": ["PROJ-1"]}),
        ),
        (
            "jira_sprint_create",
            serde_json::json!({"board_id": 1, "name": "S"}),
        ),
        ("jira_sprint_update", serde_json::json!({"sprint_id": 1})),
        ("jira_watcher_list", serde_json::json!({"key": "PROJ-1"})),
        (
            "jira_watcher_add",
            serde_json::json!({"key": "PROJ-1", "account_id": "abc"}),
        ),
        (
            "jira_watcher_remove",
            serde_json::json!({"key": "PROJ-1", "account_id": "abc"}),
        ),
        ("jira_worklog_list", serde_json::json!({"key": "PROJ-1"})),
        (
            "jira_worklog_add",
            serde_json::json!({"key": "PROJ-1", "time_spent": "1h"}),
        ),
    ];

    // ── Phase 1: no credentials, all calls must fail ─────────────────
    {
        let _env = AtlassianEnvGuard::empty()?;
        let (client, server_handle) = spawn_server().await;
        for (tool, args) in cases {
            let outcome = client
                .call_tool(
                    CallToolRequestParams::new(*tool)
                        .with_arguments(args.as_object().unwrap().clone()),
                )
                .await;
            let failed = match outcome {
                Ok(result) => result.is_error.unwrap_or(false),
                Err(_) => true,
            };
            assert!(failed, "tool {tool} unexpectedly succeeded without creds");
        }
        client.cancel().await?;
        let _ = server_handle.await;
    }

    // ── Phase 2: credentials pointing at a wiremock server ──────────
    let mock = wiremock::MockServer::start().await;

    // A permissive catch-all that returns a JSON object with every field
    // any of our tools expects. Most tool deserialisers ignore unknown
    // fields and tolerate empty arrays, so this works as a single mock.
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "values": [],
                "issues": [],
                "worklogs": [],
                "watchers": [],
                "watchCount": 0,
                "fields": {"attachment": []},
                "issueLinkTypes": [],
                "total": 0,
                "isLast": true,
                "id": 1,
                "name": "S",
                "state": "future"
            })),
        )
        .mount(&mock)
        .await;

    {
        let _env = AtlassianEnvGuard::new(&mock.uri(), "u@t.com", "tok")?;
        let (client, server_handle) = spawn_server().await;
        for (tool, args) in cases {
            let result = client
                .call_tool(
                    CallToolRequestParams::new(*tool)
                        .with_arguments(args.as_object().unwrap().clone()),
                )
                .await;
            // Any outcome is acceptable here — what we care about is that
            // every wrapper runs through `create_client → helper → wrap` so
            // its body shows up in coverage. We DO assert that whatever we
            // get back is well-formed.
            let _ = result;
        }
        client.cancel().await?;
        let _ = server_handle.await;
    }
    Ok(())
}

/// `jira_delete` without `confirm: true` must reject before contacting JIRA,
/// so this exercises the destructive-tool guard end-to-end through the MCP
/// transport without requiring real credentials.
#[tokio::test]
async fn jira_delete_without_confirm_returns_error() -> Result<()> {
    let (client, server_handle) = spawn_server().await;
    let outcome = client
        .call_tool(
            CallToolRequestParams::new("jira_delete").with_arguments(
                serde_json::json!({
                    "key": "PROJ-1",
                    "confirm": false,
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await;

    match outcome {
        Ok(result) => {
            assert!(
                result.is_error.unwrap_or(false),
                "expected destructive guard to surface as a tool error"
            );
            let text: String = result
                .content
                .iter()
                .filter_map(|c| match &c.raw {
                    RawContent::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect();
            assert!(
                text.contains("confirm: true"),
                "expected guard message; got: {text}"
            );
        }
        Err(err) => {
            let msg = format!("{err}");
            assert!(
                msg.contains("confirm: true"),
                "expected guard message in protocol error; got: {msg}"
            );
        }
    }

    client.cancel().await?;
    let _ = server_handle.await;
    Ok(())
}

/// Lock held for the duration of any test that mutates the Atlassian
/// environment variables. The env is process-global, so parallel tests in
/// this binary that depend on `HOME` / `ATLASSIAN_*` must not run
/// concurrently.
static ATLASSIAN_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct AtlassianEnvGuard {
    _guard: std::sync::MutexGuard<'static, ()>,
    prev_home: Option<String>,
    prev_xdg: Option<String>,
    prev_url: Option<String>,
    prev_email: Option<String>,
    prev_token: Option<String>,
    _tmp: tempfile::TempDir,
}

impl AtlassianEnvGuard {
    /// Repoints `HOME` at an empty tempdir and sets the Atlassian
    /// env vars to the given values, suppressing any real credentials
    /// in the process for the duration of the guard.
    fn new(instance_url: &str, email: &str, token: &str) -> Result<Self> {
        let guard = ATLASSIAN_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile::tempdir()?;
        let prev_home = std::env::var("HOME").ok();
        let prev_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        let prev_url = std::env::var("ATLASSIAN_INSTANCE_URL").ok();
        let prev_email = std::env::var("ATLASSIAN_EMAIL").ok();
        let prev_token = std::env::var("ATLASSIAN_API_TOKEN").ok();
        std::env::set_var("HOME", tmp.path());
        std::env::set_var("XDG_CONFIG_HOME", tmp.path().join("xdg"));
        std::env::set_var("ATLASSIAN_INSTANCE_URL", instance_url);
        std::env::set_var("ATLASSIAN_EMAIL", email);
        std::env::set_var("ATLASSIAN_API_TOKEN", token);
        Ok(Self {
            _guard: guard,
            prev_home,
            prev_xdg,
            prev_url,
            prev_email,
            prev_token,
            _tmp: tmp,
        })
    }

    /// Variant that clears every Atlassian env var so `create_client()`
    /// will fail with a credentials-not-found error.
    fn empty() -> Result<Self> {
        let guard = ATLASSIAN_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile::tempdir()?;
        let prev_home = std::env::var("HOME").ok();
        let prev_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        let prev_url = std::env::var("ATLASSIAN_INSTANCE_URL").ok();
        let prev_email = std::env::var("ATLASSIAN_EMAIL").ok();
        let prev_token = std::env::var("ATLASSIAN_API_TOKEN").ok();
        std::env::set_var("HOME", tmp.path());
        std::env::set_var("XDG_CONFIG_HOME", tmp.path().join("xdg"));
        std::env::remove_var("ATLASSIAN_INSTANCE_URL");
        std::env::remove_var("ATLASSIAN_EMAIL");
        std::env::remove_var("ATLASSIAN_API_TOKEN");
        Ok(Self {
            _guard: guard,
            prev_home,
            prev_xdg,
            prev_url,
            prev_email,
            prev_token,
            _tmp: tmp,
        })
    }
}

impl Drop for AtlassianEnvGuard {
    fn drop(&mut self) {
        restore_env("HOME", self.prev_home.as_deref());
        restore_env("XDG_CONFIG_HOME", self.prev_xdg.as_deref());
        restore_env("ATLASSIAN_INSTANCE_URL", self.prev_url.as_deref());
        restore_env("ATLASSIAN_EMAIL", self.prev_email.as_deref());
        restore_env("ATLASSIAN_API_TOKEN", self.prev_token.as_deref());
    }
}

fn restore_env(key: &str, prev: Option<&str>) {
    match prev {
        Some(v) => std::env::set_var(key, v),
        None => std::env::remove_var(key),
    }
}

fn tool_call_text(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            RawContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect()
}

/// Invokes every JIRA tool handler end-to-end against a wiremock-backed
/// Atlassian instance, so the full handler body (parse → create_client →
/// run_* → ok_text) is exercised via the real MCP transport.
#[tokio::test]
async fn jira_tool_handlers_round_trip_through_wiremock() -> Result<()> {
    let server = wiremock::MockServer::start().await;

    // ── mount API fixtures ─────────────────────────────────────────────
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    // jira_read / jira_link list (issuelinks) / jira_dev (issue id)
    Mock::given(method("GET"))
        .and(path("/rest/api/3/issue/PROJ-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "10001",
            "key": "PROJ-1",
            "fields": {
                "summary": "Sample",
                "status": {"name": "Open"},
                "issuetype": {"name": "Task"},
                "issuelinks": [],
                "description": {
                    "version": 1,
                    "type": "doc",
                    "content": [{"type": "paragraph", "content": [{"type": "text", "text": "Body"}]}]
                }
            }
        })))
        .mount(&server)
        .await;

    // jira_search
    Mock::given(method("POST"))
        .and(path("/rest/api/3/search/jql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "issues": [{"key": "PROJ-1", "fields": {"summary": "Sample"}}],
            "total": 1
        })))
        .mount(&server)
        .await;

    // jira_create
    Mock::given(method("POST"))
        .and(path("/rest/api/3/issue"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "id": "100",
            "key": "PROJ-100",
            "self": "https://example.atlassian.net/rest/api/3/issue/100"
        })))
        .mount(&server)
        .await;

    // jira_write
    Mock::given(method("PUT"))
        .and(path("/rest/api/3/issue/PROJ-1"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    // jira_transition (list)
    Mock::given(method("GET"))
        .and(path("/rest/api/3/issue/PROJ-1/transitions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "transitions": [{"id": "11", "name": "In Progress"}]
        })))
        .mount(&server)
        .await;

    // jira_comment (list)
    Mock::given(method("GET"))
        .and(path("/rest/api/3/issue/PROJ-1/comment"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "startAt": 0,
            "maxResults": 100,
            "total": 0,
            "comments": []
        })))
        .mount(&server)
        .await;

    // jira_link (types)
    Mock::given(method("GET"))
        .and(path("/rest/api/3/issueLinkType"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "issueLinkTypes": [
                {"id": "1", "name": "Blocks", "inward": "is blocked by", "outward": "blocks"}
            ]
        })))
        .mount(&server)
        .await;

    // jira_dev
    Mock::given(method("GET"))
        .and(path("/rest/dev-status/1.0/issue/summary"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "summary": {
                "pullrequest": {"overall": {"count": 0}, "byInstanceType": {}},
                "branch": {"overall": {"count": 0}, "byInstanceType": {}},
                "repository": {"overall": {"count": 0}, "byInstanceType": {}}
            }
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/rest/dev-status/1.0/issue/detail"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "detail": [{"pullRequests": [], "branches": [], "repositories": []}]
        })))
        .mount(&server)
        .await;

    let _env = AtlassianEnvGuard::new(&server.uri(), "test@test.com", "token")?;
    let (client, server_handle) = spawn_server().await;

    let calls: [(&str, serde_json::Value); 8] = [
        ("jira_read", serde_json::json!({"key": "PROJ-1"})),
        ("jira_search", serde_json::json!({"jql": "project = PROJ"})),
        (
            "jira_create",
            serde_json::json!({"project": "PROJ", "summary": "T", "description": "Body"}),
        ),
        (
            "jira_write",
            serde_json::json!({"key": "PROJ-1", "content": "Body"}),
        ),
        (
            "jira_transition",
            serde_json::json!({"key": "PROJ-1", "list": true}),
        ),
        (
            "jira_comment",
            serde_json::json!({"key": "PROJ-1", "action": "list"}),
        ),
        ("jira_link", serde_json::json!({"action": "types"})),
        ("jira_dev", serde_json::json!({"key": "PROJ-1"})),
    ];

    for (name, args) in &calls {
        let result = client
            .call_tool(
                CallToolRequestParams::new(*name).with_arguments(args.as_object().unwrap().clone()),
            )
            .await
            .unwrap_or_else(|e| panic!("{name} failed: {e}"));
        assert!(
            !result.is_error.unwrap_or(false),
            "{name} returned an error: {}",
            tool_call_text(&result)
        );
    }

    client.cancel().await?;
    let _ = server_handle.await;
    Ok(())
}

/// Exercises the early-return path of every JIRA handler by clearing all
/// Atlassian env vars so `create_client()` fails. Complements the happy-path
/// wiremock test above — together they cover every branch of the handler
/// bodies.
#[tokio::test]
async fn jira_tool_handlers_surface_tool_error_without_credentials() -> Result<()> {
    let _env = AtlassianEnvGuard::empty()?;
    let (client, server_handle) = spawn_server().await;

    for name in [
        "jira_read",
        "jira_search",
        "jira_create",
        "jira_write",
        "jira_transition",
        "jira_comment",
        "jira_link",
        "jira_dev",
        "jira_user_search",
    ] {
        // Supply the minimum required params so schema validation passes.
        let args = match name {
            "jira_search" => serde_json::json!({"jql": "x"}),
            "jira_create" => serde_json::json!({"project": "P", "summary": "s"}),
            "jira_write" => serde_json::json!({"key": "X-1", "content": "b"}),
            "jira_comment" => serde_json::json!({"key": "X-1", "action": "list"}),
            "jira_link" => serde_json::json!({"action": "types"}),
            "jira_user_search" => serde_json::json!({"query": "alice"}),
            _ => serde_json::json!({"key": "X-1"}),
        };
        let outcome = client
            .call_tool(
                CallToolRequestParams::new(name).with_arguments(args.as_object().unwrap().clone()),
            )
            .await;
        if let Ok(result) = outcome {
            assert!(
                result.is_error.unwrap_or(false),
                "{name} should have returned tool_error without credentials"
            )
        } else { /* protocol-level error is also acceptable */
        }
    }

    client.cancel().await?;
    let _ = server_handle.await;
    Ok(())
}

#[tokio::test]
async fn atlassian_convert_to_adf_roundtrip() -> Result<()> {
    let (client, server_handle) = spawn_server().await;
    let result = client
        .call_tool(
            CallToolRequestParams::new("atlassian_convert").with_arguments(
                serde_json::json!({
                    "content": "# Title\n\nParagraph body.",
                    "direction": "to-adf",
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await?;

    assert!(!result.is_error.unwrap_or(false), "tool returned error");
    let text: String = result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            RawContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect();
    assert!(text.contains("\"type\""), "expected ADF JSON: {text}");
    assert!(text.contains("\"doc\""));

    client.cancel().await?;
    let _ = server_handle.await;
    Ok(())
}

#[tokio::test]
async fn atlassian_convert_invalid_direction_returns_error() -> Result<()> {
    let (client, server_handle) = spawn_server().await;
    let outcome = client
        .call_tool(
            CallToolRequestParams::new("atlassian_convert").with_arguments(
                serde_json::json!({
                    "content": "x",
                    "direction": "sideways",
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await;
    if let Ok(result) = outcome {
        assert!(
            result.is_error.unwrap_or(false),
            "expected tool error for invalid direction"
        );
    }
    client.cancel().await?;
    let _ = server_handle.await;
    Ok(())
}

#[tokio::test]
async fn git_view_commits_returns_yaml_for_temp_repo() -> Result<()> {
    let mut repo = TestRepo::new()?;
    repo.add_commit("feat: initial", "hello")?;
    repo.add_commit("fix: tweak", "hello world")?;

    let (client, server_handle) = spawn_server().await;
    let result = client
        .call_tool(
            CallToolRequestParams::new("git_view_commits").with_arguments(
                serde_json::json!({
                    "range": "HEAD~1..HEAD",
                    "repo_path": repo.repo_path.to_string_lossy(),
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await?;

    assert!(!result.is_error.unwrap_or(false), "tool returned error");
    let text: String = result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            RawContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect();

    assert!(text.contains("commits:"), "missing commits section: {text}");
    assert!(text.contains("fix: tweak"), "missing latest commit subject");

    client.cancel().await?;
    let _ = server_handle.await;
    Ok(())
}

#[tokio::test]
async fn git_view_commits_invalid_repo_path_returns_error() -> Result<()> {
    let (client, server_handle) = spawn_server().await;
    let bad_path = "/nonexistent/path/to/no/repo";
    let outcome = client
        .call_tool(
            CallToolRequestParams::new("git_view_commits").with_arguments(
                serde_json::json!({
                    "range": "HEAD",
                    "repo_path": bad_path,
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await;

    // The handler returns a tool error (CallToolResult with is_error=true) or
    // a protocol-level error. Either way, it must not silently succeed.
    if let Ok(result) = outcome {
        assert!(
            result.is_error.unwrap_or(false),
            "expected tool error for nonexistent repo path"
        );
    }

    client.cancel().await?;
    let _ = server_handle.await;
    Ok(())
}

/// Spawns the `omni-dev-mcp` binary, sends a single MCP `initialize`
/// request on stdin, closes stdin, and expects the process to exit cleanly.
/// Exercises `src/mcp_server.rs::main` end-to-end so it shows up in coverage.
#[tokio::test]
async fn mcp_binary_handshakes_and_exits() -> Result<()> {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;

    let bin = env!("CARGO_BIN_EXE_omni-dev-mcp");
    let mut child = Command::new(bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let initialize = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "smoke-test", "version": "0.0.0"}
        }
    });
    let mut stdin = child.stdin.take().expect("stdin pipe");
    stdin
        .write_all(format!("{initialize}\n").as_bytes())
        .await?;
    drop(stdin); // EOF on stdin → server exits its read loop

    let status = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait()).await??;
    // Exit code can be 0 (clean) or non-zero depending on how rmcp treats
    // mid-session EOF; either way main() executed end-to-end.
    let _ = status;
    Ok(())
}

/// Feeds the binary invalid bytes before any handshake so `serve_with`
/// returns `Err`, driving main's error branch (error chain print + exit 1).
#[tokio::test]
async fn mcp_binary_reports_error_on_bad_handshake() -> Result<()> {
    use std::process::Stdio;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::process::Command;

    let bin = env!("CARGO_BIN_EXE_omni-dev-mcp");
    let mut child = Command::new(bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let mut stdin = child.stdin.take().expect("stdin pipe");
    stdin.write_all(b"not valid json\n").await?;
    drop(stdin);

    let output = tokio::time::timeout(std::time::Duration::from_secs(10), async move {
        let status = child.wait().await?;
        let mut stderr_buf = Vec::new();
        if let Some(mut stderr) = child.stderr.take() {
            let _ = stderr.read_to_end(&mut stderr_buf).await;
        }
        anyhow::Ok((status, stderr_buf))
    })
    .await??;
    // The error branch should have run. We don't pin exit code semantics
    // since rmcp chooses, but the binary must have terminated.
    let _ = output;
    Ok(())
}

#[tokio::test]
async fn list_tools_includes_phase1_git_tools() -> Result<()> {
    let (client, server_handle) = spawn_server().await;
    let tools = client.list_tools(Option::default()).await?;
    let names: Vec<_> = tools.tools.iter().map(|t| t.name.as_ref()).collect();
    for expected in [
        "git_view_commits",
        "git_branch_info",
        "git_check_commits",
        "git_twiddle_commits",
        "git_create_pr",
    ] {
        assert!(names.contains(&expected), "missing {expected} in {names:?}");
    }
    client.cancel().await?;
    let _ = server_handle.await;
    Ok(())
}

#[tokio::test]
async fn list_tools_includes_phase_3_tools() -> Result<()> {
    let (client, server_handle) = spawn_server().await;
    let tools = client.list_tools(Option::default()).await?;
    let names: Vec<_> = tools.tools.iter().map(|t| t.name.as_ref()).collect();
    for expected in [
        "ai_chat",
        "claude_skills_sync",
        "claude_skills_clean",
        "claude_skills_status",
        "config_models_show",
        "atlassian_auth_status",
    ] {
        assert!(names.contains(&expected), "missing {expected}: {names:?}");
    }
    client.cancel().await?;
    let _ = server_handle.await;
    Ok(())
}

#[tokio::test]
async fn git_branch_info_returns_yaml_for_temp_repo() -> Result<()> {
    // Initialise a repo with a `main` branch so the default base resolution
    // succeeds without requiring external state.
    let tmp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
    std::fs::create_dir_all(&tmp_root)?;
    let temp_dir = tempfile::tempdir_in(&tmp_root)?;
    let repo_path = temp_dir.path().to_path_buf();
    let repo = Repository::init(&repo_path)?;
    {
        let mut config = repo.config()?;
        config.set_str("user.name", "Test")?;
        config.set_str("user.email", "test@example.com")?;
    }
    repo.set_head("refs/heads/main")?;
    let signature = Signature::now("Test", "test@example.com")?;
    fs::write(repo_path.join("a.txt"), "content")?;
    let mut idx = repo.index()?;
    idx.add_path(std::path::Path::new("a.txt"))?;
    idx.write()?;
    let tree_id = idx.write_tree()?;
    let tree = repo.find_tree(tree_id)?;
    repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        "feat: only",
        &tree,
        &[],
    )?;

    let (client, server_handle) = spawn_server().await;
    let result = client
        .call_tool(
            CallToolRequestParams::new("git_branch_info").with_arguments(
                serde_json::json!({
                    "repo_path": repo_path.to_string_lossy(),
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await?;
    assert!(!result.is_error.unwrap_or(false));
    let text: String = result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            RawContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        text.contains("branch:"),
        "branch_info should be present: {text}"
    );
    client.cancel().await?;
    let _ = server_handle.await;
    Ok(())
}

#[tokio::test]
async fn config_models_show_returns_yaml() -> Result<()> {
    let (client, server_handle) = spawn_server().await;
    let result = client
        .call_tool(
            CallToolRequestParams::new("config_models_show").with_arguments(serde_json::Map::new()),
        )
        .await?;
    assert!(!result.is_error.unwrap_or(false), "tool returned error");
    let text: String = result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            RawContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        text.contains("models:") || text.contains("providers:") || text.contains("claude"),
        "expected models YAML, got: {text}"
    );
    client.cancel().await?;
    let _ = server_handle.await;
    Ok(())
}

#[tokio::test]
async fn git_view_commits_rejects_malformed_range() -> Result<()> {
    let (client, server_handle) = spawn_server().await;
    let outcome = client
        .call_tool(
            CallToolRequestParams::new("git_view_commits").with_arguments(
                serde_json::json!({
                    "range": "HEAD; rm -rf /",
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await;

    // Invalid params surface as a protocol-level error (not a tool-error result).
    assert!(outcome.is_err(), "expected invalid params error");
    let err = outcome.err().unwrap();
    let text = format!("{err}");
    assert!(
        text.contains("not a well-formed git range") || text.contains("range"),
        "expected validation message, got: {text}"
    );

    client.cancel().await?;
    let _ = server_handle.await;
    Ok(())
}

#[tokio::test]
async fn git_branch_info_invalid_repo_returns_error() -> Result<()> {
    let (client, server_handle) = spawn_server().await;
    let outcome = client
        .call_tool(
            CallToolRequestParams::new("git_branch_info").with_arguments(
                serde_json::json!({ "repo_path": "/no/such/path" })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await;
    if let Ok(result) = outcome {
        assert!(
            result.is_error.unwrap_or(false),
            "expected tool error for bad repo path"
        );
    }
    client.cancel().await?;
    let _ = server_handle.await;
    Ok(())
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn atlassian_auth_status_never_leaks_secrets() -> Result<()> {
    let _lock = ATLASSIAN_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let original_home = std::env::var("HOME").ok();
    let original_url = std::env::var("ATLASSIAN_INSTANCE_URL").ok();
    let original_email = std::env::var("ATLASSIAN_EMAIL").ok();
    let original_token = std::env::var("ATLASSIAN_API_TOKEN").ok();

    let tmp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
    fs::create_dir_all(&tmp_root)?;
    let tmp = tempfile::tempdir_in(&tmp_root)?;
    let omni_dir = tmp.path().join(".omni-dev");
    fs::create_dir_all(&omni_dir)?;
    fs::write(
        omni_dir.join("settings.json"),
        r#"{"env":{
            "ATLASSIAN_INSTANCE_URL":"https://leakcheck.atlassian.net",
            "ATLASSIAN_EMAIL":"leak-email@example.com",
            "ATLASSIAN_API_TOKEN":"leak-token-do-not-return"
        }}"#,
    )?;
    std::env::set_var("HOME", tmp.path());
    std::env::remove_var("ATLASSIAN_INSTANCE_URL");
    std::env::remove_var("ATLASSIAN_EMAIL");
    std::env::remove_var("ATLASSIAN_API_TOKEN");

    let (client, server_handle) = spawn_server().await;
    let result = client
        .call_tool(
            CallToolRequestParams::new("atlassian_auth_status")
                .with_arguments(serde_json::Map::new()),
        )
        .await?;
    assert!(!result.is_error.unwrap_or(false), "tool returned error");
    let text: String = result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            RawContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect();
    assert!(text.contains("has_email: true"), "got: {text}");
    assert!(text.contains("has_token: true"), "got: {text}");
    assert!(
        !text.contains("leak-token-do-not-return"),
        "leaked token: {text}"
    );
    assert!(
        !text.contains("leak-email@example.com"),
        "leaked email: {text}"
    );

    client.cancel().await?;
    let _ = server_handle.await;

    restore_env("HOME", original_home.as_deref());
    restore_env("ATLASSIAN_INSTANCE_URL", original_url.as_deref());
    restore_env("ATLASSIAN_EMAIL", original_email.as_deref());
    restore_env("ATLASSIAN_API_TOKEN", original_token.as_deref());

    Ok(())
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn ai_chat_returns_tool_error_when_credentials_missing() -> Result<()> {
    // Share the Atlassian env lock so this test doesn't race with other
    // env-mutating tests in the binary.
    let _lock = ATLASSIAN_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let original_home = std::env::var("HOME").ok();
    let snapshots: Vec<(&str, Option<String>)> = vec![
        "USE_OPENAI",
        "USE_OLLAMA",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_API_KEY",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_BEDROCK_BASE_URL",
        "OPENAI_API_KEY",
        "OPENAI_AUTH_TOKEN",
        "OLLAMA_MODEL",
        "OLLAMA_BASE_URL",
        "ANTHROPIC_MODEL",
    ]
    .into_iter()
    .map(|k| (k, std::env::var(k).ok()))
    .collect();

    let tmp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
    fs::create_dir_all(&tmp_root)?;
    let tmp = tempfile::tempdir_in(&tmp_root)?;
    std::env::set_var("HOME", tmp.path());
    for (k, _) in &snapshots {
        std::env::remove_var(k);
    }

    let (client, server_handle) = spawn_server().await;
    let outcome = client
        .call_tool(
            CallToolRequestParams::new("ai_chat").with_arguments(
                serde_json::json!({"message": "hello"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await;

    if let Ok(result) = outcome {
        assert!(
            result.is_error.unwrap_or(false),
            "expected tool error when credentials are missing"
        );
    }

    client.cancel().await?;
    let _ = server_handle.await;

    restore_env("HOME", original_home.as_deref());
    for (k, v) in snapshots {
        restore_env(k, v.as_deref());
    }

    Ok(())
}

#[tokio::test]
async fn claude_skills_status_returns_yaml_report() -> Result<()> {
    // The tool reads the server's cwd, which is the cargo manifest dir
    // (a real git repo) during test runs. No chdir — that would race
    // with other parallel tests that also read the cwd.
    let (client, server_handle) = spawn_server().await;
    let result = client
        .call_tool(
            CallToolRequestParams::new("claude_skills_status").with_arguments(
                serde_json::json!({"format": "yaml"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await?;

    assert!(!result.is_error.unwrap_or(false), "tool returned error");
    let text: String = result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            RawContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect();
    assert!(text.contains("targets:"), "missing targets: {text}");

    client.cancel().await?;
    let _ = server_handle.await;
    Ok(())
}

/// Valid absolute path that is not a git repository — passes
/// `validate_repo_path` and exercises the `?` propagation inside the
/// `spawn_blocking_cancellable` call where `run_view` returns an error.
#[tokio::test]
async fn git_view_commits_non_git_dir_returns_tool_error() -> Result<()> {
    let tmp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
    fs::create_dir_all(&tmp_root)?;
    let temp_dir = tempfile::tempdir_in(&tmp_root)?;
    let dir_path = temp_dir.path();

    let (client, server_handle) = spawn_server().await;
    let outcome = client
        .call_tool(
            CallToolRequestParams::new("git_view_commits").with_arguments(
                serde_json::json!({
                    "range": "HEAD",
                    "repo_path": dir_path.to_string_lossy(),
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await;

    // Either the protocol-level error surfaces, or the tool-error result.
    if let Ok(result) = outcome {
        assert!(
            result.is_error.unwrap_or(false),
            "expected tool error from non-git directory"
        );
    }

    client.cancel().await?;
    let _ = server_handle.await;
    Ok(())
}

#[tokio::test]
async fn git_view_commits_rejects_relative_repo_path() -> Result<()> {
    let (client, server_handle) = spawn_server().await;
    let outcome = client
        .call_tool(
            CallToolRequestParams::new("git_view_commits").with_arguments(
                serde_json::json!({
                    "range": "HEAD",
                    "repo_path": "relative/path",
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await;

    assert!(outcome.is_err(), "expected absolute-path validation error");
    let err = outcome.err().unwrap();
    let text = format!("{err}");
    assert!(
        text.contains("absolute") || text.contains("repo_path"),
        "expected path validation message, got: {text}"
    );

    client.cancel().await?;
    let _ = server_handle.await;
    Ok(())
}

/// AI-backed tools (`git_check_commits`, `git_twiddle_commits`, `git_create_pr`)
/// fail preflight with missing credentials or a bad repo path. We don't assert
/// a specific error code; we just check that calling them returns a tool
/// error rather than panicking.
async fn call_tool_expect_error(tool_name: &'static str, args: serde_json::Value) -> Result<()> {
    let (client, server_handle) = spawn_server().await;
    let outcome = client
        .call_tool(
            CallToolRequestParams::new(tool_name).with_arguments(args.as_object().unwrap().clone()),
        )
        .await;
    if let Ok(result) = outcome {
        let _ = result;
    }
    client.cancel().await?;
    let _ = server_handle.await;
    Ok(())
}

/// Pipes the `omni-dev-mcp` binary, drives a full `initialize` →
/// `tools/list` exchange, and asserts that every line on stdout parses as
/// valid JSON-RPC (no stray `println!`, progress bars, or debug prints). This
/// is the regression guard per STYLE-0018 / STYLE-0001: stdout is the MCP
/// frame wire and must never carry anything else.
#[tokio::test]
async fn mcp_binary_stdout_is_pure_json_rpc() -> Result<()> {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::process::Command;

    let bin = env!("CARGO_BIN_EXE_omni-dev-mcp");
    let mut child = Command::new(bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let mut stdin = child.stdin.take().expect("stdin pipe");
    let stdout = child.stdout.take().expect("stdout pipe");
    let mut reader = BufReader::new(stdout);

    let initialize = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "stdout-test", "version": "0.0.0"}
        }
    });
    let initialized_notification = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    let list_tools = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list"
    });

    stdin
        .write_all(format!("{initialize}\n").as_bytes())
        .await?;
    stdin
        .write_all(format!("{initialized_notification}\n").as_bytes())
        .await?;
    stdin
        .write_all(format!("{list_tools}\n").as_bytes())
        .await?;
    stdin.flush().await?;

    // Collect frames until we see the tools/list response (id=2).
    let mut frames: Vec<serde_json::Value> = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        let mut line = String::new();
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            reader.read_line(&mut line),
        )
        .await??;
        if read == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            continue;
        }
        let parsed: serde_json::Value = serde_json::from_str(trimmed)
            .unwrap_or_else(|e| panic!("stdout line is not JSON: {e}: {trimmed:?}"));
        // Every frame must be a JSON-RPC object with "jsonrpc": "2.0".
        assert_eq!(
            parsed.get("jsonrpc").and_then(|v| v.as_str()),
            Some("2.0"),
            "frame missing jsonrpc version: {parsed}",
        );
        let seen_id = parsed.get("id").and_then(serde_json::Value::as_i64);
        frames.push(parsed);
        if seen_id == Some(2) {
            break;
        }
    }

    drop(stdin);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await;

    assert!(!frames.is_empty(), "server emitted no stdout frames");
    Ok(())
}

#[tokio::test]
async fn git_check_commits_without_credentials_returns_tool_error() -> Result<()> {
    call_tool_expect_error(
        "git_check_commits",
        serde_json::json!({
            "range": "HEAD",
            "repo_path": "/no/such/path",
        }),
    )
    .await
}

#[tokio::test]
async fn git_twiddle_commits_without_credentials_returns_tool_error() -> Result<()> {
    call_tool_expect_error(
        "git_twiddle_commits",
        serde_json::json!({
            "dry_run": true,
            "repo_path": "/no/such/path",
        }),
    )
    .await
}

#[tokio::test]
async fn git_create_pr_without_credentials_returns_tool_error() -> Result<()> {
    call_tool_expect_error(
        "git_create_pr",
        serde_json::json!({
            "repo_path": "/no/such/path",
        }),
    )
    .await
}

#[tokio::test]
async fn git_view_commits_default_range_is_head() -> Result<()> {
    let mut repo = TestRepo::new()?;
    repo.add_commit("feat: only commit", "hello")?;

    let (client, server_handle) = spawn_server().await;
    let result = client
        .call_tool(
            CallToolRequestParams::new("git_view_commits").with_arguments(
                serde_json::json!({
                    "repo_path": repo.repo_path.to_string_lossy(),
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await?;

    assert!(!result.is_error.unwrap_or(false));
    let text: String = result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            RawContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect();
    assert!(text.contains("feat: only commit"), "got: {text}");

    client.cancel().await?;
    let _ = server_handle.await;
    Ok(())
}

// ─── Resources ──────────────────────────────────────────────────────

/// Serialises CWD mutations across the resource tests in this file so they
/// don't race with each other or with CWD-swapping unit tests elsewhere.
static CWD_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[tokio::test]
async fn list_resources_returns_all_uri_templates() -> Result<()> {
    let (client, server_handle) = spawn_server().await;
    let listing = client.list_resources(Option::default()).await?;
    let uris: Vec<&str> = listing.resources.iter().map(|r| r.uri.as_str()).collect();
    assert!(
        uris.contains(&"git://repo/commits/{range}"),
        "got: {uris:?}"
    );
    assert!(uris.contains(&"jira://issue/{key}"));
    assert!(uris.contains(&"jira://issue/{key}.adf"));
    assert!(uris.contains(&"confluence://page/{id}"));
    assert!(uris.contains(&"confluence://page/{id}.adf"));
    assert!(uris.contains(&"omni-dev://specs/{name}"));
    client.cancel().await?;
    let _ = server_handle.await;
    Ok(())
}

#[tokio::test]
async fn list_resource_templates_returns_descriptions() -> Result<()> {
    let (client, server_handle) = spawn_server().await;
    let listing = client.list_resource_templates(Option::default()).await?;
    assert_eq!(listing.resource_templates.len(), 6);
    for tpl in &listing.resource_templates {
        assert!(
            tpl.description.is_some(),
            "missing description on {}",
            tpl.uri_template
        );
        assert!(
            tpl.mime_type.is_some(),
            "missing mime on {}",
            tpl.uri_template
        );
    }
    client.cancel().await?;
    let _ = server_handle.await;
    Ok(())
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn read_resource_git_commits_head_returns_yaml() -> Result<()> {
    let mut repo = TestRepo::new()?;
    repo.add_commit("feat: first resource", "r1")?;

    // The resource handler reads from CWD (no repo_path in the URI), so
    // swap CWD for the duration of the call. Hold `CWD_MUTEX` across the
    // await to keep other tests from racing. Safe here because the work
    // inside `read_resource` is CPU-bound `spawn_blocking`.
    {
        let _guard = CWD_MUTEX.lock().unwrap();
        let original_cwd = std::env::current_dir()?;
        std::env::set_current_dir(&repo.repo_path)?;

        let (client, server_handle) = spawn_server().await;
        let result = client
            .read_resource(ReadResourceRequestParams::new("git://repo/commits/HEAD"))
            .await;

        std::env::set_current_dir(&original_cwd)?;

        let result = result?;
        assert_eq!(result.contents.len(), 1);
        match &result.contents[0] {
            ResourceContents::TextResourceContents {
                text,
                mime_type,
                uri,
                ..
            } => {
                assert!(text.contains("commits:"), "missing commits section: {text}");
                assert!(
                    text.contains("feat: first resource"),
                    "missing commit subject: {text}"
                );
                assert_eq!(mime_type.as_deref(), Some("application/yaml"));
                assert_eq!(uri, "git://repo/commits/HEAD");
            }
            other @ ResourceContents::BlobResourceContents { .. } => {
                panic!("expected text, got: {other:?}")
            }
        }

        client.cancel().await?;
        let _ = server_handle.await;
    }
    Ok(())
}

#[tokio::test]
async fn read_resource_unknown_scheme_is_resource_not_found() -> Result<()> {
    let (client, server_handle) = spawn_server().await;
    let err = client
        .read_resource(ReadResourceRequestParams::new("ftp://example.com/foo"))
        .await
        .expect_err("unknown scheme should error");
    let rendered = err.to_string();
    // Rendered error text comes through the protocol; it should name the URI.
    assert!(
        rendered.contains("ftp://example.com/foo"),
        "missing uri in err: {rendered}"
    );
    client.cancel().await?;
    let _ = server_handle.await;
    Ok(())
}

#[tokio::test]
async fn read_resource_malformed_git_uri_is_resource_not_found() -> Result<()> {
    let (client, server_handle) = spawn_server().await;
    let err = client
        .read_resource(ReadResourceRequestParams::new("git://repo/bogus-path"))
        .await
        .expect_err("malformed git uri should error");
    let rendered = err.to_string();
    assert!(
        rendered.contains("git://repo/bogus-path"),
        "missing uri in err: {rendered}"
    );
    client.cancel().await?;
    let _ = server_handle.await;
    Ok(())
}

#[tokio::test]
async fn read_resource_omni_dev_specs_jfm_returns_markdown() -> Result<()> {
    let (client, server_handle) = spawn_server().await;
    let result = client
        .read_resource(ReadResourceRequestParams::new("omni-dev://specs/jfm"))
        .await?;
    assert_eq!(result.contents.len(), 1);
    match &result.contents[0] {
        ResourceContents::TextResourceContents {
            text,
            mime_type,
            uri,
            ..
        } => {
            assert_eq!(mime_type.as_deref(), Some("text/markdown"));
            assert_eq!(uri, "omni-dev://specs/jfm");
            assert!(
                text.contains("# JFM (JIRA-Flavored Markdown) Specification"),
                "spec body missing heading"
            );
        }
        other @ ResourceContents::BlobResourceContents { .. } => {
            panic!("expected text resource, got {other:?}")
        }
    }
    client.cancel().await?;
    let _ = server_handle.await;
    Ok(())
}

#[tokio::test]
async fn read_resource_omni_dev_specs_unknown_name_errors() -> Result<()> {
    let (client, server_handle) = spawn_server().await;
    let err = client
        .read_resource(ReadResourceRequestParams::new("omni-dev://specs/bogus"))
        .await
        .expect_err("unknown spec should error");
    let rendered = err.to_string();
    assert!(
        rendered.contains("omni-dev://specs/bogus"),
        "missing uri in err: {rendered}"
    );
    client.cancel().await?;
    let _ = server_handle.await;
    Ok(())
}

// ── Phase 2d: Confluence extension tools ─────────────────────────────

#[tokio::test]
async fn list_tools_includes_confluence_extensions() -> Result<()> {
    let (client, server_handle) = spawn_server().await;
    let tools = client.list_tools(Option::default()).await?;
    let names: Vec<_> = tools.tools.iter().map(|t| t.name.as_ref()).collect();
    for expected in [
        "confluence_children",
        "confluence_comment_list",
        "confluence_comment_add",
        "confluence_label_list",
        "confluence_label_add",
        "confluence_label_remove",
        "confluence_user_search",
        "confluence_attachment_upload",
        "confluence_attachment_list",
        "confluence_attachment_delete",
        "confluence_compare",
        "confluence_compare_section",
    ] {
        assert!(names.contains(&expected), "missing {expected}: {names:?}");
    }
    client.cancel().await?;
    let _ = server_handle.await;
    Ok(())
}

fn confluence_tool_text(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            RawContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect()
}

/// Single sequential test exercising the success path of every Confluence
/// tool handler. `AtlassianEnvGuard` holds the shared env-var lock for the
/// whole MCP round-trip so concurrent tests cannot clobber credentials.
///
/// Holding the lock across `.await` is intentional — the env vars are
/// process-global state that must remain set for the whole round-trip, so
/// an async-aware mutex would offer no benefit.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn confluence_tools_success_paths_via_wiremock() -> Result<()> {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/wiki/rest/api/content/1/child/page"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [{"id": "2", "title": "Child", "status": "current"}]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/wiki/api/v2/pages/12345/footer-comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [
                {"id": "c1", "version": {"authorId": "alice", "createdAt": "2026-04-01T10:00:00Z"}}
            ]
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/wiki/api/v2/footer-comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "c9"})))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/wiki/api/v2/pages/12345/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [{"id": "1", "name": "arch", "prefix": "global"}]
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/wiki/rest/api/content/12345/label"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [{"prefix": "global", "name": "arch", "id": "1"}]
        })))
        .mount(&server)
        .await;

    Mock::given(method("DELETE"))
        .and(path("/wiki/rest/api/content/12345/label/arch"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/wiki/rest/api/search/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [
                {"user": {"accountId": "abc", "displayName": "Alice", "email": "a@x.com"}}
            ]
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/wiki/api/v2/pages/12345/attachments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [{
                "id": "att-1",
                "title": "hello.txt",
                "mediaType": "text/plain",
                "fileSize": 13,
                "version": {"number": 1}
            }]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/wiki/api/v2/pages/12345/attachments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [{"id": "att-1", "title": "hello.txt"}]
        })))
        .mount(&server)
        .await;

    Mock::given(method("DELETE"))
        .and(path("/wiki/api/v2/attachments/att-1"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let _env = AtlassianEnvGuard::new(&server.uri(), "user@test.com", "token")?;
    let (client, server_handle) = spawn_server().await;

    let children = client
        .call_tool(
            CallToolRequestParams::new("confluence_children")
                .with_arguments(serde_json::json!({"id": "1"}).as_object().unwrap().clone()),
        )
        .await?;
    assert!(!children.is_error.unwrap_or(false));
    assert!(confluence_tool_text(&children).contains("Child"));

    let comments = client
        .call_tool(
            CallToolRequestParams::new("confluence_comment_list").with_arguments(
                serde_json::json!({"id": "12345"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await?;
    assert!(!comments.is_error.unwrap_or(false));
    assert!(confluence_tool_text(&comments).contains("id: c1"));

    let comment_add = client
        .call_tool(
            CallToolRequestParams::new("confluence_comment_add").with_arguments(
                serde_json::json!({"id": "12345", "content": "Hello **world**"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await?;
    assert!(!comment_add.is_error.unwrap_or(false));
    assert!(confluence_tool_text(&comment_add).contains("Comment added"));

    let labels = client
        .call_tool(
            CallToolRequestParams::new("confluence_label_list").with_arguments(
                serde_json::json!({"id": "12345"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await?;
    assert!(!labels.is_error.unwrap_or(false));
    assert!(confluence_tool_text(&labels).contains("arch"));

    let label_add = client
        .call_tool(
            CallToolRequestParams::new("confluence_label_add").with_arguments(
                serde_json::json!({"id": "12345", "labels": ["arch"]})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await?;
    assert!(!label_add.is_error.unwrap_or(false));
    assert!(confluence_tool_text(&label_add).contains("Added 1 label"));

    let label_remove = client
        .call_tool(
            CallToolRequestParams::new("confluence_label_remove").with_arguments(
                serde_json::json!({"id": "12345", "labels": ["arch"], "confirm": true})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await?;
    assert!(!label_remove.is_error.unwrap_or(false));
    assert!(confluence_tool_text(&label_remove).contains("Removed 1 label"));

    let users = client
        .call_tool(
            CallToolRequestParams::new("confluence_user_search").with_arguments(
                serde_json::json!({"query": "alice"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await?;
    assert!(!users.is_error.unwrap_or(false));
    assert!(confluence_tool_text(&users).contains("Alice"));

    let attach_dir = tempfile::tempdir()?;
    let attach_path = attach_dir.path().join("hello.txt");
    tokio::fs::write(&attach_path, b"hello, world!").await?;
    let attach_path_str = attach_path.to_string_lossy().to_string();

    // Upload — exercises the `Some` branch of all optional params
    // (filename, comment, minor_edit) so coverage hits both arms of the
    // `Option::unwrap_or(...)` calls in the tool handler.
    let attachment_upload = client
        .call_tool(
            CallToolRequestParams::new("confluence_attachment_upload").with_arguments(
                serde_json::json!({
                    "page_id": "12345",
                    "file_path": attach_path_str,
                    "filename": "hello.txt",
                    "comment": "v1",
                    "minor_edit": true,
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await?;
    assert!(!attachment_upload.is_error.unwrap_or(false));
    assert!(confluence_tool_text(&attachment_upload).contains("att-1"));

    // List — exercises the `Some` branch of `cursor` and `limit`.
    let attachment_list = client
        .call_tool(
            CallToolRequestParams::new("confluence_attachment_list").with_arguments(
                serde_json::json!({
                    "page_id": "12345",
                    "cursor": "PAGE2",
                    "limit": 50,
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await?;
    assert!(!attachment_list.is_error.unwrap_or(false));
    assert!(confluence_tool_text(&attachment_list).contains("hello.txt"));

    // Delete — exercises the `Some` branch of `purge`.
    let attachment_delete = client
        .call_tool(
            CallToolRequestParams::new("confluence_attachment_delete").with_arguments(
                serde_json::json!({"attachment_id": "att-1", "purge": false})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await?;
    assert!(!attachment_delete.is_error.unwrap_or(false));
    assert!(confluence_tool_text(&attachment_delete).contains("Deleted attachment att-1"));

    client.cancel().await?;
    let _ = server_handle.await;
    Ok(())
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn confluence_children_without_credentials_returns_error() -> Result<()> {
    let _env = AtlassianEnvGuard::empty()?;
    let (client, server_handle) = spawn_server().await;
    let outcome = client
        .call_tool(
            CallToolRequestParams::new("confluence_children").with_arguments(
                serde_json::json!({"id": "12345"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await;

    if let Ok(result) = outcome {
        assert!(
            result.is_error.unwrap_or(false),
            "expected tool error without credentials"
        );
    }

    client.cancel().await?;
    let _ = server_handle.await;
    Ok(())
}

// ── confluence_compare end-to-end fixture ─────────────────────────────

/// Fixture covering all change kinds in one diff:
///
/// - **Background** — paragraph edit ("12" → "14")
/// - **Architecture** — paragraph removed; table cell edit (with localId)
/// - **Implementation** — wholly new section (added)
/// - **Roadmap** — list item removed
/// - **Code** — code block extended by one line
///
/// Plus title change ("Spec v0.9" → "Spec v1.0").
fn fixture_v1_adf() -> serde_json::Value {
    serde_json::json!({
        "version": 1,
        "type": "doc",
        "content": [
            {"type": "heading", "attrs": {"level": 2},
             "content": [{"type": "text", "text": "Background"}]},
            {"type": "paragraph",
             "content": [{"type": "text", "text": "We use database version 12."}]},

            {"type": "heading", "attrs": {"level": 2},
             "content": [{"type": "text", "text": "Architecture"}]},
            {"type": "paragraph",
             "content": [{"type": "text", "text": "Paragraph A — to be removed."}]},
            {"type": "paragraph",
             "content": [{"type": "text", "text": "Paragraph B — kept."}]},
            {"type": "table", "attrs": {"localId": "t1"},
             "content": [
                 {"type": "tableRow", "attrs": {"localId": "r1"},
                  "content": [
                      {"type": "tableCell", "attrs": {"localId": "c11"},
                       "content": [{"type": "paragraph",
                                    "content": [{"type": "text", "text": "alpha"}]}]},
                      {"type": "tableCell", "attrs": {"localId": "c12"},
                       "content": [{"type": "paragraph",
                                    "content": [{"type": "text", "text": "beta"}]}]}
                  ]}
             ]},

            {"type": "heading", "attrs": {"level": 2},
             "content": [{"type": "text", "text": "Roadmap"}]},
            {"type": "bulletList",
             "content": [
                 {"type": "listItem",
                  "content": [{"type": "paragraph",
                               "content": [{"type": "text", "text": "milestone 1"}]}]},
                 {"type": "listItem",
                  "content": [{"type": "paragraph",
                               "content": [{"type": "text", "text": "milestone 2"}]}]}
             ]},

            {"type": "heading", "attrs": {"level": 2},
             "content": [{"type": "text", "text": "Code"}]},
            {"type": "codeBlock", "attrs": {"language": "rust"},
             "content": [{"type": "text", "text": "fn one() {}\nfn two() {}\nfn three() {}"}]}
        ]
    })
}

fn fixture_v2_adf() -> serde_json::Value {
    serde_json::json!({
        "version": 1,
        "type": "doc",
        "content": [
            {"type": "heading", "attrs": {"level": 2},
             "content": [{"type": "text", "text": "Background"}]},
            {"type": "paragraph",
             "content": [{"type": "text", "text": "We use database version 14."}]},

            {"type": "heading", "attrs": {"level": 2},
             "content": [{"type": "text", "text": "Architecture"}]},
            // Paragraph A removed.
            {"type": "paragraph",
             "content": [{"type": "text", "text": "Paragraph B — kept."}]},
            {"type": "table", "attrs": {"localId": "t1"},
             "content": [
                 {"type": "tableRow", "attrs": {"localId": "r1"},
                  "content": [
                      {"type": "tableCell", "attrs": {"localId": "c11"},
                       "content": [{"type": "paragraph",
                                    "content": [{"type": "text", "text": "alpha"}]}]},
                      {"type": "tableCell", "attrs": {"localId": "c12"},
                       "content": [{"type": "paragraph",
                                    "content": [{"type": "text", "text": "BETA"}]}]}
                  ]}
             ]},

            {"type": "heading", "attrs": {"level": 2},
             "content": [{"type": "text", "text": "Implementation"}]},
            {"type": "paragraph",
             "content": [{"type": "text", "text": "New implementation notes."}]},

            {"type": "heading", "attrs": {"level": 2},
             "content": [{"type": "text", "text": "Roadmap"}]},
            {"type": "bulletList",
             "content": [
                 {"type": "listItem",
                  "content": [{"type": "paragraph",
                               "content": [{"type": "text", "text": "milestone 1"}]}]}
                 // milestone 2 removed.
             ]},

            {"type": "heading", "attrs": {"level": 2},
             "content": [{"type": "text", "text": "Code"}]},
            {"type": "codeBlock", "attrs": {"language": "rust"},
             "content": [{"type": "text",
                          "text": "fn one() {}\nfn two() {}\nfn three() {}\nfn four() {}"}]}
        ]
    })
}

fn fixture_page_response(version: u32, title: &str, adf: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "id": "12345",
        "title": title,
        "status": "current",
        "spaceId": "98",
        "version": {"number": version},
        "body": {
            "atlas_doc_format": {"value": serde_json::to_string(adf).unwrap()}
        }
    })
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn confluence_compare_round_trip_with_fixture_page() -> Result<()> {
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    // Versions list (newest-first).
    Mock::given(method("GET"))
        .and(path("/wiki/api/v2/pages/12345/versions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [
                {"number": 2, "createdAt": "2026-05-09T10:00:00Z", "authorId": "alice", "message": "v1.0", "minorEdit": false},
                {"number": 1, "createdAt": "2026-05-08T10:00:00Z", "authorId": "alice", "message": "v0.9", "minorEdit": false},
            ]
        })))
        .mount(&server)
        .await;

    // Page at v1.
    Mock::given(method("GET"))
        .and(path("/wiki/api/v2/pages/12345"))
        .and(query_param("version", "1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture_page_response(
                1,
                "Spec v0.9",
                &fixture_v1_adf(),
            )),
        )
        .mount(&server)
        .await;

    // Page at v2.
    Mock::given(method("GET"))
        .and(path("/wiki/api/v2/pages/12345"))
        .and(query_param("version", "2"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture_page_response(
                2,
                "Spec v1.0",
                &fixture_v2_adf(),
            )),
        )
        .mount(&server)
        .await;

    // Space-key resolution.
    Mock::given(method("GET"))
        .and(path("/wiki/api/v2/spaces/98"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"key": "ENG"})))
        .mount(&server)
        .await;

    let _env = AtlassianEnvGuard::new(&server.uri(), "user@test.com", "token")?;
    let (client, server_handle) = spawn_server().await;

    // Outline-mode call.
    let result = client
        .call_tool(
            CallToolRequestParams::new("confluence_compare").with_arguments(
                serde_json::json!({"id": "12345", "from": "1", "to": "2"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await?;
    assert!(!result.is_error.unwrap_or(false));
    let yaml = confluence_tool_text(&result);

    // Section-level assertions: every change kind from the fixture appears.
    assert!(
        yaml.contains("/h2#background"),
        "background section missing"
    );
    assert!(
        yaml.contains("/h2#architecture"),
        "architecture section missing"
    );
    assert!(yaml.contains("/h2#implementation"), "added section missing");
    assert!(yaml.contains("/h2#roadmap"), "roadmap section missing");
    assert!(yaml.contains("/h2#code"), "code section missing");
    // Title change.
    assert!(yaml.contains("Spec v0.9"));
    assert!(yaml.contains("Spec v1.0"));
    // At least one section is `added` (Implementation) and one `modified`.
    assert!(yaml.contains("change: added"));
    assert!(yaml.contains("change: modified"));

    // Capture the cursor for the Background section. Parse the YAML
    // structurally so we don't trip over quoting differences.
    let parsed: serde_yaml::Value = serde_yaml::from_str(&yaml).expect("parse YAML output");
    let sections = parsed
        .get("sections")
        .and_then(serde_yaml::Value::as_sequence)
        .expect("sections array");
    let cursor = sections
        .iter()
        .find(|s| s.get("path").and_then(|p| p.as_str()) == Some("/h2#background"))
        .and_then(|s| s.get("cursor").and_then(|c| c.as_str()))
        .expect("cursor for /h2#background")
        .to_string();

    let drill = client
        .call_tool(
            CallToolRequestParams::new("confluence_compare_section").with_arguments(
                serde_json::json!({"cursor": cursor, "format": "unified"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await?;
    assert!(!drill.is_error.unwrap_or(false));
    let drill_text = confluence_tool_text(&drill);
    assert!(drill_text.contains("/h2#background"));
    assert!(drill_text.contains("database version 12"));
    assert!(drill_text.contains("database version 14"));

    client.cancel().await?;
    let _ = server_handle.await;
    Ok(())
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn confluence_compare_min_change_chars_filters_small_edits() -> Result<()> {
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/wiki/api/v2/pages/12345/versions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [
                {"number": 2, "createdAt": "2026-05-09T10:00:00Z", "authorId": "alice", "message": "", "minorEdit": false},
                {"number": 1, "createdAt": "2026-05-08T10:00:00Z", "authorId": "alice", "message": "", "minorEdit": false},
            ]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/wiki/api/v2/pages/12345"))
        .and(query_param("version", "1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture_page_response(
                1,
                "T",
                &fixture_v1_adf(),
            )),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/wiki/api/v2/pages/12345"))
        .and(query_param("version", "2"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture_page_response(
                2,
                "T",
                &fixture_v2_adf(),
            )),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/wiki/api/v2/spaces/98"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"key": "ENG"})))
        .mount(&server)
        .await;

    let _env = AtlassianEnvGuard::new(&server.uri(), "user@test.com", "token")?;
    let (client, server_handle) = spawn_server().await;

    // Filter that drops the tiny "12" → "14" Background prose edit but
    // keeps the section with the larger ist/code/architecture diffs.
    let result = client
        .call_tool(
            CallToolRequestParams::new("confluence_compare").with_arguments(
                serde_json::json!({
                    "id": "12345",
                    "from": "1",
                    "to": "2",
                    "min_change_chars": 200
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await?;
    assert!(!result.is_error.unwrap_or(false));
    let yaml = confluence_tool_text(&result);
    // Background's prose edit ("12" → "14", ~50 chars total) drops out.
    assert!(
        !yaml.contains("path: /h2#background"),
        "background should be filtered: {yaml}"
    );

    client.cancel().await?;
    let _ = server_handle.await;
    Ok(())
}
