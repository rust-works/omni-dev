//! CLI commands for Atlassian credential management.

use std::io::{self, Write};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::atlassian::auth::{self, AtlassianCredentials};
use crate::atlassian::client::AtlassianClient;
use crate::utils::env::SystemEnv;
use crate::utils::settings::{active_profile_from, profile_suffix, Settings};

/// Manages Atlassian Cloud credentials.
#[derive(Parser)]
pub struct AuthCommand {
    /// The auth subcommand to execute.
    #[command(subcommand)]
    pub command: AuthSubcommands,
}

/// Auth subcommands.
#[derive(Subcommand)]
pub enum AuthSubcommands {
    /// Configures Atlassian Cloud credentials interactively.
    Login(LoginCommand),
    /// Shows the current authentication status (mirrors the `atlassian_auth_status` MCP tool).
    Status(StatusCommand),
}

impl AuthCommand {
    /// Executes the auth command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            AuthSubcommands::Login(cmd) => cmd.execute(),
            AuthSubcommands::Status(cmd) => cmd.execute().await,
        }
    }
}

/// Configures Atlassian Cloud credentials.
#[derive(Parser)]
pub struct LoginCommand;

impl LoginCommand {
    /// Prompts the user for credentials and saves them.
    pub fn execute(self) -> Result<()> {
        println!("Configure Atlassian Cloud credentials\n");
        let instance_url = prompt("Instance URL (e.g., https://myorg.atlassian.net): ")?;
        let email = prompt("Email: ")?;
        let api_token = prompt("API token: ")?;
        run_login(&instance_url, &email, &api_token)
    }
}

/// Validates credentials and persists them to `~/.omni-dev/settings.json`,
/// targeting the active profile's `env` map when a profile is selected
/// (issue #1116).
///
/// Extracted from [`LoginCommand::execute`] so the input-validation branches
/// are reachable from tests without mocking stdin.
fn run_login(instance_url: &str, email: &str, api_token: &str) -> Result<()> {
    run_login_to(
        &Settings::get_settings_path()?,
        active_profile_from(&SystemEnv).as_deref(),
        instance_url,
        email,
        api_token,
    )
}

/// [`run_login`], persisting to an explicit settings-file path and profile so
/// tests inject both instead of mutating `HOME` / `OMNI_DEV_PROFILE`
/// (issue #1030).
fn run_login_to(
    settings_path: &std::path::Path,
    profile: Option<&str>,
    instance_url: &str,
    email: &str,
    api_token: &str,
) -> Result<()> {
    if instance_url.is_empty() {
        anyhow::bail!("Instance URL is required");
    }
    if email.is_empty() {
        anyhow::bail!("Email is required");
    }
    if api_token.is_empty() {
        anyhow::bail!("API token is required");
    }

    let credentials = AtlassianCredentials {
        instance_url: instance_url.to_string(),
        email: email.to_string(),
        api_token: api_token.into(),
    };

    auth::save_credentials_to(settings_path, profile, &credentials)?;
    println!(
        "\nCredentials saved to ~/.omni-dev/settings.json{}",
        profile_suffix(profile)
    );
    println!("  Instance: {instance_url}");
    println!("  Email: {email}");
    println!("\nRun `omni-dev atlassian auth status` to verify.");

    Ok(())
}

/// Shows the current authentication status.
#[derive(Parser)]
pub struct StatusCommand;

impl StatusCommand {
    /// Verifies credentials by calling the JIRA API.
    pub async fn execute(self) -> Result<()> {
        let credentials = auth::load_credentials()?;
        let client = AtlassianClient::from_credentials(&credentials)?;
        run_auth_status(&client, &credentials.instance_url).await
    }
}

/// Verifies authentication and displays the current user.
async fn run_auth_status(client: &AtlassianClient, instance_url: &str) -> Result<()> {
    println!("Checking authentication to {instance_url}...");

    let user = client.get_myself().await?;

    println!("Authenticated as: {}", user.display_name);
    if let Some(ref email) = user.email_address {
        println!("Email: {email}");
    }
    println!("Account ID: {}", user.account_id);
    println!("Instance: {instance_url}");

    Ok(())
}

