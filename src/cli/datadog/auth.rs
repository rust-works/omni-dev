//! CLI commands for Datadog credential management.

use std::io::{self, Write};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Deserialize;

use crate::datadog::auth::{self, DatadogCredentials, DEFAULT_SITE};
use crate::datadog::client::DatadogClient;
use crate::utils::env::SystemEnv;
use crate::utils::settings::{active_profile_from, profile_suffix, Settings};

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
    /// Shows the current authentication status (mirrors the `datadog_auth_status` MCP tool).
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
        let app_key = prompt("Application key: ")?;
        let site_raw = prompt(&format!("Site [default: {DEFAULT_SITE}]: "))?;
        run_login(&api_key, &app_key, &site_raw)
    }
}

/// Validates credentials and persists them to `~/.omni-dev/settings.json`,
/// targeting the active profile's `env` map when a profile is selected
/// (issue #1116).
///
/// Extracted from [`LoginCommand::execute`] so the input-validation and
/// site-normalisation branches are reachable from tests without mocking
/// stdin.
fn run_login(api_key: &str, app_key: &str, site_raw: &str) -> Result<()> {
    run_login_to(
        &Settings::get_settings_path()?,
        active_profile_from(&SystemEnv).as_deref(),
        api_key,
        app_key,
        site_raw,
    )
}

/// [`run_login`], persisting to an explicit settings-file path and profile so
/// tests inject both instead of mutating `HOME` / `OMNI_DEV_PROFILE`
/// (issue #1030).
fn run_login_to(
    settings_path: &std::path::Path,
    profile: Option<&str>,
    api_key: &str,
    app_key: &str,
    site_raw: &str,
) -> Result<()> {
    if api_key.is_empty() {
        anyhow::bail!("API key is required");
    }
    if app_key.is_empty() {
        anyhow::bail!("Application key is required");
    }
    let site = if site_raw.is_empty() {
        DEFAULT_SITE.to_string()
    } else {
        auth::normalize_site(site_raw)
    };

    let credentials = DatadogCredentials {
        api_key: api_key.into(),
        app_key: app_key.into(),
        site: site.clone(),
    };

    auth::save_credentials_to(settings_path, profile, &credentials)?;
    println!(
        "\nCredentials saved to ~/.omni-dev/settings.json{}",
        profile_suffix(profile)
    );
    println!("  Site: {site}");
    println!("\nRun `omni-dev datadog auth status` to verify.");

    Ok(())
}

/// Removes Datadog API credentials.
#[derive(Parser)]
pub struct LogoutCommand;

impl LogoutCommand {
    /// Removes Datadog credential keys from settings.json — from the active
    /// profile's `env` map when a profile is selected (issue #1116).
    pub fn execute(self) -> Result<()> {
        run_logout(
            &Settings::get_settings_path()?,
            active_profile_from(&SystemEnv).as_deref(),
        )
    }
}

