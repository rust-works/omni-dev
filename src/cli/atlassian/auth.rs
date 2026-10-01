//! CLI commands for Atlassian credential management.

use std::io::{self, Write};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::atlassian::auth::{self, AtlassianAuth, AtlassianCredentials, AuthMode};
use crate::atlassian::client::{AtlassianClient, AuthService};
use crate::utils::env::SystemEnv;
use crate::utils::settings::{active_profile_from, profile_suffix, Settings};

/// Manages Atlassian credentials.
#[derive(Parser)]
pub struct AuthCommand {
    /// The auth subcommand to execute.
    #[command(subcommand)]
    pub command: AuthSubcommands,
}

/// Auth subcommands.
#[derive(Subcommand)]
pub enum AuthSubcommands {
    /// Configures Atlassian credentials interactively.
    Login(LoginCommand),
    /// Removes Atlassian credentials from settings.json.
    Logout(LogoutCommand),
    /// Shows the current authentication status (mirrors the `atlassian_auth_status` MCP tool).
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

/// Configures Atlassian credentials.
#[derive(Parser)]
pub struct LoginCommand {
    /// Authentication: basic for Cloud, bearer for Server/Data Center PATs.
    #[arg(long, value_enum, default_value = "basic")]
    pub auth_mode: AuthMode,
}

impl LoginCommand {
    /// Prompts the user for credentials and saves them.
    pub fn execute(self) -> Result<()> {
        println!("Configure Atlassian credentials\n");
        let instance_url = prompt("Instance URL (e.g., https://myorg.atlassian.net): ")?;
        let credentials = match self.auth_mode {
            AuthMode::Basic => AtlassianAuth::Basic {
                email: prompt("Email: ")?,
                api_token: prompt("API token: ")?.into(),
            },
            AuthMode::Bearer => AtlassianAuth::Bearer {
                token: prompt("Personal Access Token: ")?.into(),
            },
        };
        let path = Settings::get_settings_path()?;
        let profile = active_profile_from(&SystemEnv);
        check_login_sources(&path, profile.as_deref(), &credentials, &SystemEnv)?;
        save_login_to(&path, profile.as_deref(), &instance_url, credentials)
    }
}

/// Validates credentials and persists them to `~/.omni-dev/settings.json`,
/// targeting the active profile's `env` map when a profile is selected
/// (issue #1116).
///
/// Extracted from [`LoginCommand::execute`] so the input-validation branches
/// are reachable from tests without mocking stdin.
#[cfg(test)]
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
#[cfg(test)]
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

    save_login_to(
        settings_path,
        profile,
        instance_url,
        AtlassianAuth::Basic {
            email: email.to_string(),
            api_token: api_token.into(),
        },
    )
}

/// Refuses process sources that would shadow or conflict with the saved mode.
fn check_login_sources(
    path: &std::path::Path,
    profile: Option<&str>,
    auth: &AtlassianAuth,
    raw: &impl crate::utils::env::EnvSource,
) -> Result<()> {
    let (selected, opposite) = match auth {
        AtlassianAuth::Basic { .. } => (auth::ATLASSIAN_API_TOKEN, auth::ATLASSIAN_PAT),
        AtlassianAuth::Bearer { .. } => (auth::ATLASSIAN_PAT, auth::ATLASSIAN_API_TOKEN),
    };
    Settings::ensure_secrets_replaceable(path, profile, &[selected], raw)?;
    for key in [selected, opposite] {
        if crate::utils::secret_env::secret_var_is_set(raw, key) {
            anyhow::bail!(
                "{key} has a process environment source; unset it before saving credentials"
            );
        }
    }
    Ok(())
}

fn save_login_to(
    settings_path: &std::path::Path,
    profile: Option<&str>,
    instance_url: &str,
    auth: AtlassianAuth,
) -> Result<()> {
    let credentials = AtlassianCredentials {
        instance_url: instance_url.to_string(),
        auth,
    };
    auth::save_credentials_to(settings_path, profile, &credentials)?;
    println!(
        "\nCredentials saved to ~/.omni-dev/settings.json{}",
        profile_suffix(profile)
    );
    println!("  Instance: {instance_url}");
    println!("  Authentication: {:?}", credentials.auth.mode());
    if let AtlassianAuth::Basic { email, .. } = &credentials.auth {
        println!("  Email: {email}");
    }
    println!("\nRun `omni-dev atlassian auth status` to verify.");
    Ok(())
}

/// Removes Atlassian credentials.
#[derive(Parser)]
pub struct LogoutCommand;

impl LogoutCommand {
    /// Removes Atlassian credential keys from settings.json — from the active
    /// profile's `env` map when a profile is selected (issue #1116).
    pub fn execute(self) -> Result<()> {
        run_logout(
            &Settings::get_settings_path()?,
            active_profile_from(&SystemEnv).as_deref(),
        )
    }
}

