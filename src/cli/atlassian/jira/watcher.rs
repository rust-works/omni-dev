//! CLI commands for JIRA issue watchers.

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::atlassian::client::{AtlassianClient, JiraWatcherList};
use crate::cli::atlassian::format::{output_as, OutputFormat};
use crate::cli::atlassian::helpers::create_client;

/// Manages watchers on a JIRA issue.
#[derive(Parser)]
pub struct WatcherCommand {
    /// The watcher subcommand to execute.
    #[command(subcommand)]
    pub command: WatcherSubcommands,
}

/// Watcher subcommands.
#[derive(Subcommand)]
pub enum WatcherSubcommands {
    /// Lists current watchers on an issue.
    List(ListCommand),
    /// Adds a user as a watcher on an issue.
    Add(AddCommand),
    /// Removes a user from watchers on an issue.
    Remove(RemoveCommand),
}

impl WatcherCommand {
    /// Executes the watcher command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            WatcherSubcommands::List(cmd) => cmd.execute().await,
            WatcherSubcommands::Add(cmd) => cmd.execute().await,
            WatcherSubcommands::Remove(cmd) => cmd.execute().await,
        }
    }
}

/// Lists current watchers on an issue.
#[derive(Parser)]
pub struct ListCommand {
    /// Issue key (e.g., "PROJ-123").
    pub key: String,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ListCommand {
    /// Fetches and displays watchers.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        run_list(&client, &self.key, &self.output).await
    }
}

/// Fetches and displays watchers using the given client.
async fn run_list(client: &AtlassianClient, key: &str, output: &OutputFormat) -> Result<()> {
    let result = client.get_watchers(key).await?;
    if output_as(&result, output)? {
        return Ok(());
    }
    print_watchers(&result);
    Ok(())
}

/// Adds a user as a watcher on an issue.
#[derive(Parser)]
pub struct AddCommand {
    /// Issue key (e.g., "PROJ-123").
    pub key: String,

    /// Account ID of the user to add.
    #[arg(long)]
    pub user: String,
}

impl AddCommand {
    /// Adds the user as a watcher.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        run_add(&client, &self.key, &self.user).await
    }
}

/// Adds a watcher using the given client.
async fn run_add(client: &AtlassianClient, key: &str, user: &str) -> Result<()> {
    client.add_watcher(key, user).await?;
    println!("Added watcher {user} to {key}.");
    Ok(())
}

/// Removes a user from watchers on an issue.
#[derive(Parser)]
pub struct RemoveCommand {
    /// Issue key (e.g., "PROJ-123").
    pub key: String,

    /// Account ID of the user to remove.
    #[arg(long)]
    pub user: String,
}

impl RemoveCommand {
    /// Removes the user from watchers.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        run_remove(&client, &self.key, &self.user).await
    }
}

/// Removes a watcher using the given client.
async fn run_remove(client: &AtlassianClient, key: &str, user: &str) -> Result<()> {
    client.remove_watcher(key, user).await?;
    println!("Removed watcher {user} from {key}.");
    Ok(())
}

/// Prints watchers as a formatted table.
fn print_watchers(result: &JiraWatcherList) {
    if result.watchers.is_empty() {
        println!("No watchers found.");
        return;
    }

    let name_width = result
        .watchers
        .iter()
        .map(|w| w.display_name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let id_width = result
        .watchers
        .iter()
        .map(|w| w.account_id.len())
        .max()
        .unwrap_or(10)
        .max(10);

    println!("{:<name_width$}  {:<id_width$}", "NAME", "ACCOUNT ID");
    println!(
        "{:<name_width$}  {:<id_width$}",
        "-".repeat(name_width),
        "-".repeat(id_width),
    );

    for watcher in &result.watchers {
        println!(
            "{:<name_width$}  {:<id_width$}",
            watcher.display_name, watcher.account_id
        );
    }

    println!("\n{} watcher(s) total.", result.watch_count);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::atlassian::client::JiraUser;

    fn sample_user(name: &str, account_id: &str) -> JiraUser {
        JiraUser {
            display_name: name.to_string(),
            email_address: None,
            account_id: account_id.to_string(),
        }
    }

    fn mock_client(base_url: &str) -> AtlassianClient {
        AtlassianClient::new(base_url, "user@test.com", "token").unwrap()
    }

    // -- print_watchers ------------------------------------------------

    #[test]
    fn print_watchers_empty() {
        let result = JiraWatcherList {
            watchers: vec![],
            watch_count: 0,
        };
        print_watchers(&result);
    }

    #[test]
    fn print_watchers_with_data() {
        let result = JiraWatcherList {
            watchers: vec![sample_user("Alice", "abc123"), sample_user("Bob", "def456")],
            watch_count: 2,
        };
        print_watchers(&result);
    }

    #[test]
    fn print_watchers_count_exceeds_list() {
        let result = JiraWatcherList {
            watchers: vec![sample_user("Alice", "abc123")],
            watch_count: 5,
        };
        print_watchers(&result);
    }

    // -- run_list -------------------------------------------------------

    #[tokio::test]
    async fn run_list_table_output() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/watchers",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "watchCount": 1,
                    "watchers": [{"accountId": "abc123", "displayName": "Alice"}]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let result = run_list(&client, "PROJ-1", &OutputFormat::Table).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn run_list_json_output() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/watchers",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "watchCount": 0,
                    "watchers": []
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let result = run_list(&client, "PROJ-1", &OutputFormat::Json).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn run_list_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/NOPE-1/watchers",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let err = run_list(&client, "NOPE-1", &OutputFormat::Table)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    // -- run_add --------------------------------------------------------

    #[tokio::test]
    async fn run_add_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/watchers",
            ))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let result = run_add(&client, "PROJ-1", "abc123").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn run_add_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/watchers",
            ))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let err = run_add(&client, "PROJ-1", "abc123").await.unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    // -- run_remove -----------------------------------------------------

    #[tokio::test]
    async fn run_remove_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/watchers",
            ))
            .and(wiremock::matchers::query_param("accountId", "abc123"))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let result = run_remove(&client, "PROJ-1", "abc123").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn run_remove_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/watchers",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let err = run_remove(&client, "PROJ-1", "abc123").await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    // -- dispatch -------------------------------------------------------

    #[test]
    fn watcher_command_list_variant() {
        let cmd = WatcherCommand {
            command: WatcherSubcommands::List(ListCommand {
                key: "PROJ-1".to_string(),
                output: OutputFormat::Table,
            }),
        };
        assert!(matches!(cmd.command, WatcherSubcommands::List(_)));
    }

    #[test]
    fn watcher_command_add_variant() {
        let cmd = WatcherCommand {
            command: WatcherSubcommands::Add(AddCommand {
                key: "PROJ-1".to_string(),
                user: "abc123".to_string(),
            }),
        };
        assert!(matches!(cmd.command, WatcherSubcommands::Add(_)));
    }

    #[test]
    fn watcher_command_remove_variant() {
        let cmd = WatcherCommand {
            command: WatcherSubcommands::Remove(RemoveCommand {
                key: "PROJ-1".to_string(),
                user: "abc123".to_string(),
            }),
        };
        assert!(matches!(cmd.command, WatcherSubcommands::Remove(_)));
    }
}
