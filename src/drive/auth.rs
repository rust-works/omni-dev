//! Drive OAuth2 authentication: authorization-code + PKCE login, credential
//! storage, and in-memory access-token refresh.
//!
//! See [ADR-0069](../../../docs/adrs/adr-0069.md) for the design rationale
//! (applying [ADR-0063](../../../docs/adrs/adr-0063.md), Gmail's OAuth2
//! credential-storage design, to a second Google API). The loopback-listener
//! and browser-launch shape follows `crate::gmail::auth`, itself following
//! the Snowflake client's external-browser SSO flow
//! (`crate::snowflake::client`'s private `auth` module), extended with PKCE
//! (RFC 7636), a `state` nonce, and an `error=` branch — none of which a
//! static-token or SSO-only flow needs.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine as _;
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;

use crate::drive::account::{self, ResolvedAccount};
use crate::drive::chrome_profile;
use crate::drive::error::{DriveError, GrantContext};
use crate::request_log;
use crate::utils::browser_command::split_browser_command;
use crate::utils::env::SystemEnv;
use crate::utils::secret::Secret;
use crate::utils::settings::{active_profile_from, DriveAccountSettings, DriveSettings, Settings};

/// Environment variable / settings key for the user's Google Cloud OAuth2
/// client id.
pub const DRIVE_CLIENT_ID: &str = "DRIVE_CLIENT_ID";
/// Environment variable / settings key for the user's Google Cloud OAuth2
/// client secret.
pub const DRIVE_CLIENT_SECRET: &str = "DRIVE_CLIENT_SECRET";
/// Environment variable / settings key for the stored OAuth2 refresh token.
pub const DRIVE_REFRESH_TOKEN: &str = "DRIVE_REFRESH_TOKEN";
/// Environment variable / settings key recording the scope granted at login.
pub const DRIVE_SCOPE: &str = "DRIVE_SCOPE";
/// Environment variable overriding the real Drive API host.
///
/// Process-env only — never written to `settings.json` by `auth login`,
/// unlike the four keys above (`crate::drive::client::DriveClient`'s
/// default base URL). Useful for:
/// - Tests that point at a wiremock server (e.g. `http://127.0.0.1:PORT`).
/// - Environments where outbound traffic must go through a forced proxy.
///
/// Mirrors `GMAIL_API_URL` (`crate::gmail::auth`); Drive has no per-tenant
/// site/region the override is *deriving from* — it's a flat replacement of
/// the one real host, not a site substitution.
pub const DRIVE_API_URL: &str = "DRIVE_API_URL";

/// Google's OAuth2 authorization endpoint. Identical to Gmail's — shared
/// Google infrastructure, not a Drive-specific host.
const AUTHORIZATION_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
/// Google's OAuth2 token endpoint. Identical to Gmail's.
const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
/// The single read-only Drive scope this feature ever requests.
///
/// Unlike Gmail's readonly/modify split, there is no `DriveScope` enum —
/// this feature is read-only by design (see
/// [ADR-0069](../../../docs/adrs/adr-0069.md) §2).
pub const SCOPE_READONLY: &str = "https://www.googleapis.com/auth/drive.readonly";

/// How long to wait for the browser sign-in callback before giving up.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(120);
/// How much slack to leave before an access token's tracked expiry before
/// proactively refreshing it.
const REFRESH_SKEW: TimeDelta = TimeDelta::seconds(60);
/// Upper bound on a trusted `expires_in` from the token endpoint. Comfortably
/// inside what `TimeDelta::seconds` and `DateTime<Utc>` addition can
/// represent without panicking, and far beyond any real OAuth token
/// lifetime — an out-of-range value is clamped rather than trusted, so a
/// misbehaving or malicious token endpoint can't crash the process (#1531).
const MAX_EXPIRES_IN_SECONDS: i64 = 100 * 365 * 24 * 60 * 60;

/// Drive OAuth2 credentials.
#[derive(Debug, Clone)]
pub struct DriveCredentials {
    /// OAuth2 client id (not secret — visible in the browser's own network
    /// traffic during login regardless).
    pub client_id: String,
    /// OAuth2 client secret (redacted in `Debug` output).
    pub client_secret: Secret,
    /// The stored refresh token (redacted in `Debug` output).
    pub refresh_token: Secret,
    /// The scope granted at the login that produced this refresh token.
    /// Always [`SCOPE_READONLY`] once granted — stored as a plain `String`
    /// (not an enum, unlike Gmail's `GmailScope`) purely for
    /// status-reporting parity with Gmail.
    pub scope: String,
}

/// Secret-free presence/scope report, safe to serialise (e.g. over MCP).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DriveAuthStatus {
    /// Whether [`DRIVE_CLIENT_ID`] is present.
    pub has_client_id: bool,
    /// Whether [`DRIVE_CLIENT_SECRET`] is present.
    pub has_client_secret: bool,
    /// Whether [`DRIVE_REFRESH_TOKEN`] is present.
    pub has_refresh_token: bool,
    /// The granted scope, if recorded. `None` when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// Resolves the active Drive account for this call (mirrors Gmail's issue
/// #1500), folding an explicit per-call override together with the ambient
/// `--account`/[`account::DRIVE_ACCOUNT_ENV`] value. The one seam every
/// credential CRUD entry point in this module routes through.
pub(crate) fn resolve(drive: &DriveSettings, explicit: Option<&str>) -> Result<ResolvedAccount> {
    let explicit = fold_explicit(explicit);
    account::resolve_account(&SystemEnv, drive, explicit.as_deref())
}

/// Like [`resolve`], but for account-creating writes (`drive auth login`,
/// `drive auth import`) — see [`account::resolve_account_for_write`] for why
/// an explicit target need not already exist.
pub(crate) fn resolve_for_write(
    drive: &DriveSettings,
    explicit: Option<&str>,
) -> Result<ResolvedAccount> {
    let explicit = fold_explicit(explicit);
    account::resolve_account_for_write(&SystemEnv, drive, explicit.as_deref())
}

/// Folds an explicit per-call account override together with the ambient
/// `--account`/[`account::DRIVE_ACCOUNT_ENV`] value — shared by [`resolve`]
/// and [`resolve_for_write`].
fn fold_explicit(explicit: Option<&str>) -> Option<String> {
    explicit
        .map(str::to_string)
        .or_else(|| account::active_drive_account_from(&SystemEnv))
}

/// Resolves the [`BrowserConfig`] `drive auth login` should open the
/// authorization URL with, honoring a named account's manual
/// `browser_command` override and opt-in automatic Chrome-profile
/// resolution (mirrors Gmail's issue #1505). `explicit` is folded exactly
/// like [`resolve_for_write`]'s. A [`ResolvedAccount::Unconfigured`] account
/// (no named accounts configured, or a literal credential env set) always
/// yields [`BrowserLaunch::Auto`].
pub(crate) fn resolve_browser_config_for(
    drive: &DriveSettings,
    explicit: Option<&str>,
) -> Result<BrowserConfig> {
    match resolve_for_write(drive, explicit)? {
        ResolvedAccount::Unconfigured => Ok(BrowserConfig::default()),
        ResolvedAccount::Named(name) => build_browser_config(
            drive.accounts.get(&name),
            chrome_profile::resolve_launch_command,
        ),
    }
}

/// The pure/injectable core of [`resolve_browser_config_for`] —
/// `resolve_chrome_profile` is [`chrome_profile::resolve_launch_command`] in
/// production, a stub in tests, so this stays testable without touching a
/// real Chrome install.
///
/// Precedence:
/// 1. `account.browser_command` set (non-blank) → used verbatim; a
///    malformed command is a hard error.
/// 2. `account.chrome_profile_from_email` set *and* `account.email_address`
///    set → automatic resolution; any resolution failure (per
///    `resolve_chrome_profile`'s fail-open contract) falls back to `Auto`.
/// 3. Otherwise → `Auto`.
fn build_browser_config(
    account: Option<&DriveAccountSettings>,
    resolve_chrome_profile: impl FnOnce(&str) -> Option<Vec<String>>,
) -> Result<BrowserConfig> {
    let Some(account) = account else {
        return Ok(BrowserConfig::default());
    };

    if let Some(command) = account
        .browser_command
        .as_deref()
        .map(str::trim)
        .filter(|command| !command.is_empty())
    {
        return Ok(BrowserConfig {
            launch: BrowserLaunch::Command(split_browser_command("browser_command", command)?),
            ..BrowserConfig::default()
        });
    }

    if account.chrome_profile_from_email {
        if let Some(email) = account.email_address.as_deref() {
            if let Some(args) = resolve_chrome_profile(email) {
                return Ok(BrowserConfig {
                    launch: BrowserLaunch::Command(args),
                    ..BrowserConfig::default()
                });
            }
        } else {
            tracing::info!(
                "chrome_profile_from_email is set but email_address is not; \
                 falling back to the default browser"
            );
        }
    }

    Ok(BrowserConfig::default())
}

/// Loads Drive credentials from environment variables or settings.json.
///
/// Environment variables take precedence over the settings file.
pub fn load_credentials() -> Result<DriveCredentials> {
    load_credentials_with(&crate::utils::settings::SettingsEnv::load())
}

/// [`load_credentials`], but honoring the named-account resolution (mirrors
/// Gmail's issue #1500). `explicit` is the already-resolved `--account`/
/// [`account::DRIVE_ACCOUNT_ENV`] override, if any (`None` still resolves
/// the ambient env var — see [`resolve`]). Falls through to
/// [`load_credentials_with`]'s exact behavior when no named account applies
/// — an empty `drive.accounts` map or the literal-env bypass both resolve
/// to [`ResolvedAccount::Unconfigured`], and [`load_credentials_with`]
/// naturally fails loudly with [`DriveError::CredentialsNotFound`] (whose
/// message names `drive auth login`) when there is nothing to load.
pub(crate) fn load_credentials_for(explicit: Option<&str>) -> Result<DriveCredentials> {
    let settings = Settings::load().unwrap_or_default();
    match resolve(&settings.drive, explicit)? {
        ResolvedAccount::Unconfigured => {
            let profile = active_profile_from(&SystemEnv);
            load_credentials_with(&crate::utils::settings::SettingsEnv::from_settings(
                settings,
                profile.as_deref(),
            ))
        }
        ResolvedAccount::Named(name) => load_named_credentials(&settings.drive, &name),
    }
}

/// Reads `drive.accounts.<name>` into [`DriveCredentials`], wrapping
/// `client_secret`/`refresh_token` into [`Secret`] immediately, mirroring
/// [`load_credentials_with`].
fn load_named_credentials(drive: &DriveSettings, name: &str) -> Result<DriveCredentials> {
    let account = drive
        .accounts
        .get(name)
        .ok_or(DriveError::CredentialsNotFound)?;
    let client_id = account
        .client_id
        .clone()
        .ok_or(DriveError::CredentialsNotFound)?;
    let client_secret = account
        .client_secret
        .clone()
        .ok_or(DriveError::CredentialsNotFound)?;
    let refresh_token = account
        .refresh_token
        .clone()
        .ok_or(DriveError::CredentialsNotFound)?;
    let scope = account
        .scope
        .clone()
        .unwrap_or_else(|| SCOPE_READONLY.to_string());

    Ok(DriveCredentials {
        client_id,
        client_secret: client_secret.into(),
        refresh_token: refresh_token.into(),
        scope,
    })
}