/// Removes Atlassian credential keys from an explicit settings-file path and
/// profile so tests inject both instead of mutating `HOME` /
/// `OMNI_DEV_PROFILE` (issue #1030).
fn run_logout(settings_path: &std::path::Path, profile: Option<&str>) -> Result<()> {
    let removed = auth::remove_credentials_at(settings_path, profile)?;
    if removed {
        println!(
            "Atlassian credentials removed from ~/.omni-dev/settings.json{}",
            profile_suffix(profile)
        );
    } else {
        println!("No Atlassian credentials were configured.");
    }
    Ok(())
}

/// Shows the current authentication status.
#[derive(Parser)]
pub struct StatusCommand {
    /// `--instance` override for the tenant being checked.
    #[command(flatten)]
    pub instance: super::InstanceArg,
    /// Service whose current-user endpoint verifies the credentials.
    #[arg(long, value_enum, default_value = "jira")]
    pub service: AuthService,
}

impl StatusCommand {
    /// Verifies credentials by calling the JIRA API.
    pub async fn execute(self) -> Result<()> {
        self.instance.apply();
        let credentials = auth::load_credentials()?;
        let client = AtlassianClient::from_credentials(&credentials)?;
        println!("Authentication: {:?}", credentials.auth.mode());
        run_auth_status(&client, &credentials.instance_url, self.service).await
    }
}

