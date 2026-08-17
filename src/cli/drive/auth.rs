//! CLI commands for Drive credential management.

use std::io::{self, Write};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::enable_raw_mode;

use crate::drive::about_api::AboutApi;
use crate::drive::account;
use crate::drive::auth::{self, DriveScope};
use crate::drive::client::DriveClient;
use crate::utils::env::EnvSource;
use crate::utils::secret::Secret;
use crate::utils::settings::{Settings, SettingsEnv};
use crate::utils::terminal::RawModeGuard;

/// Manages Drive OAuth2 credentials.
#[derive(Parser)]
pub struct AuthCommand {
    /// The auth subcommand to execute.
    #[command(subcommand)]
    pub command: AuthSubcommands,
}

/// Auth subcommands. No `Import` — unlike Gmail there is no
/// `src/drive/import.rs`; Drive has no `client_secret.json`-import path.
#[derive(Subcommand)]
pub enum AuthSubcommands {
    /// Runs the Drive OAuth2 login flow (opens a browser).
    Login(LoginCommand),
    /// Removes the stored Drive refresh token from settings.json.
    Logout(LogoutCommand),
    /// Shows the current authentication status.
    Status(StatusCommand),
}

impl AuthCommand {
    pub async fn execute(self) -> Result<()> {
        match self.command {
            AuthSubcommands::Login(cmd) => cmd.execute().await,
            AuthSubcommands::Logout(cmd) => cmd.execute(),
            AuthSubcommands::Status(cmd) => cmd.execute().await,
        }
    }
}

/// Runs the Drive OAuth2 login flow.
#[derive(Parser)]
pub struct LoginCommand {
    /// Request the `drive.metadata` scope (needed for `rename`/`move`) in
    /// addition to `drive.readonly`. Without this flag, only read access is
    /// granted.
    #[arg(long)]
    pub write: bool,
}

impl LoginCommand {
    /// Reads the user's Google Cloud OAuth2 client credentials from the
    /// environment or settings.json, prompting interactively when neither
    /// has them, then runs the login flow.
    pub async fn execute(self) -> Result<()> {
        run_login(&SettingsEnv::load(), self.write).await
    }
}

/// [`LoginCommand::execute`] over an injected [`EnvSource`]. Production
/// passes a [`SettingsEnv`] (process env, falling back to settings.json);
/// tests pass a plain `MapEnv` representing the already-resolved value.
async fn run_login(env: &(impl EnvSource + Sync), write: bool) -> Result<()> {
    let (client_id, client_secret) =
        resolve_login_credentials(env, prompt_client_id, prompt_client_secret)?;
    let scope = resolve_scope(write);

    let settings = Settings::load().unwrap_or_default();
    let browser = auth::resolve_browser_config_for(&settings.drive, None)?;

    let status = auth::login_for(None, &client_id, &client_secret, scope, &browser).await?;

    println!("\nCredentials saved to ~/.omni-dev/settings.json");
    println!("  Granted scope: {}", status.scope.unwrap_or_default());
    println!("\nRun `omni-dev drive auth status` to verify.");
    Ok(())
}

/// Resolves the OAuth2 client id/secret from `env`, falling back to the
/// given prompts when either is absent — the pure decision extracted from
/// [`run_login`] so it's testable without a real terminal.
fn resolve_login_credentials(
    env: &impl EnvSource,
    prompt_client_id: impl FnOnce() -> Result<String>,
    prompt_client_secret: impl FnOnce() -> Result<Secret>,
) -> Result<(String, Secret)> {
    let client_id = match env.var(auth::DRIVE_CLIENT_ID) {
        Some(v) => v,
        None => prompt_client_id()?,
    };
    let client_secret = match env.var(auth::DRIVE_CLIENT_SECRET) {
        Some(v) => Secret::new(v),
        None => prompt_client_secret()?,
    };
    Ok((client_id, client_secret))
}

/// Maps `--write` to the scope requested at login.
fn resolve_scope(write: bool) -> DriveScope {
    if write {
        DriveScope::Metadata
    } else {
        DriveScope::ReadOnly
    }
}