/// [`load_credentials`] over an injected
/// [`EnvSource`](crate::utils::env::EnvSource).
///
/// Tests pass a pure `MapEnv` so credential resolution is exercised without
/// mutating the process environment (issue #1030 / STYLE-0028).
pub(crate) fn load_credentials_with(
    env: &impl crate::utils::env::EnvSource,
) -> Result<DriveCredentials> {
    let client_id = env
        .var(DRIVE_CLIENT_ID)
        .ok_or(DriveError::CredentialsNotFound)?;
    let client_secret = env
        .var(DRIVE_CLIENT_SECRET)
        .ok_or(DriveError::CredentialsNotFound)?;
    let refresh_token = env
        .var(DRIVE_REFRESH_TOKEN)
        .ok_or(DriveError::CredentialsNotFound)?;
    let scope = env
        .var(DRIVE_SCOPE)
        .unwrap_or_else(|| SCOPE_READONLY.to_string());

    Ok(DriveCredentials {
        client_id,
        client_secret: client_secret.into(),
        refresh_token: refresh_token.into(),
        scope,
    })
}

/// Builds a [`DriveAuthStatus`] from the current settings / environment.
///
/// Reports credential presence without leaking any secret values. Safe to
/// call with no credentials configured.
pub fn status() -> DriveAuthStatus {
    status_with(&crate::utils::settings::SettingsEnv::load())
}

/// [`status`] over an injected [`EnvSource`](crate::utils::env::EnvSource).
pub(crate) fn status_with(env: &impl crate::utils::env::EnvSource) -> DriveAuthStatus {
    DriveAuthStatus {
        has_client_id: env.var(DRIVE_CLIENT_ID).is_some(),
        has_client_secret: env.var(DRIVE_CLIENT_SECRET).is_some(),
        has_refresh_token: env.var(DRIVE_REFRESH_TOKEN).is_some(),
        scope: env.var(DRIVE_SCOPE),
    }
}

/// [`status`], but honoring the named-account resolution (mirrors Gmail's
/// issue #1500). `explicit` is the already-resolved `--account`/
/// [`account::DRIVE_ACCOUNT_ENV`] override, if any. Unlike [`status`], this
/// can fail — once named accounts exist, resolution itself can (e.g. an
/// unknown or ambiguous account) — so callers that want [`status`]'s
/// never-fails presence report keep calling that instead.
///
/// Only compiled with the `mcp` feature — the MCP `drive_auth_status` tool
/// is its sole consumer; the CLI's `drive auth status` goes through
/// [`load_credentials_for`] instead.
#[cfg(feature = "mcp")]
pub(crate) fn status_for(explicit: Option<&str>) -> Result<DriveAuthStatus> {
    let settings = Settings::load().unwrap_or_default();
    match resolve(&settings.drive, explicit)? {
        ResolvedAccount::Unconfigured => {
            let profile = active_profile_from(&SystemEnv);
            Ok(status_with(
                &crate::utils::settings::SettingsEnv::from_settings(settings, profile.as_deref()),
            ))
        }
        ResolvedAccount::Named(name) => Ok(status_from_named(&settings.drive, &name)),
    }
}

/// Builds a [`DriveAuthStatus`] from `drive.accounts.<name>`'s presence
/// flags — the named-account counterpart of [`status_with`].
///
/// Only compiled with the `mcp` feature — see [`status_for`], its sole
/// caller.
#[cfg(feature = "mcp")]
fn status_from_named(drive: &DriveSettings, name: &str) -> DriveAuthStatus {
    let account = drive.accounts.get(name);
    DriveAuthStatus {
        has_client_id: account.is_some_and(|a| a.client_id.is_some()),
        has_client_secret: account.is_some_and(|a| a.client_secret.is_some()),
        has_refresh_token: account.is_some_and(|a| a.refresh_token.is_some()),
        scope: account.and_then(|a| a.scope.clone()),
    }
}

/// Opportunistic `email_address` backfill for `name`, populated by `drive
/// auth status --all` after a successful live API call. Never used for
/// authentication, never written by `login`/`import`. A no-op when `name`
/// already has an `email_address` — an explicit or previously-backfilled
/// value is never overwritten (mirrors Gmail's issue #1505).
pub(crate) fn record_account_email(name: &str, email: &str) -> Result<()> {
    let settings = Settings::load().unwrap_or_default();
    if settings
        .drive
        .accounts
        .get(name)
        .is_some_and(|account| account.email_address.is_some())
    {
        return Ok(());
    }
    Settings::upsert_drive_account(
        &Settings::get_settings_path()?,
        name,
        &[(
            "email_address",
            serde_json::Value::String(email.to_string()),
        )],
    )
}

/// Saves Drive credentials to `~/.omni-dev/settings.json`.
///
/// Merges the four credential keys into the active profile's `env` map (the
/// base `env` when no profile is active), preserving all other settings.
pub fn save_credentials(credentials: &DriveCredentials) -> Result<()> {
    save_credentials_to(
        &Settings::get_settings_path()?,
        active_profile_from(&SystemEnv).as_deref(),
        credentials,
    )
}

/// [`save_credentials`], writing to an explicit settings-file path and env
/// map (`profiles.<name>.env` when `profile` is `Some`, base `env` otherwise).
pub(crate) fn save_credentials_to(
    settings_path: &Path,
    profile: Option<&str>,
    credentials: &DriveCredentials,
) -> Result<()> {
    Settings::upsert_env_vars_in(
        settings_path,
        profile,
        &[
            (DRIVE_CLIENT_ID, credentials.client_id.as_str()),
            (
                DRIVE_CLIENT_SECRET,
                credentials.client_secret.expose_secret(),
            ),
            (
                DRIVE_REFRESH_TOKEN,
                credentials.refresh_token.expose_secret(),
            ),
            (DRIVE_SCOPE, credentials.scope.as_str()),
        ],
    )
}

/// The `drive.accounts.<name>` field names/values for `credentials` — the
/// named-account counterpart of the flat `DRIVE_*` env keys
/// [`save_credentials_to`] writes.
fn named_account_vars(credentials: &DriveCredentials) -> [(&str, serde_json::Value); 4] {
    [
        (
            "client_id",
            serde_json::Value::String(credentials.client_id.clone()),
        ),
        (
            "client_secret",
            serde_json::Value::String(credentials.client_secret.expose_secret().to_string()),
        ),
        (
            "refresh_token",
            serde_json::Value::String(credentials.refresh_token.expose_secret().to_string()),
        ),
        (
            "scope",
            serde_json::Value::String(credentials.scope.clone()),
        ),
    ]
}

/// Removes Drive credential keys from `~/.omni-dev/settings.json` — this
/// *is* `drive auth logout`.
///
/// Returns `true` if any Drive key was present and removed, `false`
/// otherwise.
pub fn remove_credentials() -> Result<bool> {
    remove_credentials_at(
        &Settings::get_settings_path()?,
        active_profile_from(&SystemEnv).as_deref(),
    )
}

/// [`remove_credentials`], operating on an explicit settings-file path and
/// env map.
pub(crate) fn remove_credentials_at(settings_path: &Path, profile: Option<&str>) -> Result<bool> {
    Settings::remove_env_vars_in(
        settings_path,
        profile,
        &[
            DRIVE_CLIENT_ID,
            DRIVE_CLIENT_SECRET,
            DRIVE_REFRESH_TOKEN,
            DRIVE_SCOPE,
        ],
    )
}

/// [`remove_credentials`], but honoring the named-account resolution
/// (mirrors Gmail's issue #1500). `explicit` is the already-resolved
/// `--account`/[`account::DRIVE_ACCOUNT_ENV`] override, if any. Removes the
/// whole `drive.accounts.<name>` entry — an account is coherent as a unit.
pub(crate) fn remove_credentials_for(explicit: Option<&str>) -> Result<bool> {
    let settings = Settings::load().unwrap_or_default();
    match resolve(&settings.drive, explicit)? {
        ResolvedAccount::Unconfigured => remove_credentials_at(
            &Settings::get_settings_path()?,
            active_profile_from(&SystemEnv).as_deref(),
        ),
        ResolvedAccount::Named(name) => {
            Settings::remove_drive_account(&Settings::get_settings_path()?, &name)
        }
    }
}

// ── Browser launch ──────────────────────────────────────────────────────

/// How to open the authorization URL during login.
///
/// Deliberately duplicated from (not shared with) `crate::gmail::auth`'s
/// identical type (itself duplicated from
/// [`crate::snowflake::client::config::BrowserLaunch`]) — a small, stable
/// shape with no existing "generic browser launch" module to promote into;
/// extract only on a third consumer (see
/// [ADR-0069](../../../docs/adrs/adr-0069.md) §4).
#[derive(Clone, Debug, Default)]
pub enum BrowserLaunch {
    /// Open with the OS default handler (`open` / `xdg-open` / `start`).
    #[default]
    Auto,
    /// Run a custom command; `{url}` (or a trailing arg) receives the
    /// authorization URL. Use this to target a specific Chrome profile,
    /// e.g. `Google Chrome --profile-directory=Profile 1 --new-window {url}`.
    Command(Vec<String>),
    /// Do not open a browser; the authorization URL is logged for manual
    /// opening.
    Manual,
}

/// Loopback OAuth2 callback settings.
#[derive(Clone, Debug)]
pub struct BrowserConfig {
    /// How to open the authorization URL.
    pub launch: BrowserLaunch,
    /// Bind address for the loopback callback listener.
    pub callback_addr: IpAddr,
    /// Bind port for the callback listener (`0` = OS-assigned ephemeral port).
    pub callback_port: u16,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            launch: BrowserLaunch::Auto,
            callback_addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            callback_port: 0,
        }
    }
}

/// Opens `url` in the configured browser.
// `{url}` is a literal command placeholder we substitute, not a format string.
#[allow(clippy::literal_string_with_formatting_args)]
fn open_browser(launch: &BrowserLaunch, url: &str) -> Result<()> {
    match launch {
        BrowserLaunch::Manual => {
            tracing::info!("Open this URL in a browser to sign in to Drive:\n{url}");
            Ok(())
        }
        BrowserLaunch::Command(args) => {
            let mut parts = args.iter();
            let program = parts
                .next()
                .ok_or_else(|| DriveError::InvalidBrowserCommand("empty browser command".into()))?;
            let mut command = Command::new(program);
            let mut placed = false;
            for arg in parts {
                if arg.contains("{url}") {
                    command.arg(arg.replace("{url}", url));
                    placed = true;
                } else {
                    command.arg(arg);
                }
            }
            if !placed {
                command.arg(url);
            }
            spawn_detached(command)
        }
        BrowserLaunch::Auto => {
            let program = if cfg!(target_os = "macos") {
                "open"
            } else if cfg!(target_os = "windows") {
                "explorer"
            } else {
                "xdg-open"
            };
            let mut command = Command::new(program);
            command.arg(url);
            spawn_detached(command)
        }
    }
}

/// Spawns a browser command detached from this process's stdio.
fn spawn_detached(mut command: Command) -> Result<()> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .context("Failed to launch the browser")
}

// ── PKCE + state ────────────────────────────────────────────────────────

