//! CLI commands for Gmail credential management.

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use serde::Deserialize;

use crate::gmail::auth::{self, BrowserConfig, GmailScope};
use crate::gmail::client::GmailClient;
use crate::utils::env::{EnvSource, SystemEnv};
use crate::utils::secret::Secret;

/// Manages Gmail OAuth2 credentials.
#[derive(Parser)]
pub struct AuthCommand {
    /// The auth subcommand to execute.
    #[command(subcommand)]
    pub command: AuthSubcommands,
}

/// Auth subcommands.
#[derive(Subcommand)]
pub enum AuthSubcommands {
    /// Runs the Gmail OAuth2 login flow (opens a browser). Interactive-only —
    /// login has no MCP equivalent.
    Login(LoginCommand),
    /// Removes the stored Gmail refresh token from settings.json.
    Logout(LogoutCommand),
    /// Shows the current authentication status (mirrors the `gmail_auth_status` MCP tool).
    Status(StatusCommand),
}

impl AuthCommand {
    /// Executes the auth command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            AuthSubcommands::Login(cmd) => cmd.execute().await,
            AuthSubcommands::Logout(cmd) => cmd.execute(),
            AuthSubcommands::Status(cmd) => cmd.execute().await,
        }
    }
}

/// Runs the Gmail OAuth2 login flow.
#[derive(Parser)]
pub struct LoginCommand {
    /// Request the `gmail.modify` scope (needed for `label add`/`remove`) in
    /// addition to `gmail.readonly`. Without this flag, only read access is
    /// granted.
    #[arg(long)]
    pub modify: bool,
}

impl LoginCommand {
    /// Reads the user's Google Cloud OAuth2 client credentials from the
    /// environment and runs the login flow.
    pub async fn execute(self) -> Result<()> {
        run_login(&SystemEnv, self.modify).await
    }
}

/// [`LoginCommand::execute`] over an injected [`EnvSource`].
async fn run_login(env: &(impl EnvSource + Sync), modify: bool) -> Result<()> {
    let client_id = env.var(auth::GMAIL_CLIENT_ID).ok_or_else(|| {
        anyhow!(
            "GMAIL_CLIENT_ID is not set. Create your own Google Cloud OAuth2 client (see \
             docs/gmail.md) and set GMAIL_CLIENT_ID/GMAIL_CLIENT_SECRET before running \
             `omni-dev gmail auth login`."
        )
    })?;
    let client_secret = env.var(auth::GMAIL_CLIENT_SECRET).ok_or_else(|| {
        anyhow!(
            "GMAIL_CLIENT_SECRET is not set. Create your own Google Cloud OAuth2 client (see \
             docs/gmail.md) and set GMAIL_CLIENT_ID/GMAIL_CLIENT_SECRET before running \
             `omni-dev gmail auth login`."
        )
    })?;
    let scope = resolve_scope(modify);

    let status = auth::login(
        &client_id,
        &Secret::new(client_secret),
        scope,
        &BrowserConfig::default(),
    )
    .await?;

    println!("\nCredentials saved to ~/.omni-dev/settings.json");
    println!("  Granted scope: {}", status.scope.unwrap_or_default());
    println!("\nRun `omni-dev gmail auth status` to verify.");
    Ok(())
}

/// Maps `--modify` to the scope requested at login.
fn resolve_scope(modify: bool) -> GmailScope {
    if modify {
        GmailScope::Modify
    } else {
        GmailScope::ReadOnly
    }
}

/// Removes the stored Gmail refresh token.
#[derive(Parser)]
pub struct LogoutCommand;

impl LogoutCommand {
    /// Removes Gmail credential keys from settings.json — from the active
    /// profile's `env` map when a profile is selected.
    pub fn execute(self) -> Result<()> {
        run_logout()
    }
}

fn run_logout() -> Result<()> {
    let removed = auth::remove_credentials()?;
    if removed {
        println!("Gmail credentials removed from ~/.omni-dev/settings.json");
    } else {
        println!("No Gmail credentials were configured.");
    }
    Ok(())
}

