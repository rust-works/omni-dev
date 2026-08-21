//! CLI commands for Gmail credential management.

use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::enable_raw_mode;

use crate::cli::gmail::helpers;
use crate::gmail::account;
use crate::gmail::auth::{self, GmailScope};
use crate::gmail::client::GmailClient;
use crate::gmail::import;
use crate::gmail::profile_api::ProfileApi;
use crate::utils::env::{EnvSource, SystemEnv};
use crate::utils::secret::Secret;
use crate::utils::settings::{active_profile_from, profile_suffix, Settings, SettingsEnv};
use crate::utils::terminal::RawModeGuard;

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
    /// Imports an OAuth2 client id/secret from a downloaded `client_secret.json`.
    Import(ImportCommand),
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
            AuthSubcommands::Import(cmd) => cmd.execute(),
            AuthSubcommands::Login(cmd) => cmd.execute().await,
            AuthSubcommands::Logout(cmd) => cmd.execute(),
            AuthSubcommands::Status(cmd) => cmd.execute().await,
        }
    }
}

/// Imports an OAuth2 client id/secret from `client_secret.json`.
#[derive(Parser)]
pub struct ImportCommand {
    /// Explicit path to client_secret.json. Omit to auto-discover via
    /// $GMAIL_CLIENT_SECRET_FILE, ~/.config/gws/client_secret.json, or
    /// ~/Downloads/client_secret_*.apps.googleusercontent.com.json.
    pub path: Option<PathBuf>,
}

