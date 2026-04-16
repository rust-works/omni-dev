//! CLI commands for Atlassian credential management.

use std::io::{self, Write};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::atlassian::auth::{self, AtlassianCredentials};
use crate::atlassian::client::AtlassianClient;

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
    /// Shows the current authentication status.
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
        if instance_url.is_empty() {
            anyhow::bail!("Instance URL is required");
        }

        let email = prompt("Email: ")?;
        if email.is_empty() {
            anyhow::bail!("Email is required");
        }

        let api_token = prompt("API token: ")?;
        if api_token.is_empty() {
            anyhow::bail!("API token is required");
        }

        let credentials = AtlassianCredentials {
            instance_url: instance_url.clone(),
            email: email.clone(),
            api_token,
        };

        auth::save_credentials(&credentials)?;
        println!("\nCredentials saved to ~/.omni-dev/settings.json");
        println!("  Instance: {instance_url}");
        println!("  Email: {email}");
        println!("\nRun `omni-dev atlassian auth status` to verify.");

        Ok(())
    }
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
}