/// Shows the current authentication status.
#[derive(Parser)]
pub struct StatusCommand;

impl StatusCommand {
    /// Verifies credentials by calling `users.getProfile` — deliberately
    /// **live**, unlike the `gmail_auth_status` MCP tool's presence-only
    /// report, since that's the point of a CLI status command a human runs
    /// interactively.
    pub async fn execute(self) -> Result<()> {
        let credentials = auth::load_credentials()?;
        let scope = credentials.scope;
        let client = GmailClient::from_credentials(&credentials)?;
        run_auth_status(&client, scope).await
    }
}

#[derive(Debug, Deserialize)]
struct ProfileResponse {
    #[serde(rename = "emailAddress")]
    email_address: String,
    #[serde(rename = "messagesTotal")]
    messages_total: i64,
}

/// Calls `users.getProfile` and reports whether the stored refresh token is
/// still accepted.
async fn run_auth_status(client: &GmailClient, scope: GmailScope) -> Result<()> {
    println!("Checking Gmail authentication...");

    let url = format!("{}/gmail/v1/users/me/profile", client.base_url());
    let response = client.get_json(&url).await?;

    let status = response.status();
    if !status.is_success() {
        return Err(GmailClient::response_to_error(response).await.into());
    }

    let profile: ProfileResponse = response
        .json()
        .await
        .context("Failed to parse users.getProfile response")?;

    println!("Authenticated as: {}", profile.email_address);
    println!("Messages in mailbox: {}", profile.messages_total);
    println!(
        "Granted scope: {}",
        if scope.allows_modify() {
            "gmail.readonly, gmail.modify"
        } else {
            "gmail.readonly"
        }
    );
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::gmail::auth::{GMAIL_CLIENT_ID, GMAIL_CLIENT_SECRET};
    use crate::gmail::test_support::EnvGuard;
    use crate::test_support::env::MapEnv;

    #[test]
    fn auth_command_login_dispatch() {
        let cmd = AuthCommand {
            command: AuthSubcommands::Login(LoginCommand { modify: false }),
        };
        assert!(matches!(cmd.command, AuthSubcommands::Login(_)));
    }

    // ── resolve_scope ────────────────────────────────────────────

    #[test]
    fn resolve_scope_maps_modify_flag() {
        assert_eq!(resolve_scope(true), GmailScope::Modify);
        assert_eq!(resolve_scope(false), GmailScope::ReadOnly);
    }

    // ── AuthCommand::execute dispatch ───────────────────────────────

    #[tokio::test]
    async fn auth_command_execute_routes_login_and_surfaces_missing_credentials() {
        let guard = EnvGuard::take();
        let _dir = guard.clear_credentials();

        let cmd = AuthCommand {
            command: AuthSubcommands::Login(LoginCommand { modify: false }),
        };
        let err = cmd.execute().await.unwrap_err();
        assert!(err.to_string().contains("GMAIL_CLIENT_ID"));
    }

    #[tokio::test]
    async fn auth_command_execute_routes_logout() {
        let guard = EnvGuard::take();
        let _dir = guard.clear_credentials();

        let cmd = AuthCommand {
            command: AuthSubcommands::Logout(LogoutCommand),
        };
        cmd.execute().await.unwrap();
    }

    #[tokio::test]
    async fn auth_command_execute_routes_status_and_surfaces_missing_credentials() {
        let guard = EnvGuard::take();
        let _dir = guard.clear_credentials();

        let cmd = AuthCommand {
            command: AuthSubcommands::Status(StatusCommand),
        };
        let err = cmd.execute().await.unwrap_err();
        assert!(err.to_string().contains("not configured"));
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

    // ── run_login env validation ──────────────────────────────────

    #[tokio::test]
    async fn run_login_errors_when_client_id_missing() {
        let env = MapEnv::new().with(GMAIL_CLIENT_SECRET, "s");
        let err = run_login(&env, false).await.unwrap_err();
        assert!(err.to_string().contains("GMAIL_CLIENT_ID"));
    }

    #[tokio::test]
    async fn run_login_errors_when_client_secret_missing() {
        let env = MapEnv::new().with(GMAIL_CLIENT_ID, "c");
        let err = run_login(&env, false).await.unwrap_err();
        assert!(err.to_string().contains("GMAIL_CLIENT_SECRET"));
    }

    // ── LoginCommand::execute glue ──────────────────────────────────

    #[tokio::test]
    async fn login_command_execute_errors_when_credentials_missing() {
        let guard = EnvGuard::take();
        let _dir = guard.clear_credentials();

        let cmd = LoginCommand { modify: false };
        let err = cmd.execute().await.unwrap_err();
        assert!(err.to_string().contains("GMAIL_CLIENT_ID"));
    }

    // ── run_logout / LogoutCommand::execute ─────────────────────────

    #[test]
    fn run_logout_reports_none_configured_when_absent() {
        let guard = EnvGuard::take();
        let _dir = guard.clear_credentials();

        run_logout().unwrap();
    }

    #[test]
    fn run_logout_removes_previously_saved_credentials() {
        use crate::gmail::auth::GmailCredentials;
        let guard = EnvGuard::take();
        let _dir = guard.clear_credentials();

        auth::save_credentials(&GmailCredentials {
            client_id: "client".to_string(),
            client_secret: Secret::new("secret"),
            refresh_token: Secret::new("refresh"),
            scope: GmailScope::ReadOnly,
        })
        .unwrap();

        run_logout().unwrap();
        assert!(!auth::status().has_refresh_token);
    }

    #[test]
    fn logout_command_execute_delegates_to_run_logout() {
        let guard = EnvGuard::take();
        let _dir = guard.clear_credentials();

        LogoutCommand.execute().unwrap();
    }

    // ── StatusCommand::execute glue ─────────────────────────────────

    #[tokio::test]
    async fn status_command_execute_errors_when_credentials_missing() {
        let guard = EnvGuard::take();
        let _dir = guard.clear_credentials();

        let err = StatusCommand.execute().await.unwrap_err();
        assert!(err.to_string().contains("not configured"));
    }

    // ── run_auth_status ────────────────────────────────────────────

    fn mock_client(base_url: &str) -> GmailClient {
        use crate::gmail::auth::GmailCredentials;
        let creds = GmailCredentials {
            client_id: "client".to_string(),
            client_secret: Secret::new("secret"),
            refresh_token: Secret::new("refresh"),
            scope: GmailScope::ReadOnly,
        };
        let mut client = GmailClient::new(base_url, &creds).unwrap();
        crate::gmail::client::test_support::replace_session(
            &mut client,
            &creds,
            &format!("{base_url}/token"),
        );
        client
    }

    async fn mount_token_endpoint(server: &wiremock::MockServer) {
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/token"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "test-token",
                    "expires_in": 3600,
                })),
            )
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn run_auth_status_success() {
        let server = wiremock::MockServer::start().await;
        mount_token_endpoint(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/profile"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "emailAddress": "user@example.com",
                    "messagesTotal": 100,
                    "historyId": "1000",
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        assert!(run_auth_status(&client, GmailScope::ReadOnly).await.is_ok());
    }

    #[tokio::test]
    async fn run_auth_status_reports_modify_scope() {
        let server = wiremock::MockServer::start().await;
        mount_token_endpoint(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/profile"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "emailAddress": "user@example.com",
                    "messagesTotal": 100,
                    "historyId": "1000",
                })),
            )
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        assert!(run_auth_status(&client, GmailScope::Modify).await.is_ok());
    }

    #[tokio::test]
    async fn run_auth_status_api_error() {
        let server = wiremock::MockServer::start().await;
        mount_token_endpoint(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/profile"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let err = run_auth_status(&client, GmailScope::ReadOnly)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("403"));
        assert!(msg.contains("Forbidden"));
    }
}