/// Prompts for the OAuth2 client id. Unlike Gmail there is no `drive auth
/// import` to point at — points at Google Cloud Console / ADR-0069 instead
/// (same reason `client_id` isn't itself a secret: visible in the browser's
/// own network traffic during login regardless).
fn prompt_client_id() -> Result<String> {
    print!(
        "DRIVE_CLIENT_ID is not set. Create an OAuth2 client id in Google Cloud Console (see \
         docs/adrs/adr-0069.md) and set DRIVE_CLIENT_ID, or paste it here.\nClient id: "
    );
    io::stdout().flush().context("Failed to flush stdout")?;

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("Failed to read user input")?;
    Ok(input.trim().to_string())
}

/// Prompts for the OAuth2 client secret on stdin without echoing it —
/// raw-mode key-by-key reading via `crossterm`, printing nothing per
/// keystroke (not even a placeholder character, to avoid leaking the
/// secret's length).
fn prompt_client_secret() -> Result<Secret> {
    print!("Client secret: ");
    io::stdout().flush().context("Failed to flush stdout")?;

    enable_raw_mode().context("Failed to enable terminal raw mode")?;
    let guard = RawModeGuard;

    let mut buffer = String::new();
    loop {
        if let Event::Key(key_event) = event::read().context("Failed to read terminal input")? {
            match apply_secret_key(&mut buffer, key_event) {
                SecretKeyOutcome::Continue => {}
                SecretKeyOutcome::Finished => break,
                SecretKeyOutcome::Aborted => anyhow::bail!("Aborted"),
            }
        }
    }
    drop(guard);
    println!();

    Ok(Secret::new(buffer))
}

/// Result of feeding one key event to [`apply_secret_key`].
#[derive(Debug, PartialEq, Eq)]
enum SecretKeyOutcome {
    /// The buffer was updated (or the event was ignored); keep reading.
    Continue,
    /// Enter was pressed; the secret is complete.
    Finished,
    /// Ctrl+C was pressed; the prompt should abort.
    Aborted,
}

/// Applies one key event to the in-progress secret `buffer` — the pure
/// decision extracted from [`prompt_client_secret`]'s read loop so it's
/// unit-testable with synthesized [`KeyEvent`]s instead of a real terminal.
///
/// Ignores anything that isn't a `Press`: crossterm delivers both `Press`
/// and `Release` for every keystroke on Windows (unconditionally — see
/// `KeyEvent::kind`'s own doc) and on Unix when the kitty keyboard protocol
/// is enabled, so without this filter every character would be pushed onto
/// `buffer` twice. Because this prompt echoes nothing, that doubling would
/// be invisible until it surfaced later as a misattributed `invalid_client`
/// error at `auth login`.
fn apply_secret_key(buffer: &mut String, key: KeyEvent) -> SecretKeyOutcome {
    if key.kind != KeyEventKind::Press {
        return SecretKeyOutcome::Continue;
    }
    match key.code {
        KeyCode::Enter => SecretKeyOutcome::Finished,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            SecretKeyOutcome::Aborted
        }
        KeyCode::Char(c) => {
            buffer.push(c);
            SecretKeyOutcome::Continue
        }
        KeyCode::Backspace => {
            buffer.pop();
            SecretKeyOutcome::Continue
        }
        _ => SecretKeyOutcome::Continue,
    }
}

/// Removes the stored Drive refresh token.
#[derive(Parser)]
pub struct LogoutCommand;

impl LogoutCommand {
    pub fn execute(self) -> Result<()> {
        run_logout()
    }
}

fn run_logout() -> Result<()> {
    let removed = auth::remove_credentials_for(None)?;
    if removed {
        println!("Drive credentials removed from ~/.omni-dev/settings.json");
    } else {
        println!("No Drive credentials were configured.");
    }
    Ok(())
}

/// Shows the current authentication status.
#[derive(Parser)]
pub struct StatusCommand {
    /// Reports status for every configured Drive account.
    #[arg(long)]
    pub all: bool,
}