/// A pending login's PKCE verifier and CSRF `state` nonce, generated fresh
/// per login attempt and never persisted.
struct PendingLogin {
    state: String,
    code_verifier: String,
}

fn generate_pending_login() -> PendingLogin {
    PendingLogin {
        state: crate::browser::auth::generate_token(),
        code_verifier: crate::browser::auth::generate_token(),
    }
}

/// Derives the PKCE `code_challenge` (RFC 7636, `S256` method) from a
/// `code_verifier`.
fn code_challenge(code_verifier: &str) -> String {
    let digest = Sha256::digest(code_verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

fn build_authorization_url(
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    code_challenge: &str,
) -> Result<Url> {
    let mut url =
        Url::parse(AUTHORIZATION_ENDPOINT).context("Invalid Drive authorization endpoint")?;
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", SCOPE_READONLY)
        .append_pair("state", state)
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("access_type", "offline")
        // Without forcing re-consent, Google may not re-issue a refresh
        // token on a second login — which would silently break re-auth
        // after the 7-day testing-mode refresh-token expiry.
        .append_pair("prompt", "consent");
    Ok(url)
}

// ── Loopback callback capture ───────────────────────────────────────────

/// The parsed loopback callback: either `code`+`state`, or an `error`
/// (optionally with `error_description`).
#[derive(Debug)]
pub(crate) struct CallbackResult {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// Binds the loopback callback listener, returning it along with the
/// OS-assigned port so the authorization URL's `redirect_uri` can be built
/// before the browser is opened.
pub(crate) async fn bind_callback_listener(browser: &BrowserConfig) -> Result<(TcpListener, u16)> {
    let listener = TcpListener::bind(SocketAddr::new(
        browser.callback_addr,
        browser.callback_port,
    ))
    .await
    .context("Failed to start the local OAuth callback listener")?;
    let port = listener
        .local_addr()
        .context("Failed to read the callback listener's port")?
        .port();
    Ok((listener, port))
}

/// Waits for the browser's callback connection using the default
/// [`CALLBACK_TIMEOUT`].
pub(crate) async fn wait_for_callback(listener: TcpListener) -> Result<CallbackResult> {
    wait_for_callback_with_timeout(listener, CALLBACK_TIMEOUT).await
}

/// Accepts one loopback connection and extracts the OAuth callback's query
/// parameters from the redirected `GET` request line.
///
/// Never logs the raw request or query string — only that a callback was
/// received — so the authorization `code` can never reach the request log
/// via this path (see ADR-0063's redaction discussion).
pub(crate) async fn wait_for_callback_with_timeout(
    listener: TcpListener,
    timeout: Duration,
) -> Result<CallbackResult> {
    let (mut stream, _addr) = tokio::time::timeout(timeout, listener.accept())
        .await
        .map_err(|_| DriveError::CallbackTimeout(timeout.as_secs()))?
        .context("Failed to accept the browser's callback connection")?;

    let mut buf = vec![0u8; 8192];
    let n = stream
        .read(&mut buf)
        .await
        .context("Failed to read the callback request")?;
    let request = String::from_utf8_lossy(&buf[..n]);

    let result = parse_callback(&request).ok_or(DriveError::MalformedCallback)?;
    tracing::info!("Drive OAuth callback received");

    let body = if result.error.is_some() {
        "<html><body>Sign-in failed. You can close this tab and check the terminal.</body></html>"
    } else {
        "<html><body>Drive sign-in complete. You can close this tab.</body></html>"
    };
    let response =
        format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n{body}");
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;

    Ok(result)
}

/// Extracts `code`/`state`/`error`/`error_description` from an HTTP
/// request's first line only — headers and body are never inspected.
fn parse_callback(request: &str) -> Option<CallbackResult> {
    let first_line = request.lines().next()?;
    let path = first_line.split_whitespace().nth(1)?; // "/?code=…&state=…"
    let query = path.split_once('?')?.1;

    let mut result = CallbackResult {
        code: None,
        state: None,
        error: None,
        error_description: None,
    };
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "code" => result.code = Some(value.into_owned()),
            "state" => result.state = Some(value.into_owned()),
            "error" => result.error = Some(value.into_owned()),
            "error_description" => result.error_description = Some(value.into_owned()),
            _ => {}
        }
    }
    Some(result)
}

// ── Token exchange / refresh ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: i64,
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenErrorResponse {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

async fn exchange_code_for_tokens(
    http: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    client_secret: &str,
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
) -> Result<TokenResponse> {
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("client_id", client_id),
        ("client_secret", client_secret),
        ("redirect_uri", redirect_uri),
        ("code_verifier", code_verifier),
    ];
    post_token_request(http, token_endpoint, &params, GrantContext::CodeExchange).await
}

async fn refresh_access_token(
    http: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
) -> Result<TokenResponse> {
    let params = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
        ("client_secret", client_secret),
    ];
    post_token_request(http, token_endpoint, &params, GrantContext::Refresh).await
}

/// POSTs a token request. All secrets travel in the `.form(...)` body, never
/// the URL — `token_endpoint` carries no query string, so the request-log's
/// URL redaction has nothing to redact and nothing to miss either.
async fn post_token_request(
    http: &reqwest::Client,
    token_endpoint: &str,
    params: &[(&str, &str)],
    context: GrantContext,
) -> Result<TokenResponse> {
    let started = std::time::Instant::now();
    let result = http.post(token_endpoint).form(params).send().await;
    request_log::record_http_result("drive", "POST", token_endpoint, started, &result);
    let response = result.context("Failed to send token request to Google")?;

    if !response.status().is_success() {
        let body = response.text().await.unwrap_or_default();
        if let Ok(err) = serde_json::from_str::<TokenErrorResponse>(&body) {
            if err.error == "invalid_grant" {
                return Err(DriveError::InvalidGrant(context).into());
            }
            return Err(anyhow::anyhow!(
                "Google token endpoint rejected the request: {} ({})",
                err.error,
                err.error_description.unwrap_or_default()
            ));
        }
        return Err(anyhow::anyhow!(
            "Google token endpoint returned an unparsable error body: {body}"
        ));
    }

    response
        .json::<TokenResponse>()
        .await
        .context("Failed to parse Google's token response")
}

// ── Session (in-memory access-token lifecycle) ──────────────────────────

/// The mutable access-token state, refreshed by [`DriveSession::refresh_locked`].
struct TokenState {
    access_token: Secret,
    expires_at: DateTime<Utc>,
}

/// A live Drive OAuth2 session: holds the refresh token and the current
/// in-memory access token, refreshing on demand.
///
/// Uses [`tokio::sync::Mutex`] (not `std::sync::Mutex`) held *across* the
/// refresh network call — mirrors `crate::gmail::auth::GmailSession`'s
/// explicit single-flight-refresh design (issue #1465's "concurrent callers
/// don't stampede" requirement): a second concurrent caller blocks on this
/// mutex and, once unblocked, observes the already-refreshed token instead
/// of issuing a second POST.
pub struct DriveSession {
    http: reqwest::Client,
    client_id: String,
    client_secret: Secret,
    refresh_token: Secret,
    token_endpoint: String,
    state: tokio::sync::Mutex<TokenState>,
}

impl DriveSession {
    /// Creates a session against Google's real token endpoint.
    pub(crate) fn new(http: reqwest::Client, credentials: &DriveCredentials) -> Self {
        Self::new_with_token_endpoint(http, credentials, TOKEN_ENDPOINT)
    }

    /// [`new`](Self::new) against an explicit token endpoint — the test seam
    /// for pointing at a wiremock server.
    pub(crate) fn new_with_token_endpoint(
        http: reqwest::Client,
        credentials: &DriveCredentials,
        token_endpoint: &str,
    ) -> Self {
        Self {
            http,
            client_id: credentials.client_id.clone(),
            client_secret: credentials.client_secret.clone(),
            refresh_token: credentials.refresh_token.clone(),
            token_endpoint: token_endpoint.to_string(),
            state: tokio::sync::Mutex::new(TokenState {
                access_token: Secret::new(""),
                // No access token is ever persisted (ADR-0063 Decision 2), so
                // every fresh session starts expired and refreshes on its
                // very first call.
                expires_at: DateTime::<Utc>::MIN_UTC,
            }),
        }
    }

    /// Returns a valid access token, refreshing proactively when the
    /// tracked expiry is within [`REFRESH_SKEW`].
    pub(crate) async fn access_token(&self) -> Result<Secret> {
        let mut state = self.state.lock().await;
        if Utc::now() + REFRESH_SKEW >= state.expires_at {
            self.refresh_locked(&mut state).await?;
        }
        Ok(state.access_token.clone())
    }

    /// Forces a refresh, but only if `observed` is still the current token —
    /// i.e. no other caller already refreshed while this caller was waiting
    /// on the lock. Used as the reactive safety net after an HTTP 401 (clock
    /// skew, or server-side revocation the proactive check can't see).
    pub(crate) async fn force_refresh(&self, observed: &Secret) -> Result<Secret> {
        let mut state = self.state.lock().await;
        if state.access_token != *observed {
            return Ok(state.access_token.clone());
        }
        self.refresh_locked(&mut state).await?;
        Ok(state.access_token.clone())
    }

    async fn refresh_locked(&self, state: &mut TokenState) -> Result<()> {
        let response = refresh_access_token(
            &self.http,
            &self.token_endpoint,
            &self.client_id,
            self.client_secret.expose_secret(),
            self.refresh_token.expose_secret(),
        )
        .await?;
        state.access_token = response.access_token.into();
        let expires_in = response.expires_in.clamp(0, MAX_EXPIRES_IN_SECONDS);
        state.expires_at = Utc::now() + TimeDelta::seconds(expires_in);
        Ok(())
    }
}

// ── Login orchestration ─────────────────────────────────────────────────

/// Runs the OAuth2 authorization-code + PKCE login flow, persisting the
/// resulting refresh token to `~/.omni-dev/settings.json`.
pub async fn login(
    client_id: &str,
    client_secret: &Secret,
    browser: &BrowserConfig,
) -> Result<DriveAuthStatus> {
    login_to(
        &Settings::get_settings_path()?,
        active_profile_from(&SystemEnv).as_deref(),
        client_id,
        client_secret,
        browser,
        TOKEN_ENDPOINT,
    )
    .await
}

/// [`login`], writing to an explicit settings-file path/profile and against
/// an explicit token endpoint — the test seam for a wiremock server.
pub(crate) async fn login_to(
    settings_path: &Path,
    profile: Option<&str>,
    client_id: &str,
    client_secret: &Secret,
    browser: &BrowserConfig,
    token_endpoint: &str,
) -> Result<DriveAuthStatus> {
    let credentials = run_login_flow(client_id, client_secret, browser, token_endpoint).await?;
    save_credentials_to(settings_path, profile, &credentials)?;
    Ok(status_from_credentials(&credentials))
}

