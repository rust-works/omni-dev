//! CLI commands for Datadog credential management.

use std::io::{self, Write};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Deserialize;

use crate::datadog::auth::{self, DatadogCredentials, DEFAULT_SITE};
use crate::datadog::client::DatadogClient;

/// Manages Datadog API credentials.
#[derive(Parser)]
pub struct AuthCommand {
    /// The auth subcommand to execute.
    #[command(subcommand)]
    pub command: AuthSubcommands,
}

/// Auth subcommands.
#[derive(Subcommand)]
pub enum AuthSubcommands {
    /// Configures Datadog API credentials interactively.
    Login(LoginCommand),
    /// Removes Datadog API credentials from settings.json.
    Logout(LogoutCommand),
    /// Shows the current authentication status.
    Status(StatusCommand),
}

impl AuthCommand {
    /// Executes the auth command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            AuthSubcommands::Login(cmd) => cmd.execute(),
            AuthSubcommands::Logout(cmd) => cmd.execute(),
            AuthSubcommands::Status(cmd) => cmd.execute().await,
        }
    }
}

/// Configures Datadog API credentials.
#[derive(Parser)]
pub struct LoginCommand;

impl LoginCommand {
    /// Prompts the user for credentials and saves them.
    pub fn execute(self) -> Result<()> {
        println!("Configure Datadog API credentials\n");

        let api_key = prompt("API key: ")?;
        if api_key.is_empty() {
            anyhow::bail!("API key is required");
        }

        let app_key = prompt("Application key: ")?;
        if app_key.is_empty() {
            anyhow::bail!("Application key is required");
        }

        let site_raw = prompt(&format!("Site [default: {DEFAULT_SITE}]: "))?;
        let site = if site_raw.is_empty() {
            DEFAULT_SITE.to_string()
        } else {
            auth::normalize_site(&site_raw)
        };

        let credentials = DatadogCredentials {
            api_key,
            app_key,
            site: site.clone(),
        };

        auth::save_credentials(&credentials)?;
        println!("\nCredentials saved to ~/.omni-dev/settings.json");
        println!("  Site: {site}");
        println!("\nRun `omni-dev datadog auth status` to verify.");

        Ok(())
    }
}

/// Removes Datadog API credentials.
#[derive(Parser)]
pub struct LogoutCommand;

impl LogoutCommand {
    /// Removes Datadog credential keys from settings.json.
    pub fn execute(self) -> Result<()> {
        let removed = auth::remove_credentials()?;
        if removed {
            println!("Datadog credentials removed from ~/.omni-dev/settings.json");
        } else {
            println!("No Datadog credentials were configured.");
        }
        Ok(())
    }
}

/// Shows the current authentication status.
#[derive(Parser)]
pub struct StatusCommand;

impl StatusCommand {
    /// Verifies credentials by calling `/api/v1/validate`.
    pub async fn execute(self) -> Result<()> {
        let credentials = auth::load_credentials()?;
        let site = credentials.site.clone();
        let client = DatadogClient::from_credentials(&credentials)?;
        run_auth_status(&client, &site).await
    }
}

#[derive(Debug, Deserialize)]
struct ValidateResponse {
    #[serde(default)]
    valid: bool,
}

/// Calls `/api/v1/validate` and reports whether the API+APP key pair is accepted.
async fn run_auth_status(client: &DatadogClient, site: &str) -> Result<()> {
    println!("Checking Datadog authentication for site '{site}'...");

    let url = format!("{}/api/v1/validate", client.base_url());
    let response = client.get_json(&url).await?;

    let status = response.status();
    if !status.is_success() {
        return Err(DatadogClient::response_to_error(response).await.into());
    }

    let validate: ValidateResponse = response
        .json()
        .await
        .context("Failed to parse /api/v1/validate response")?;

    if validate.valid {
        println!("Authenticated successfully.");
    } else {
        println!("Datadog reported credentials as invalid.");
    }
    println!("Site: {site}");
    println!("Base URL: {}", client.base_url());

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
    fn auth_command_logout_dispatch() {
        let cmd = AuthCommand {
            command: AuthSubcommands::Logout(LogoutCommand),
        };
        assert!(matches!(cmd.command, AuthSubcommands::Logout(_)));
    }

    #[test]
    fn auth_command_status_dispatch() {
        let cmd = AuthCommand {
            command: AuthSubcommands::Status(StatusCommand),
        };
        assert!(matches!(cmd.command, AuthSubcommands::Status(_)));
    }

    // ── run_auth_status ────────────────────────────────────────────

    fn mock_client(base_url: &str) -> DatadogClient {
        DatadogClient::new(base_url, "api", "app").unwrap()
    }

    #[tokio::test]
    async fn run_auth_status_success() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/validate"))
            .and(wiremock::matchers::header("DD-API-KEY", "api"))
            .and(wiremock::matchers::header("DD-APPLICATION-KEY", "app"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"valid": true})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        assert!(run_auth_status(&client, "datadoghq.com").await.is_ok());
    }

    #[tokio::test]
    async fn run_auth_status_valid_false_still_ok() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/validate"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"valid": false})),
            )
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        assert!(run_auth_status(&client, "datadoghq.com").await.is_ok());
    }

    #[tokio::test]
    async fn run_auth_status_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/validate"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let err = run_auth_status(&client, "datadoghq.com").await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("403"));
        assert!(msg.contains("Forbidden"));
    }

    #[tokio::test]
    async fn run_auth_status_surfaces_rate_limit_on_exhausted_retries() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/validate"))
            .respond_with(
                wiremock::ResponseTemplate::new(429)
                    .append_header("Retry-After", "0")
                    .append_header("X-RateLimit-Remaining", "0")
                    .append_header("X-RateLimit-Reset", "3")
                    .set_body_string("slow down"),
            )
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let err = run_auth_status(&client, "datadoghq.com").await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("429"));
        assert!(msg.contains("rate-limit"));
    }
}