impl ImportCommand {
    /// Discovers, parses, and saves the client id/secret. Honors
    /// `--account`/`OMNI_DEV_GMAIL_ACCOUNT` (issue #1500).
    pub fn execute(self) -> Result<()> {
        let outcome = import::import_client_credentials_for(None, self.path.as_deref())?;
        println!("Found {} (Desktop app client)", outcome.path.display());
        println!("  Client id: {}", outcome.client_id);
        println!(
            "Client id/secret saved to ~/.omni-dev/settings.json{}",
            profile_suffix(active_profile_from(&SystemEnv).as_deref())
        );
        println!("\nRun `omni-dev gmail auth login` to authorize.");
        Ok(())
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
    /// environment or settings.json, prompting interactively when neither
    /// has them, then runs the login flow.
    pub async fn execute(self) -> Result<()> {
        run_login(&SettingsEnv::load(), self.modify).await
    }
}

/// [`LoginCommand::execute`] over an injected [`EnvSource`]. Production
/// passes a [`SettingsEnv`] (process env, falling back to settings.json);
/// tests pass a plain `MapEnv` representing the already-resolved value.
async fn run_login(env: &(impl EnvSource + Sync), modify: bool) -> Result<()> {
    let (client_id, client_secret) =
        resolve_login_credentials(env, prompt_client_id, prompt_client_secret)?;
    let scope = resolve_scope(modify);

    // Snapshot whether this call could be the first empty→non-empty
    // transition (issue #1500) *before* logging in — `login_for` mutates
    // settings.json on success, so the check must run first.
    let settings = Settings::load().unwrap_or_default();
    let might_shadow_legacy = {
        let legacy = auth::status();
        let had_legacy =
            legacy.has_client_id || legacy.has_client_secret || legacy.has_refresh_token;
        account::is_first_legacy_to_named_transition(&settings.gmail, had_legacy)
    };
    let browser = auth::resolve_browser_config_for(&settings.gmail, None)?;

    let status = auth::login_for(None, &client_id, &client_secret, scope, &browser).await?;

    if might_shadow_legacy {
        let settings = Settings::load().unwrap_or_default();
        if !settings.gmail.accounts.is_empty() {
            helpers::print_shadowing_notice();
        }
    }

    println!("\nCredentials saved to ~/.omni-dev/settings.json");
    println!("  Granted scope: {}", status.scope.unwrap_or_default());
    println!("\nRun `omni-dev gmail auth status` to verify.");
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
    let client_id = match env.var(auth::GMAIL_CLIENT_ID) {
        Some(v) => v,
        None => prompt_client_id()?,
    };
    let client_secret = match env.var(auth::GMAIL_CLIENT_SECRET) {
        Some(v) => Secret::new(v),
        None => prompt_client_secret()?,
    };
    Ok((client_id, client_secret))
}

/// Prompts for the OAuth2 client id on stdin. Echoes normally — the id
/// isn't secret (it's visible in the browser's own network traffic during
/// login regardless, same rationale as `GmailCredentials::client_id`).
fn prompt_client_id() -> Result<String> {
    print!(
        "GMAIL_CLIENT_ID is not set. Run `omni-dev gmail auth import` first, or paste it here \
         (see docs/gmail.md).\nClient id: "
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
    /// Removes Gmail credential keys from settings.json — from the
    /// resolved account (issue #1500), falling back to the active
    /// profile's `env` map / base `env` when no named account applies.
    pub fn execute(self) -> Result<()> {
        run_logout()
    }
}

fn run_logout() -> Result<()> {
    let removed = auth::remove_credentials_for(None)?;
    if removed {
        println!("Gmail credentials removed from ~/.omni-dev/settings.json");
    } else {
        println!("No Gmail credentials were configured.");
    }
    Ok(())
}

/// Shows the current authentication status.
#[derive(Parser)]
pub struct StatusCommand {
    /// Reports status for every configured Gmail account instead of just
    /// the resolved one. Degenerates to today's single-account output when
    /// no named accounts are configured (issue #1500).
    #[arg(long)]
    pub all: bool,
}

impl StatusCommand {
    /// Verifies credentials by calling `users.getProfile` — deliberately
    /// **live**, unlike the `gmail_auth_status` MCP tool's presence-only
    /// report, since that's the point of a CLI status command a human runs
    /// interactively.
    pub async fn execute(self) -> Result<()> {
        if self.all {
            return run_auth_status_all().await;
        }
        let credentials = auth::load_credentials_for(None)?;
        let scope = credentials.scope;
        let client = GmailClient::from_credentials(&credentials)?;
        run_auth_status(&client, scope).await
    }
}

/// `gmail auth status --all`: reports one status block per configured
/// account, backfilling each account's cached `email_address` on success.
/// Degenerates to [`run_auth_status`]'s single-account behavior when no
/// named accounts are configured (the zero-migration path).
async fn run_auth_status_all() -> Result<()> {
    let settings = Settings::load().unwrap_or_default();
    let accounts = account::list_accounts(&settings.gmail);
    if accounts.is_empty() {
        let credentials = auth::load_credentials_for(None)?;
        let scope = credentials.scope;
        let client = GmailClient::from_credentials(&credentials)?;
        return run_auth_status(&client, scope).await;
    }
    let client_for = |name: &str| -> Result<(GmailClient, GmailScope)> {
        let credentials = auth::load_credentials_for(Some(name))?;
        let scope = credentials.scope;
        let client = GmailClient::from_credentials(&credentials)?;
        Ok((client, scope))
    };
    run_auth_status_all_with(&accounts, &client_for).await
}

/// Runs every account's status check concurrently and prints the results
/// back out in `accounts`' original order — `join_all`'s output preserves
/// input order regardless of completion order, so no extra bookkeeping is
/// needed to undo the concurrency once the results are in hand.
///
/// `client_for` is a seam: production builds a client from stored
/// credentials, tests point it at a `wiremock` server per account name.
async fn run_auth_status_all_with<F>(
    accounts: &[account::AccountSummary],
    client_for: &F,
) -> Result<()>
where
    F: Fn(&str) -> Result<(GmailClient, GmailScope)> + Sync,
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

/// Loads `name`'s credentials, builds a client, and reports its status —
/// the per-account body of [`run_auth_status_all_with`]'s concurrent set.
async fn report_one_account_status<F>(name: &str, client_for: &F) -> Result<String>
where
    F: Fn(&str) -> Result<(GmailClient, GmailScope)> + Sync,
{
    let (client, scope) = client_for(name)?;
    run_auth_status_for(&client, scope, Some(name)).await
}

/// Calls `users.getProfile` and reports whether the stored refresh token is
/// still accepted.
async fn run_auth_status(client: &GmailClient, scope: GmailScope) -> Result<()> {
    let body = run_auth_status_for(client, scope, None).await?;
    print!("{body}");
    Ok(())
}

/// [`run_auth_status`], additionally backfilling `account_name`'s cached
/// `email_address` in settings.json on success when given (`gmail auth
/// status --all`, issue #1500) — the single-account path (`account_name:
/// None`) never touches settings.json. Returns the formatted report rather
/// than printing it, so callers running several accounts concurrently
/// (`run_auth_status_all_with`) can print them back out in a stable order
/// instead of interleaving live `println!`s.
async fn run_auth_status_for(
    client: &GmailClient,
    scope: GmailScope,
    account_name: Option<&str>,
) -> Result<String> {
    let mut out = String::new();
    out.push_str("Checking Gmail authentication...\n");

    let profile = ProfileApi::new(client).get().await?;

    out.push_str(&format!("Authenticated as: {}\n", profile.email_address));
    out.push_str(&format!(
        "Messages in mailbox: {}\n",
        profile.messages_total
    ));
    out.push_str(&format!(
        "Granted scope: {}\n",
        if scope.allows_modify() {
            "gmail.readonly, gmail.modify"
        } else {
            "gmail.readonly"
        }
    ));

    if let Some(name) = account_name {
        // Best-effort cache backfill: if two accounts in the same --all run
        // both need a fresh email, concurrent read-modify-write to
        // settings.json can drop one (no file lock). Self-heals on the next
        // status check, which always re-fetches the email live.
        auth::record_account_email(name, &profile.email_address)?;
    }
    Ok(out)
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

    #[test]
    fn auth_command_import_dispatch() {
        let cmd = AuthCommand {
            command: AuthSubcommands::Import(ImportCommand { path: None }),
        };
        assert!(matches!(cmd.command, AuthSubcommands::Import(_)));
    }

    // ── resolve_scope ────────────────────────────────────────────

    #[test]
    fn resolve_scope_maps_modify_flag() {
        assert_eq!(resolve_scope(true), GmailScope::Modify);
        assert_eq!(resolve_scope(false), GmailScope::ReadOnly);
    }

    // ── apply_secret_key ─────────────────────────────────────────

    #[test]
    fn apply_secret_key_pushes_a_pressed_character() {
        let mut buffer = String::new();
        let outcome = apply_secret_key(
            &mut buffer,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        );
        assert_eq!(outcome, SecretKeyOutcome::Continue);
        assert_eq!(buffer, "a");
    }

    #[test]
    fn apply_secret_key_ignores_a_release_event_for_the_same_character() {
        // Regression for the Windows/kitty-keyboard-protocol case: crossterm
        // delivers both Press and Release for every keystroke there, so a
        // naive match on `code` alone (ignoring `kind`) would push 'a' twice.
        let mut buffer = String::new();
        apply_secret_key(
            &mut buffer,
            KeyEvent::new_with_kind(KeyCode::Char('a'), KeyModifiers::NONE, KeyEventKind::Press),
        );
        let outcome = apply_secret_key(
            &mut buffer,
            KeyEvent::new_with_kind(
                KeyCode::Char('a'),
                KeyModifiers::NONE,
                KeyEventKind::Release,
            ),
        );
        assert_eq!(outcome, SecretKeyOutcome::Continue);
        assert_eq!(buffer, "a");
    }

    #[test]
    fn apply_secret_key_backspace_pops_the_last_character() {
        let mut buffer = "ab".to_string();
        apply_secret_key(
            &mut buffer,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        );
        assert_eq!(buffer, "a");
    }

    #[test]
    fn apply_secret_key_backspace_on_empty_buffer_is_a_no_op() {
        let mut buffer = String::new();
        let outcome = apply_secret_key(
            &mut buffer,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        );
        assert_eq!(outcome, SecretKeyOutcome::Continue);
        assert_eq!(buffer, "");
    }

    #[test]
    fn apply_secret_key_enter_finishes() {
        let mut buffer = "secret".to_string();
        let outcome = apply_secret_key(
            &mut buffer,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert_eq!(outcome, SecretKeyOutcome::Finished);
        assert_eq!(buffer, "secret");
    }

    #[test]
    fn apply_secret_key_ctrl_c_aborts() {
        let mut buffer = "partial".to_string();
        let outcome = apply_secret_key(
            &mut buffer,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );
        assert_eq!(outcome, SecretKeyOutcome::Aborted);
    }

    #[test]
    fn apply_secret_key_plain_c_is_not_mistaken_for_ctrl_c() {
        let mut buffer = String::new();
        let outcome = apply_secret_key(
            &mut buffer,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
        );
        assert_eq!(outcome, SecretKeyOutcome::Continue);
        assert_eq!(buffer, "c");
    }

    // ── AuthCommand::execute dispatch ───────────────────────────────
    //
    // Login's execute() prompts on stdin when credentials are missing (no
    // more hard "missing credentials" error to assert against without a
    // real terminal), so its dispatch is covered by the no-IO
    // `auth_command_login_dispatch` test above instead — same reasoning
    // `datadog`/`atlassian`'s always-prompting `LoginCommand::execute`
    // already follow (no automated test of the real stdin path, only of
    // the extracted pure logic below).

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
            command: AuthSubcommands::Status(StatusCommand { all: false }),
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
            command: AuthSubcommands::Status(StatusCommand { all: false }),
        };
        assert!(matches!(cmd.command, AuthSubcommands::Status(_)));
    }

    // ── resolve_login_credentials ───────────────────────────────────

    #[test]
    fn resolve_login_credentials_uses_env_when_present() {
        let env = MapEnv::new()
            .with(GMAIL_CLIENT_ID, "id")
            .with(GMAIL_CLIENT_SECRET, "sec");
        let (id, secret) = resolve_login_credentials(
            &env,
            || anyhow::bail!("should not prompt for client id"),
            || anyhow::bail!("should not prompt for client secret"),
        )
        .unwrap();
        assert_eq!(id, "id");
        assert_eq!(secret.expose_secret(), "sec");
    }

    #[test]
    fn resolve_login_credentials_prompts_for_missing_client_id_only() {
        let env = MapEnv::new().with(GMAIL_CLIENT_SECRET, "sec");
        let (id, secret) = resolve_login_credentials(
            &env,
            || Ok("prompted-id".to_string()),
            || anyhow::bail!("should not prompt for client secret"),
        )
        .unwrap();
        assert_eq!(id, "prompted-id");
        assert_eq!(secret.expose_secret(), "sec");
    }

    #[test]
    fn resolve_login_credentials_prompts_for_missing_client_secret_only() {
        let env = MapEnv::new().with(GMAIL_CLIENT_ID, "id");
        let (id, secret) = resolve_login_credentials(
            &env,
            || anyhow::bail!("should not prompt for client id"),
            || Ok(Secret::new("prompted-secret")),
        )
        .unwrap();
        assert_eq!(id, "id");
        assert_eq!(secret.expose_secret(), "prompted-secret");
    }

    #[test]
    fn resolve_login_credentials_prompts_for_both_when_env_empty() {
        let env = MapEnv::new();
        let (id, secret) = resolve_login_credentials(
            &env,
            || Ok("prompted-id".to_string()),
            || Ok(Secret::new("prompted-secret")),
        )
        .unwrap();
        assert_eq!(id, "prompted-id");
        assert_eq!(secret.expose_secret(), "prompted-secret");
    }

    #[test]
    fn resolve_login_credentials_propagates_client_id_prompt_error() {
        let env = MapEnv::new();
        let err = resolve_login_credentials(
            &env,
            || anyhow::bail!("Aborted"),
            || anyhow::bail!("should not be called"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("Aborted"));
    }

    #[test]
    fn resolve_login_credentials_propagates_client_secret_prompt_error() {
        let env = MapEnv::new().with(GMAIL_CLIENT_ID, "id");
        let err = resolve_login_credentials(
            &env,
            || anyhow::bail!("should not prompt for client id"),
            || anyhow::bail!("Aborted"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("Aborted"));
    }

    // ── run_login ─────────────────────────────────────────────────
    //
    // The OAuth2 exchange itself (`login_for`, reached only after browser
    // resolution succeeds) needs a real loopback callback and is covered at
    // the `gmail::auth::login_to` layer instead — see the "AuthCommand::execute
    // dispatch" note above for why `run_login`'s happy path stays untested
    // here. This covers the settings-load/browser-resolution prelude that
    // runs before that exchange, by making browser resolution itself fail
    // fast (a malformed `browser_command`) so the flow never reaches the
    // network.

    #[tokio::test]
    async fn run_login_surfaces_a_malformed_browser_command_before_the_oauth_exchange() {
        let guard = EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        Settings::upsert_gmail_account(
            &settings_path,
            "work",
            &[(
                "browser_command",
                serde_json::Value::String("chrome \"--flag".to_string()),
            )],
        )
        .unwrap();

        let env = MapEnv::new()
            .with(GMAIL_CLIENT_ID, "id")
            .with(GMAIL_CLIENT_SECRET, "secret");

        let err = run_login(&env, false).await.unwrap_err();
        assert!(err.to_string().contains("browser_command"));
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

        let err = StatusCommand { all: false }.execute().await.unwrap_err();
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
                    "threadsTotal": 50,
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
                    "threadsTotal": 50,
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
        let guard = EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        for name in ["work", "personal"] {
            Settings::upsert_gmail_account(
                &settings_path,
                name,
                &[("client_id", serde_json::Value::String("id".to_string()))],
            )
            .unwrap();
        }

        // "work" is listed first but resolves last, proving the recorded
        // results line back up with their own account rather than whichever
        // completed first.
        let server_work = wiremock::MockServer::start().await;
        mount_token_endpoint(&server_work).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/profile"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({
                        "emailAddress": "work@example.com",
                        "messagesTotal": 100,
                        "threadsTotal": 50,
                        "historyId": "1000",
                    }))
                    .set_delay(std::time::Duration::from_millis(50)),
            )
            .mount(&server_work)
            .await;
        let client_work = mock_client(&server_work.uri());