/// Prompts the user for input on a single line.
fn prompt(message: &str) -> Result<String> {
    print!("{message}");
    io::stdout().flush().context("Failed to flush stdout")?;

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("Failed to read user input")?;

    Ok(input.trim().to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn auth_command_login_dispatch() {
        let cmd = AuthCommand {
            command: AuthSubcommands::Login(LoginCommand),
        };
        assert!(matches!(cmd.command, AuthSubcommands::Login(_)));
    }

    #[test]
    fn auth_command_status_dispatch() {
        let cmd = AuthCommand {
            command: AuthSubcommands::Status(StatusCommand),
        };
        assert!(matches!(cmd.command, AuthSubcommands::Status(_)));
    }

    // ── run_login ──────────────────────────────────────────────────

    fn temp_settings() -> (tempfile::TempDir, std::path::PathBuf) {
        std::fs::create_dir_all("tmp").ok();
        let dir = tempfile::TempDir::new_in("tmp").unwrap();
        let path = dir.path().join(".omni-dev").join("settings.json");
        (dir, path)
    }

    #[test]
    fn run_login_rejects_empty_instance_url() {
        let err = run_login("", "me@test.com", "tok").unwrap_err();
        assert!(err.to_string().contains("Instance URL"));
    }

    #[test]
    fn run_login_rejects_empty_email() {
        let err = run_login("https://org.atlassian.net", "", "tok").unwrap_err();
        assert!(err.to_string().contains("Email"));
    }

    #[test]
    fn run_login_rejects_empty_api_token() {
        let err = run_login("https://org.atlassian.net", "me@test.com", "").unwrap_err();
        assert!(err.to_string().contains("API token"));
    }

    #[test]
    fn run_login_to_persists_credentials() {
        let (_dir, settings_path) = temp_settings();

        run_login_to(
            &settings_path,
            None,
            "https://org.atlassian.net",
            "me@test.com",
            "tok-1",
        )
        .unwrap();

        let val: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(
            val["env"]["ATLASSIAN_INSTANCE_URL"],
            "https://org.atlassian.net"
        );
        assert_eq!(val["env"]["ATLASSIAN_EMAIL"], "me@test.com");
        assert_eq!(val["env"]["ATLASSIAN_API_TOKEN"], "tok-1");
    }

    #[test]
    fn run_login_to_with_profile_persists_under_profile() {
        let (_dir, settings_path) = temp_settings();

        run_login_to(
            &settings_path,
            Some("work"),
            "https://work.atlassian.net",
            "me@work.com",
            "tok-w",
        )
        .unwrap();

        let val: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(
            val["profiles"]["work"]["env"]["ATLASSIAN_EMAIL"],
            "me@work.com"
        );
        assert!(val["env"].get("ATLASSIAN_EMAIL").is_none());
    }

    // ── run_auth_status ────────────────────────────────────────────

    fn mock_client(base_url: &str) -> AtlassianClient {
        AtlassianClient::new(base_url, "user@test.com", "token").unwrap()
    }

    #[tokio::test]
    async fn run_auth_status_success() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/myself"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "accountId": "abc123",
                    "displayName": "Alice",
                    "emailAddress": "alice@test.com"
                })),
            )
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        assert!(run_auth_status(&client, &server.uri()).await.is_ok());
    }

    #[tokio::test]
    async fn run_auth_status_no_email() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/myself"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "accountId": "abc123",
                    "displayName": "Alice"
                })),
            )
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        assert!(run_auth_status(&client, &server.uri()).await.is_ok());
    }

    #[tokio::test]
    async fn run_auth_status_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/myself"))
            .respond_with(wiremock::ResponseTemplate::new(401).set_body_string("Unauthorized"))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let err = run_auth_status(&client, &server.uri()).await.unwrap_err();
        assert!(err.to_string().contains("401"));
    }
}
