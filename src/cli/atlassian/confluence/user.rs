//! CLI commands for Confluence user operations.

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::atlassian::client::{AtlassianClient, ConfluenceUserSearchResults};
use crate::cli::atlassian::format::{output_as, OutputFormat};
use crate::cli::atlassian::helpers::create_client;

/// Confluence user operations.
#[derive(Parser)]
pub struct UserCommand {
    /// The user subcommand to execute.
    #[command(subcommand)]
    pub command: UserSubcommands,
}

/// Confluence user subcommands.
#[derive(Subcommand)]
pub enum UserSubcommands {
    /// Searches Confluence users by display name or email.
    Search(UserSearchCommand),
}

impl UserCommand {
    /// Executes the user command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            UserSubcommands::Search(cmd) => cmd.execute().await,
        }
    }
}

/// Searches Confluence users by display name or email.
#[derive(Parser)]
pub struct UserSearchCommand {
    /// Search text (matches display name or email).
    #[arg(long)]
    pub query: String,

    /// Maximum number of results, 0 for unlimited (default: 25).
    #[arg(long, default_value_t = 25)]
    pub limit: u32,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl UserSearchCommand {
    /// Executes the user search and prints results.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        run_search(&client, &self.query, self.limit, &self.output).await
    }
}

/// Fetches and displays Confluence users using the given client.
async fn run_search(
    client: &AtlassianClient,
    query: &str,
    limit: u32,
    output: &OutputFormat,
) -> Result<()> {
    let result = client.search_confluence_users(query, limit).await?;
    if output_as(&result, output)? {
        return Ok(());
    }
    print_user_results(&result);
    Ok(())
}

/// Prints user search results as a formatted table.
fn print_user_results(result: &ConfluenceUserSearchResults) {
    if result.users.is_empty() {
        println!("No users found.");
        return;
    }

    // Calculate column widths
    let id_width = result
        .users
        .iter()
        .map(|u| u.account_id.len())
        .max()
        .unwrap_or(10)
        .max(10);
    let name_width = result
        .users
        .iter()
        .map(|u| u.display_name.len())
        .max()
        .unwrap_or(4)
        .max(4);

    // Header
    let email_sep = "-".repeat(5);
    println!(
        "{:<id_width$}  {:<name_width$}  EMAIL",
        "ACCOUNT_ID", "NAME"
    );
    println!(
        "{:<id_width$}  {:<name_width$}  {email_sep}",
        "-".repeat(id_width),
        "-".repeat(name_width),
    );

    // Rows
    for user in &result.users {
        let email = user.email.as_deref().unwrap_or("-");
        println!(
            "{:<id_width$}  {:<name_width$}  {email}",
            user.account_id, user.display_name
        );
    }

    // Pagination note
    if result.total > result.users.len() as u32 {
        println!(
            "\nShowing {} of {} results.",
            result.users.len(),
            result.total
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::atlassian::client::ConfluenceUserSearchResult;

    fn mock_client(base_url: &str) -> AtlassianClient {
        AtlassianClient::new(base_url, "user@test.com", "token").unwrap()
    }

    fn sample_user(
        account_id: &str,
        display_name: &str,
        email: Option<&str>,
    ) -> ConfluenceUserSearchResult {
        ConfluenceUserSearchResult {
            account_id: account_id.to_string(),
            display_name: display_name.to_string(),
            email: email.map(String::from),
        }
    }

    // ── print_user_results ────────────────────────────────────────

    #[test]
    fn print_results_empty() {
        let result = ConfluenceUserSearchResults {
            users: vec![],
            total: 0,
        };
        print_user_results(&result);
    }

    #[test]
    fn print_results_with_users() {
        let result = ConfluenceUserSearchResults {
            users: vec![
                sample_user("abc123", "Alice Smith", Some("alice@example.com")),
                sample_user("def456", "Bob Jones", Some("bob@example.com")),
            ],
            total: 2,
        };
        print_user_results(&result);
    }

    #[test]
    fn print_results_with_missing_email() {
        let result = ConfluenceUserSearchResults {
            users: vec![sample_user("abc123", "Alice Smith", None)],
            total: 1,
        };
        print_user_results(&result);
    }

    #[test]
    fn print_results_with_pagination() {
        let result = ConfluenceUserSearchResults {
            users: vec![sample_user(
                "abc123",
                "Alice Smith",
                Some("alice@example.com"),
            )],
            total: 50,
        };
        print_user_results(&result);
    }

    // ── UserSearchCommand struct ──────────────────────────────────

    #[test]
    fn user_search_command_defaults() {
        let cmd = UserSearchCommand {
            query: "alice".to_string(),
            limit: 25,
            output: OutputFormat::Table,
        };
        assert_eq!(cmd.query, "alice");
        assert_eq!(cmd.limit, 25);
    }

    // ── run_search ────────────────────────────────────────────────

    #[tokio::test]
    async fn run_search_table_output() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/rest/api/search/user"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {
                            "accountId": "abc123",
                            "displayName": "Alice Smith",
                            "email": "alice@example.com"
                        }
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let result = run_search(&client, "alice", 25, &OutputFormat::Table).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn run_search_json_output() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/rest/api/search/user"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {
                            "accountId": "abc123",
                            "displayName": "Alice Smith"
                        }
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let result = run_search(&client, "alice", 25, &OutputFormat::Json).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn run_search_yaml_output() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/rest/api/search/user"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let result = run_search(&client, "nobody", 25, &OutputFormat::Yaml).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn run_search_api_error() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/rest/api/search/user"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let result = run_search(&client, "alice", 25, &OutputFormat::Table).await;
        assert!(result.is_err());
    }
}
