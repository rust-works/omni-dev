//! CLI commands for Confluence user operations.

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::atlassian::client::AtlassianClient;
use crate::atlassian::confluence_types::{ConfluenceUserGetResults, ConfluenceUserSearchResults};
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
    /// Searches Confluence users by display name or email (mirrors the `confluence_user_search` MCP tool).
    Search(UserSearchCommand),
    /// Resolves account IDs to user records — the reverse of `search` (mirrors the `confluence_user_get` MCP tool).
    Get(UserGetCommand),
}

impl UserCommand {
    /// Executes the user command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            UserSubcommands::Search(cmd) => cmd.execute().await,
            UserSubcommands::Get(cmd) => cmd.execute().await,
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
        .map(|u| u.account_id.as_deref().unwrap_or("-").len())
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
        let account_id = user.account_id.as_deref().unwrap_or("-");
        println!(
            "{:<id_width$}  {:<name_width$}  {email}",
            account_id, user.display_name
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

/// Resolves Confluence account IDs to user records (the reverse of `search`).
#[derive(Parser)]
pub struct UserGetCommand {
    /// Account ID to resolve (repeat for a bulk lookup). Unknown or deactivated
    /// IDs are returned as a stub record with an `error` field rather than
    /// failing the whole batch.
    #[arg(long = "account-id", required = true)]
    pub account_id: Vec<String>,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl UserGetCommand {
    /// Executes the user lookup and prints results.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        run_get(&client, &self.account_id, &self.output).await
    }
}

/// Resolves the given account IDs and prints the records using the given client.
async fn run_get(
    client: &AtlassianClient,
    account_ids: &[String],
    output: &OutputFormat,
) -> Result<()> {
    let result = client.get_confluence_users(account_ids).await?;
    if output_as(&result, output)? {
        return Ok(());
    }
    print_user_get_results(&result);
    Ok(())
}

/// Prints resolved Confluence user records as a formatted table.
fn print_user_get_results(result: &ConfluenceUserGetResults) {
    if result.users.is_empty() {
        println!("No users resolved.");
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

    for user in &result.users {
        // A failed lookup carries only account_id + error; surface the reason
        // in the NAME column so a table reader still sees what went wrong.
        let name = match (&user.display_name, &user.error) {
            (Some(name), _) => name.clone(),
            (None, Some(err)) => format!("({err})"),
            (None, None) => "-".to_string(),
        };
        let email = user.email.as_deref().unwrap_or("-");
        println!(
            "{:<id_width$}  {:<name_width$}  {email}",
            user.account_id, name
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
    use crate::atlassian::confluence_types::{ConfluenceUserRecord, ConfluenceUserSearchResult};

    fn mock_client(base_url: &str) -> AtlassianClient {
        AtlassianClient::new(base_url, "user@test.com", "token").unwrap()
    }

    fn sample_user(
        account_id: Option<&str>,
        display_name: &str,
        email: Option<&str>,
    ) -> ConfluenceUserSearchResult {
        ConfluenceUserSearchResult {
            account_id: account_id.map(String::from),
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
                sample_user(Some("abc123"), "Alice Smith", Some("alice@example.com")),
                sample_user(Some("def456"), "Bob Jones", Some("bob@example.com")),
            ],
            total: 2,
        };
        print_user_results(&result);
    }

    #[test]
    fn print_results_with_missing_email() {
        let result = ConfluenceUserSearchResults {
            users: vec![sample_user(Some("abc123"), "Alice Smith", None)],
            total: 1,
        };
        print_user_results(&result);
    }

    #[test]
    fn print_results_with_missing_account_id() {
        let result = ConfluenceUserSearchResults {
            users: vec![sample_user(None, "App User", None)],
            total: 1,
        };
        print_user_results(&result);
    }

    #[test]
    fn print_results_with_pagination() {
        let result = ConfluenceUserSearchResults {
            users: vec![sample_user(
                Some("abc123"),
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
                            "user": {
                                "accountId": "abc123",
                                "displayName": "Alice Smith",
                                "email": "alice@example.com"
                            }
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
                            "user": {
                                "accountId": "abc123",
                                "displayName": "Alice Smith"
                            }
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

    // ── user get (account ID → record) ────────────────────────────

    fn sample_record(
        account_id: &str,
        display_name: Option<&str>,
        error: Option<&str>,
    ) -> ConfluenceUserRecord {
        ConfluenceUserRecord {
            account_id: account_id.to_string(),
            display_name: display_name.map(String::from),
            email: None,
            account_type: Some("atlassian".to_string()),
            active: None,
            error: error.map(String::from),
        }
    }

    #[test]
    fn print_get_results_empty() {
        print_user_get_results(&ConfluenceUserGetResults { users: vec![] });
    }

    #[test]
    fn print_get_results_with_users_and_error() {
        let result = ConfluenceUserGetResults {
            users: vec![
                sample_record("abc123", Some("Alice Smith"), None),
                sample_record("bad", None, Some("HTTP 404")),
                // No display name and no error — exercises the "-" fallback arm.
                sample_record("noinfo", None, None),
            ],
        };
        print_user_get_results(&result);
    }

    #[tokio::test]
    async fn run_get_table_output() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/rest/api/user"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "accountId": "abc123",
                    "accountType": "atlassian",
                    "displayName": "Alice Smith",
                    "email": "alice@example.com"
                })),
            )
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let ids = vec!["abc123".to_string()];
        assert!(run_get(&client, &ids, &OutputFormat::Table).await.is_ok());
    }

    #[tokio::test]
    async fn run_get_json_output_stub_on_404() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/rest/api/user"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not found"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let ids = vec!["missing".to_string()];
        assert!(run_get(&client, &ids, &OutputFormat::Json).await.is_ok());
    }

    #[tokio::test]
    async fn run_get_batch_survives_bad_id() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/rest/api/user"))
            .and(wiremock::matchers::query_param("accountId", "good"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "accountId": "good",
                    "displayName": "Good User"
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/rest/api/user"))
            .and(wiremock::matchers::query_param("accountId", "bad"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not found"))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let ids = vec!["good".to_string(), "bad".to_string()];
        assert!(run_get(&client, &ids, &OutputFormat::Yaml).await.is_ok());
    }

    // ── execute() with env-backed credentials ─────────────────────

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::atlassian::auth::test_util::AUTH_ENV_MUTEX
            .lock()
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

    #[tokio::test(flavor = "current_thread")]
    async fn user_get_command_execute_round_trip() {
        let _lock = env_lock();
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/rest/api/user"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "accountId": "abc123",
                    "accountType": "atlassian",
                    "displayName": "Alice"
                })),
            )
            .mount(&server)
            .await;
        let _env = EnvGuard::set(&server.uri());

        let cmd = UserGetCommand {
            account_id: vec!["abc123".to_string()],
            output: OutputFormat::Json,
        };
        cmd.execute().await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn user_command_dispatches_to_get() {
        let _lock = env_lock();
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/rest/api/user"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not found"))
            .mount(&server)
            .await;
        let _env = EnvGuard::set(&server.uri());

        let cmd = UserCommand {
            command: UserSubcommands::Get(UserGetCommand {
                account_id: vec!["missing".to_string()],
                output: OutputFormat::Yaml,
            }),
        };
        cmd.execute().await.unwrap();
    }
}