/// [`login`], but honoring the named-account resolution (mirrors Gmail's
/// issue #1500): runs the same OAuth2 flow, then persists to
/// `drive.accounts.<name>` when a named account is active instead of the
/// legacy `env`/profile map. `explicit` is the already-resolved
/// `--account`/[`account::DRIVE_ACCOUNT_ENV`] override, if any — resolved
/// via [`resolve_for_write`], so an explicit name need not already be
/// configured (this is how a new account is created).
pub(crate) async fn login_for(
    explicit: Option<&str>,
    client_id: &str,
    client_secret: &Secret,
    browser: &BrowserConfig,
) -> Result<DriveAuthStatus> {
    let settings = Settings::load().unwrap_or_default();
    match resolve_for_write(&settings.drive, explicit)? {
        ResolvedAccount::Unconfigured => {
            login_to(
                &Settings::get_settings_path()?,
                active_profile_from(&SystemEnv).as_deref(),
                client_id,
                client_secret,
                browser,
                TOKEN_ENDPOINT,
            )
            .await
        }
        ResolvedAccount::Named(name) => {
            let credentials =
                run_login_flow(client_id, client_secret, browser, TOKEN_ENDPOINT).await?;
            Settings::upsert_drive_account(
                &Settings::get_settings_path()?,
                &name,
                &named_account_vars(&credentials),
            )?;
            Ok(status_from_credentials(&credentials))
        }
    }
}

/// Runs the OAuth2 authorization-code + PKCE flow against `token_endpoint`
/// and returns the resulting credentials, without persisting them — the
/// shared core both [`login_to`] (legacy path) and [`login_for`]'s Named
/// branch build on.
async fn run_login_flow(
    client_id: &str,
    client_secret: &Secret,
    browser: &BrowserConfig,
    token_endpoint: &str,
) -> Result<DriveCredentials> {
    let (listener, port) = bind_callback_listener(browser).await?;
    let redirect_uri = format!("http://127.0.0.1:{port}");

    let pending = generate_pending_login();
    let challenge = code_challenge(&pending.code_verifier);
    let auth_url = build_authorization_url(client_id, &redirect_uri, &pending.state, &challenge)?;
    open_browser(&browser.launch, auth_url.as_str())?;

    let callback = wait_for_callback(listener).await?;
    if let Some(error) = callback.error {
        return Err(DriveError::authorization_denied(
            &error,
            callback.error_description.as_deref(),
        )
        .into());
    }
    let (Some(code), Some(returned_state)) = (callback.code, callback.state) else {
        return Err(DriveError::MalformedCallback.into());
    };
    // Plain equality, not constant-time: `state` is a CSRF nonce carried in
    // a browser-visible URL, not a secret — there's nothing for a timing
    // side-channel to extract here (unlike `constant_time_eq`'s real use
    // guarding a bridge auth token in `src/browser/auth.rs`).
    if returned_state != pending.state {
        return Err(DriveError::StateMismatch.into());
    }

    let http = reqwest::Client::builder()
        .connect_timeout(crate::utils::http::connect_timeout())
        .read_timeout(crate::utils::http::read_timeout())
        .build()
        .context("Failed to build HTTP client")?;
    let tokens = exchange_code_for_tokens(
        &http,
        token_endpoint,
        client_id,
        client_secret.expose_secret(),
        &code,
        &pending.code_verifier,
        &redirect_uri,
    )
    .await?;
    let refresh_token = tokens
        .refresh_token
        .ok_or(DriveError::MalformedTokenResponse("refresh_token"))?;
    let granted_raw = tokens.scope.unwrap_or_default();
    if !granted_raw.split_whitespace().any(|s| s == SCOPE_READONLY) {
        let received = if granted_raw.trim().is_empty() {
            "none".to_string()
        } else {
            granted_raw
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(", ")
        };
        return Err(DriveError::NoScopeGranted(received).into());
    }

    Ok(DriveCredentials {
        client_id: client_id.to_string(),
        client_secret: client_secret.clone(),
        refresh_token: refresh_token.into(),
        scope: SCOPE_READONLY.to_string(),
    })
}

