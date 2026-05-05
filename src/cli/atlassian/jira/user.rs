//! CLI commands for JIRA user operations.

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::atlassian::client::{AtlassianClient, JiraUserSearchResults};
use crate::cli::atlassian::format::{output_as, OutputFormat};
use crate::cli::atlassian::helpers::create_client;

/// JIRA user operations.
#[derive(Parser)]
pub struct UserCommand {
    /// The user subcommand to execute.
    #[command(subcommand)]
    pub command: UserSubcommands,
}

/// JIRA user subcommands.
#[derive(Subcommand)]
pub enum UserSubcommands {
    /// Searches JIRA users by display name or email substring.
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

/// Searches JIRA users by display name or email substring.
#[derive(Parser)]
pub struct UserSearchCommand {
    /// Search text (matches display name or email substring).
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

async fn run_search(
    client: &AtlassianClient,
    query: &str,
    limit: u32,
    output: &OutputFormat,
) -> Result<()> {
    let result = client.search_jira_users(query, limit).await?;
    if output_as(&result, output)? {
        return Ok(());
    }
    print_user_results(&result);
    Ok(())
}

/// Prints JIRA user search results as a formatted table.
fn print_user_results(result: &JiraUserSearchResults) {
    if result.users.is_empty() {
        println!("No users found.");
        return;
    }

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
        .map(|u| u.display_name.as_deref().unwrap_or("-").len())
        .max()
        .unwrap_or(4)
        .max(4);

    println!(
        "{:<id_width$}  {:<name_width$}  ACTIVE  EMAIL",
        "ACCOUNT_ID", "NAME"
    );
    println!(
        "{:<id_width$}  {:<name_width$}  ------  -----",
        "-".repeat(id_width),
        "-".repeat(name_width),
    );

    for user in &result.users {
        let name = user.display_name.as_deref().unwrap_or("-");
        let email = user.email_address.as_deref().unwrap_or("-");
        let active = if user.active { "yes" } else { "no" };
        println!(
            "{:<id_width$}  {:<name_width$}  {active:<6}  {email}",
            user.account_id, name,
        );
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::await_holding_lock // env lock intentionally held across await on a single-thread runtime
)]
mod tests {
    use super::*;
    use crate::atlassian::auth::{ATLASSIAN_API_TOKEN, ATLASSIAN_EMAIL, ATLASSIAN_INSTANCE_URL};
    use crate::atlassian::client::JiraUserSearchResult;

    fn mock_client(base_url: &str) -> AtlassianClient {
        AtlassianClient::new(base_url, "user@test.com", "token").unwrap()
    }

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    struct EnvGuard;

    impl EnvGuard {
        fn set(instance_url: &str) -> Self {
            std::env::set_var(ATLASSIAN_INSTANCE_URL, instance_url);
            std::env::set_var(ATLASSIAN_EMAIL, "user@test.com");
            std::env::set_var(ATLASSIAN_API_TOKEN, "fake-token");
            Self
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            std::env::remove_var(ATLASSIAN_INSTANCE_URL);
            std::env::remove_var(ATLASSIAN_EMAIL);
            std::env::remove_var(ATLASSIAN_API_TOKEN);
        }
    }

    fn sample_user(
        account_id: &str,
        display_name: Option<&str>,
        email: Option<&str>,
        active: bool,
    ) -> JiraUserSearchResult {
        JiraUserSearchResult {
            account_id: account_id.to_string(),
            display_name: display_name.map(String::from),
            email_address: email.map(String::from),
            active,
            account_type: Some("atlassian".to_string()),
        }
    }

    #[test]
    fn print_results_empty() {
        let result = JiraUserSearchResults {
            users: vec![],
            count: 0,
        };
        print_user_results(&result);
    }

    #[test]
    fn print_results_with_users() {
        let result = JiraUserSearchResults {
            users: vec![
                sample_user("abc123", Some("Alice"), Some("alice@example.com"), true),
                sample_user("def456", Some("Bob"), None, true),
                sample_user("ghi789", None, None, false),
            ],
            count: 3,
        };
        print_user_results(&result);
    }

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

    #[tokio::test]
    async fn run_search_table_output() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/user/search"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([
                    {
                        "accountId": "abc123",
                        "displayName": "Alice Smith",
                        "emailAddress": "alice@example.com",
                        "active": true,
                        "accountType": "atlassian"
                    }
                ])),
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
            .and(wiremock::matchers::path("/rest/api/3/user/search"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
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
            .and(wiremock::matchers::path("/rest/api/3/user/search"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .expect(1)
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let result = run_search(&client, "nobody", 25, &OutputFormat::Yaml).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn run_search_api_error_propagates() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/user/search"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .expect(1)
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let result = run_search(&client, "alice", 25, &OutputFormat::Table).await;
        assert!(result.is_err());
    }

    // ── execute() with env-backed credentials ─────────────────────

    #[tokio::test(flavor = "current_thread")]
    async fn user_search_command_execute_round_trip() {
        let _lock = env_lock();
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/user/search"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([
                    {
                        "accountId": "abc123",
                        "displayName": "Alice",
                        "active": true,
                        "accountType": "atlassian"
                    }
                ])),
            )
            .expect(1)
            .mount(&server)
            .await;
        let _env = EnvGuard::set(&server.uri());

        let cmd = UserSearchCommand {
            query: "alice".to_string(),
            limit: 25,
            output: OutputFormat::Json,
        };
        cmd.execute().await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn user_command_dispatches_to_search() {
        let _lock = env_lock();
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/user/search"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .expect(1)
            .mount(&server)
            .await;
        let _env = EnvGuard::set(&server.uri());

        let cmd = UserCommand {
            command: UserSubcommands::Search(UserSearchCommand {
                query: "nobody".to_string(),
                limit: 25,
                output: OutputFormat::Yaml,
            }),
        };
        cmd.execute().await.unwrap();
    }
}
