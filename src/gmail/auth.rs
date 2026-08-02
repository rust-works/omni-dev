//! Gmail OAuth2 authentication: authorization-code + PKCE login, credential
//! storage, and in-memory access-token refresh.
//!
//! See [ADR-0063](../../../docs/adrs/adr-0063.md) for the design rationale.
//! The loopback-listener + browser-launch shape follows the Snowflake
//! client's external-browser SSO flow (`crate::snowflake::client`'s private
//! `auth` module), extended with PKCE (RFC 7636), a `state` nonce, and an
//! `error=` branch — none of which a static-token or SSO-only flow needs.

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

use crate::gmail::error::{GmailError, GrantContext};
use crate::request_log;
use crate::utils::env::SystemEnv;
use crate::utils::secret::Secret;
use crate::utils::settings::{active_profile_from, Settings};

/// Environment variable / settings key for the user's Google Cloud OAuth2
/// client id.
pub const GMAIL_CLIENT_ID: &str = "GMAIL_CLIENT_ID";
/// Environment variable / settings key for the user's Google Cloud OAuth2
/// client secret.
pub const GMAIL_CLIENT_SECRET: &str = "GMAIL_CLIENT_SECRET";
/// Environment variable / settings key for the stored OAuth2 refresh token.
pub const GMAIL_REFRESH_TOKEN: &str = "GMAIL_REFRESH_TOKEN";
/// Environment variable / settings key recording the scope granted at login.
pub const GMAIL_SCOPE: &str = "GMAIL_SCOPE";

/// Google's OAuth2 authorization endpoint.
const AUTHORIZATION_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
/// Google's OAuth2 token endpoint.
const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
/// Read-only Gmail scope — the default.
pub const SCOPE_READONLY: &str = "https://www.googleapis.com/auth/gmail.readonly";
/// Read-write Gmail scope — required for label add/remove; opt-in.
pub const SCOPE_MODIFY: &str = "https://www.googleapis.com/auth/gmail.modify";

/// How long to wait for the browser sign-in callback before giving up.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(120);
/// How much slack to leave before an access token's tracked expiry before
/// proactively refreshing it.
const REFRESH_SKEW: TimeDelta = TimeDelta::seconds(60);

/// The Gmail OAuth2 scope granted at login.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GmailScope {
    /// List/read messages, threads, labels, and history. Default; never
    /// requests send.
    #[default]
    ReadOnly,
    /// Everything [`ReadOnly`](Self::ReadOnly) grants, plus label add/remove
    /// (`batchModify`).
    Modify,
}

impl GmailScope {
    /// Returns the wire scope string Google expects.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => SCOPE_READONLY,
            Self::Modify => SCOPE_MODIFY,
        }
    }

    /// Parses Google's space-separated granted-scope response, treating the
    /// presence of the modify scope anywhere in it as [`Modify`](Self::Modify).
    #[must_use]
    pub fn from_granted(granted: &str) -> Self {
        if granted
            .split_whitespace()
            .any(|scope| scope == SCOPE_MODIFY)
        {
            Self::Modify
        } else {
            Self::ReadOnly
        }
    }

    /// Whether this scope allows label-mutating calls (`batchModify`).
    #[must_use]
    pub fn allows_modify(self) -> bool {
        matches!(self, Self::Modify)
    }
}

/// Gmail OAuth2 credentials.
#[derive(Debug, Clone)]
pub struct GmailCredentials {
    /// OAuth2 client id (not secret — visible in the browser's own network
    /// traffic during login regardless).
    pub client_id: String,
    /// OAuth2 client secret (redacted in `Debug` output).
    pub client_secret: Secret,
    /// The stored refresh token (redacted in `Debug` output).
    pub refresh_token: Secret,
    /// The scope granted at the login that produced this refresh token.
    pub scope: GmailScope,
}

