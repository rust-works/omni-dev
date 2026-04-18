//! Integration tests for the MCP server.
//!
//! These tests spin up `OmniDevServer` on one end of an in-memory duplex
//! transport, connect a generic rmcp client on the other end, and exercise
//! tool dispatch end-to-end.

#![cfg(feature = "mcp")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use git2::{Repository, Signature};
use rmcp::{
    model::{CallToolRequestParams, RawContent},
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