/// Removes Datadog credential keys from an explicit settings-file path and
/// profile so tests inject both instead of mutating `HOME` /
/// `OMNI_DEV_PROFILE` (issue #1030).
fn run_logout(settings_path: &std::path::Path, profile: Option<&str>) -> Result<()> {
    let removed = auth::remove_credentials_at(settings_path, profile)?;
    if removed {
        println!(
            "Datadog credentials removed from ~/.omni-dev/settings.json{}",
            profile_suffix(profile)
        );
    } else {
        println!("No Datadog credentials were configured.");
    }
    Ok(())
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
    use std::fs;

    use super::*;
    use crate::datadog::auth::{DATADOG_API_KEY, DATADOG_APP_KEY, DATADOG_SITE};

    /// Builds a settings-file path inside a fresh project-local tempdir.
    fn temp_settings() -> (tempfile::TempDir, std::path::PathBuf) {
        std::fs::create_dir_all("tmp").ok();
        let dir = tempfile::TempDir::new_in("tmp").unwrap();
        let path = dir.path().join(".omni-dev").join("settings.json");
        (dir, path)
    }

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

    // ── run_login ──────────────────────────────────────────────────

    #[test]
    fn run_login_rejects_empty_api_key() {
        let err = run_login("", "app", "").unwrap_err();
        assert!(err.to_string().contains("API key"));
    }

    #[test]
    fn run_login_rejects_empty_app_key() {
        let err = run_login("api", "", "").unwrap_err();
        assert!(err.to_string().contains("Application key"));
    }

    #[test]
    fn run_login_defaults_site_when_blank_and_persists() {
        let (_dir, settings_path) = temp_settings();

        run_login_to(&settings_path, None, "api-1", "app-1", "").unwrap();

        let content = fs::read_to_string(&settings_path).unwrap();
        let val: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(val["env"]["DATADOG_API_KEY"], "api-1");
        assert_eq!(val["env"]["DATADOG_APP_KEY"], "app-1");
        assert_eq!(val["env"]["DATADOG_SITE"], DEFAULT_SITE);
    }

    #[test]
    fn run_login_normalises_provided_site() {
        let (_dir, settings_path) = temp_settings();

        run_login_to(
            &settings_path,
            None,
            "api",
            "app",
            "https://api.us5.datadoghq.com/",
        )
        .unwrap();

        let content = fs::read_to_string(&settings_path).unwrap();
        let val: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(val["env"]["DATADOG_SITE"], "us5.datadoghq.com");
    }

    // ── run_logout ────────────────────────────────────────────────

    #[test]
    fn run_logout_removes_credentials_when_present() {
        let (dir, settings_path) = temp_settings();
        fs::create_dir_all(dir.path().join(".omni-dev")).unwrap();
        fs::write(
            &settings_path,
            r#"{"env": {
                "DATADOG_API_KEY": "a",
                "DATADOG_APP_KEY": "b",
                "DATADOG_SITE": "datadoghq.com",
                "OTHER": "keep"
            }}"#,
        )
        .unwrap();

        run_logout(&settings_path, None).unwrap();

        let val: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert!(val["env"].get(DATADOG_API_KEY).is_none());
        assert!(val["env"].get(DATADOG_APP_KEY).is_none());
        assert!(val["env"].get(DATADOG_SITE).is_none());
        assert_eq!(val["env"]["OTHER"], "keep");
    }

    #[test]
    fn run_logout_is_idempotent_when_no_credentials() {
        let (_dir, settings_path) = temp_settings();
        run_logout(&settings_path, None).unwrap();
    }

    /// `LogoutCommand::execute` resolves the settings path from `HOME` and
    /// the profile from `OMNI_DEV_PROFILE`, so this one test redirects both
    /// under the shared [`crate::atlassian::auth::test_util::EnvGuard`];
    /// every other logout test injects them into `run_logout` (issue #1030).
    #[test]
    fn logout_command_execute_resolves_default_settings_path() {
        let guard = crate::atlassian::auth::test_util::EnvGuard::take();
        let dir = guard.clear_credentials();
        let omni_dir = dir.path().join(".omni-dev");
        fs::create_dir_all(&omni_dir).unwrap();
        let settings_path = omni_dir.join("settings.json");
        fs::write(&settings_path, r#"{"env": {"DATADOG_API_KEY": "a"}}"#).unwrap();

        LogoutCommand.execute().unwrap();

        let val: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert!(val["env"].get(DATADOG_API_KEY).is_none());
    }

    // ── profile-targeted login/logout (issue #1116) ───────────────

    #[test]
    fn run_login_to_with_profile_persists_under_profile() {
        let (_dir, settings_path) = temp_settings();

        run_login_to(&settings_path, Some("work"), "api-p", "app-p", "").unwrap();

        let val: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(val["profiles"]["work"]["env"]["DATADOG_API_KEY"], "api-p");
        assert_eq!(val["profiles"]["work"]["env"]["DATADOG_SITE"], DEFAULT_SITE);
        assert!(val["env"].get("DATADOG_API_KEY").is_none());
    }

    #[test]
    fn run_logout_with_profile_removes_profile_credentials_and_keeps_base() {
        let (dir, settings_path) = temp_settings();
        fs::create_dir_all(dir.path().join(".omni-dev")).unwrap();
        fs::write(
            &settings_path,
            r#"{
                "env": {"DATADOG_API_KEY": "base"},
                "profiles": {"work": {"env": {"DATADOG_API_KEY": "work"}}}
            }"#,
        )
        .unwrap();

        run_logout(&settings_path, Some("work")).unwrap();

        let val: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert!(val["profiles"]["work"]["env"]
            .get(DATADOG_API_KEY)
            .is_none());
        assert_eq!(val["env"]["DATADOG_API_KEY"], "base");
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

    // StatusCommand::execute is trivial glue (create_client + run_auth_status);
    // the validate success/error paths are covered by the run_auth_status
    // wiremock tests above, and credential resolution by load_credentials_with /
    // create_client_from. No env-mutating execute-level test is needed (#1030).

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