/// Secret-free presence/scope report, safe to serialise (e.g. over MCP).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GmailAuthStatus {
    /// Whether [`GMAIL_CLIENT_ID`] is present.
    pub has_client_id: bool,
    /// Whether [`GMAIL_CLIENT_SECRET`] is present.
    pub has_client_secret: bool,
    /// Whether [`GMAIL_REFRESH_TOKEN`] is present.
    pub has_refresh_token: bool,
    /// The granted scope, if recorded. `None` when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// Loads Gmail credentials from environment variables or settings.json.
///
/// Environment variables take precedence over the settings file.
pub fn load_credentials() -> Result<GmailCredentials> {
    load_credentials_with(&crate::utils::settings::SettingsEnv::load())
}

/// [`load_credentials`] over an injected
/// [`EnvSource`](crate::utils::env::EnvSource).
///
/// Tests pass a pure `MapEnv` so credential resolution is exercised without
/// mutating the process environment (issue #1030 / STYLE-0028).
pub(crate) fn load_credentials_with(
    env: &impl crate::utils::env::EnvSource,
) -> Result<GmailCredentials> {
    let client_id = env
        .var(GMAIL_CLIENT_ID)
        .ok_or(GmailError::CredentialsNotFound)?;
    let client_secret = env
        .var(GMAIL_CLIENT_SECRET)
        .ok_or(GmailError::CredentialsNotFound)?;
    let refresh_token = env
        .var(GMAIL_REFRESH_TOKEN)
        .ok_or(GmailError::CredentialsNotFound)?;
    let scope = env
        .var(GMAIL_SCOPE)
        .map(|s| GmailScope::from_granted(&s))
        .unwrap_or_default();

    Ok(GmailCredentials {
        client_id,
        client_secret: client_secret.into(),
        refresh_token: refresh_token.into(),
        scope,
    })
}

/// Builds a [`GmailAuthStatus`] from the current settings / environment.
///
/// Reports credential presence without leaking any secret values. Safe to
/// call with no credentials configured.
pub fn status() -> GmailAuthStatus {
    status_with(&crate::utils::settings::SettingsEnv::load())
}

/// [`status`] over an injected [`EnvSource`](crate::utils::env::EnvSource).
pub(crate) fn status_with(env: &impl crate::utils::env::EnvSource) -> GmailAuthStatus {
    GmailAuthStatus {
        has_client_id: env.var(GMAIL_CLIENT_ID).is_some(),
        has_client_secret: env.var(GMAIL_CLIENT_SECRET).is_some(),
        has_refresh_token: env.var(GMAIL_REFRESH_TOKEN).is_some(),
        scope: env.var(GMAIL_SCOPE),
    }
}

/// Saves Gmail credentials to `~/.omni-dev/settings.json`.
///
/// Merges the four credential keys into the active profile's `env` map (the
/// base `env` when no profile is active), preserving all other settings.
pub fn save_credentials(credentials: &GmailCredentials) -> Result<()> {
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
    credentials: &GmailCredentials,
) -> Result<()> {
    Settings::upsert_env_vars_in(
        settings_path,
        profile,
        &[
            (GMAIL_CLIENT_ID, credentials.client_id.as_str()),
            (
                GMAIL_CLIENT_SECRET,
                credentials.client_secret.expose_secret(),
            ),
            (
                GMAIL_REFRESH_TOKEN,
                credentials.refresh_token.expose_secret(),
            ),
            (GMAIL_SCOPE, credentials.scope.as_str()),
        ],
    )
}

/// Removes Gmail credential keys from `~/.omni-dev/settings.json` — this
/// *is* `gmail auth logout`.
///
/// Returns `true` if any Gmail key was present and removed, `false`
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
            GMAIL_CLIENT_ID,
            GMAIL_CLIENT_SECRET,
            GMAIL_REFRESH_TOKEN,
            GMAIL_SCOPE,
        ],
    )
}

// ── Browser launch ──────────────────────────────────────────────────────

