//! CLI commands for managing Confluence page watchers.
//!
//! Confluence's public API exposes per-user watch state (check / add / remove),
//! not a list of every watcher on a page (that requires admin and is not part
//! of the stable REST surface), so — unlike `jira watcher` — this offers
//! `status` rather than `list`.

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::atlassian::confluence_api::ConfluenceApi;
use crate::cli::atlassian::helpers::create_client;

/// Manages watchers on a Confluence page.
#[derive(Parser)]
pub struct WatcherCommand {
    /// The watcher subcommand to execute.
    #[command(subcommand)]
    pub command: WatcherSubcommands,
}

/// Watcher subcommands.
#[derive(Subcommand)]
pub enum WatcherSubcommands {
    /// Reports whether a user watches a page (mirrors the `confluence_watcher_status` MCP tool).
    Status(WatchArgs),
    /// Adds a watcher to a page (mirrors the `confluence_watcher_add` MCP tool).
    Add(WatchArgs),
    /// Removes a watcher from a page (mirrors the `confluence_watcher_remove` MCP tool).
    Remove(WatchArgs),
}

impl WatcherCommand {
    /// Executes the watcher command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            WatcherSubcommands::Status(args) => args.run_status().await,
            WatcherSubcommands::Add(args) => args.run_add().await,
            WatcherSubcommands::Remove(args) => args.run_remove().await,
        }
    }
}

/// Shared arguments for the watcher subcommands.
#[derive(Parser)]
pub struct WatchArgs {
    /// Confluence page (content) ID.
    pub id: String,

    /// Atlassian `accountId` of the user. Defaults to the authenticated user.
    #[arg(long)]
    pub account_id: Option<String>,
}

impl WatchArgs {
    async fn run_status(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);
        let status = api
            .is_watching_content(&self.id, self.account_id.as_deref())
            .await?;
        let who = self.account_id.as_deref().unwrap_or("you");
        println!(
            "{who} {} watching page {}.",
            if status.watching {
                "are/is"
            } else {
                "are/is NOT"
            },
            self.id
        );
        Ok(())
    }

    async fn run_add(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);
        api.add_content_watcher(&self.id, self.account_id.as_deref())
            .await?;
        let who = self.account_id.as_deref().unwrap_or("you");
        println!("Added watcher {who} to page {}.", self.id);
        Ok(())
    }

    async fn run_remove(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);
        api.remove_content_watcher(&self.id, self.account_id.as_deref())
            .await?;
        let who = self.account_id.as_deref().unwrap_or("you");
        println!("Removed watcher {who} from page {}.", self.id);
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn watcher_command_status_variant() {
        let cmd = WatcherCommand {
            command: WatcherSubcommands::Status(WatchArgs {
                id: "12345".to_string(),
                account_id: None,
            }),
        };
        assert!(matches!(cmd.command, WatcherSubcommands::Status(_)));
    }

    #[test]
    fn watcher_command_add_variant() {
        let cmd = WatcherCommand {
            command: WatcherSubcommands::Add(WatchArgs {
                id: "12345".to_string(),
                account_id: Some("acc-1".to_string()),
            }),
        };
        assert!(matches!(cmd.command, WatcherSubcommands::Add(_)));
    }

    #[test]
    fn watcher_command_remove_variant() {
        let cmd = WatcherCommand {
            command: WatcherSubcommands::Remove(WatchArgs {
                id: "12345".to_string(),
                account_id: None,
            }),
        };
        assert!(matches!(cmd.command, WatcherSubcommands::Remove(_)));
    }

    // ── execute() end-to-end (drives create_client + the API call) ──

    fn watch_mock(method: &str) -> wiremock::Mock {
        wiremock::Mock::given(wiremock::matchers::method(method))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/user/watch/content/12345",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(1)
    }

    #[tokio::test]
    async fn status_execute_reports_watching() {
        use crate::test_support::atlassian_env::AtlassianEnvGuard;
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/user/watch/content/12345",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"watching": true})),
            )
            .mount(&server)
            .await;
        let _env = AtlassianEnvGuard::new(&server.uri(), "u@t.com", "tok");
        WatchArgs {
            id: "12345".to_string(),
            account_id: None,
        }
        .run_status()
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn status_execute_reports_not_watching_for_account() {
        use crate::test_support::atlassian_env::AtlassianEnvGuard;
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/user/watch/content/12345",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"watching": false})),
            )
            .mount(&server)
            .await;
        let _env = AtlassianEnvGuard::new(&server.uri(), "u@t.com", "tok");
        WatchArgs {
            id: "12345".to_string(),
            account_id: Some("acc-1".to_string()),
        }
        .run_status()
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn add_execute_posts_watcher() {
        use crate::test_support::atlassian_env::AtlassianEnvGuard;
        let server = wiremock::MockServer::start().await;
        watch_mock("POST").mount(&server).await;
        let _env = AtlassianEnvGuard::new(&server.uri(), "u@t.com", "tok");
        // Routed through the parent so the `Add` dispatch arm is covered too.
        WatcherCommand {
            command: WatcherSubcommands::Add(WatchArgs {
                id: "12345".to_string(),
                account_id: Some("acc-1".to_string()),
            }),
        }
        .execute()
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn remove_execute_deletes_watcher() {
        use crate::test_support::atlassian_env::AtlassianEnvGuard;
        let server = wiremock::MockServer::start().await;
        watch_mock("DELETE").mount(&server).await;
        let _env = AtlassianEnvGuard::new(&server.uri(), "u@t.com", "tok");
        // Routed through the parent so the `Remove` dispatch arm is covered too.
        WatcherCommand {
            command: WatcherSubcommands::Remove(WatchArgs {
                id: "12345".to_string(),
                account_id: None,
            }),
        }
        .execute()
        .await
        .unwrap();
    }
}
