//! CLI command for moving/reparenting Confluence pages.
//!
//! Same-space only — cross-space moves are not supported by the v2 API.

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};

use crate::atlassian::confluence_api::ConfluenceApi;
use crate::atlassian::confluence_types::{MovePosition as ApiMovePosition, MovedPage};
use crate::cli::atlassian::helpers::create_client;

/// Position for a Confluence page move.
#[derive(Clone, Copy, Debug, Default, ValueEnum, PartialEq, Eq)]
pub enum MovePosition {
    /// Place the page as the last child of the target (target becomes the new parent).
    #[default]
    Append,
    /// Place the page as a sibling immediately before the target.
    Before,
    /// Place the page as a sibling immediately after the target.
    After,
}

impl From<MovePosition> for ApiMovePosition {
    fn from(value: MovePosition) -> Self {
        match value {
            MovePosition::Append => Self::Append,
            MovePosition::Before => Self::Before,
            MovePosition::After => Self::After,
        }
    }
}

/// Moves or reparents a Confluence page within its current space.
///
/// Same-space only — cross-space moves are not supported by the v2 API.
#[derive(Parser)]
pub struct MoveCommand {
    /// Confluence page ID to move.
    pub id: String,

    /// Target page ID — new parent for `append`, or sibling reference for `before`/`after`.
    #[arg(long)]
    pub target: String,

    /// Position relative to the target.
    #[arg(long, value_enum, default_value_t = MovePosition::Append)]
    pub position: MovePosition,
}

impl MoveCommand {
    /// Executes the move command.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);
        run_move(&api, &self.id, &self.target, self.position.into()).await
    }
}

/// Performs the move and prints the resulting [`MovedPage`] as YAML.
async fn run_move(
    api: &ConfluenceApi,
    page_id: &str,
    target_id: &str,
    position: ApiMovePosition,
) -> Result<()> {
    let moved = api.move_page(page_id, target_id, position).await?;
    print_moved_page(&moved)
}

fn print_moved_page(moved: &MovedPage) -> Result<()> {
    let yaml = serde_yaml::to_string(moved).context("Failed to serialize moved page as YAML")?;
    print!("{yaml}");
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::atlassian::client::AtlassianClient;

    // ── MoveCommand struct ─────────────────────────────────────────

    #[test]
    fn move_command_struct_fields() {
        let cmd = MoveCommand {
            id: "12345".to_string(),
            target: "456".to_string(),
            position: MovePosition::Append,
        };
        assert_eq!(cmd.id, "12345");
        assert_eq!(cmd.target, "456");
        assert_eq!(cmd.position, MovePosition::Append);
    }

    #[test]
    fn move_position_default_is_append() {
        assert_eq!(MovePosition::default(), MovePosition::Append);
    }

    #[test]
    fn move_position_from_cli_to_api() {
        assert_eq!(
            ApiMovePosition::from(MovePosition::Append),
            ApiMovePosition::Append
        );
        assert_eq!(
            ApiMovePosition::from(MovePosition::Before),
            ApiMovePosition::Before
        );
        assert_eq!(
            ApiMovePosition::from(MovePosition::After),
            ApiMovePosition::After
        );
    }

    // ── print_moved_page ───────────────────────────────────────────

    #[test]
    fn print_moved_page_emits_valid_yaml() {
        let moved = MovedPage {
            id: "12345".to_string(),
            title: "Moved".to_string(),
            parent_id: Some("456".to_string()),
            ancestors: vec!["10".to_string(), "456".to_string()],
        };
        // Only checks that no error is returned; output goes to stdout.
        assert!(print_moved_page(&moved).is_ok());
    }

    // ── run_move ───────────────────────────────────────────────────

    async fn mock_move_endpoints(server: &wiremock::MockServer, position: &str) {
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path(format!(
                "/wiki/rest/api/content/12345/move/{position}/456"
            )))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"pageId": "12345"})),
            )
            .mount(server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345"))
            .and(wiremock::matchers::query_param("include-ancestors", "true"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "12345",
                    "title": "Moved",
                    "status": "current",
                    "spaceId": "98765",
                    "parentId": "456",
                    "ancestors": [{"id": "10"}, {"id": "456"}]
                })),
            )
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn run_move_append_success() {
        let server = wiremock::MockServer::start().await;
        mock_move_endpoints(&server, "append").await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let result = run_move(&api, "12345", "456", ApiMovePosition::Append).await;
        assert!(result.is_ok(), "got: {result:?}");
    }

    #[tokio::test]
    async fn run_move_before_success() {
        let server = wiremock::MockServer::start().await;
        mock_move_endpoints(&server, "before").await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let result = run_move(&api, "12345", "456", ApiMovePosition::Before).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn run_move_after_success() {
        let server = wiremock::MockServer::start().await;
        mock_move_endpoints(&server, "after").await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let result = run_move(&api, "12345", "456", ApiMovePosition::After).await;
        assert!(result.is_ok());
    }

    /// Drives `MoveCommand::execute` past `create_client()` with injected
    /// fake credentials so the dispatch line runs. The downstream API call
    /// fails (no mock server) and we only care that the entrypoint executes.
    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::await_holding_lock)]
    async fn move_command_execute_dispatch() {
        // Serialise on the one canonical env mutex (issue #950); this mutation
        // was previously unguarded and could clobber concurrent credential tests.
        let _lock = crate::atlassian::auth::test_util::AUTH_ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::set_var("ATLASSIAN_INSTANCE_URL", "http://127.0.0.1:1");
        std::env::set_var("ATLASSIAN_EMAIL", "test@example.com");
        std::env::set_var("ATLASSIAN_API_TOKEN", "fake-token");

        let cmd = MoveCommand {
            id: "12345".to_string(),
            target: "456".to_string(),
            position: MovePosition::Append,
        };
        let _ = cmd.execute().await;

        std::env::remove_var("ATLASSIAN_INSTANCE_URL");
        std::env::remove_var("ATLASSIAN_EMAIL");
        std::env::remove_var("ATLASSIAN_API_TOKEN");
    }

    #[tokio::test]
    async fn run_move_propagates_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/move/append/456",
            ))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let api = ConfluenceApi::new(client);
        let err = run_move(&api, "12345", "456", ApiMovePosition::Append)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("insufficient permissions"));
    }
}