/// Verifies authentication and displays the current user.
async fn run_auth_status(
    client: &AtlassianClient,
    instance_url: &str,
    service: AuthService,
) -> Result<()> {
    println!("Checking authentication to {instance_url}...");

    let user = client.auth_identity(service).await?;

    println!("Authenticated as: {}", user.display_name);
    if let Some(ref email) = user.email_address {
        println!("Email: {email}");
    }
    if let Some(id) = user.account_id {
        println!("Account ID: {id}");
    }
    if let Some(name) = user.username {
        println!("Username: {name}");
    }
    if let Some(key) = user.key {
        println!("User key: {key}");
    }
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
            command: AuthSubcommands::Login(LoginCommand {
                auth_mode: AuthMode::Basic,
            }),
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
            command: AuthSubcommands::Status(StatusCommand {
                instance: crate::cli::atlassian::InstanceArg::default(),
                service: AuthService::Jira,
            }),
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

    // ── run_logout ─────────────────────────────────────────────────

    #[test]
    fn run_logout_removes_credentials_when_present() {
        use crate::atlassian::auth::{
            ATLASSIAN_API_TOKEN, ATLASSIAN_EMAIL, ATLASSIAN_INSTANCE_URL,
        };
        let (dir, settings_path) = temp_settings();
        std::fs::create_dir_all(dir.path().join(".omni-dev")).unwrap();
        std::fs::write(
            &settings_path,
            r#"{"env": {
                "ATLASSIAN_INSTANCE_URL": "https://org.atlassian.net",
                "ATLASSIAN_EMAIL": "me@test.com",
                "ATLASSIAN_API_TOKEN": "tok",
                "OTHER": "keep"
            }}"#,
        )
        .unwrap();

        run_logout(&settings_path, None).unwrap();

        let val: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert!(val["env"].get(ATLASSIAN_INSTANCE_URL).is_none());
        assert!(val["env"].get(ATLASSIAN_EMAIL).is_none());
        assert!(val["env"].get(ATLASSIAN_API_TOKEN).is_none());
        assert_eq!(val["env"]["OTHER"], "keep");
    }

    #[test]
    fn run_logout_is_idempotent_when_no_credentials() {
        let (_dir, settings_path) = temp_settings();
        run_logout(&settings_path, None).unwrap();
    }

    #[test]
    fn run_logout_with_profile_removes_profile_credentials_and_keeps_base() {
        use crate::atlassian::auth::ATLASSIAN_EMAIL;
        let (dir, settings_path) = temp_settings();
        std::fs::create_dir_all(dir.path().join(".omni-dev")).unwrap();
        std::fs::write(
            &settings_path,
            r#"{
                "env": {"ATLASSIAN_EMAIL": "base@test.com"},
                "profiles": {"work": {"env": {"ATLASSIAN_EMAIL": "work@test.com"}}}
            }"#,
        )
        .unwrap();

        run_logout(&settings_path, Some("work")).unwrap();

        let val: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert!(val["profiles"]["work"]["env"]
            .get(ATLASSIAN_EMAIL)
            .is_none());
        assert_eq!(val["env"]["ATLASSIAN_EMAIL"], "base@test.com");
    }

    /// Drives the `Logout` arm of `AuthCommand::execute` (the dispatch match),
    /// not just `LogoutCommand::execute` directly.
    #[tokio::test]
    async fn auth_command_execute_logout_arm() {
        use crate::atlassian::auth::ATLASSIAN_EMAIL;
        let guard = crate::atlassian::auth::test_util::EnvGuard::take();
        let dir = guard.clear_credentials();
        let omni_dir = dir.path().join(".omni-dev");
        std::fs::create_dir_all(&omni_dir).unwrap();
        let settings_path = omni_dir.join("settings.json");
        std::fs::write(
            &settings_path,
            r#"{"env": {"ATLASSIAN_EMAIL": "me@test.com"}}"#,
        )
        .unwrap();

        AuthCommand {
            command: AuthSubcommands::Logout(LogoutCommand),
        }
        .execute()
        .await
        .unwrap();

        let val: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert!(val["env"].get(ATLASSIAN_EMAIL).is_none());
    }

    /// `LogoutCommand::execute` resolves the settings path from `HOME` and the
    /// profile from `OMNI_DEV_PROFILE`, so this one test redirects both under
    /// the shared [`crate::atlassian::auth::test_util::EnvGuard`]; every other
    /// logout test injects them into `run_logout` (issue #1030).
    #[test]
    fn logout_command_execute_resolves_default_settings_path() {
        use crate::atlassian::auth::ATLASSIAN_EMAIL;
        let guard = crate::atlassian::auth::test_util::EnvGuard::take();
        let dir = guard.clear_credentials();
        let omni_dir = dir.path().join(".omni-dev");
        std::fs::create_dir_all(&omni_dir).unwrap();
        let settings_path = omni_dir.join("settings.json");
        std::fs::write(
            &settings_path,
            r#"{"env": {"ATLASSIAN_EMAIL": "me@test.com"}}"#,
        )
        .unwrap();

        LogoutCommand.execute().unwrap();

        let val: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert!(val["env"].get(ATLASSIAN_EMAIL).is_none());
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
        assert!(run_auth_status(&client, &server.uri(), AuthService::Jira)
            .await
            .is_ok());
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
        assert!(run_auth_status(&client, &server.uri(), AuthService::Jira)
            .await
            .is_ok());
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
        let err = run_auth_status(&client, &server.uri(), AuthService::Jira)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("401"));
    }
    #[test]
    fn auth_flags_parse_defaults_and_explicit_modes() {
        let cmd = AuthCommand::try_parse_from(["auth", "login"]).unwrap();
        assert!(matches!(
            cmd.command,
            AuthSubcommands::Login(LoginCommand {
                auth_mode: AuthMode::Basic
            })
        ));
        let cmd = AuthCommand::try_parse_from(["auth", "login", "--auth-mode", "bearer"]).unwrap();
        assert!(matches!(
            cmd.command,
            AuthSubcommands::Login(LoginCommand {
                auth_mode: AuthMode::Bearer
            })
        ));
        let cmd =
            AuthCommand::try_parse_from(["auth", "status", "--service", "confluence"]).unwrap();
        assert!(matches!(
            cmd.command,
            AuthSubcommands::Status(StatusCommand {
                service: AuthService::Confluence,
                ..
            })
        ));
        assert!(AuthCommand::try_parse_from(["auth", "login", "--auth-mode", "oauth"]).is_err());
    }

    #[test]
    fn bearer_login_persists_without_email_and_rejects_blank() {
        let (_dir, path) = temp_settings();
        save_login_to(
            &path,
            None,
            "https://self.example",
            AtlassianAuth::Bearer {
                token: "pat".into(),
            },
        )
        .unwrap();
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["env"][auth::ATLASSIAN_PAT], "pat");
        assert!(saved["env"].get(auth::ATLASSIAN_EMAIL).is_none());
        assert!(save_login_to(
            &path,
            None,
            "https://self.example",
            AtlassianAuth::Bearer { token: " ".into() }
        )
        .is_err());
    }

    #[test]
    fn login_refuses_process_sources_and_selected_helper() {
        use crate::test_support::env::MapEnv;
        let (_dir, path) = temp_settings();
        let credentials = AtlassianAuth::Bearer {
            token: "pat".into(),
        };
        for key in [
            auth::ATLASSIAN_PAT,
            "ATLASSIAN_PAT_FILE",
            "ATLASSIAN_PAT_COMMAND",
            auth::ATLASSIAN_API_TOKEN,
            "ATLASSIAN_API_TOKEN_FILE",
            "ATLASSIAN_API_TOKEN_COMMAND",
        ] {
            assert!(check_login_sources(
                &path,
                None,
                &credentials,
                &MapEnv::new().with(key, "source")
            )
            .is_err());
            assert!(!path.exists());
        }
        assert!(check_login_sources(&path, None, &credentials, &MapEnv::new()).is_ok());
    }
}