        let server_personal = wiremock::MockServer::start().await;
        mount_token_endpoint(&server_personal).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/profile"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "emailAddress": "personal@example.com",
                    "messagesTotal": 5,
                    "threadsTotal": 2,
                    "historyId": "2000",
                })),
            )
            .mount(&server_personal)
            .await;
        let client_personal = mock_client(&server_personal.uri());

        let clients = std::sync::Mutex::new(std::collections::HashMap::from([
            ("work".to_string(), (client_work, GmailScope::ReadOnly)),
            (
                "personal".to_string(),
                (client_personal, GmailScope::ReadOnly),
            ),
        ]));
        let client_for = |name: &str| -> Result<(GmailClient, GmailScope)> {
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
            settings.gmail.accounts["work"].email_address.as_deref(),
            Some("work@example.com")
        );
        assert_eq!(
            settings.gmail.accounts["personal"].email_address.as_deref(),
            Some("personal@example.com")
        );
    }

    #[tokio::test]
    async fn run_auth_status_all_with_reports_error_for_one_account() {
        let guard = EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        for name in ["work", "broken"] {
            Settings::upsert_gmail_account(
                &settings_path,
                name,
                &[("client_id", serde_json::Value::String("id".to_string()))],
            )
            .unwrap();
        }

        let server_work = wiremock::MockServer::start().await;
        mount_token_endpoint(&server_work).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/profile"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "emailAddress": "work@example.com",
                    "messagesTotal": 100,
                    "threadsTotal": 50,
                    "historyId": "1000",
                })),
            )
            .mount(&server_work)
            .await;
        let client_work = mock_client(&server_work.uri());

        let server_broken = wiremock::MockServer::start().await;
        mount_token_endpoint(&server_broken).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/profile"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server_broken)
            .await;
        let client_broken = mock_client(&server_broken.uri());

        let clients = std::sync::Mutex::new(std::collections::HashMap::from([
            ("work".to_string(), (client_work, GmailScope::ReadOnly)),
            ("broken".to_string(), (client_broken, GmailScope::ReadOnly)),
        ]));
        let client_for = |name: &str| -> Result<(GmailClient, GmailScope)> {
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
            settings.gmail.accounts["work"].email_address.as_deref(),
            Some("work@example.com")
        );
        assert_eq!(settings.gmail.accounts["broken"].email_address, None);
    }
}