impl StatusCommand {
    /// Deliberately **live** (calls `about.get`), unlike a presence-only
    /// report. Does NOT use `auth::status_for`/`status_from_named` — those
    /// are `#[cfg(feature = "mcp")]`-gated and reserved for the Drive MCP
    /// surface (#1525).
    pub async fn execute(self) -> Result<()> {
        if self.all {
            return run_auth_status_all().await;
        }
        let credentials = auth::load_credentials_for(None)?;
        let scope = credentials.scope;
        let client = DriveClient::from_credentials(&credentials)?;
        run_auth_status(&client, scope).await
    }
}

async fn run_auth_status_all() -> Result<()> {
    let settings = Settings::load().unwrap_or_default();
    let accounts = account::list_accounts(&settings.drive);
    if accounts.is_empty() {
        // Zero-migration guarantee: degenerates to the single-account path.
        let credentials = auth::load_credentials_for(None)?;
        let scope = credentials.scope;
        let client = DriveClient::from_credentials(&credentials)?;
        return run_auth_status(&client, scope).await;
    }
    let client_for = |name: &str| -> Result<(DriveClient, DriveScope)> {
        let credentials = auth::load_credentials_for(Some(name))?;
        let scope = credentials.scope;
        let client = DriveClient::from_credentials(&credentials)?;
        Ok((client, scope))
    };
    run_auth_status_all_with(&accounts, &client_for).await
}

/// Runs every account's status check concurrently and prints the results back
/// out in `accounts`' original order — `join_all`'s output preserves input
/// order regardless of completion order, so no extra bookkeeping is needed to
/// undo the concurrency once the results are in hand.
///
/// `client_for` is a seam: production builds a client from stored
/// credentials, tests point it at a `wiremock` server per account name.
async fn run_auth_status_all_with<F>(
    accounts: &[account::AccountSummary],
    client_for: &F,
) -> Result<()>
where
    F: Fn(&str) -> Result<(DriveClient, DriveScope)> + Sync,
{
    let futs = accounts.iter().map(|summary| async move {
        (
            summary.name.clone(),
            report_one_account_status(&summary.name, client_for).await,
        )
    });
    let results = futures::future::join_all(futs).await;
    for (name, result) in results {
        println!("\n== {name} ==");
        match result {
            Ok(body) => print!("{body}"),
            Err(err) => println!("  error: {err}"),
        }
    }
    Ok(())
}

async fn report_one_account_status<F>(name: &str, client_for: &F) -> Result<String>
where
    F: Fn(&str) -> Result<(DriveClient, DriveScope)> + Sync,
{
    let (client, scope) = client_for(name)?;
    run_auth_status_for(&client, scope, Some(name)).await
}

async fn run_auth_status(client: &DriveClient, scope: DriveScope) -> Result<()> {
    let body = run_auth_status_for(client, scope, None).await?;
    print!("{body}");
    Ok(())
}