/// Builds the "just authenticated" [`DriveAuthStatus`] from freshly-obtained
/// `credentials` (all fields present by construction).
fn status_from_credentials(credentials: &DriveCredentials) -> DriveAuthStatus {
    DriveAuthStatus {
        has_client_id: true,
        has_client_secret: true,
        has_refresh_token: true,
        scope: Some(credentials.scope.clone()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Arc;

    use super::*;

    // ── Pure helpers ─────────────────────────────────────────────────

    #[test]
    fn code_challenge_matches_rfc_7636_test_vector() {
        // RFC 7636 Appendix B.1.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let expected = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert_eq!(code_challenge(verifier), expected);
    }

    #[test]
    fn code_challenge_output_is_url_safe_no_padding() {
        let challenge = code_challenge("some-verifier-value");
        assert!(!challenge.contains('+'));
        assert!(!challenge.contains('/'));
        assert!(!challenge.contains('='));
    }

    #[test]
    fn generate_pending_login_state_and_verifier_are_distinct_and_rfc_compliant_length() {
        let pending = generate_pending_login();
        assert_ne!(pending.state, pending.code_verifier);
        assert!(pending.code_verifier.len() >= 43 && pending.code_verifier.len() <= 128);
        assert!(pending
            .code_verifier
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn build_authorization_url_includes_pkce_state_and_offline_consent_params() {
        let url = build_authorization_url(
            "client-123",
            "http://127.0.0.1:5555",
            "state-abc",
            "challenge-xyz",
        )
        .unwrap();
        let query: std::collections::HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(query.get("client_id").unwrap(), "client-123");
        assert_eq!(query.get("redirect_uri").unwrap(), "http://127.0.0.1:5555");
        assert_eq!(query.get("response_type").unwrap(), "code");
        assert_eq!(query.get("scope").unwrap(), SCOPE_READONLY);
        assert_eq!(query.get("state").unwrap(), "state-abc");
        assert_eq!(query.get("code_challenge").unwrap(), "challenge-xyz");
        assert_eq!(query.get("code_challenge_method").unwrap(), "S256");
        assert_eq!(query.get("access_type").unwrap(), "offline");
        assert_eq!(query.get("prompt").unwrap(), "consent");
    }

    #[test]
    fn parse_callback_extracts_code_and_state() {
        let request = "GET /?code=abc123&state=xyz789 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
        let result = parse_callback(request).unwrap();
        assert_eq!(result.code.as_deref(), Some("abc123"));
        assert_eq!(result.state.as_deref(), Some("xyz789"));
        assert!(result.error.is_none());
    }

    #[test]
    fn parse_callback_extracts_error_and_error_description() {
        let request =
            "GET /?error=access_denied&error_description=user+declined&state=xyz HTTP/1.1\r\n\r\n";
        let result = parse_callback(request).unwrap();
        assert_eq!(result.error.as_deref(), Some("access_denied"));
        assert_eq!(result.error_description.as_deref(), Some("user declined"));
    }

    #[test]
    fn parse_callback_missing_query_string_is_none() {
        assert!(parse_callback("GET / HTTP/1.1\r\n\r\n").is_none());
        assert!(parse_callback("garbage").is_none());
    }

    #[test]
    fn parse_callback_ignores_unrecognized_query_keys() {
        let request = "GET /?code=abc&state=xyz&foo=bar HTTP/1.1\r\n\r\n";
        let result = parse_callback(request).unwrap();
        assert_eq!(result.code.as_deref(), Some("abc"));
        assert_eq!(result.state.as_deref(), Some("xyz"));
    }

    #[test]
    fn open_browser_manual_logs_and_succeeds() {
        assert!(open_browser(&BrowserLaunch::Manual, "https://example/auth").is_ok());
    }

    #[test]
    fn open_browser_command_substitutes_url_placeholder() {
        let launch = BrowserLaunch::Command(vec!["true".to_string(), "--url={url}".to_string()]);
        assert!(open_browser(&launch, "https://example/auth").is_ok());
    }

    #[test]
    fn open_browser_command_appends_url_when_no_placeholder() {
        let launch = BrowserLaunch::Command(vec!["true".to_string()]);
        assert!(open_browser(&launch, "https://example/auth").is_ok());
    }

    #[test]
    fn open_browser_command_passes_through_args_without_the_placeholder() {
        // A trailing flag with no `{url}` substring (e.g. `--verbose`) is
        // passed to the command unmodified, and the URL is still appended
        // since no arg claimed the placeholder.
        let launch = BrowserLaunch::Command(vec!["true".to_string(), "--verbose".to_string()]);
        assert!(open_browser(&launch, "https://example/auth").is_ok());
    }

    #[test]
    fn open_browser_command_rejects_empty_args() {
        let launch = BrowserLaunch::Command(vec![]);
        let err = open_browser(&launch, "u").unwrap_err();
        assert!(err.to_string().contains("empty browser command"));
    }

    // ── named_account_vars ───────────────────────────────────────────────

    #[test]
    fn named_account_vars_maps_credentials_to_json_string_values() {
        let credentials = DriveCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: SCOPE_READONLY.to_string(),
        };
        assert_eq!(
            named_account_vars(&credentials),
            [
                (
                    "client_id",
                    serde_json::Value::String("client-1".to_string())
                ),
                (
                    "client_secret",
                    serde_json::Value::String("secret-1".to_string())
                ),
                (
                    "refresh_token",
                    serde_json::Value::String("refresh-1".to_string())
                ),
                (
                    "scope",
                    serde_json::Value::String(SCOPE_READONLY.to_string())
                ),
            ]
        );
    }

    // ── build_browser_config (mirrors Gmail's issue #1505) ──────────────

    fn assert_is_auto(config: BrowserConfig) {
        assert!(matches!(config.launch, BrowserLaunch::Auto));
    }

    #[test]
    fn build_browser_config_defaults_to_auto_with_no_account() {
        assert_is_auto(build_browser_config(None, |_| panic!("must not be called")).unwrap());
    }

    #[test]
    fn build_browser_config_defaults_to_auto_with_no_opt_in() {
        let account = DriveAccountSettings {
            email_address: Some("alice@example.com".to_string()),
            ..DriveAccountSettings::default()
        };
        // chrome_profile_from_email is false, so the resolver must never run
        // even though email_address is set.
        assert_is_auto(
            build_browser_config(Some(&account), |_| panic!("must not be called")).unwrap(),
        );
    }

    #[test]
    fn build_browser_config_uses_browser_command_verbatim() {
        let account = DriveAccountSettings {
            browser_command: Some("chrome --new-window {url}".to_string()),
            ..DriveAccountSettings::default()
        };
        let config =
            build_browser_config(Some(&account), |_| panic!("must not be called")).unwrap();
        assert!(matches!(
            config.launch,
            BrowserLaunch::Command(args) if args == vec!["chrome", "--new-window", "{url}"]
        ));
    }

    #[test]
    fn build_browser_config_browser_command_wins_over_chrome_profile_from_email() {
        let account = DriveAccountSettings {
            browser_command: Some("chrome {url}".to_string()),
            chrome_profile_from_email: true,
            email_address: Some("alice@example.com".to_string()),
            ..DriveAccountSettings::default()
        };
        let config =
            build_browser_config(Some(&account), |_| panic!("must not be called")).unwrap();
        assert!(matches!(config.launch, BrowserLaunch::Command(_)));
    }

    #[test]
    fn build_browser_config_rejects_a_malformed_browser_command() {
        let account = DriveAccountSettings {
            browser_command: Some("chrome \"--flag".to_string()),
            ..DriveAccountSettings::default()
        };
        let err =
            build_browser_config(Some(&account), |_| panic!("must not be called")).unwrap_err();
        assert!(err.to_string().contains("browser_command"));
    }

    #[test]
    fn build_browser_config_resolves_the_chrome_profile_when_opted_in() {
        let account = DriveAccountSettings {
            chrome_profile_from_email: true,
            email_address: Some("alice@example.com".to_string()),
            ..DriveAccountSettings::default()
        };
        let config = build_browser_config(Some(&account), |email| {
            assert_eq!(email, "alice@example.com");
            Some(vec!["chrome-stub".to_string(), "{url}".to_string()])
        })
        .unwrap();
        assert!(matches!(
            config.launch,
            BrowserLaunch::Command(args) if args == vec!["chrome-stub", "{url}"]
        ));
    }

    #[test]
    fn build_browser_config_falls_back_to_auto_when_chrome_resolution_fails() {
        let account = DriveAccountSettings {
            chrome_profile_from_email: true,
            email_address: Some("alice@example.com".to_string()),
            ..DriveAccountSettings::default()
        };
        assert_is_auto(build_browser_config(Some(&account), |_| None).unwrap());
    }

    #[test]
    fn build_browser_config_is_auto_when_opted_in_but_no_email_address() {
        let account = DriveAccountSettings {
            chrome_profile_from_email: true,
            ..DriveAccountSettings::default()
        };
        assert_is_auto(
            build_browser_config(Some(&account), |_| panic!("must not be called")).unwrap(),
        );
    }

    // ── resolve_browser_config_for (mirrors Gmail's issue #1505) ────────

    #[test]
    fn resolve_browser_config_for_unconfigured_account_defaults_to_auto() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();
        std::env::set_var(DRIVE_CLIENT_ID, "literal-id");
        std::env::set_var(DRIVE_CLIENT_SECRET, "literal-secret");
        std::env::set_var(DRIVE_REFRESH_TOKEN, "literal-refresh");

        let drive = DriveSettings::default();
        assert_is_auto(resolve_browser_config_for(&drive, None).unwrap());
    }

    #[test]
    fn resolve_browser_config_for_named_account_without_chrome_opt_in_defaults_to_auto() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let mut drive = DriveSettings::default();
        drive.accounts.insert(
            "work".to_string(),
            DriveAccountSettings {
                email_address: Some("alice@example.com".to_string()),
                ..DriveAccountSettings::default()
            },
        );

        // chrome_profile_from_email is false, so this never touches the
        // real chrome_profile::resolve_launch_command resolver.
        assert_is_auto(resolve_browser_config_for(&drive, Some("work")).unwrap());
    }

    // ── Loopback listener (real sockets, no wiremock) ───────────────────

    #[tokio::test]
    async fn wait_for_callback_times_out_when_nothing_connects() {
        let browser = BrowserConfig::default();
        let (listener, _port) = bind_callback_listener(&browser).await.unwrap();
        let err = wait_for_callback_with_timeout(listener, Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(matches!(
            err.downcast_ref::<DriveError>(),
            Some(DriveError::CallbackTimeout(_))
        ));
    }

    #[tokio::test]
    async fn wait_for_callback_reads_a_real_connection() {
        let browser = BrowserConfig::default();
        let (listener, port) = bind_callback_listener(&browser).await.unwrap();

        let client = tokio::spawn(async move {
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            stream
                .write_all(b"GET /?code=abc&state=xyz HTTP/1.1\r\n\r\n")
                .await
                .unwrap();
        });

        let result = wait_for_callback(listener).await.unwrap();
        client.await.unwrap();
        assert_eq!(result.code.as_deref(), Some("abc"));
        assert_eq!(result.state.as_deref(), Some("xyz"));
    }

    #[tokio::test]
    async fn wait_for_callback_malformed_request_line_is_malformed_callback() {
        let browser = BrowserConfig::default();
        let (listener, port) = bind_callback_listener(&browser).await.unwrap();

        let client = tokio::spawn(async move {
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            stream.write_all(b"not an http request").await.unwrap();
        });

        let err = wait_for_callback(listener).await.unwrap_err();
        client.await.unwrap();
        assert!(matches!(
            err.downcast_ref::<DriveError>(),
            Some(DriveError::MalformedCallback)
        ));
    }

    // ── Token exchange / refresh (wiremock) ─────────────────────────────

    #[tokio::test]
    async fn exchange_code_for_tokens_posts_expected_form_body() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/token"))
            .and(wiremock::matchers::body_string_contains(
                "grant_type=authorization_code",
            ))
            .and(wiremock::matchers::body_string_contains(
                "code_verifier=verifier-1",
            ))
            .and(wiremock::matchers::body_string_contains(
                "redirect_uri=http%3A%2F%2F127.0.0.1%3A9999",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "at-1",
                    "refresh_token": "rt-1",
                    "expires_in": 3600,
                    "scope": SCOPE_READONLY,
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let http = reqwest::Client::new();
        let token_endpoint = format!("{}/token", server.uri());
        let response = exchange_code_for_tokens(
            &http,
            &token_endpoint,
            "client-1",
            "secret-1",
            "code-1",
            "verifier-1",
            "http://127.0.0.1:9999",
        )
        .await
        .unwrap();
        assert_eq!(response.access_token, "at-1");
        assert_eq!(response.refresh_token.as_deref(), Some("rt-1"));
    }

    #[tokio::test]
    async fn exchange_code_for_tokens_maps_invalid_grant_to_pkce_flavored_message() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": "invalid_grant",
                    "error_description": "Bad Request",
                })),
            )
            .mount(&server)
            .await;

        let http = reqwest::Client::new();
        let err = exchange_code_for_tokens(
            &http,
            &server.uri(),
            "c",
            "s",
            "code",
            "verifier",
            "http://127.0.0.1:1",
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("PKCE"));
    }

    // ── login_to (end-to-end: state mismatch / access_denied) ───────────
    //
    // These exercise `login_to` itself rather than its sub-components in
    // isolation, so a regression in how it wires the loopback callback into
    // the state-mismatch/access_denied branches would actually be caught.
    // The callback port is picked by binding-then-dropping a std listener
    // (a well-known "reserve a free port" trick) so the test's connector
    // task can dial it directly — `login_to` binds the real listener before
    // opening the browser, so the connector retries briefly to cover the
    // small window before that bind completes.
    //
    // That reserve-then-drop is itself a TOCTOU race against the rest of the
    // parallel test suite: another test can grab the same ephemeral port
    // before `login_to`'s own bind runs, which fails outright rather than
    // retrying (mirrors Gmail's issue #1489). `run_with_port_retry` bounds a
    // retry of the whole reserve/connect/bind attempt on exactly that
    // failure, so a lost race just tries again with a fresh port instead of
    // flaking.

    async fn connect_and_send(port: u16, request_line: &[u8]) {
        let mut stream = loop {
            match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
                Ok(stream) => break stream,
                Err(_) => tokio::time::sleep(Duration::from_millis(2)).await,
            }
        };
        stream.write_all(request_line).await.unwrap();
    }

    fn reserve_free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// True if `err` is `bind_callback_listener`'s wrapped `AddrInUse` —
    /// i.e. some other process/test won the race for the port reserved by
    /// [`reserve_free_port`] before `login_to` could rebind it.
    fn is_callback_bind_conflict(err: &anyhow::Error) -> bool {
        err.to_string()
            .contains("Failed to start the local OAuth callback listener")
    }

    const PORT_RETRY_ATTEMPTS: u32 = 5;

    /// Runs `attempt`, which reserves its own port via [`reserve_free_port`]
    /// and returns `login_to`'s result, retrying up to
    /// [`PORT_RETRY_ATTEMPTS`] times when the attempt loses the ephemeral
    /// port race (see the module comment above `connect_and_send`).
    async fn run_with_port_retry<F, Fut>(mut attempt: F) -> Result<DriveAuthStatus>
    where
        F: FnMut(u16) -> Fut,
        Fut: std::future::Future<Output = Result<DriveAuthStatus>>,
    {
        for remaining in (0..PORT_RETRY_ATTEMPTS).rev() {
            let result = attempt(reserve_free_port()).await;
            let is_retryable_conflict =
                matches!(&result, Err(err) if remaining > 0 && is_callback_bind_conflict(err));
            if !is_retryable_conflict {
                return result;
            }
        }
        unreachable!("loop always returns on its last iteration")
    }

    /// Awaits `connector` normally, unless `result` shows `login_to` lost
    /// the callback-port race — in which case the connector, which will
    /// never see a connection on the now-taken port, is aborted instead of
    /// hung.
    async fn finish_connector(
        connector: tokio::task::JoinHandle<()>,
        result: &Result<DriveAuthStatus>,
    ) {
        match result {
            Err(err) if is_callback_bind_conflict(err) => connector.abort(),
            _ => connector.await.unwrap(),
        }
    }

    /// Polls `path` until it holds non-empty content, then returns it —
    /// used to read back the authorization URL that `open_browser`'s
    /// captured shell command writes asynchronously.
    async fn wait_for_captured_url(path: &Path) -> String {
        loop {
            if let Ok(contents) = std::fs::read_to_string(path) {
                if !contents.is_empty() {
                    return contents;
                }
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    /// Shared body for the three `login_to_*` tests that drive a single
    /// fixed callback request line through `login_to` and expect it to
    /// error: reserves a port, spawns the connector, calls `login_to`, and
    /// retries the whole attempt (via [`run_with_port_retry`]) if it loses
    /// the ephemeral-port race. Asserts no settings file was written and
    /// returns the resulting error for the caller to inspect.
    async fn run_login_to_expect_err(request_line: &'static [u8]) -> anyhow::Error {
        std::fs::create_dir_all("tmp").ok();
        let temp_dir = tempfile::TempDir::new_in("tmp").unwrap();
        let settings_path = temp_dir.path().join("settings.json");

        let result = run_with_port_retry(|port| {
            let settings_path = settings_path.clone();
            async move {
                let browser = BrowserConfig {
                    launch: BrowserLaunch::Manual,
                    callback_addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
                    callback_port: port,
                };
                let connector = tokio::spawn(connect_and_send(port, request_line));

                let result = login_to(
                    &settings_path,
                    None,
                    "client-id",
                    &Secret::new("client-secret"),
                    &browser,
                    "http://127.0.0.1:1/token", // never reached — fails before exchange
                )
                .await;

                finish_connector(connector, &result).await;
                result
            }
        })
        .await;

        let err = result.unwrap_err();
        assert!(!settings_path.exists());
        err
    }

    #[tokio::test]
    async fn run_with_port_retry_retries_after_a_callback_bind_conflict_then_succeeds() {
        let attempts = AtomicU32::new(0);

        let status = run_with_port_retry(|_port| {
            let attempt_no = attempts.fetch_add(1, Ordering::SeqCst);
            async move {
                if attempt_no == 0 {
                    Err(anyhow::anyhow!(
                        "Failed to start the local OAuth callback listener: address in use"
                    ))
                } else {
                    Ok(DriveAuthStatus {
                        has_client_id: true,
                        has_client_secret: true,
                        has_refresh_token: true,
                        scope: None,
                    })
                }
            }
        })
        .await
        .unwrap();

        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert!(status.has_client_id);
    }

    #[tokio::test]
    async fn run_with_port_retry_does_not_retry_a_non_conflict_error() {
        let attempts = AtomicU32::new(0);

        let result = run_with_port_retry(|_port| {
            attempts.fetch_add(1, Ordering::SeqCst);
            async move { Err(anyhow::anyhow!("some other failure")) }
        })
        .await;

        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn finish_connector_aborts_when_login_to_lost_the_port_race() {
        let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
        let connector = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            let _ = tx.send(());
        });
        let result: Result<DriveAuthStatus> = Err(anyhow::anyhow!(
            "Failed to start the local OAuth callback listener: address in use"
        ));

        tokio::time::timeout(Duration::from_secs(5), finish_connector(connector, &result))
            .await
            .expect("finish_connector must not wait for an aborted connector");

        assert!(
            rx.try_recv().is_err(),
            "connector must have been aborted, not run to completion"
        );
    }

    #[tokio::test]
    async fn finish_connector_awaits_connector_when_login_to_succeeds() {
        let ran = Arc::new(AtomicBool::new(false));
        let ran_clone = ran.clone();
        let connector = tokio::spawn(async move {
            ran_clone.store(true, Ordering::SeqCst);
        });
        let result = Ok(DriveAuthStatus {
            has_client_id: true,
            has_client_secret: true,
            has_refresh_token: true,
            scope: None,
        });

        finish_connector(connector, &result).await;

        assert!(ran.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn wait_for_captured_url_polls_until_content_is_written() {
        std::fs::create_dir_all("tmp").ok();
        let temp_dir = tempfile::TempDir::new_in("tmp").unwrap();
        let path = temp_dir.path().join("captured-url.txt");

        let write_path = path.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            std::fs::write(&write_path, "").unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
            std::fs::write(&write_path, "https://example.com/authorize").unwrap();
        });

        let contents = wait_for_captured_url(&path).await;
        assert_eq!(contents, "https://example.com/authorize");
    }

    #[tokio::test]
    async fn login_to_rejects_a_callback_with_mismatched_state() {
        let err =
            run_login_to_expect_err(b"GET /?code=abc&state=the-wrong-state HTTP/1.1\r\n\r\n").await;

        assert!(matches!(
            err.downcast_ref::<DriveError>(),
            Some(DriveError::StateMismatch)
        ));
    }

    #[tokio::test]
    async fn login_to_surfaces_access_denied_from_the_callback() {
        let err = run_login_to_expect_err(
            b"GET /?error=access_denied&error_description=user+declined HTTP/1.1\r\n\r\n",
        )
        .await;

        match err.downcast_ref::<DriveError>() {
            Some(DriveError::AuthorizationDenied(message)) => {
                assert!(message.contains("access_denied"));
                assert!(message.contains("user declined"));
            }
            other => panic!("expected AuthorizationDenied, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn login_to_rejects_a_callback_missing_code_and_state() {
        let err = run_login_to_expect_err(b"GET /?foo=bar HTTP/1.1\r\n\r\n").await;

        assert!(matches!(
            err.downcast_ref::<DriveError>(),
            Some(DriveError::MalformedCallback)
        ));
    }

    #[tokio::test]
    async fn login_to_completes_full_success_flow_and_persists_credentials() {
        // Captures the real authorization URL `login_to` generates (with its
        // randomly-generated CSRF `state`) by pointing the browser launch at
        // a shell command instead of an actual browser: `open_browser`
        // substitutes `{url}` into the command's args and spawns it, so a
        // tiny `/bin/sh` one-liner writes the URL to a file we can read back
        // — letting this test drive the full success path (state echoed
        // correctly, token exchange, credential persistence) without ever
        // opening a real browser or needing to predict the CSRF nonce.
        std::fs::create_dir_all("tmp").ok();
        let temp_dir = tempfile::TempDir::new_in("tmp").unwrap();
        let capture_path = temp_dir.path().join("captured-url.txt");
        let settings_path = temp_dir.path().join("settings.json");

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/token"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "at-1",
                    "refresh_token": "rt-1",
                    "expires_in": 3600,
                    "scope": SCOPE_READONLY,
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let status = run_with_port_retry(|port| {
            let capture_path = capture_path.clone();
            let settings_path = settings_path.clone();
            let token_endpoint = format!("{}/token", server.uri());
            async move {
                let browser = BrowserConfig {
                    launch: BrowserLaunch::Command(vec![
                        "/bin/sh".to_string(),
                        "-c".to_string(),
                        format!("printf '%s' \"$0\" > '{}'", capture_path.display()),
                        "{url}".to_string(),
                    ]),
                    callback_addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
                    callback_port: port,
                };

                let connector = tokio::spawn(async move {
                    let auth_url = wait_for_captured_url(&capture_path).await;
                    let parsed = Url::parse(&auth_url).unwrap();
                    let state = parsed
                        .query_pairs()
                        .find(|(k, _)| k == "state")
                        .map(|(_, v)| v.into_owned())
                        .expect("authorization URL must carry a state param");
                    connect_and_send(
                        port,
                        format!("GET /?code=auth-code&state={state} HTTP/1.1\r\n\r\n").as_bytes(),
                    )
                    .await;
                });

                let result = login_to(
                    &settings_path,
                    None,
                    "client-id",
                    &Secret::new("client-secret"),
                    &browser,
                    &token_endpoint,
                )
                .await;

                finish_connector(connector, &result).await;
                result
            }
        })
        .await
        .unwrap();

        assert!(status.has_client_id);
        assert!(status.has_client_secret);
        assert!(status.has_refresh_token);
        assert_eq!(status.scope.as_deref(), Some(SCOPE_READONLY));

        let saved = std::fs::read_to_string(&settings_path).unwrap();
        assert!(saved.contains("rt-1"));
        assert!(saved.contains("client-id"));
    }

    /// Full mocked login round trip (real state nonce echoed back via the
    /// captured-authorization-URL trick, like
    /// `login_to_completes_full_success_flow_and_persists_credentials`
    /// above), with an injectable token-response body — the seam the
    /// scope-validation tests below use to simulate Google granting no
    /// Drive scope.
    async fn run_login_to_with_token_response(
        token_response_body: serde_json::Value,
    ) -> (Result<DriveAuthStatus>, std::path::PathBuf) {
        std::fs::create_dir_all("tmp").ok();
        let temp_dir = tempfile::TempDir::new_in("tmp").unwrap();
        let capture_path = temp_dir.path().join("captured-url.txt");
        let settings_path = temp_dir.path().join("settings.json");

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/token"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(&token_response_body))
            .expect(1)
            .mount(&server)
            .await;

        let result = run_with_port_retry(|port| {
            let capture_path = capture_path.clone();
            let settings_path = settings_path.clone();
            let token_endpoint = format!("{}/token", server.uri());
            async move {
                let browser = BrowserConfig {
                    launch: BrowserLaunch::Command(vec![
                        "/bin/sh".to_string(),
                        "-c".to_string(),
                        format!("printf '%s' \"$0\" > '{}'", capture_path.display()),
                        "{url}".to_string(),
                    ]),
                    callback_addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
                    callback_port: port,
                };

                let connector = tokio::spawn(async move {
                    let auth_url = wait_for_captured_url(&capture_path).await;
                    let parsed = Url::parse(&auth_url).unwrap();
                    let state = parsed
                        .query_pairs()
                        .find(|(k, _)| k == "state")
                        .map(|(_, v)| v.into_owned())
                        .expect("authorization URL must carry a state param");
                    connect_and_send(
                        port,
                        format!("GET /?code=auth-code&state={state} HTTP/1.1\r\n\r\n").as_bytes(),
                    )
                    .await;
                });

                let result = login_to(
                    &settings_path,
                    None,
                    "client-id",
                    &Secret::new("client-secret"),
                    &browser,
                    &token_endpoint,
                )
                .await;

                finish_connector(connector, &result).await;
                result
            }
        })
        .await;

        (result, settings_path)
    }

    #[tokio::test]
    async fn login_to_rejects_a_grant_with_no_drive_scope() {
        let (result, settings_path) = run_login_to_with_token_response(serde_json::json!({
            "access_token": "at-1",
            "refresh_token": "rt-1",
            "expires_in": 3600,
            "scope": "openid email profile",
        }))
        .await;

        let err = result.unwrap_err();
        assert!(matches!(
            err.downcast_ref::<DriveError>(),
            Some(DriveError::NoScopeGranted(received)) if received == "openid, email, profile"
        ));
        assert!(!settings_path.exists());
    }

    #[tokio::test]
    async fn login_to_rejects_a_grant_with_missing_scope_field() {
        let (result, settings_path) = run_login_to_with_token_response(serde_json::json!({
            "access_token": "at-1",
            "refresh_token": "rt-1",
            "expires_in": 3600,
        }))
        .await;

        let err = result.unwrap_err();
        assert!(matches!(
            err.downcast_ref::<DriveError>(),
            Some(DriveError::NoScopeGranted(received)) if received == "none"
        ));
        assert!(!settings_path.exists());
    }

    // ── login_for (named-account login orchestration, mirrors Gmail's
    // issue #1500) ───────────────────────────────────────────────────────
    //
    // `login_for` hardcodes the real `TOKEN_ENDPOINT` in both branches
    // (unlike `login_to`, which takes one as an explicit test seam), so a
    // full success round trip can't be driven against a wiremock server
    // here. These instead drive a callback with a mismatched `state` —
    // which `run_login_flow` rejects *before* ever reaching the token
    // endpoint — to exercise account resolution (`Settings::load` +
    // `resolve_for_write`) and, for the named branch, the `run_login_flow`
    // call site itself, without any real network call.

    #[tokio::test]
    async fn login_for_unconfigured_account_rejects_a_callback_with_mismatched_state() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");

        let result = run_with_port_retry(|port| async move {
            let browser = BrowserConfig {
                launch: BrowserLaunch::Manual,
                callback_addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
                callback_port: port,
            };
            let connector = tokio::spawn(connect_and_send(
                port,
                b"GET /?code=abc&state=the-wrong-state HTTP/1.1\r\n\r\n",
            ));

            let result =
                login_for(None, "client-id", &Secret::new("client-secret"), &browser).await;

            finish_connector(connector, &result).await;
            result
        })
        .await;

        let err = result.unwrap_err();
        assert!(matches!(
            err.downcast_ref::<DriveError>(),
            Some(DriveError::StateMismatch)
        ));
        assert!(!settings_path.exists());
    }

    #[tokio::test]
    async fn login_for_named_account_rejects_a_callback_with_mismatched_state() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");

        let result = run_with_port_retry(|port| async move {
            let browser = BrowserConfig {
                launch: BrowserLaunch::Manual,
                callback_addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
                callback_port: port,
            };
            let connector = tokio::spawn(connect_and_send(
                port,
                b"GET /?code=abc&state=the-wrong-state HTTP/1.1\r\n\r\n",
            ));

            let result = login_for(
                Some("work"),
                "client-id",
                &Secret::new("client-secret"),
                &browser,
            )
            .await;

            finish_connector(connector, &result).await;
            result
        })
        .await;

        let err = result.unwrap_err();
        assert!(matches!(
            err.downcast_ref::<DriveError>(),
            Some(DriveError::StateMismatch)
        ));
        assert!(!settings_path.exists());
    }

    #[tokio::test]
    async fn refresh_access_token_posts_grant_type_refresh_token_and_parses_expires_in() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::body_string_contains(
                "grant_type=refresh_token",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "at-2",
                    "expires_in": 1800,
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let http = reqwest::Client::new();
        let response = refresh_access_token(&http, &server.uri(), "c", "s", "rt-1")
            .await
            .unwrap();
        assert_eq!(response.access_token, "at-2");
        assert_eq!(response.expires_in, 1800);
    }

    #[tokio::test]
    async fn refresh_access_token_maps_invalid_grant_to_testing_mode_message() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": "invalid_grant",
                })),
            )
            .mount(&server)
            .await;

        let http = reqwest::Client::new();
        let err = refresh_access_token(&http, &server.uri(), "c", "s", "rt-1")
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("7 days"));
        assert!(msg.contains("Testing"));
    }

    #[tokio::test]
    async fn refresh_access_token_falls_back_to_raw_body_when_error_is_unparsable() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(400).set_body_string("not json"))
            .mount(&server)
            .await;

        let http = reqwest::Client::new();
        let err = refresh_access_token(&http, &server.uri(), "c", "s", "rt-1")
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unparsable error body"));
        assert!(msg.contains("not json"));
    }

    #[tokio::test]
    async fn token_request_propagates_network_errors() {
        let http = reqwest::Client::new();
        let err = refresh_access_token(&http, "http://127.0.0.1:1", "c", "s", "rt")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Failed to send token request"));
    }

    #[tokio::test]
    async fn token_request_errors_on_unparsable_response_body() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let http = reqwest::Client::new();
        let err = refresh_access_token(&http, &server.uri(), "c", "s", "rt")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Failed to parse"));
    }

    // ── DriveSession ─────────────────────────────────────────────────

    fn test_credentials() -> DriveCredentials {
        DriveCredentials {
            client_id: "client-1".to_string(),
            client_secret: "secret-1".into(),
            refresh_token: "refresh-1".into(),
            scope: SCOPE_READONLY.to_string(),
        }
    }

    #[tokio::test]
    async fn access_token_refreshes_on_first_call() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "at-first",
                    "expires_in": 3600,
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let session = DriveSession::new_with_token_endpoint(
            reqwest::Client::new(),
            &test_credentials(),
            &server.uri(),
        );
        let token = session.access_token().await.unwrap();
        assert_eq!(token.expose_secret(), "at-first");
    }

    #[tokio::test]
    async fn access_token_reuses_cached_token_within_skew_window() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "at-cached",
                    "expires_in": 3600,
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let session = DriveSession::new_with_token_endpoint(
            reqwest::Client::new(),
            &test_credentials(),
            &server.uri(),
        );
        let first = session.access_token().await.unwrap();
        let second = session.access_token().await.unwrap();
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn access_token_proactively_refreshes_when_within_skew_window() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "at-short",
                    // Less than REFRESH_SKEW (60s), so the second call also refreshes.
                    "expires_in": 30,
                })),
            )
            .expect(2)
            .mount(&server)
            .await;

        let session = DriveSession::new_with_token_endpoint(
            reqwest::Client::new(),
            &test_credentials(),
            &server.uri(),
        );
        session.access_token().await.unwrap();
        session.access_token().await.unwrap();
    }

    #[tokio::test]
    async fn access_token_refresh_clamps_overflowing_expires_in_without_panicking() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "at-overflow",
                    // Unvalidated, this overflows both TimeDelta::seconds and
                    // the subsequent DateTime<Utc> addition (#1531).
                    "expires_in": i64::MAX,
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let session = DriveSession::new_with_token_endpoint(
            reqwest::Client::new(),
            &test_credentials(),
            &server.uri(),
        );
        let token = session.access_token().await.unwrap();
        assert_eq!(token.expose_secret(), "at-overflow");
    }

    #[tokio::test]
    async fn access_token_refresh_clamps_negative_expires_in_to_immediately_expired() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "at-negative",
                    "expires_in": -3600,
                })),
            )
            // Negative expires_in clamps to 0, so the token is already
            // stale and the second call refreshes again.
            .expect(2)
            .mount(&server)
            .await;

        let session = DriveSession::new_with_token_endpoint(
            reqwest::Client::new(),
            &test_credentials(),
            &server.uri(),
        );
        session.access_token().await.unwrap();
        session.access_token().await.unwrap();
    }

    #[tokio::test]
    async fn force_refresh_concurrent_callers_do_not_stampede() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "at-bootstrap",
                    "expires_in": 3600,
                })),
            )
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "at-refreshed",
                    "expires_in": 3600,
                })),
            )
            .expect(1)
            .with_priority(2)
            .mount(&server)
            .await;

        let session = DriveSession::new_with_token_endpoint(
            reqwest::Client::new(),
            &test_credentials(),
            &server.uri(),
        );
        let bootstrapped = session.access_token().await.unwrap();
        assert_eq!(bootstrapped.expose_secret(), "at-bootstrap");

        let (a, b) = tokio::join!(
            session.force_refresh(&bootstrapped),
            session.force_refresh(&bootstrapped)
        );
        let a = a.unwrap();
        let b = b.unwrap();
        assert_eq!(a, b);
        assert_eq!(a.expose_secret(), "at-refreshed");
    }

    #[tokio::test]
    async fn force_refresh_skips_network_call_when_token_already_rotated() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "at-a",
                    "expires_in": 3600,
                })),
            )
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "at-b",
                    "expires_in": 3600,
                })),
            )
            .expect(1)
            .with_priority(2)
            .mount(&server)
            .await;

        let session = DriveSession::new_with_token_endpoint(
            reqwest::Client::new(),
            &test_credentials(),
            &server.uri(),
        );
        let stale = Secret::new("at-never-issued");
        let bootstrapped = session.access_token().await.unwrap();
        assert_eq!(bootstrapped.expose_secret(), "at-a");

        // `stale` was never the live token, so this should reuse the current
        // one without an extra network call.
        let result = session.force_refresh(&stale).await.unwrap();
        assert_eq!(result, bootstrapped);

        // A real force_refresh against the actual current token does POST.
        let refreshed = session.force_refresh(&bootstrapped).await.unwrap();
        assert_eq!(refreshed.expose_secret(), "at-b");
    }

    // ── Secret non-leakage ───────────────────────────────────────────

    #[test]
    fn drive_credentials_debug_redacts_client_secret_and_refresh_token() {
        let creds = DriveCredentials {
            client_id: "client-visible".to_string(),
            client_secret: "sekret-client-secret".into(),
            refresh_token: "sekret-refresh-token".into(),
            scope: SCOPE_READONLY.to_string(),
        };
        let debug = format!("{creds:?}");
        assert!(debug.contains("DriveCredentials"));
        assert!(debug.contains("client-visible"));
        assert!(!debug.contains("sekret-client-secret"));
        assert!(!debug.contains("sekret-refresh-token"));
        assert!(debug.contains("client_secret: <redacted>"));
        assert!(debug.contains("refresh_token: <redacted>"));
    }

    #[test]
    fn drive_auth_status_yaml_serialization_contains_no_secret_values() {
        let env = crate::test_support::env::MapEnv::new()
            .with(DRIVE_CLIENT_ID, "client-id-value")
            .with(DRIVE_CLIENT_SECRET, "sekret-do-not-leak")
            .with(DRIVE_REFRESH_TOKEN, "sekret-refresh-do-not-leak")
            .with(DRIVE_SCOPE, SCOPE_READONLY);
        let status = status_with(&env);
        let yaml = serde_yaml::to_string(&status).unwrap();
        assert!(!yaml.contains("sekret-do-not-leak"));
        assert!(!yaml.contains("sekret-refresh-do-not-leak"));
    }

    // ── Env-DI boundary tests ────────────────────────────────────────

    use crate::test_support::env::MapEnv;

    #[test]
    fn status_reports_all_false_when_nothing_configured() {
        let status = status_with(&MapEnv::new());
        assert!(!status.has_client_id);
        assert!(!status.has_client_secret);
        assert!(!status.has_refresh_token);
        assert_eq!(status.scope, None);
    }

    #[test]
    fn status_reports_scope_when_present() {
        let env = MapEnv::new().with(DRIVE_SCOPE, SCOPE_READONLY);
        let status = status_with(&env);
        assert_eq!(status.scope.as_deref(), Some(SCOPE_READONLY));
    }

    #[test]
    fn load_credentials_errors_when_client_id_missing() {
        let env = MapEnv::new()
            .with(DRIVE_CLIENT_SECRET, "s")
            .with(DRIVE_REFRESH_TOKEN, "r");
        let err = load_credentials_with(&env).unwrap_err();
        assert!(err.to_string().contains("not configured"));
    }

    #[test]
    fn load_credentials_errors_when_client_secret_missing() {
        let env = MapEnv::new()
            .with(DRIVE_CLIENT_ID, "c")
            .with(DRIVE_REFRESH_TOKEN, "r");
        assert!(load_credentials_with(&env).is_err());
    }

    #[test]
    fn load_credentials_errors_when_refresh_token_missing() {
        let env = MapEnv::new()
            .with(DRIVE_CLIENT_ID, "c")
            .with(DRIVE_CLIENT_SECRET, "s");
        assert!(load_credentials_with(&env).is_err());
    }

    #[test]
    fn load_credentials_succeeds_with_all_three_present() {
        let env = MapEnv::new()
            .with(DRIVE_CLIENT_ID, "c")
            .with(DRIVE_CLIENT_SECRET, "s")
            .with(DRIVE_REFRESH_TOKEN, "r");
        let creds = load_credentials_with(&env).unwrap();
        assert_eq!(creds.client_id, "c");
        assert_eq!(creds.scope, SCOPE_READONLY);
    }

    /// Save + remove round-trip against injected settings-file paths — no
    /// `HOME` mutation, so the test needs no lock.
    #[test]
    fn save_then_remove_round_trip() {
        // ── Part 1: creates file from scratch ──────────────────────
        {
            let temp_dir = {
                std::fs::create_dir_all("tmp").ok();
                tempfile::TempDir::new_in("tmp").unwrap()
            };
            let settings_path = temp_dir.path().join(".omni-dev").join("settings.json");

            let creds = DriveCredentials {
                client_id: "client-1".to_string(),
                client_secret: "secret-1".into(),
                refresh_token: "refresh-1".into(),
                scope: SCOPE_READONLY.to_string(),
            };
            save_credentials_to(&settings_path, None, &creds).unwrap();

            assert!(settings_path.exists());
            let val: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
            assert_eq!(val["env"]["DRIVE_CLIENT_ID"], "client-1");
            assert_eq!(val["env"]["DRIVE_CLIENT_SECRET"], "secret-1");
            assert_eq!(val["env"]["DRIVE_REFRESH_TOKEN"], "refresh-1");
            assert_eq!(val["env"]["DRIVE_SCOPE"], SCOPE_READONLY);

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::metadata(&settings_path).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o600);
            }
        }

        // ── Part 2: merges into existing settings ──────────────────
        {
            let temp_dir = {
                std::fs::create_dir_all("tmp").ok();
                tempfile::TempDir::new_in("tmp").unwrap()
            };
            let omni_dir = temp_dir.path().join(".omni-dev");
            fs::create_dir_all(&omni_dir).unwrap();
            let settings_path = omni_dir.join("settings.json");
            fs::write(
                &settings_path,
                r#"{"env": {"OTHER_KEY": "keep_me"}, "extra": true}"#,
            )
            .unwrap();

            let creds = DriveCredentials {
                client_id: "client-2".to_string(),
                client_secret: "secret-2".into(),
                refresh_token: "refresh-2".into(),
                scope: SCOPE_READONLY.to_string(),
            };
            save_credentials_to(&settings_path, None, &creds).unwrap();

            let val: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
            assert_eq!(val["env"]["OTHER_KEY"], "keep_me");
            assert_eq!(val["extra"], true);
            assert_eq!(val["env"]["DRIVE_SCOPE"], SCOPE_READONLY);
        }

        // ── Part 3: remove clears the four keys, preserves others ──
        {
            let temp_dir = {
                std::fs::create_dir_all("tmp").ok();
                tempfile::TempDir::new_in("tmp").unwrap()
            };
            let omni_dir = temp_dir.path().join(".omni-dev");
            fs::create_dir_all(&omni_dir).unwrap();
            let settings_path = omni_dir.join("settings.json");
            fs::write(
                &settings_path,
                r#"{"env": {
                    "DRIVE_CLIENT_ID": "a",
                    "DRIVE_CLIENT_SECRET": "b",
                    "DRIVE_REFRESH_TOKEN": "c",
                    "DRIVE_SCOPE": "d",
                    "OTHER_KEY": "keep"
                }}"#,
            )
            .unwrap();

            let removed = remove_credentials_at(&settings_path, None).unwrap();
            assert!(removed);

            let val: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
            assert!(val["env"].get("DRIVE_CLIENT_ID").is_none());
            assert!(val["env"].get("DRIVE_CLIENT_SECRET").is_none());
            assert!(val["env"].get("DRIVE_REFRESH_TOKEN").is_none());
            assert!(val["env"].get("DRIVE_SCOPE").is_none());
            assert_eq!(val["env"]["OTHER_KEY"], "keep");
        }

        // ── Part 4: remove returns false when nothing to remove ────
        {
            let temp_dir = {
                std::fs::create_dir_all("tmp").ok();
                tempfile::TempDir::new_in("tmp").unwrap()
            };
            let settings_path = temp_dir.path().join(".omni-dev").join("settings.json");
            let removed = remove_credentials_at(&settings_path, None).unwrap();
            assert!(!removed);
        }
    }

    /// Save + remove round-trip against a profile-targeted env map.
    #[test]
    fn save_then_remove_round_trip_in_profile() {
        let temp_dir = {
            std::fs::create_dir_all("tmp").ok();
            tempfile::TempDir::new_in("tmp").unwrap()
        };
        let omni_dir = temp_dir.path().join(".omni-dev");
        fs::create_dir_all(&omni_dir).unwrap();
        let settings_path = omni_dir.join("settings.json");
        fs::write(&settings_path, r#"{"env": {"OTHER_KEY": "keep_me"}}"#).unwrap();

        let creds = DriveCredentials {
            client_id: "client-p".to_string(),
            client_secret: "secret-p".into(),
            refresh_token: "refresh-p".into(),
            scope: SCOPE_READONLY.to_string(),
        };
        save_credentials_to(&settings_path, Some("work"), &creds).unwrap();

        let val: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(
            val["profiles"]["work"]["env"]["DRIVE_CLIENT_ID"],
            "client-p"
        );
        assert!(val["env"].get("DRIVE_CLIENT_ID").is_none());
        assert_eq!(val["env"]["OTHER_KEY"], "keep_me");

        let removed = remove_credentials_at(&settings_path, Some("work")).unwrap();
        assert!(removed);
        let val: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert!(val["profiles"]["work"]["env"]
            .get("DRIVE_CLIENT_ID")
            .is_none());

        let removed = remove_credentials_at(&settings_path, Some("work")).unwrap();
        assert!(!removed);
    }

    /// The production wrappers resolve `~/.omni-dev/settings.json` from
    /// `HOME` and the active profile from `OMNI_DEV_PROFILE`, so this one
    /// test must redirect both via [`crate::drive::test_support::EnvGuard`].
    #[test]
    fn save_and_remove_credentials_resolve_default_settings_path() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();

        let creds = DriveCredentials {
            client_id: "wrapper-client".to_string(),
            client_secret: "wrapper-secret".into(),
            refresh_token: "wrapper-refresh".into(),
            scope: SCOPE_READONLY.to_string(),
        };
        save_credentials(&creds).unwrap();

        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        let val: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(val["env"]["DRIVE_CLIENT_ID"], "wrapper-client");

        assert!(remove_credentials().unwrap());
        let val: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert!(val["env"].get("DRIVE_CLIENT_ID").is_none());
    }

    // ── named-account dispatch (mirrors Gmail's issue #1500) ────────────
    //
    // These exercise the production `*_for` wrappers, so — like
    // `save_and_remove_credentials_resolve_default_settings_path` above —
    // they must redirect `HOME` via `EnvGuard`.

    #[test]
    fn load_credentials_for_named_reads_from_drive_accounts() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        Settings::upsert_drive_account(
            &settings_path,
            "work",
            &[
                (
                    "client_id",
                    serde_json::Value::String("work-id".to_string()),
                ),
                (
                    "client_secret",
                    serde_json::Value::String("work-secret".to_string()),
                ),
                (
                    "refresh_token",
                    serde_json::Value::String("work-refresh".to_string()),
                ),
                (
                    "scope",
                    serde_json::Value::String(SCOPE_READONLY.to_string()),
                ),
            ],
        )
        .unwrap();

        let creds = load_credentials_for(Some("work")).unwrap();
        assert_eq!(creds.client_id, "work-id");
        assert_eq!(creds.client_secret.expose_secret(), "work-secret");
        assert_eq!(creds.refresh_token.expose_secret(), "work-refresh");
        assert_eq!(creds.scope, SCOPE_READONLY);
    }

    #[test]
    fn load_credentials_for_unknown_named_account_errors() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        Settings::upsert_drive_account(
            &settings_path,
            "work",
            &[(
                "client_id",
                serde_json::Value::String("work-id".to_string()),
            )],
        )
        .unwrap();

        let err = load_credentials_for(Some("bogus")).unwrap_err();
        assert!(err.to_string().contains("unknown Drive account 'bogus'"));
    }

    #[test]
    fn load_credentials_for_falls_back_to_env_when_accounts_empty() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();
        std::env::set_var(DRIVE_CLIENT_ID, "literal-id");
        std::env::set_var(DRIVE_CLIENT_SECRET, "literal-secret");
        std::env::set_var(DRIVE_REFRESH_TOKEN, "literal-refresh");

        let creds = load_credentials_for(None).unwrap();
        assert_eq!(creds.client_id, "literal-id");
    }

    #[test]
    fn load_credentials_for_none_honors_ambient_account_env_var() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        Settings::upsert_drive_account(
            &settings_path,
            "work",
            &[
                (
                    "client_id",
                    serde_json::Value::String("work-id".to_string()),
                ),
                (
                    "client_secret",
                    serde_json::Value::String("work-secret".to_string()),
                ),
                (
                    "refresh_token",
                    serde_json::Value::String("work-refresh".to_string()),
                ),
            ],
        )
        .unwrap();
        std::env::set_var(account::DRIVE_ACCOUNT_ENV, "work");

        let creds = load_credentials_for(None).unwrap();
        assert_eq!(creds.client_id, "work-id");
    }

    #[test]
    fn remove_credentials_for_named_removes_whole_account() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        Settings::upsert_drive_account(
            &settings_path,
            "work",
            &[("client_id", serde_json::Value::String("id".to_string()))],
        )
        .unwrap();

        assert!(remove_credentials_for(Some("work")).unwrap());
        let val: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert!(val["drive"]["accounts"].get("work").is_none());
    }

    #[cfg(feature = "mcp")]
    #[test]
    fn status_for_named_reports_presence_from_account() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        Settings::upsert_drive_account(
            &settings_path,
            "work",
            &[
                ("client_id", serde_json::Value::String("id".to_string())),
                (
                    "scope",
                    serde_json::Value::String(SCOPE_READONLY.to_string()),
                ),
            ],
        )
        .unwrap();

        let status = status_for(Some("work")).unwrap();
        assert!(status.has_client_id);
        assert!(!status.has_client_secret);
        assert!(!status.has_refresh_token);
        assert_eq!(status.scope.as_deref(), Some(SCOPE_READONLY));
    }

    #[cfg(feature = "mcp")]
    #[test]
    fn status_for_unconfigured_matches_status_with_when_accounts_empty() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();
        std::env::set_var(DRIVE_CLIENT_ID, "literal-id");

        let status = status_for(None).unwrap();
        assert!(status.has_client_id);
        assert!(!status.has_refresh_token);
    }

    #[test]
    fn record_account_email_writes_email_address_only() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        Settings::upsert_drive_account(
            &settings_path,
            "work",
            &[("client_id", serde_json::Value::String("id".to_string()))],
        )
        .unwrap();

        record_account_email("work", "alice@work.com").unwrap();

        let val: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(
            val["drive"]["accounts"]["work"]["email_address"],
            "alice@work.com"
        );
        assert_eq!(val["drive"]["accounts"]["work"]["client_id"], "id");
    }

    #[test]
    fn record_account_email_does_not_overwrite_an_existing_value() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        Settings::upsert_drive_account(
            &settings_path,
            "work",
            &[(
                "email_address",
                serde_json::Value::String("manually-set@work.com".to_string()),
            )],
        )
        .unwrap();

        record_account_email("work", "alice@work.com").unwrap();

        let val: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(
            val["drive"]["accounts"]["work"]["email_address"],
            "manually-set@work.com"
        );
    }

    /// The empty-`drive.accounts` fallback path (mirrors Gmail's issue
    /// #1500 zero-migration guarantee): with `drive.accounts` empty, the
    /// account-aware `_for(None, ...)` wrappers must behave byte-identically
    /// to the direct env/settings wrappers they sit beside. Both sandboxes
    /// are seeded via the same unchanged [`save_credentials`] (no
    /// account-aware save wrapper exists — `login_for` persists directly,
    /// since a resolve-then-save round trip would reject the very name
    /// being created), so this isolates `load`/`remove`.
    #[test]
    fn accounts_empty_load_remove_byte_identical_via_direct_and_for_wrappers() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let creds = DriveCredentials {
            client_id: "id".to_string(),
            client_secret: "secret".into(),
            refresh_token: "refresh".into(),
            scope: SCOPE_READONLY.to_string(),
        };

        let dir_direct = guard.clear_credentials();
        save_credentials(&creds).unwrap();
        let direct_written =
            fs::read_to_string(dir_direct.path().join(".omni-dev").join("settings.json")).unwrap();
        let direct_loaded = load_credentials().unwrap();
        let direct_removed = remove_credentials().unwrap();

        let dir_for = guard.clear_credentials();
        save_credentials(&creds).unwrap();
        let for_written =
            fs::read_to_string(dir_for.path().join(".omni-dev").join("settings.json")).unwrap();
        let for_loaded = load_credentials_for(None).unwrap();
        let for_removed = remove_credentials_for(None).unwrap();

        assert_eq!(direct_written, for_written);
        assert_eq!(direct_loaded.client_id, for_loaded.client_id);
        assert_eq!(
            direct_loaded.client_secret.expose_secret(),
            for_loaded.client_secret.expose_secret()
        );
        assert_eq!(direct_loaded.scope, for_loaded.scope);
        assert_eq!(direct_removed, for_removed);
    }
}