/// How to open the authorization URL during login.
///
/// Deliberately duplicated from (not shared with)
/// [`crate::snowflake::client::config::BrowserLaunch`] — a small, stable
/// shape with no existing "generic browser launch" module to promote into;
/// extract only on a third consumer.
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
            tracing::info!("Open this URL in a browser to sign in to Gmail:\n{url}");
            Ok(())
        }
        BrowserLaunch::Command(args) => {
            let mut parts = args.iter();
            let program = parts
                .next()
                .ok_or_else(|| GmailError::InvalidBrowserCommand("empty browser command".into()))?;
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
    scope: GmailScope,
    state: &str,
    code_challenge: &str,
) -> Result<Url> {
    let mut url =
        Url::parse(AUTHORIZATION_ENDPOINT).context("Invalid Gmail authorization endpoint")?;
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", scope.as_str())
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
        .map_err(|_| GmailError::CallbackTimeout(timeout.as_secs()))?
        .context("Failed to accept the browser's callback connection")?;

    let mut buf = vec![0u8; 8192];
    let n = stream
        .read(&mut buf)
        .await
        .context("Failed to read the callback request")?;
    let request = String::from_utf8_lossy(&buf[..n]);

    let result = parse_callback(&request).ok_or(GmailError::MalformedCallback)?;
    tracing::info!("Gmail OAuth callback received");

    let body = if result.error.is_some() {
        "<html><body>Sign-in failed. You can close this tab and check the terminal.</body></html>"
    } else {
        "<html><body>Gmail sign-in complete. You can close this tab.</body></html>"
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
    request_log::record_http_result("gmail", "POST", token_endpoint, started, &result);
    let response = result.context("Failed to send token request to Google")?;

    if !response.status().is_success() {
        let body = response.text().await.unwrap_or_default();
        if let Ok(err) = serde_json::from_str::<TokenErrorResponse>(&body) {
            if err.error == "invalid_grant" {
                return Err(GmailError::InvalidGrant(context).into());
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

/// The mutable access-token state, refreshed by [`GmailSession::refresh_locked`].
struct TokenState {
    access_token: Secret,
    expires_at: DateTime<Utc>,
}

/// A live Gmail OAuth2 session: holds the refresh token and the current
/// in-memory access token, refreshing on demand.
///
/// Uses [`tokio::sync::Mutex`] (not `std::sync::Mutex`) held *across* the
/// refresh network call — unlike
/// [`SnowflakeSession::renew`](crate::snowflake::client::SnowflakeSession::renew),
/// which releases its lock before the network call and accepts concurrent
/// refreshes racing each other. Gmail's design requires single-flight
/// refresh (issue #1465's explicit "concurrent callers don't stampede"
/// requirement): a second concurrent caller blocks on this mutex and, once
/// unblocked, observes the already-refreshed token instead of issuing a
/// second POST.
pub struct GmailSession {
    http: reqwest::Client,
    client_id: String,
    client_secret: Secret,
    refresh_token: Secret,
    token_endpoint: String,
    state: tokio::sync::Mutex<TokenState>,
}

impl GmailSession {
    /// Creates a session against Google's real token endpoint.
    pub(crate) fn new(http: reqwest::Client, credentials: &GmailCredentials) -> Self {
        Self::new_with_token_endpoint(http, credentials, TOKEN_ENDPOINT)
    }

    /// [`new`](Self::new) against an explicit token endpoint — the test seam
    /// for pointing at a wiremock server.
    pub(crate) fn new_with_token_endpoint(
        http: reqwest::Client,
        credentials: &GmailCredentials,
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
        state.expires_at = Utc::now() + TimeDelta::seconds(response.expires_in);
        Ok(())
    }
}

// ── Login orchestration ─────────────────────────────────────────────────

/// Runs the OAuth2 authorization-code + PKCE login flow, persisting the
/// resulting refresh token to `~/.omni-dev/settings.json`.
pub async fn login(
    client_id: &str,
    client_secret: &Secret,
    scope: GmailScope,
    browser: &BrowserConfig,
) -> Result<GmailAuthStatus> {
    login_to(
        &Settings::get_settings_path()?,
        active_profile_from(&SystemEnv).as_deref(),
        client_id,
        client_secret,
        scope,
        browser,
        TOKEN_ENDPOINT,
    )
    .await
}

/// [`login`], writing to an explicit settings-file path/profile and against
/// an explicit token endpoint — the test seam for a wiremock server.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn login_to(
    settings_path: &Path,
    profile: Option<&str>,
    client_id: &str,
    client_secret: &Secret,
    scope: GmailScope,
    browser: &BrowserConfig,
    token_endpoint: &str,
) -> Result<GmailAuthStatus> {
    let (listener, port) = bind_callback_listener(browser).await?;
    let redirect_uri = format!("http://127.0.0.1:{port}");

    let pending = generate_pending_login();
    let challenge = code_challenge(&pending.code_verifier);
    let auth_url =
        build_authorization_url(client_id, &redirect_uri, scope, &pending.state, &challenge)?;
    open_browser(&browser.launch, auth_url.as_str())?;

    let callback = wait_for_callback(listener).await?;
    if let Some(error) = callback.error {
        return Err(GmailError::authorization_denied(
            &error,
            callback.error_description.as_deref(),
        )
        .into());
    }
    let (Some(code), Some(returned_state)) = (callback.code, callback.state) else {
        return Err(GmailError::MalformedCallback.into());
    };
    // Plain equality, not constant-time: `state` is a CSRF nonce carried in
    // a browser-visible URL, not a secret — there's nothing for a timing
    // side-channel to extract here (unlike `constant_time_eq`'s real use
    // guarding a bridge auth token in `src/browser/auth.rs`).
    if returned_state != pending.state {
        return Err(GmailError::StateMismatch.into());
    }

    let http = reqwest::Client::builder()
        .timeout(crate::utils::http::REQUEST_TIMEOUT)
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
        .ok_or(GmailError::MalformedTokenResponse("refresh_token"))?;
    let granted_scope = tokens.scope.map_or(scope, |s| GmailScope::from_granted(&s));

    let credentials = GmailCredentials {
        client_id: client_id.to_string(),
        client_secret: client_secret.clone(),
        refresh_token: refresh_token.into(),
        scope: granted_scope,
    };
    save_credentials_to(settings_path, profile, &credentials)?;

    Ok(GmailAuthStatus {
        has_client_id: true,
        has_client_secret: true,
        has_refresh_token: true,
        scope: Some(granted_scope.as_str().to_string()),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::fs;

    use super::*;

    // ── Pure helpers ─────────────────────────────────────────────────

    #[test]
    fn scope_as_str_matches_google_scope_strings() {
        assert_eq!(GmailScope::ReadOnly.as_str(), SCOPE_READONLY);
        assert_eq!(GmailScope::Modify.as_str(), SCOPE_MODIFY);
    }

    #[test]
    fn scope_from_granted_detects_modify_anywhere_in_the_list() {
        assert_eq!(
            GmailScope::from_granted(&format!("{SCOPE_READONLY} {SCOPE_MODIFY}")),
            GmailScope::Modify
        );
        assert_eq!(
            GmailScope::from_granted(SCOPE_READONLY),
            GmailScope::ReadOnly
        );
        assert_eq!(GmailScope::from_granted(""), GmailScope::ReadOnly);
    }

    #[test]
    fn scope_allows_modify() {
        assert!(!GmailScope::ReadOnly.allows_modify());
        assert!(GmailScope::Modify.allows_modify());
    }

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
            GmailScope::ReadOnly,
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
    fn build_authorization_url_uses_modify_scope_when_requested() {
        let url = build_authorization_url("c", "http://127.0.0.1:1", GmailScope::Modify, "s", "ch")
            .unwrap();
        let query: std::collections::HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(query.get("scope").unwrap(), SCOPE_MODIFY);
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

    // ── Loopback listener (real sockets, no wiremock) ───────────────────

    #[tokio::test]
    async fn wait_for_callback_times_out_when_nothing_connects() {
        let browser = BrowserConfig::default();
        let (listener, _port) = bind_callback_listener(&browser).await.unwrap();
        let err = wait_for_callback_with_timeout(listener, Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(matches!(
            err.downcast_ref::<GmailError>(),
            Some(GmailError::CallbackTimeout(_))
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
            err.downcast_ref::<GmailError>(),
            Some(GmailError::MalformedCallback)
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

    #[tokio::test]
    async fn login_to_rejects_a_callback_with_mismatched_state() {
        let port = reserve_free_port();
        let browser = BrowserConfig {
            launch: BrowserLaunch::Manual,
            callback_addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            callback_port: port,
        };
        let connector = tokio::spawn(connect_and_send(
            port,
            b"GET /?code=abc&state=the-wrong-state HTTP/1.1\r\n\r\n",
        ));

        std::fs::create_dir_all("tmp").ok();
        let temp_dir = tempfile::TempDir::new_in("tmp").unwrap();
        let settings_path = temp_dir.path().join("settings.json");

        let err = login_to(
            &settings_path,
            None,
            "client-id",
            &Secret::new("client-secret"),
            GmailScope::ReadOnly,
            &browser,
            "http://127.0.0.1:1/token", // never reached — state check fails first
        )
        .await
        .unwrap_err();
        connector.await.unwrap();

        assert!(matches!(
            err.downcast_ref::<GmailError>(),
            Some(GmailError::StateMismatch)
        ));
        assert!(!settings_path.exists());
    }

    #[tokio::test]
    async fn login_to_surfaces_access_denied_from_the_callback() {
        let port = reserve_free_port();
        let browser = BrowserConfig {
            launch: BrowserLaunch::Manual,
            callback_addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            callback_port: port,
        };
        let connector = tokio::spawn(connect_and_send(
            port,
            b"GET /?error=access_denied&error_description=user+declined HTTP/1.1\r\n\r\n",
        ));

        std::fs::create_dir_all("tmp").ok();
        let temp_dir = tempfile::TempDir::new_in("tmp").unwrap();
        let settings_path = temp_dir.path().join("settings.json");

        let err = login_to(
            &settings_path,
            None,
            "client-id",
            &Secret::new("client-secret"),
            GmailScope::ReadOnly,
            &browser,
            "http://127.0.0.1:1/token", // never reached — denied before exchange
        )
        .await
        .unwrap_err();
        connector.await.unwrap();

        match err.downcast_ref::<GmailError>() {
            Some(GmailError::AuthorizationDenied(message)) => {
                assert!(message.contains("access_denied"));
                assert!(message.contains("user declined"));
            }
            other => panic!("expected AuthorizationDenied, got {other:?}"),
        }
        assert!(!settings_path.exists());
    }

    #[tokio::test]
    async fn login_to_rejects_a_callback_missing_code_and_state() {
        let port = reserve_free_port();
        let browser = BrowserConfig {
            launch: BrowserLaunch::Manual,
            callback_addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            callback_port: port,
        };
        let connector = tokio::spawn(connect_and_send(port, b"GET /?foo=bar HTTP/1.1\r\n\r\n"));

        std::fs::create_dir_all("tmp").ok();
        let temp_dir = tempfile::TempDir::new_in("tmp").unwrap();
        let settings_path = temp_dir.path().join("settings.json");

        let err = login_to(
            &settings_path,
            None,
            "client-id",
            &Secret::new("client-secret"),
            GmailScope::ReadOnly,
            &browser,
            "http://127.0.0.1:1/token", // never reached — malformed before exchange
        )
        .await
        .unwrap_err();
        connector.await.unwrap();

        assert!(matches!(
            err.downcast_ref::<GmailError>(),
            Some(GmailError::MalformedCallback)
        ));
        assert!(!settings_path.exists());
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
        let port = reserve_free_port();
        std::fs::create_dir_all("tmp").ok();
        let temp_dir = tempfile::TempDir::new_in("tmp").unwrap();
        let capture_path = temp_dir.path().join("captured-url.txt");

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

        let connector = tokio::spawn(async move {
            let auth_url = loop {
                if let Ok(contents) = std::fs::read_to_string(&capture_path) {
                    if !contents.is_empty() {
                        break contents;
                    }
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            };
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

        let settings_path = temp_dir.path().join("settings.json");
        let status = login_to(
            &settings_path,
            None,
            "client-id",
            &Secret::new("client-secret"),
            GmailScope::ReadOnly,
            &browser,
            &format!("{}/token", server.uri()),
        )
        .await
        .unwrap();
        connector.await.unwrap();

        assert!(status.has_client_id);
        assert!(status.has_client_secret);
        assert!(status.has_refresh_token);
        assert_eq!(status.scope.as_deref(), Some(SCOPE_READONLY));

        let saved = std::fs::read_to_string(&settings_path).unwrap();
        assert!(saved.contains("rt-1"));
        assert!(saved.contains("client-id"));
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

    // ── GmailSession ─────────────────────────────────────────────────

    fn test_credentials() -> GmailCredentials {
        GmailCredentials {
            client_id: "client-1".to_string(),
            client_secret: "secret-1".into(),
            refresh_token: "refresh-1".into(),
            scope: GmailScope::ReadOnly,
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

        let session = GmailSession::new_with_token_endpoint(
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

        let session = GmailSession::new_with_token_endpoint(
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

        let session = GmailSession::new_with_token_endpoint(
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

        let session = GmailSession::new_with_token_endpoint(
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

        let session = GmailSession::new_with_token_endpoint(
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
    fn gmail_credentials_debug_redacts_client_secret_and_refresh_token() {
        let creds = GmailCredentials {
            client_id: "client-visible".to_string(),
            client_secret: "sekret-client-secret".into(),
            refresh_token: "sekret-refresh-token".into(),
            scope: GmailScope::ReadOnly,
        };
        let debug = format!("{creds:?}");
        assert!(debug.contains("GmailCredentials"));
        assert!(debug.contains("client-visible"));
        assert!(!debug.contains("sekret-client-secret"));
        assert!(!debug.contains("sekret-refresh-token"));
        assert!(debug.contains("client_secret: <redacted>"));
        assert!(debug.contains("refresh_token: <redacted>"));
    }

    #[test]
    fn gmail_auth_status_yaml_serialization_contains_no_secret_values() {
        let env = crate::test_support::env::MapEnv::new()
            .with(GMAIL_CLIENT_ID, "client-id-value")
            .with(GMAIL_CLIENT_SECRET, "sekret-do-not-leak")
            .with(GMAIL_REFRESH_TOKEN, "sekret-refresh-do-not-leak")
            .with(GMAIL_SCOPE, SCOPE_READONLY);
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
        let env = MapEnv::new().with(GMAIL_SCOPE, SCOPE_MODIFY);
        let status = status_with(&env);
        assert_eq!(status.scope.as_deref(), Some(SCOPE_MODIFY));
    }

    #[test]
    fn load_credentials_errors_when_client_id_missing() {
        let env = MapEnv::new()
            .with(GMAIL_CLIENT_SECRET, "s")
            .with(GMAIL_REFRESH_TOKEN, "r");
        let err = load_credentials_with(&env).unwrap_err();
        assert!(err.to_string().contains("not configured"));
    }

    #[test]
    fn load_credentials_errors_when_client_secret_missing() {
        let env = MapEnv::new()
            .with(GMAIL_CLIENT_ID, "c")
            .with(GMAIL_REFRESH_TOKEN, "r");
        assert!(load_credentials_with(&env).is_err());
    }

    #[test]
    fn load_credentials_errors_when_refresh_token_missing() {
        let env = MapEnv::new()
            .with(GMAIL_CLIENT_ID, "c")
            .with(GMAIL_CLIENT_SECRET, "s");
        assert!(load_credentials_with(&env).is_err());
    }

    #[test]
    fn load_credentials_succeeds_with_all_three_present() {
        let env = MapEnv::new()
            .with(GMAIL_CLIENT_ID, "c")
            .with(GMAIL_CLIENT_SECRET, "s")
            .with(GMAIL_REFRESH_TOKEN, "r");
        let creds = load_credentials_with(&env).unwrap();
        assert_eq!(creds.client_id, "c");
        assert_eq!(creds.scope, GmailScope::ReadOnly);
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

            let creds = GmailCredentials {
                client_id: "client-1".to_string(),
                client_secret: "secret-1".into(),
                refresh_token: "refresh-1".into(),
                scope: GmailScope::ReadOnly,
            };
            save_credentials_to(&settings_path, None, &creds).unwrap();

            assert!(settings_path.exists());
            let val: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
            assert_eq!(val["env"]["GMAIL_CLIENT_ID"], "client-1");
            assert_eq!(val["env"]["GMAIL_CLIENT_SECRET"], "secret-1");
            assert_eq!(val["env"]["GMAIL_REFRESH_TOKEN"], "refresh-1");
            assert_eq!(val["env"]["GMAIL_SCOPE"], SCOPE_READONLY);

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

            let creds = GmailCredentials {
                client_id: "client-2".to_string(),
                client_secret: "secret-2".into(),
                refresh_token: "refresh-2".into(),
                scope: GmailScope::Modify,
            };
            save_credentials_to(&settings_path, None, &creds).unwrap();

            let val: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
            assert_eq!(val["env"]["OTHER_KEY"], "keep_me");
            assert_eq!(val["extra"], true);
            assert_eq!(val["env"]["GMAIL_SCOPE"], SCOPE_MODIFY);
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
                    "GMAIL_CLIENT_ID": "a",
                    "GMAIL_CLIENT_SECRET": "b",
                    "GMAIL_REFRESH_TOKEN": "c",
                    "GMAIL_SCOPE": "d",
                    "OTHER_KEY": "keep"
                }}"#,
            )
            .unwrap();

            let removed = remove_credentials_at(&settings_path, None).unwrap();
            assert!(removed);

            let val: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
            assert!(val["env"].get("GMAIL_CLIENT_ID").is_none());
            assert!(val["env"].get("GMAIL_CLIENT_SECRET").is_none());
            assert!(val["env"].get("GMAIL_REFRESH_TOKEN").is_none());
            assert!(val["env"].get("GMAIL_SCOPE").is_none());
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

        let creds = GmailCredentials {
            client_id: "client-p".to_string(),
            client_secret: "secret-p".into(),
            refresh_token: "refresh-p".into(),
            scope: GmailScope::ReadOnly,
        };
        save_credentials_to(&settings_path, Some("work"), &creds).unwrap();

        let val: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(
            val["profiles"]["work"]["env"]["GMAIL_CLIENT_ID"],
            "client-p"
        );
        assert!(val["env"].get("GMAIL_CLIENT_ID").is_none());
        assert_eq!(val["env"]["OTHER_KEY"], "keep_me");

        let removed = remove_credentials_at(&settings_path, Some("work")).unwrap();
        assert!(removed);
        let val: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert!(val["profiles"]["work"]["env"]
            .get("GMAIL_CLIENT_ID")
            .is_none());

        let removed = remove_credentials_at(&settings_path, Some("work")).unwrap();
        assert!(!removed);
    }

    /// The production wrappers resolve `~/.omni-dev/settings.json` from
    /// `HOME` and the active profile from `OMNI_DEV_PROFILE`, so this one
    /// test must redirect both via [`crate::gmail::test_support::EnvGuard`].
    #[test]
    fn save_and_remove_credentials_resolve_default_settings_path() {
        let guard = crate::gmail::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();

        let creds = GmailCredentials {
            client_id: "wrapper-client".to_string(),
            client_secret: "wrapper-secret".into(),
            refresh_token: "wrapper-refresh".into(),
            scope: GmailScope::ReadOnly,
        };
        save_credentials(&creds).unwrap();

        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        let val: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(val["env"]["GMAIL_CLIENT_ID"], "wrapper-client");

        assert!(remove_credentials().unwrap());
        let val: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert!(val["env"].get("GMAIL_CLIENT_ID").is_none());
    }
}