/// Calls `about.get` and reports whether the stored refresh token is still
/// accepted; backfills `account_name`'s cached `email_address` on success.
/// Returns the formatted report rather than printing it, so callers running
/// several accounts concurrently (`run_auth_status_all_with`) can print them
/// back out in a stable order instead of interleaving live `println!`s.
async fn run_auth_status_for(
    client: &DriveClient,
    scope: DriveScope,
    account_name: Option<&str>,
) -> Result<String> {
    let mut out = String::new();
    out.push_str("Checking Drive authentication...\n");

    let about = AboutApi::new(client).get().await?;
    let email = about.user.email_address.unwrap_or_default();

    out.push_str(&format!("Authenticated as: {email}\n"));
    out.push_str(&format!(
        "Granted scope: {}\n",
        if scope.allows_write() {
            "drive.readonly, drive.metadata"
        } else {
            "drive.readonly"
        }
    ));

    if let (Some(name), false) = (account_name, email.is_empty()) {
        // Best-effort cache backfill: if two accounts in the same --all run
        // both need a fresh email, concurrent read-modify-write to
        // settings.json can drop one (no file lock). Self-heals on the next
        // status check, which always re-fetches the email live.
        auth::record_account_email(name, &email)?;
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::test_support::env::MapEnv;
    use crate::utils::settings::Settings;

    fn test_credentials() -> auth::DriveCredentials {
        auth::DriveCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: DriveScope::ReadOnly,
        }
    }

    async fn client_with_bootstrapped_token(server: &wiremock::MockServer) -> DriveClient {
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

        let mut client = DriveClient::new(&server.uri(), &test_credentials()).unwrap();
        crate::drive::client::test_support::replace_session(
            &mut client,
            &test_credentials(),
            &format!("{}/token", server.uri()),
        );
        client
    }

    // ── resolve_login_credentials ────────────────────────────────────

    #[test]
    fn resolve_login_credentials_uses_env_when_both_present() {
        let env = MapEnv::new()
            .with(auth::DRIVE_CLIENT_ID, "id-from-env")
            .with(auth::DRIVE_CLIENT_SECRET, "secret-from-env");
        let (id, secret) = resolve_login_credentials(
            &env,
            || panic!("should not prompt for client id"),
            || panic!("should not prompt for client secret"),
        )
        .unwrap();
        assert_eq!(id, "id-from-env");
        assert_eq!(secret.expose_secret(), "secret-from-env");
    }

    #[test]
    fn resolve_login_credentials_prompts_for_missing_client_id() {
        let env = MapEnv::new().with(auth::DRIVE_CLIENT_SECRET, "secret-from-env");
        let (id, secret) = resolve_login_credentials(
            &env,
            || Ok("id-from-prompt".to_string()),
            || panic!("should not prompt for client secret"),
        )
        .unwrap();
        assert_eq!(id, "id-from-prompt");
        assert_eq!(secret.expose_secret(), "secret-from-env");
    }

    #[test]
    fn resolve_login_credentials_prompts_for_missing_client_secret() {
        let env = MapEnv::new().with(auth::DRIVE_CLIENT_ID, "id-from-env");
        let (id, secret) = resolve_login_credentials(
            &env,
            || panic!("should not prompt for client id"),
            || Ok(Secret::new("secret-from-prompt")),
        )
        .unwrap();
        assert_eq!(id, "id-from-env");
        assert_eq!(secret.expose_secret(), "secret-from-prompt");
    }

    #[test]
    fn resolve_login_credentials_prompts_for_both_when_absent() {
        let env = MapEnv::new();
        let (id, secret) = resolve_login_credentials(
            &env,
            || Ok("id-from-prompt".to_string()),
            || Ok(Secret::new("secret-from-prompt")),
        )
        .unwrap();
        assert_eq!(id, "id-from-prompt");
        assert_eq!(secret.expose_secret(), "secret-from-prompt");
    }

    #[test]
    fn resolve_login_credentials_propagates_prompt_errors() {
        let env = MapEnv::new();
        let err = resolve_login_credentials(
            &env,
            || Err(anyhow::anyhow!("aborted")),
            || panic!("should not reach secret prompt"),
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "aborted");
    }

    // ── apply_secret_key ─────────────────────────────────────────────

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn apply_secret_key_appends_char() {
        let mut buffer = String::new();
        assert_eq!(
            apply_secret_key(&mut buffer, press(KeyCode::Char('a'))),
            SecretKeyOutcome::Continue
        );
        assert_eq!(buffer, "a");
    }

    #[test]
    fn apply_secret_key_backspace_removes_last_char() {
        let mut buffer = "ab".to_string();
        apply_secret_key(&mut buffer, press(KeyCode::Backspace));
        assert_eq!(buffer, "a");
    }

    #[test]
    fn apply_secret_key_enter_finishes() {
        let mut buffer = String::new();
        assert_eq!(
            apply_secret_key(&mut buffer, press(KeyCode::Enter)),
            SecretKeyOutcome::Finished
        );
    }

    #[test]
    fn apply_secret_key_ctrl_c_aborts() {
        let mut buffer = String::new();
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(
            apply_secret_key(&mut buffer, key),
            SecretKeyOutcome::Aborted
        );
    }

    #[test]
    fn apply_secret_key_ignores_non_press_events() {
        let mut buffer = String::new();
        let release = KeyEvent::new_with_kind(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        assert_eq!(
            apply_secret_key(&mut buffer, release),
            SecretKeyOutcome::Continue
        );
        assert_eq!(buffer, "");
    }

    #[test]
    fn apply_secret_key_ignores_unhandled_keys() {
        let mut buffer = String::new();
        assert_eq!(
            apply_secret_key(&mut buffer, press(KeyCode::Left)),
            SecretKeyOutcome::Continue
        );
        assert_eq!(buffer, "");
    }

    // ── AuthCommand::execute routing ─────────────────────────────────

    #[tokio::test]
    async fn auth_command_execute_routes_logout() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let cmd = AuthCommand {
            command: AuthSubcommands::Logout(LogoutCommand),
        };
        cmd.execute().await.unwrap();
    }

    #[tokio::test]
    async fn auth_command_execute_routes_status_and_surfaces_missing_credentials() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let cmd = AuthCommand {
            command: AuthSubcommands::Status(StatusCommand { all: false }),
        };
        let err = cmd.execute().await.unwrap_err();
        assert!(err.to_string().contains("not configured"));
    }

    // ── run_logout ───────────────────────────────────────────────────

    #[test]
    fn run_logout_reports_none_configured_when_absent() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();
        run_logout().unwrap();
    }

    #[test]
    fn run_logout_removes_previously_saved_credentials() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();
        auth::save_credentials(&test_credentials()).unwrap();

        run_logout().unwrap();
        let err = auth::load_credentials_for(None).unwrap_err();
        assert!(err.to_string().contains("not configured"));
    }

    #[test]
    fn logout_command_execute_delegates_to_run_logout() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();
        LogoutCommand.execute().unwrap();
    }

    // ── StatusCommand ────────────────────────────────────────────────

    #[tokio::test]
    async fn status_command_execute_errors_when_credentials_missing() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let err = StatusCommand { all: false }.execute().await.unwrap_err();
        assert!(err.to_string().contains("not configured"));
    }

    // ── run_login ────────────────────────────────────────────────────

    #[tokio::test]
    async fn run_login_surfaces_a_malformed_browser_command_before_the_oauth_exchange() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        Settings::upsert_drive_account(
            &settings_path,
            "work",
            &[(
                "browser_command",
                serde_json::Value::String("chrome \"--flag".to_string()),
            )],
        )
        .unwrap();
        std::env::set_var(crate::drive::account::DRIVE_ACCOUNT_ENV, "work");

        let env = MapEnv::new()
            .with(auth::DRIVE_CLIENT_ID, "id")
            .with(auth::DRIVE_CLIENT_SECRET, "secret");
        let err = run_login(&env, false).await.unwrap_err();
        assert!(!err.to_string().is_empty());
    }

    // ── run_auth_status ──────────────────────────────────────────────

    #[tokio::test]
    async fn run_auth_status_success() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/about"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "user": {"emailAddress": "user@example.com"},
                })),
            )
            .mount(&server)
            .await;

        run_auth_status(&client, DriveScope::ReadOnly)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_auth_status_api_error() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/about"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;

        let err = run_auth_status(&client, DriveScope::ReadOnly)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn run_auth_status_backfills_email_when_account_name_given() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        Settings::upsert_drive_account(
            &settings_path,
            "work",
            &[("client_id", serde_json::Value::String("id".to_string()))],
        )
        .unwrap();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/about"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "user": {"emailAddress": "work@example.com"},
                })),
            )
            .mount(&server)
            .await;

        run_auth_status_for(&client, DriveScope::ReadOnly, Some("work"))
            .await
            .unwrap();

        let settings = Settings::load().unwrap();
        assert_eq!(
            settings.drive.accounts["work"].email_address.as_deref(),
            Some("work@example.com")
        );
    }

    #[tokio::test]
    async fn run_auth_status_does_not_backfill_when_account_name_none() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/about"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "user": {"emailAddress": "user@example.com"},
                })),
            )
            .mount(&server)
            .await;

        // No account name: should not error, and there's no account entry
        // to backfill into (nothing to assert beyond a clean success).
        run_auth_status_for(&client, DriveScope::ReadOnly, None)
            .await
            .unwrap();
    }

    // ── run_auth_status_all_with ─────────────────────────────────────

    fn account_summary(name: &str) -> account::AccountSummary {
        account::AccountSummary {
            name: name.to_string(),
            email_address: None,
            scope: None,
            is_default: false,
        }
    }

    #[tokio::test]
    async fn run_auth_status_all_with_backfills_each_account_despite_uneven_latency() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        for name in ["work", "personal"] {
            Settings::upsert_drive_account(
                &settings_path,
                name,
                &[("client_id", serde_json::Value::String("id".to_string()))],
            )
            .unwrap();
        }

        // "work" is listed first but resolves last, proving the printed/
        // recorded results line back up with their own account rather than
        // whichever completed first.
        let server_work = wiremock::MockServer::start().await;
        let client_work = client_with_bootstrapped_token(&server_work).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/about"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(
                        serde_json::json!({"user": {"emailAddress": "work@example.com"}}),
                    )
                    .set_delay(std::time::Duration::from_millis(50)),
            )
            .mount(&server_work)
            .await;

        let server_personal = wiremock::MockServer::start().await;
        let client_personal = client_with_bootstrapped_token(&server_personal).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/about"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "user": {"emailAddress": "personal@example.com"},
                })),
            )
            .mount(&server_personal)
            .await;

        let clients = std::sync::Mutex::new(std::collections::HashMap::from([
            ("work".to_string(), (client_work, DriveScope::ReadOnly)),
            (
                "personal".to_string(),
                (client_personal, DriveScope::ReadOnly),
            ),
        ]));
        let client_for = |name: &str| -> Result<(DriveClient, DriveScope)> {
            clients
                .lock()
                .unwrap()
                .remove(name)
                .ok_or_else(|| anyhow::anyhow!("no test client for {name}"))
        };

        let accounts = vec![account_summary("work"), account_summary("personal")];
        run_auth_status_all_with(&accounts, &client_for)
            .await
            .unwrap();

        let settings = Settings::load().unwrap();
        assert_eq!(
            settings.drive.accounts["work"].email_address.as_deref(),
            Some("work@example.com")
        );
        assert_eq!(
            settings.drive.accounts["personal"].email_address.as_deref(),
            Some("personal@example.com")
        );
    }

    #[tokio::test]
    async fn run_auth_status_all_with_reports_error_for_one_account() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        for name in ["work", "broken"] {
            Settings::upsert_drive_account(
                &settings_path,
                name,
                &[("client_id", serde_json::Value::String("id".to_string()))],
            )
            .unwrap();
        }

        let server_work = wiremock::MockServer::start().await;
        let client_work = client_with_bootstrapped_token(&server_work).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/about"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "user": {"emailAddress": "work@example.com"},
                })),
            )
            .mount(&server_work)
            .await;

        let server_broken = wiremock::MockServer::start().await;
        let client_broken = client_with_bootstrapped_token(&server_broken).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/about"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server_broken)
            .await;

        let clients = std::sync::Mutex::new(std::collections::HashMap::from([
            ("work".to_string(), (client_work, DriveScope::ReadOnly)),
            ("broken".to_string(), (client_broken, DriveScope::ReadOnly)),
        ]));
        let client_for = |name: &str| -> Result<(DriveClient, DriveScope)> {
            clients
                .lock()
                .unwrap()
                .remove(name)
                .ok_or_else(|| anyhow::anyhow!("no test client for {name}"))
        };

        let accounts = vec![account_summary("work"), account_summary("broken")];
        // Errors are per-account and non-fatal: the overall call still
        // succeeds even though one account's status check failed.
        run_auth_status_all_with(&accounts, &client_for)
            .await
            .unwrap();

        let settings = Settings::load().unwrap();
        assert_eq!(
            settings.drive.accounts["work"].email_address.as_deref(),
            Some("work@example.com")
        );
        assert_eq!(settings.drive.accounts["broken"].email_address, None);
    }
}
