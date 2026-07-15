//! CLI command for copying a Confluence page.

use anyhow::Result;
use clap::Parser;

use crate::atlassian::confluence_api::ConfluenceApi;
use crate::cli::atlassian::helpers::create_client;

/// Copies a single Confluence page under a destination parent page.
///
/// The copy carries the source page's attachments, labels, and properties (but
/// not its restrictions). Single-page copy only.
#[derive(Parser)]
pub struct CopyCommand {
    /// Source Confluence page ID to copy.
    pub id: String,

    /// Destination parent page ID the copy is placed under.
    #[arg(long)]
    pub parent: String,

    /// Title for the new copied page.
    #[arg(long)]
    pub title: String,
}

impl CopyCommand {
    /// Executes the copy command.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);
        let new_id = api.copy_page(&self.id, &self.parent, &self.title).await?;
        println!(
            "Copied page {} to new page {} ({:?}).",
            self.id, new_id, self.title
        );
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn copy_command_struct_fields() {
        let cmd = CopyCommand {
            id: "12345".to_string(),
            parent: "456".to_string(),
            title: "Copy".to_string(),
        };
        assert_eq!(cmd.id, "12345");
        assert_eq!(cmd.parent, "456");
        assert_eq!(cmd.title, "Copy");
    }

    /// Drives `CopyCommand::execute` past `create_client()` with injected
    /// fake credentials so the dispatch line runs against a mock server.
    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::await_holding_lock)]
    async fn copy_command_execute_dispatch() {
        let _lock = crate::atlassian::auth::test_util::AUTH_ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/copy",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "99999"})),
            )
            .mount(&server)
            .await;

        std::env::set_var("ATLASSIAN_INSTANCE_URL", server.uri());
        std::env::set_var("ATLASSIAN_EMAIL", "test@example.com");
        std::env::set_var("ATLASSIAN_API_TOKEN", "fake-token");

        // Ensure no global --instance override leaks in from another test.
        std::env::remove_var("OMNI_DEV_ATLASSIAN_INSTANCE");

        let cmd = CopyCommand {
            id: "12345".to_string(),
            parent: "456".to_string(),
            title: "Copy".to_string(),
        };
        let result = cmd.execute().await;

        std::env::remove_var("ATLASSIAN_INSTANCE_URL");
        std::env::remove_var("ATLASSIAN_EMAIL");
        std::env::remove_var("ATLASSIAN_API_TOKEN");

        assert!(result.is_ok(), "got: {result:?}");
    }
}
