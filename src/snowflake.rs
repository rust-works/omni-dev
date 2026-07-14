//! Account-agnostic Snowflake query engine hosted by the daemon.
//!
//! Each `(account, user)` keeps a **bounded pool** of authenticated sessions
//! (see [`session`]). A query checks one out, applies any per-query context with
//! `USE` (skipping `USE`s already in effect), runs concurrently with other
//! checkouts, and returns it. This gives **concurrent queries on a single
//! authentication identity** while still honoring per-query
//! `warehouse`/`role`/`database`/`schema`, with the number of browser auths
//! capped at the pool size and grown lazily.
//!
//! A background keep-alive heartbeat ([`SnowflakeEngine::start_heartbeat`])
//! periodically heartbeats idle sessions so their master tokens stay valid and
//! an idle pool never re-prompts browser SSO.
//!
//! This is the standalone engine, analogous to [`crate::browser`]; the daemon
//! adapter lives in [`crate::daemon::services::snowflake`].

pub mod client;
pub mod session;

use std::sync::{Mutex as StdMutex, MutexGuard};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use chrono::TimeDelta;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::utils::env::EnvSource;
use crate::utils::secret::Secret;
use crate::utils::settings::Settings;
use client::{
    AuthMethod, BrowserConfig, BrowserLaunch, Error as ClientError, KeyPairConfig, Row,
    SnowflakeClient, SnowflakeClientConfig, SnowflakeSession,
};
use session::{PoolRegistry, QueryContext, SessionInfo, SessionKey, SessionPool};

/// Env var (with `~/.omni-dev/settings.json` fallback) for the default account.
const ENV_ACCOUNT: &str = "SNOWFLAKE_ACCOUNT";
/// Env var for the default user.
const ENV_USER: &str = "SNOWFLAKE_USER";
/// Env var overriding the API host (verbatim), instead of deriving
/// `<account>.snowflakecomputing.com`. Needed for AWS/Azure PrivateLink
/// endpoints and gov/custom hosts.
const ENV_HOST: &str = "SNOWFLAKE_HOST";
/// Env var for the default warehouse.
const ENV_WAREHOUSE: &str = "SNOWFLAKE_WAREHOUSE";
/// Env var for the default role.
const ENV_ROLE: &str = "SNOWFLAKE_ROLE";
/// Env var for the default database.
const ENV_DATABASE: &str = "SNOWFLAKE_DATABASE";
/// Env var for the default schema.
const ENV_SCHEMA: &str = "SNOWFLAKE_SCHEMA";
/// Env var for the per-`(account, user)` pool size (max concurrent sessions).
const ENV_POOL_SIZE: &str = "SNOWFLAKE_POOL_SIZE";
/// Env var for the per-request HTTP timeout (seconds).
const ENV_HTTP_TIMEOUT: &str = "SNOWFLAKE_HTTP_TIMEOUT";
/// Env var for the overall sign-in deadline (seconds).
const ENV_AUTH_TIMEOUT: &str = "SNOWFLAKE_AUTH_TIMEOUT";
/// Env var for the overall per-query deadline incl. async polling (seconds).
const ENV_QUERY_TIMEOUT: &str = "SNOWFLAKE_QUERY_TIMEOUT";
/// Env var for the idle-session keep-alive heartbeat interval (seconds; `0`
/// disables the heartbeat).
const ENV_HEARTBEAT_INTERVAL: &str = "SNOWFLAKE_HEARTBEAT_INTERVAL";
/// Env var selecting the auth method: `externalbrowser` (default; interactive
/// SSO), `programmatic_access_token`, or `snowflake_jwt` (both non-interactive).
const ENV_AUTHENTICATOR: &str = "SNOWFLAKE_AUTHENTICATOR";
/// Env var for the programmatic access token (PAT auth).
const ENV_TOKEN: &str = "SNOWFLAKE_TOKEN";
/// Env var for the path to an unencrypted PKCS#8 PEM private key (JWT auth).
const ENV_PRIVATE_KEY_PATH: &str = "SNOWFLAKE_PRIVATE_KEY_PATH";
/// Env var for an inline unencrypted PKCS#8 PEM private key (alternative to the
/// path).
const ENV_PRIVATE_KEY: &str = "SNOWFLAKE_PRIVATE_KEY";
/// Env var for an encrypted key's passphrase. Recognized but not yet supported;
/// setting it with the JWT method is a clear error.
const ENV_PRIVATE_KEY_PASSPHRASE: &str = "SNOWFLAKE_PRIVATE_KEY_PASSPHRASE";
/// Env var for the external-browser SSO launch command: a single command line
/// with a `{url}` placeholder (or the URL is appended as a trailing arg when the
/// placeholder is absent). Quote-aware (single/double quotes, backslash escapes)
/// so program paths and argument values may contain spaces, e.g.
/// `/Applications/Google Chrome.app/Contents/MacOS/Google Chrome
/// --profile-directory="Profile 1" --new-window {url}`. Blank/unset opens the OS
/// default handler ([`BrowserLaunch::Auto`]). Ignored by the non-interactive
/// auth methods (PAT / key-pair JWT), which open no browser.
const ENV_BROWSER_COMMAND: &str = "SNOWFLAKE_BROWSER_COMMAND";

/// Default pool size when `SNOWFLAKE_POOL_SIZE` is unset: the max concurrent
/// queries (and max browser auths) per `(account, user)`.
const DEFAULT_POOL_SIZE: usize = 4;
/// Default overall sign-in deadline: comfortably over the SSO callback wait so a
/// genuine sign-in completes, but bounded so a hung auth can't hold the gate.
const DEFAULT_AUTH_TIMEOUT: Duration = Duration::from_secs(150);
/// Max characters of SQL shown in the "running" preview for a busy session.
const SQL_PREVIEW_MAX: usize = 60;
/// Default keep-alive heartbeat interval: a quarter of the default 3600s
/// session-token validity (Snowflake's own drivers clamp their heartbeat
/// frequency to 900–3600s), so an idle session renews comfortably before
/// either token lapses.
const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(900);
/// Extra margin past the heartbeat interval when deciding to proactively renew
/// an idle session's token before it would lapse mid-cycle.
const HEARTBEAT_RENEW_MARGIN_SECS: i64 = 60;

/// Engine defaults, resolved from environment variables and then
/// `~/.omni-dev/settings.json` (the Atlassian credential-resolution pattern).
///
/// Account/user/context are optional; a request supplies its own and falls back
/// to these. There is **no** hardcoded account list or alias map.
#[derive(Clone, Debug)]
pub struct SnowflakeEngineConfig {
    /// Default account when a request omits `--account`.
    pub default_account: Option<String>,
    /// Default user when a request omits `--user`.
    pub default_user: Option<String>,
    /// Override the API host (verbatim) instead of deriving
    /// `<account>.snowflakecomputing.com`. Set for PrivateLink / gov / custom
    /// hosts; applies to every session this engine creates.
    pub default_host: Option<String>,
    /// Default warehouse applied at session creation.
    pub default_warehouse: Option<String>,
    /// Default role applied at session creation.
    pub default_role: Option<String>,
    /// Default database applied at session creation.
    pub default_database: Option<String>,
    /// Default schema applied at session creation.
    pub default_schema: Option<String>,
    /// How sessions authenticate (SSO by default; PAT or key-pair JWT for
    /// non-interactive/headless use). The credential is the same for every
    /// session in every pool this engine creates.
    pub auth: AuthMethod,
    /// Max concurrent sessions (and browser auths) per `(account, user)`.
    pub pool_size: usize,
    /// Per-request HTTP timeout for REST calls.
    pub http_timeout: Duration,
    /// Overall deadline for one sign-in (SSO + login) so a hung auth can't hold
    /// the shared auth gate indefinitely.
    pub auth_timeout: Duration,
    /// Overall deadline for one query (submit + async-result polling).
    pub query_timeout: Duration,
    /// How often the background task heartbeats idle sessions to keep their
    /// master token alive. Zero disables the heartbeat.
    pub heartbeat_interval: Duration,
}

impl Default for SnowflakeEngineConfig {
    fn default() -> Self {
        Self {
            default_account: None,
            default_user: None,
            default_host: None,
            default_warehouse: None,
            default_role: None,
            default_database: None,
            default_schema: None,
            auth: AuthMethod::ExternalBrowser(BrowserConfig::default()),
            pool_size: DEFAULT_POOL_SIZE,
            http_timeout: client::config::DEFAULT_HTTP_TIMEOUT,
            auth_timeout: DEFAULT_AUTH_TIMEOUT,
            query_timeout: client::config::DEFAULT_QUERY_TIMEOUT,
            heartbeat_interval: DEFAULT_HEARTBEAT_INTERVAL,
        }
    }
}

impl SnowflakeEngineConfig {
    /// Resolves defaults from env vars (then settings.json). Cheap and
    /// side-effect-free; never authenticates.
    ///
    /// # Errors
    ///
    /// If `SNOWFLAKE_AUTHENTICATOR` names an unknown method or a selected
    /// non-interactive method is missing its credential (see
    /// [`resolve_auth_method`]).
    pub fn from_env_and_settings() -> Result<Self> {
        let settings = Settings::load().unwrap_or_default();
        let pool_size = settings
            .get_env_var(ENV_POOL_SIZE)
            .and_then(|s| s.trim().parse::<usize>().ok())
            .filter(|&n| n >= 1)
            .unwrap_or(DEFAULT_POOL_SIZE);
        let secs = |key: &str| {
            settings
                .get_env_var(key)
                .and_then(|s| s.trim().parse::<u64>().ok())
                .filter(|&n| n >= 1)
                .map(Duration::from_secs)
        };
        let private_key_pem = match settings.get_env_var(ENV_PRIVATE_KEY_PATH) {
            Some(path) => Some(
                std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {ENV_PRIVATE_KEY_PATH} '{path}'"))?,
            ),
            None => settings.get_env_var(ENV_PRIVATE_KEY),
        };
        let auth = resolve_auth_method(
            settings.get_env_var(ENV_AUTHENTICATOR).as_deref(),
            settings.get_env_var(ENV_BROWSER_COMMAND),
            settings.get_env_var(ENV_TOKEN),
            private_key_pem,
            settings.get_env_var(ENV_PRIVATE_KEY_PASSPHRASE),
        )?;
        Ok(Self {
            default_account: settings.get_env_var(ENV_ACCOUNT),
            default_user: settings.get_env_var(ENV_USER),
            default_host: host_override_from(settings.get_env_var(ENV_HOST)),
            default_warehouse: settings.get_env_var(ENV_WAREHOUSE),
            default_role: settings.get_env_var(ENV_ROLE),
            default_database: settings.get_env_var(ENV_DATABASE),
            default_schema: settings.get_env_var(ENV_SCHEMA),
            auth,
            pool_size,
            http_timeout: secs(ENV_HTTP_TIMEOUT).unwrap_or(client::config::DEFAULT_HTTP_TIMEOUT),
            auth_timeout: secs(ENV_AUTH_TIMEOUT).unwrap_or(DEFAULT_AUTH_TIMEOUT),
            query_timeout: secs(ENV_QUERY_TIMEOUT).unwrap_or(client::config::DEFAULT_QUERY_TIMEOUT),
            heartbeat_interval: heartbeat_interval_from(
                settings.get_env_var(ENV_HEARTBEAT_INTERVAL),
            ),
        })
    }
}

/// Normalizes the `SNOWFLAKE_HOST` override: trims surrounding whitespace (e.g. a
/// trailing newline from a `$(cat …)`-style value) and treats a blank value as
/// unset, so an empty override never shadows the derived host.
fn host_override_from(raw: Option<String>) -> Option<String> {
    raw.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Parses the heartbeat-interval setting: seconds, with `0` meaning disabled.
/// Unset or unparseable values fall back to the default. (The `secs` helper in
/// [`SnowflakeEngineConfig::from_env_and_settings`] rejects `0`, which here is
/// a meaningful value.)
fn heartbeat_interval_from(raw: Option<String>) -> Duration {
    raw.and_then(|s| s.trim().parse::<u64>().ok())
        .map_or(DEFAULT_HEARTBEAT_INTERVAL, Duration::from_secs)
}

/// Resolves the [`AuthMethod`] from the `SNOWFLAKE_AUTHENTICATOR` selector and
/// the method-specific credential vars (`private_key_pem` is the already-read
/// key material, from a file or inline). An unset or blank selector keeps
/// external-browser SSO, preserving the pre-#1108 default. The PAT secret is
/// trimmed (dropping a stray trailing newline from `$(cat …)`-style values).
///
/// `browser_command` (the raw `SNOWFLAKE_BROWSER_COMMAND` value) configures how
/// external-browser SSO opens the sign-in URL: a non-blank value becomes a
/// [`BrowserLaunch::Command`] (parsed by [`split_browser_command`]); blank/unset
/// keeps [`BrowserLaunch::Auto`]. It is ignored by the non-interactive methods,
/// which open no browser.
///
/// # Errors
///
/// If the selector is unknown, a non-interactive method is selected without its
/// credential, an encrypted key passphrase is set (unsupported), or (for
/// external-browser) `browser_command` is present but cannot be tokenized.
fn resolve_auth_method(
    authenticator: Option<&str>,
    browser_command: Option<String>,
    token: Option<String>,
    private_key_pem: Option<String>,
    passphrase: Option<String>,
) -> Result<AuthMethod> {
    let selector = authenticator.unwrap_or("").trim().to_ascii_lowercase();
    match selector.as_str() {
        "" | "externalbrowser" | "external_browser" => {
            let launch = match browser_command
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty())
            {
                Some(cmd) => BrowserLaunch::Command(split_browser_command(&cmd)?),
                None => BrowserLaunch::Auto,
            };
            Ok(AuthMethod::ExternalBrowser(BrowserConfig {
                launch,
                ..BrowserConfig::default()
            }))
        }
        "programmatic_access_token" | "pat" => {
            let token = token
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .ok_or_else(|| {
                    anyhow!("{ENV_AUTHENTICATOR}={selector} requires {ENV_TOKEN} to be set")
                })?;
            Ok(AuthMethod::ProgrammaticAccessToken {
                token: Secret::from(token),
            })
        }
        "snowflake_jwt" | "keypair" | "key_pair" | "jwt" => {
            if passphrase.is_some_and(|p| !p.trim().is_empty()) {
                bail!(
                    "{ENV_PRIVATE_KEY_PASSPHRASE} is set, but encrypted private keys are not yet \
                     supported; decrypt the key with `openssl pkcs8 -in key.p8 -out \
                     key_unencrypted.p8` and unset {ENV_PRIVATE_KEY_PASSPHRASE}"
                );
            }
            let private_key_pem = private_key_pem
                .filter(|k| !k.trim().is_empty())
                .ok_or_else(|| {
                    anyhow!(
                        "{ENV_AUTHENTICATOR}={selector} requires {ENV_PRIVATE_KEY_PATH} or \
                         {ENV_PRIVATE_KEY}"
                    )
                })?;
            Ok(AuthMethod::KeyPairJwt(KeyPairConfig {
                private_key_pem: Secret::from(private_key_pem),
            }))
        }
        other => Err(anyhow!(
            "unknown {ENV_AUTHENTICATOR} '{other}' \
             (expected externalbrowser, programmatic_access_token, or snowflake_jwt)"
        )),
    }
}

/// Tokenizes a `SNOWFLAKE_BROWSER_COMMAND` value into program + args with
/// POSIX-style quoting so a program path or an argument value may contain
/// spaces (`Google Chrome.app`, `--profile-directory="Profile 1"`):
///
/// - unquoted whitespace separates tokens;
/// - single quotes take their contents literally;
/// - double quotes group while a backslash escapes only `"` or `\` (so
///   Windows-style paths keep their other backslashes);
/// - an unquoted backslash escapes the next character.
///
/// The `{url}` placeholder is left intact here; the client's `open_browser`
/// substitutes it (or appends the URL) at launch time.
///
/// # Errors
///
/// If a quote is left unterminated, or the value tokenizes to zero words.
fn split_browser_command(raw: &str) -> Result<Vec<String>> {
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    // Distinguishes an empty quoted arg (`""`) from "no arg accumulated yet".
    let mut has_word = false;
    let mut chars = raw.chars();

    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if has_word {
                    words.push(std::mem::take(&mut current));
                    has_word = false;
                }
            }
            '\'' => {
                has_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => current.push(ch),
                        None => {
                            bail!("{ENV_BROWSER_COMMAND} has an unterminated single quote: {raw}")
                        }
                    }
                }
            }
            '"' => {
                has_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(ch @ ('"' | '\\')) => current.push(ch),
                            Some(ch) => {
                                current.push('\\');
                                current.push(ch);
                            }
                            None => bail!(
                                "{ENV_BROWSER_COMMAND} has an unterminated double quote: {raw}"
                            ),
                        },
                        Some(ch) => current.push(ch),
                        None => {
                            bail!("{ENV_BROWSER_COMMAND} has an unterminated double quote: {raw}")
                        }
                    }
                }
            }
            '\\' => {
                has_word = true;
                match chars.next() {
                    Some(ch) => current.push(ch),
                    None => current.push('\\'),
                }
            }
            ch => {
                has_word = true;
                current.push(ch);
            }
        }
    }
    if has_word {
        words.push(current);
    }
    if words.is_empty() {
        bail!("{ENV_BROWSER_COMMAND} is set but contains no command");
    }
    Ok(words)
}

/// A single arbitrary-SQL query request routed to the engine.
///
/// `account`/`user` and the per-query context default to the engine config when
/// omitted. Serialized by the CLI client (after [`fill_defaults_from`]
/// resolution) and deserialized from the daemon `query` op payload.
///
/// [`fill_defaults_from`]: QueryRequest::fill_defaults_from
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct QueryRequest {
    /// Target account; falls back to `SNOWFLAKE_ACCOUNT`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Authenticating user; falls back to `SNOWFLAKE_USER`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Per-query `USE WAREHOUSE` override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warehouse: Option<String>,
    /// Per-query `USE ROLE` override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Per-query `USE DATABASE` override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// Per-query `USE SCHEMA` override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// The SQL to execute.
    pub sql: String,
}

impl QueryRequest {
    /// Fills each unset identity/context field from `env`.
    ///
    /// Called by the CLI client with a profile-aware source
    /// ([`SettingsEnv`](crate::utils::settings::SettingsEnv)) so that
    /// `--profile` / `OMNI_DEV_PROFILE` resolve in the invoking process —
    /// the daemon's startup defaults then only back-fill requests that still
    /// omit a field (e.g. bare socket clients). Explicit values are never
    /// overwritten.
    pub fn fill_defaults_from(&mut self, env: &impl EnvSource) {
        let fill = |slot: &mut Option<String>, key: &str| {
            if slot.is_none() {
                *slot = env.var(key);
            }
        };
        fill(&mut self.account, ENV_ACCOUNT);
        fill(&mut self.user, ENV_USER);
        fill(&mut self.warehouse, ENV_WAREHOUSE);
        fill(&mut self.role, ENV_ROLE);
        fill(&mut self.database, ENV_DATABASE);
        fill(&mut self.schema, ENV_SCHEMA);
    }
    /// The per-query context overrides (the `Some` fields override the session
    /// base context).
    fn overrides(&self) -> QueryContext {
        QueryContext {
            warehouse: self.warehouse.clone(),
            role: self.role.clone(),
            database: self.database.clone(),
            schema: self.schema.clone(),
        }
    }
}

/// The running keep-alive heartbeat loop: cancelled and awaited on shutdown.
struct HeartbeatTask {
    token: CancellationToken,
    handle: JoinHandle<()>,
}

/// The account-agnostic Snowflake query engine: lazy multiplexed auth, bounded
/// per-identity session pools, and concurrent arbitrary-SQL execution.
///
/// A background keep-alive heartbeat for idle sessions is started by
/// [`start_heartbeat`](Self::start_heartbeat) and stopped by
/// [`shutdown`](Self::shutdown).
pub struct SnowflakeEngine {
    config: SnowflakeEngineConfig,
    registry: PoolRegistry,
    heartbeat: StdMutex<Option<HeartbeatTask>>,
}

impl SnowflakeEngine {
    /// Builds an engine. Cheap — no eager auth or I/O.
    #[must_use]
    pub fn new(config: SnowflakeEngineConfig) -> Self {
        Self {
            config,
            registry: PoolRegistry::new(),
            heartbeat: StdMutex::new(None),
        }
    }

    /// Starts the background keep-alive heartbeat loop (idempotent).
    ///
    /// Every `heartbeat_interval` the loop heartbeats each pool's idle sessions
    /// — renewing a session token about to lapse — so the server-side
    /// `CLIENT_SESSION_KEEP_ALIVE` actually extends the master token and an
    /// idle pool survives past the token TTL without a new browser SSO (#1107).
    /// Busy sessions are skipped; the query path keeps them alive inline.
    ///
    /// No-op when the interval is zero (disabled) or when called outside a
    /// tokio runtime. Stopped by [`shutdown`](Self::shutdown).
    pub fn start_heartbeat(&self) {
        let interval = self.config.heartbeat_interval;
        if interval.is_zero() {
            return;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            tracing::debug!("no tokio runtime; Snowflake keep-alive heartbeat not started");
            return;
        }
        let mut guard = self.lock_heartbeat();
        if guard.is_some() {
            return;
        }
        let token = CancellationToken::new();
        let loop_token = token.clone();
        let registry = self.registry.clone();
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = loop_token.cancelled() => break,
                    () = tokio::time::sleep(interval) => {
                        heartbeat_all_pools(&registry, interval).await;
                    }
                }
            }
        });
        *guard = Some(HeartbeatTask { token, handle });
    }

    fn lock_heartbeat(&self) -> MutexGuard<'_, Option<HeartbeatTask>> {
        self.heartbeat
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A snapshot of every active pool.
    #[must_use]
    pub fn sessions(&self) -> Vec<SessionInfo> {
        self.registry.snapshot()
    }

    /// The number of active pools (`(account, user)` identities).
    #[must_use]
    pub fn pool_count(&self) -> usize {
        self.registry.len()
    }

    /// Evicts the pool for `(account, user)`. Returns whether one existed.
    pub fn disconnect(&self, account: &str, user: &str) -> bool {
        let key = SessionKey::new(normalize_account(account), user.trim());
        self.registry.remove(&key).is_some()
    }

    /// Evicts the pool with the given id. Returns whether one existed.
    pub fn disconnect_by_id(&self, id: u64) -> bool {
        self.registry.remove_by_id(id).is_some()
    }

    /// Evicts every pool. Returns how many were removed.
    pub fn disconnect_all(&self) -> usize {
        self.registry.take_all().len()
    }

    /// Cancels the running query on the `(account, user)` pool — one specific
    /// member when `member` is `Some`, else every busy member. Returns how many
    /// statements an abort was issued for. Does **not** create a pool, so an
    /// unknown identity cancels nothing.
    ///
    /// The pooled session frees itself promptly (its poll loop returns a cancelled
    /// error within one poll interval) rather than waiting out the query timeout.
    pub async fn cancel(&self, account: &str, user: &str, member: Option<u64>) -> usize {
        let key = SessionKey::new(normalize_account(account), user.trim());
        match self.registry.get(&key) {
            Some(pool) => cancel_pool(&pool, member).await,
            None => 0,
        }
    }

    /// Like [`cancel`](Self::cancel) but selects the pool by its numeric id (as
    /// shown by `sessions`).
    pub async fn cancel_by_id(&self, id: u64, member: Option<u64>) -> usize {
        match self.registry.get_by_id(id) {
            Some(pool) => cancel_pool(&pool, member).await,
            None => 0,
        }
    }

    /// Cancels every running query across all pools. Returns how many statements
    /// an abort was issued for.
    pub async fn cancel_all(&self) -> usize {
        let mut cancelled = 0;
        for pool in self.registry.pools() {
            cancelled += cancel_pool(&pool, None).await;
        }
        cancelled
    }

    /// Stops the keep-alive heartbeat, then drops every pool (and its sessions).
    pub async fn shutdown(&self) {
        let task = self.lock_heartbeat().take();
        if let Some(task) = task {
            task.token.cancel();
            let _ = task.handle.await;
        }
        let pools = self.registry.take_all();
        drop(pools);
    }

    /// Runs arbitrary SQL against the `(account, user)` pool, authenticating a
    /// session on first use, and returns a self-describing `{ columns, rows }`
    /// payload. Concurrent calls run on separate pooled sessions (up to the pool
    /// size).
    ///
    /// # Errors
    ///
    /// Returns an error if no account/user can be resolved, a context flag is not
    /// a valid identifier, authentication fails, or the query fails. On a
    /// session-expiry error that session is discarded and the next query
    /// re-authenticates.
    pub async fn query(&self, req: QueryRequest) -> Result<Value> {
        let account = normalize_account(
            req.account
                .as_deref()
                .or(self.config.default_account.as_deref())
                .ok_or_else(|| {
                    anyhow!("no Snowflake account: pass --account or set SNOWFLAKE_ACCOUNT")
                })?,
        );
        let user = req
            .user
            .as_deref()
            .or(self.config.default_user.as_deref())
            .ok_or_else(|| anyhow!("no Snowflake user: pass --user or set SNOWFLAKE_USER"))?
            .trim()
            .to_string();
        validate_context(&req)?;

        let key = SessionKey::new(account, user);
        let overrides = req.overrides();
        let pool = self.registry.get_or_create(&key, self.config.pool_size);

        // Check out a session. The pool reuses an idle one when available
        // (re-checking after the auth gate so a session freed mid-auth is reused),
        // and only authenticates a new one — serialized to one browser at a time
        // by the pool's shared auth gate — when none is idle and it is under
        // capacity. The permit inside the checkout caps concurrency at pool_size.
        let cfg = self.config.clone();
        let create_key = key.clone();
        let auth_timeout = self.config.auth_timeout;
        let checkout = pool
            .checkout(move || async move {
                // Overall sign-in deadline so a hung auth releases the gate.
                match tokio::time::timeout(
                    auth_timeout,
                    create_session_with_base(&create_key, &cfg),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => Err(ClientError::Auth(format!(
                        "Snowflake sign-in timed out after {auth_timeout:?}"
                    ))),
                }
            })
            .await
            .map_err(|e| {
                anyhow::Error::new(e).context(format!(
                    "failed to authenticate Snowflake session for {} / {}",
                    key.account, key.user
                ))
            })?;

        // Proactively renew a session whose token is about to expire, before use.
        if checkout
            .session()
            .session_expiring_within(TimeDelta::seconds(120))
            && checkout.session().renew().await.is_err()
        {
            pool.discard(checkout);
            return Err(anyhow!(
                "Snowflake session expired and was discarded — re-run the query to re-authenticate"
            ));
        }

        // Record what this member is now running, so menus/status show it, and
        // capture an abort handle so a concurrent `cancel` can stop it while the
        // session is checked out (and thus unreachable through the pool slot).
        pool.start_query(
            checkout.id(),
            sql_preview(&req.sql, SQL_PREVIEW_MAX),
            Some(checkout.session().abort_handle()),
        );

        // Apply the requested context and run the query, transparently renewing
        // the token and retrying once if it expires mid-flight.
        let target = checkout.base().overlay(&overrides);
        match run_with_renew(checkout.session(), checkout.current(), &target, &req.sql).await {
            Ok(rows) => {
                pool.touch();
                pool.checkin(checkout, target);
                Ok(client::rows_to_payload(&rows))
            }
            Err(e) if e.is_session_expired() => {
                pool.discard(checkout);
                Err(anyhow!(
                    "Snowflake session expired and was discarded — re-run the query to re-authenticate"
                ))
            }
            Err(e) => {
                // Log the underlying cause server-side and surface it to the
                // client (the daemon reply uses the full anyhow chain).
                tracing::warn!("Snowflake query failed: {e}");
                // The session's context is uncertain after a failure; check in
                // with an empty context so the next reuse re-applies every dimension.
                pool.checkin(checkout, QueryContext::default());
                Err(anyhow::Error::new(e).context("Snowflake query failed"))
            }
        }
    }
}

impl Drop for SnowflakeEngine {
    fn drop(&mut self) {
        // Best-effort: an engine dropped without `shutdown()` must not leave the
        // heartbeat loop running forever. Cancellation is sync; the task itself
        // is detached and exits on its next select.
        let task = self.lock_heartbeat().take();
        if let Some(task) = task {
            task.token.cancel();
        }
    }
}

/// Aborts running statements on one pool (a specific `member`, else all busy
/// members), returning how many aborts were issued. Handles are snapshotted under
/// the pool's (sync) slot lock and each abort is awaited **after** the lock is
/// released. A failed abort is logged and skipped, not surfaced.
async fn cancel_pool(pool: &SessionPool, member: Option<u64>) -> usize {
    let handles = pool.abort_handles(member);
    let mut cancelled = 0;
    for handle in handles {
        match handle.abort().await {
            Ok(true) => cancelled += 1,
            Ok(false) => {}
            Err(e) => tracing::warn!(pool = pool.id(), "Snowflake query cancel failed: {e}"),
        }
    }
    cancelled
}

/// Sends one keep-alive round to every pool's currently-idle sessions,
/// discarding any session that is dead beyond renewal.
async fn heartbeat_all_pools(registry: &PoolRegistry, interval: Duration) {
    for pool in registry.pools() {
        let checkouts = pool.checkout_all_idle();
        if checkouts.is_empty() {
            continue;
        }
        let total = checkouts.len();
        let mut kept = 0usize;
        for checkout in checkouts {
            if keep_session_alive(checkout.session(), interval).await {
                pool.restore(checkout);
                kept += 1;
            } else {
                pool.discard(checkout);
            }
        }
        tracing::debug!(
            pool = pool.id(),
            kept,
            total,
            "Snowflake keep-alive heartbeat round"
        );
    }
}

/// Keeps one idle session alive: proactively renews a session token that would
/// lapse before the next tick (the heartbeat itself is authorized by the
/// session token), then heartbeats so the server extends the master token.
///
/// Returns whether the session is still usable: `false` only when the master
/// token has expired (a full re-auth is unavoidable), so the caller discards
/// it. Transient errors keep the session for the next tick.
async fn keep_session_alive(session: &SnowflakeSession, interval: Duration) -> bool {
    let margin_secs = i64::try_from(interval.as_secs())
        .unwrap_or(i64::MAX)
        .saturating_add(HEARTBEAT_RENEW_MARGIN_SECS);
    let margin = TimeDelta::try_seconds(margin_secs).unwrap_or(TimeDelta::MAX);
    if session.session_expiring_within(margin) {
        match session.renew().await {
            Ok(()) => {}
            Err(e) if e.is_session_expired() => {
                tracing::warn!("Snowflake keep-alive: master token expired; discarding session");
                return false;
            }
            Err(e) => {
                // Transient: keep the session and try again next tick.
                tracing::warn!("Snowflake keep-alive renew failed: {e}");
                return true;
            }
        }
    }
    match session.heartbeat().await {
        Ok(()) => true,
        Err(e) if e.is_session_expired() => match session.renew().await {
            Ok(()) => true,
            Err(renew_err) if renew_err.is_session_expired() => {
                tracing::warn!("Snowflake keep-alive: master token expired; discarding session");
                false
            }
            Err(renew_err) => {
                tracing::warn!("Snowflake keep-alive renew failed: {renew_err}");
                true
            }
        },
        Err(e) => {
            tracing::warn!("Snowflake keep-alive heartbeat failed: {e}");
            true
        }
    }
}

/// Authenticates a session (via the engine's configured auth method), enables
/// keep-alive, and captures its base (account/user default) context.
async fn create_session_with_base(
    key: &SessionKey,
    config: &SnowflakeEngineConfig,
) -> client::Result<(SnowflakeSession, QueryContext)> {
    let mut cfg = SnowflakeClientConfig::external_browser(&key.account, &key.user);
    cfg.auth = config.auth.clone();
    cfg.host = config.default_host.clone();
    cfg.warehouse = config.default_warehouse.clone();
    cfg.role = config.default_role.clone();
    cfg.database = config.default_database.clone();
    cfg.schema = config.default_schema.clone();
    cfg.http_timeout = config.http_timeout;
    cfg.query_timeout = config.query_timeout;

    let client = SnowflakeClient::new(cfg)?;
    let session = client.create_session().await?;
    session
        .query("ALTER SESSION SET CLIENT_SESSION_KEEP_ALIVE = true")
        .await?;
    let base = capture_base_context(&session).await?;
    Ok((session, base))
}

/// Reads the session's effective default context so per-query overrides can
/// later be reset back to it.
async fn capture_base_context(session: &SnowflakeSession) -> client::Result<QueryContext> {
    let rows = session
        .query("SELECT CURRENT_WAREHOUSE(), CURRENT_ROLE(), CURRENT_DATABASE(), CURRENT_SCHEMA()")
        .await?;
    let Some(row) = rows.first() else {
        return Ok(QueryContext::default());
    };
    Ok(QueryContext {
        warehouse: row.raw_at(0).map(str::to_string),
        role: row.raw_at(1).map(str::to_string),
        database: row.raw_at(2).map(str::to_string),
        schema: row.raw_at(3).map(str::to_string),
    })
}

/// Applies the context and runs the SQL, transparently renewing the session
/// token (via the master token) and retrying once if it expired mid-flight.
async fn run_with_renew(
    session: &SnowflakeSession,
    current: &QueryContext,
    target: &QueryContext,
    sql: &str,
) -> client::Result<Vec<Row>> {
    match apply_and_query(session, current, target, sql).await {
        Err(e) if e.is_session_expired() => {
            session.renew().await?;
            // Re-apply the full context on the renewed session, then retry.
            apply_and_query(session, &QueryContext::default(), target, sql).await
        }
        other => other,
    }
}

/// Issues any needed `USE` statements, then runs the SQL.
async fn apply_and_query(
    session: &SnowflakeSession,
    current: &QueryContext,
    target: &QueryContext,
    sql: &str,
) -> client::Result<Vec<Row>> {
    apply_context(session, current, target).await?;
    session.query(sql).await
}

/// Issues `USE` for each context dimension whose target differs from the
/// session's current value. Target names are either validated user overrides or
/// Snowflake-reported base names.
async fn apply_context(
    session: &SnowflakeSession,
    current: &QueryContext,
    target: &QueryContext,
) -> client::Result<()> {
    for (keyword, cur, tgt) in [
        (
            "WAREHOUSE",
            current.warehouse.as_deref(),
            target.warehouse.as_deref(),
        ),
        ("ROLE", current.role.as_deref(), target.role.as_deref()),
        (
            "DATABASE",
            current.database.as_deref(),
            target.database.as_deref(),
        ),
        (
            "SCHEMA",
            current.schema.as_deref(),
            target.schema.as_deref(),
        ),
    ] {
        if let Some(name) = tgt {
            if cur != Some(name) {
                session
                    .query(format!("USE {keyword} {name}").as_str())
                    .await?;
            }
        }
    }
    Ok(())
}

/// Normalizes an account identifier for keying (Snowflake is case-insensitive).
fn normalize_account(account: &str) -> String {
    account.trim().to_ascii_uppercase()
}

/// A single-line, length-bounded preview of SQL for the "running" display
/// (collapses whitespace/newlines; appends `…` when truncated).
fn sql_preview(sql: &str, max: usize) -> String {
    let collapsed = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > max {
        let head: String = collapsed.chars().take(max).collect();
        format!("{}…", head.trim_end())
    } else {
        collapsed
    }
}

/// Validates every present context flag as a safe Snowflake identifier before it
/// is interpolated into a `USE …` statement.
fn validate_context(req: &QueryRequest) -> Result<()> {
    for (name, value) in [
        ("warehouse", req.warehouse.as_deref()),
        ("role", req.role.as_deref()),
        ("database", req.database.as_deref()),
        ("schema", req.schema.as_deref()),
    ] {
        if let Some(value) = value {
            validate_identifier(name, value)?;
        }
    }
    Ok(())
}

/// Rejects context values that are not bare Snowflake identifiers (letters,
/// digits, `_`, `$`, `.`), so a `--warehouse` flag cannot smuggle extra SQL into
/// the `USE …` statement.
fn validate_identifier(field: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        bail!("--{field} must not be empty");
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '.'))
    {
        bail!(
            "--{field} '{value}' is not a valid Snowflake identifier \
             (allowed: letters, digits, '_', '$', '.')"
        );
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::test_support::env::MapEnv;

    #[test]
    fn default_config_has_a_nonzero_pool_size() {
        assert!(SnowflakeEngineConfig::default().pool_size >= 1);
    }

    #[test]
    fn heartbeat_interval_from_parses_seconds_zero_and_garbage() {
        assert_eq!(heartbeat_interval_from(None), DEFAULT_HEARTBEAT_INTERVAL);
        assert_eq!(
            heartbeat_interval_from(Some("300".to_string())),
            Duration::from_secs(300)
        );
        // `0` is meaningful: it disables the heartbeat.
        assert_eq!(
            heartbeat_interval_from(Some(" 0 ".to_string())),
            Duration::ZERO
        );
        assert_eq!(
            heartbeat_interval_from(Some("garbage".to_string())),
            DEFAULT_HEARTBEAT_INTERVAL
        );
    }

    #[test]
    fn host_override_from_trims_and_treats_blank_as_unset() {
        assert_eq!(host_override_from(None), None);
        assert_eq!(host_override_from(Some("   ".to_string())), None);
        assert_eq!(
            host_override_from(Some(
                "  acct.privatelink.snowflakecomputing.com\n".to_string()
            )),
            Some("acct.privatelink.snowflakecomputing.com".to_string())
        );
    }

    #[test]
    fn split_browser_command_splits_on_unquoted_whitespace() {
        assert_eq!(
            split_browser_command("chrome --new-window {url}").unwrap(),
            vec!["chrome", "--new-window", "{url}"]
        );
    }

    #[test]
    fn split_browser_command_keeps_quoted_spaces_together() {
        // The motivating case: a program path and an argument value both with
        // spaces, plus the `{url}` placeholder left intact for later substitution.
        assert_eq!(
            split_browser_command(
                "'/Applications/Google Chrome.app/Contents/MacOS/Google Chrome' \
                 --profile-directory=\"Profile 1\" --new-window {url}"
            )
            .unwrap(),
            vec![
                "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
                "--profile-directory=Profile 1",
                "--new-window",
                "{url}",
            ]
        );
    }

    #[test]
    fn split_browser_command_handles_backslash_escapes() {
        // An unquoted backslash escapes the next char; inside double quotes only
        // `\"` and `\\` are escapes, so other backslashes (Windows paths) survive.
        assert_eq!(
            split_browser_command(r#"chrome a\ b "c\"d" "e\\f" "g\h""#).unwrap(),
            vec!["chrome", "a b", "c\"d", "e\\f", "g\\h"]
        );
    }

    #[test]
    fn split_browser_command_rejects_unterminated_quotes() {
        assert!(split_browser_command("chrome \"--flag").is_err());
        assert!(split_browser_command("chrome '--flag").is_err());
    }

    #[test]
    fn split_browser_command_rejects_an_empty_command() {
        assert!(split_browser_command("   ").is_err());
        assert!(split_browser_command("").is_err());
    }

    #[test]
    fn resolve_auth_method_defaults_to_external_browser() {
        // Unset, blank, and the explicit name all keep interactive SSO.
        for selector in [
            None,
            Some(""),
            Some("  "),
            Some("externalbrowser"),
            Some("EXTERNALBROWSER"),
        ] {
            assert!(matches!(
                resolve_auth_method(selector, None, None, None, None).unwrap(),
                AuthMethod::ExternalBrowser(BrowserConfig {
                    launch: BrowserLaunch::Auto,
                    ..
                })
            ));
        }
    }

    #[test]
    fn resolve_auth_method_threads_browser_command() {
        let auth = resolve_auth_method(
            Some("externalbrowser"),
            Some(
                "\"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome\" \
                 --profile-directory=\"Profile 1\" --new-window {url}"
                    .to_string(),
            ),
            None,
            None,
            None,
        )
        .unwrap();
        let AuthMethod::ExternalBrowser(BrowserConfig {
            launch: BrowserLaunch::Command(args),
            ..
        }) = auth
        else {
            panic!("expected an external-browser Command launch");
        };
        assert_eq!(
            args,
            vec![
                "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".to_string(),
                "--profile-directory=Profile 1".to_string(),
                "--new-window".to_string(),
                "{url}".to_string(),
            ]
        );
    }

    #[test]
    fn resolve_auth_method_blank_browser_command_is_auto() {
        // A blank/whitespace command is treated as unset, not a parse error.
        assert!(matches!(
            resolve_auth_method(None, Some("   ".to_string()), None, None, None).unwrap(),
            AuthMethod::ExternalBrowser(BrowserConfig {
                launch: BrowserLaunch::Auto,
                ..
            })
        ));
    }

    #[test]
    fn resolve_auth_method_rejects_a_malformed_browser_command() {
        let err = resolve_auth_method(None, Some("chrome \"--flag".to_string()), None, None, None)
            .unwrap_err();
        assert!(err.to_string().contains(ENV_BROWSER_COMMAND));
    }

    #[test]
    fn resolve_auth_method_ignores_browser_command_for_non_interactive_methods() {
        // PAT and JWT open no browser, so a set command is harmlessly ignored.
        assert!(matches!(
            resolve_auth_method(
                Some("pat"),
                Some("chrome {url}".to_string()),
                Some("tok".to_string()),
                None,
                None,
            )
            .unwrap(),
            AuthMethod::ProgrammaticAccessToken { .. }
        ));
        assert!(matches!(
            resolve_auth_method(
                Some("snowflake_jwt"),
                Some("chrome {url}".to_string()),
                None,
                Some("pem".to_string()),
                None,
            )
            .unwrap(),
            AuthMethod::KeyPairJwt(_)
        ));
    }

    #[test]
    fn resolve_auth_method_reads_pat_and_trims_it() {
        let auth = resolve_auth_method(
            Some("programmatic_access_token"),
            None,
            Some("  tok-123\n".to_string()),
            None,
            None,
        )
        .unwrap();
        let AuthMethod::ProgrammaticAccessToken { token } = auth else {
            panic!("expected a PAT auth method");
        };
        assert_eq!(token.expose_secret(), "tok-123");
    }

    #[test]
    fn resolve_auth_method_accepts_the_pat_alias() {
        assert!(matches!(
            resolve_auth_method(Some("pat"), None, Some("t".to_string()), None, None).unwrap(),
            AuthMethod::ProgrammaticAccessToken { .. }
        ));
    }

    #[test]
    fn resolve_auth_method_errors_when_pat_is_missing_or_blank() {
        assert!(
            resolve_auth_method(Some("programmatic_access_token"), None, None, None, None).is_err()
        );
        assert!(
            resolve_auth_method(Some("pat"), None, Some("   ".to_string()), None, None).is_err()
        );
    }

    #[test]
    fn resolve_auth_method_reads_key_pair_pem() {
        let auth = resolve_auth_method(
            Some("snowflake_jwt"),
            None,
            None,
            Some("-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----".to_string()),
            None,
        )
        .unwrap();
        let AuthMethod::KeyPairJwt(cfg) = auth else {
            panic!("expected a key-pair auth method");
        };
        assert!(cfg
            .private_key_pem
            .expose_secret()
            .contains("BEGIN PRIVATE KEY"));
    }

    #[test]
    fn resolve_auth_method_accepts_key_pair_aliases() {
        for selector in ["snowflake_jwt", "keypair", "key_pair", "jwt"] {
            assert!(matches!(
                resolve_auth_method(Some(selector), None, None, Some("pem".to_string()), None)
                    .unwrap(),
                AuthMethod::KeyPairJwt(_)
            ));
        }
    }

    #[test]
    fn resolve_auth_method_errors_when_key_is_missing() {
        assert!(resolve_auth_method(Some("snowflake_jwt"), None, None, None, None).is_err());
        assert!(resolve_auth_method(
            Some("snowflake_jwt"),
            None,
            None,
            Some("  ".to_string()),
            None
        )
        .is_err());
    }

    #[test]
    fn resolve_auth_method_rejects_an_encrypted_key_passphrase() {
        let err = resolve_auth_method(
            Some("snowflake_jwt"),
            None,
            None,
            Some("pem".to_string()),
            Some("hunter2".to_string()),
        )
        .unwrap_err();
        assert!(err.to_string().contains(ENV_PRIVATE_KEY_PASSPHRASE));
    }

    #[test]
    fn resolve_auth_method_rejects_an_unknown_selector() {
        let err = resolve_auth_method(Some("carrier-pigeon"), None, None, None, None).unwrap_err();
        assert!(err.to_string().contains("carrier-pigeon"));
    }

    #[tokio::test]
    async fn start_heartbeat_is_a_noop_when_disabled() {
        let engine = SnowflakeEngine::new(SnowflakeEngineConfig {
            heartbeat_interval: Duration::ZERO,
            ..SnowflakeEngineConfig::default()
        });
        engine.start_heartbeat();
        assert!(engine.lock_heartbeat().is_none());
    }

    #[test]
    fn start_heartbeat_is_a_noop_outside_a_runtime() {
        let engine = SnowflakeEngine::new(SnowflakeEngineConfig::default());
        engine.start_heartbeat();
        assert!(engine.lock_heartbeat().is_none());
    }

    #[tokio::test]
    async fn start_heartbeat_is_idempotent_and_shutdown_stops_it() {
        let engine = SnowflakeEngine::new(SnowflakeEngineConfig::default());
        engine.start_heartbeat();
        // Cancelling the running task's token lets a replacement be detected: a
        // second start must keep this task, not spawn (and orphan) a fresh one.
        engine.lock_heartbeat().as_ref().unwrap().token.cancel();
        engine.start_heartbeat();
        assert!(
            engine
                .lock_heartbeat()
                .as_ref()
                .unwrap()
                .token
                .is_cancelled(),
            "second start must not replace the running task"
        );
        engine.shutdown().await;
        assert!(engine.lock_heartbeat().is_none());
    }

    #[tokio::test]
    async fn cancel_selectors_are_zero_on_an_empty_engine() {
        // No pools exist, so every selector cancels nothing — and crucially does
        // not create a pool (offline, no auth).
        let engine = SnowflakeEngine::new(SnowflakeEngineConfig::default());
        assert_eq!(engine.cancel("ACCT", "me", None).await, 0);
        assert_eq!(engine.cancel("ACCT", "me", Some(3)).await, 0);
        assert_eq!(engine.cancel_by_id(7, None).await, 0);
        assert_eq!(engine.cancel_all().await, 0);
        assert_eq!(engine.pool_count(), 0, "cancel must not create a pool");
    }

    #[tokio::test]
    async fn cancel_aborts_a_running_query_on_a_pooled_session() {
        use serde_json::json;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // The query goes async and parks in the poll loop, so its statement stays
        // published (and thus cancellable) for the whole test.
        Mock::given(method("POST"))
            .and(path("/queries/v1/query-request"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true, "code": "333333", "data": { "getResultUrl": "/poll/1" }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/poll/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true, "code": "333333", "data": {}
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/queries/v1/abort-request"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "success": true, "data": {} })),
            )
            .mount(&server)
            .await;

        // Inject a mock-backed session into a pool, bypassing the (live-only) SSO,
        // and record it as running with its abort handle — the state the engine's
        // `query` path would set.
        let engine = SnowflakeEngine::new(SnowflakeEngineConfig::default());
        let pool = engine
            .registry
            .get_or_create(&SessionKey::new("ACCT", "me"), 4);
        let pool_id = pool.id();
        let uri = server.uri();
        let checkout = pool
            .checkout(|| async {
                Ok::<_, std::convert::Infallible>((
                    client::test_session(&uri, Duration::from_secs(5)),
                    QueryContext::default(),
                ))
            })
            .await
            .unwrap();
        let member_id = checkout.id();
        pool.start_query(
            member_id,
            "SELECT LONG".to_string(),
            Some(checkout.session().abort_handle()),
        );

        // Run the (parked) query concurrently with the cancels. `select!` returns
        // as soon as the cancels complete, dropping the still-running query.
        tokio::select! {
            _ = checkout.session().query("SELECT LONG") => panic!("query should stay parked"),
            counts = async {
                // Let the query publish its in-flight statement before cancelling.
                tokio::time::sleep(Duration::from_millis(100)).await;
                let by_pair = engine.cancel("ACCT", "me", None).await;
                let by_id = engine.cancel_by_id(pool_id, Some(member_id)).await;
                let by_all = engine.cancel_all().await;
                (by_pair, by_id, by_all)
            } => {
                assert_eq!(counts, (1, 1, 1), "every selector aborts the running statement");
            }
        }

        // One abort was posted per cancel call.
        let aborts = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == "/queries/v1/abort-request")
            .count();
        assert_eq!(aborts, 3);
    }

    #[test]
    fn normalize_account_uppercases_and_trims() {
        assert_eq!(normalize_account("  my-org.acct  "), "MY-ORG.ACCT");
    }

    #[test]
    fn validate_identifier_accepts_bare_identifiers() {
        for value in ["WH", "my_wh", "DB.SCHEMA", "wh$1"] {
            assert!(validate_identifier("warehouse", value).is_ok(), "{value}");
        }
    }

    #[test]
    fn validate_identifier_rejects_injection_and_empty() {
        for value in ["", "wh; DROP TABLE t", "wh OR 1=1", "wh'", "a b"] {
            assert!(validate_identifier("warehouse", value).is_err(), "{value}");
        }
    }

    #[test]
    fn fill_defaults_from_fills_unset_fields() {
        let env = MapEnv::new()
            .with(ENV_ACCOUNT, "ACCT")
            .with(ENV_USER, "me")
            .with(ENV_WAREHOUSE, "WH")
            .with(ENV_ROLE, "R")
            .with(ENV_DATABASE, "DB")
            .with(ENV_SCHEMA, "S");
        let mut req = QueryRequest {
            sql: "SELECT 1".to_string(),
            ..QueryRequest::default()
        };
        req.fill_defaults_from(&env);
        assert_eq!(req.account.as_deref(), Some("ACCT"));
        assert_eq!(req.user.as_deref(), Some("me"));
        assert_eq!(req.warehouse.as_deref(), Some("WH"));
        assert_eq!(req.role.as_deref(), Some("R"));
        assert_eq!(req.database.as_deref(), Some("DB"));
        assert_eq!(req.schema.as_deref(), Some("S"));
    }

    #[test]
    fn fill_defaults_from_keeps_explicit_values() {
        let env = MapEnv::new()
            .with(ENV_ACCOUNT, "ENV_ACCT")
            .with(ENV_USER, "env_user");
        let mut req = QueryRequest {
            account: Some("FLAG_ACCT".to_string()),
            sql: "SELECT 1".to_string(),
            ..QueryRequest::default()
        };
        req.fill_defaults_from(&env);
        assert_eq!(req.account.as_deref(), Some("FLAG_ACCT"));
        assert_eq!(req.user.as_deref(), Some("env_user"));
    }

    #[test]
    fn fill_defaults_from_leaves_unresolved_fields_none() {
        let mut req = QueryRequest {
            sql: "SELECT 1".to_string(),
            ..QueryRequest::default()
        };
        req.fill_defaults_from(&MapEnv::new());
        assert!(req.account.is_none());
        assert!(req.user.is_none());
        assert!(req.warehouse.is_none());
    }

    #[test]
    fn query_request_serializes_without_none_fields() {
        let req = QueryRequest {
            account: Some("ACCT".to_string()),
            sql: "SELECT 1".to_string(),
            ..QueryRequest::default()
        };
        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(
            value,
            serde_json::json!({ "account": "ACCT", "sql": "SELECT 1" })
        );
    }

    #[test]
    fn validate_context_checks_each_present_flag() {
        let mut req = QueryRequest {
            sql: "SELECT 1".to_string(),
            ..QueryRequest::default()
        };
        assert!(validate_context(&req).is_ok());
        req.role = Some("good_role".to_string());
        assert!(validate_context(&req).is_ok());
        req.database = Some("bad; drop".to_string());
        assert!(validate_context(&req).is_err());
    }

    #[tokio::test]
    async fn query_without_account_errors_without_network() {
        let engine = SnowflakeEngine::new(SnowflakeEngineConfig::default());
        let err = engine
            .query(QueryRequest {
                sql: "SELECT 1".to_string(),
                ..QueryRequest::default()
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("account"));
    }

    #[tokio::test]
    async fn query_without_user_errors_without_network() {
        let engine = SnowflakeEngine::new(SnowflakeEngineConfig {
            default_account: Some("ACCT".to_string()),
            ..SnowflakeEngineConfig::default()
        });
        let err = engine
            .query(QueryRequest {
                sql: "SELECT 1".to_string(),
                ..QueryRequest::default()
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("user"));
    }

    #[test]
    fn disconnect_and_sessions_on_empty_engine() {
        let engine = SnowflakeEngine::new(SnowflakeEngineConfig::default());
        assert_eq!(engine.pool_count(), 0);
        assert!(engine.sessions().is_empty());
        assert!(!engine.disconnect("ACCT", "user"));
        assert!(!engine.disconnect_by_id(1));
        assert_eq!(engine.disconnect_all(), 0);
    }

    #[test]
    fn sql_preview_collapses_whitespace_and_truncates() {
        // Whitespace/newlines collapse to single spaces; short SQL is unchanged.
        assert_eq!(sql_preview("SELECT   1\n  FROM t", 60), "SELECT 1 FROM t");
        // Over-length SQL is truncated with an ellipsis.
        let long = format!("SELECT {}", "a".repeat(100));
        let preview = sql_preview(&long, 20);
        assert!(preview.ends_with('…'));
        assert!(preview.chars().count() <= 21, "{preview}");
    }

    #[test]
    fn overrides_extracts_only_the_set_dimensions() {
        let req = QueryRequest {
            warehouse: Some("WH".to_string()),
            schema: Some("S".to_string()),
            sql: "SELECT 1".to_string(),
            ..QueryRequest::default()
        };
        let overrides = req.overrides();
        assert_eq!(overrides.warehouse.as_deref(), Some("WH"));
        assert_eq!(overrides.schema.as_deref(), Some("S"));
        assert!(overrides.role.is_none());
        assert!(overrides.database.is_none());
    }

    mod orchestration {
        use super::*;
        use serde_json::json;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        /// Mounts a `query-request` handler that returns `data` for every POST.
        async fn mount_query(server: &MockServer, data: serde_json::Value) {
            Mock::given(method("POST"))
                .and(path("/queries/v1/query-request"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({ "success": true, "data": data })),
                )
                .mount(server)
                .await;
        }

        #[tokio::test]
        async fn capture_base_context_reads_current_context() {
            let server = MockServer::start().await;
            mount_query(
                &server,
                json!({
                    "rowtype": [
                        { "name": "CURRENT_WAREHOUSE()", "type": "text" },
                        { "name": "CURRENT_ROLE()", "type": "text" },
                        { "name": "CURRENT_DATABASE()", "type": "text" },
                        { "name": "CURRENT_SCHEMA()", "type": "text" },
                    ],
                    "rowset": [["WH", "R", "DB", "S"]],
                }),
            )
            .await;
            let session = client::test_session(&server.uri(), Duration::from_secs(5));
            let base = capture_base_context(&session).await.unwrap();
            assert_eq!(base.warehouse.as_deref(), Some("WH"));
            assert_eq!(base.role.as_deref(), Some("R"));
            assert_eq!(base.database.as_deref(), Some("DB"));
            assert_eq!(base.schema.as_deref(), Some("S"));
        }

        #[tokio::test]
        async fn capture_base_context_defaults_when_no_rows() {
            let server = MockServer::start().await;
            mount_query(
                &server,
                json!({ "rowtype": [{ "name": "X", "type": "text" }], "rowset": [] }),
            )
            .await;
            let session = client::test_session(&server.uri(), Duration::from_secs(5));
            assert_eq!(
                capture_base_context(&session).await.unwrap(),
                QueryContext::default()
            );
        }

        #[tokio::test]
        async fn apply_context_issues_use_only_for_differing_dimensions() {
            let server = MockServer::start().await;
            mount_query(&server, json!({ "rowtype": [], "rowset": [] })).await;
            let session = client::test_session(&server.uri(), Duration::from_secs(5));

            let current = QueryContext {
                warehouse: Some("WH".to_string()),
                ..QueryContext::default()
            };
            let target = QueryContext {
                warehouse: Some("WH".to_string()), // same → no USE
                role: Some("R2".to_string()),      // differs → one USE
                ..QueryContext::default()
            };
            apply_context(&session, &current, &target).await.unwrap();

            let reqs = server.received_requests().await.unwrap();
            assert_eq!(reqs.len(), 1, "only the differing dimension issues a USE");
            assert!(String::from_utf8_lossy(&reqs[0].body).contains("USE ROLE R2"));
        }

        #[tokio::test]
        async fn run_with_renew_runs_without_renew_when_not_expired() {
            let server = MockServer::start().await;
            mount_query(
                &server,
                json!({ "rowtype": [{ "name": "N", "type": "text" }], "rowset": [["x"]] }),
            )
            .await;
            let session = client::test_session(&server.uri(), Duration::from_secs(5));
            let ctx = QueryContext::default();
            let rows = run_with_renew(&session, &ctx, &ctx, "SELECT 1")
                .await
                .unwrap();
            assert_eq!(rows.len(), 1);
        }

        #[tokio::test]
        async fn run_with_renew_renews_and_retries_once_on_expiry() {
            let server = MockServer::start().await;
            // First query attempt: session expired (then this rule is exhausted).
            Mock::given(method("POST"))
                .and(path("/queries/v1/query-request"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "success": false, "code": "390112", "message": "expired", "data": {}
                })))
                .up_to_n_times(1)
                .with_priority(1)
                .mount(&server)
                .await;
            // Renew succeeds.
            Mock::given(method("POST"))
                .and(path("/session/token-request"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "success": true,
                    "data": { "sessionToken": "fresh", "validityInSecondsST": 3600 }
                })))
                .mount(&server)
                .await;
            // The retried query succeeds.
            Mock::given(method("POST"))
                .and(path("/queries/v1/query-request"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "success": true,
                    "data": { "rowtype": [{ "name": "N", "type": "text" }], "rowset": [["x"]] }
                })))
                .with_priority(2)
                .mount(&server)
                .await;

            let session = client::test_session(&server.uri(), Duration::from_secs(5));
            let ctx = QueryContext::default();
            let rows = run_with_renew(&session, &ctx, &ctx, "SELECT 1")
                .await
                .unwrap();
            assert_eq!(rows.len(), 1, "renewed and retried transparently");
        }
    }

    mod keep_alive {
        use super::*;
        use serde_json::json;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        /// A short interval so `session_expiring_within(interval + margin)` is
        /// false for the test session's fresh 3600s token.
        const INTERVAL: Duration = Duration::from_secs(60);

        /// Mounts a `session/heartbeat` handler answering with `body`.
        async fn mount_heartbeat(server: &MockServer, body: serde_json::Value) {
            Mock::given(method("POST"))
                .and(path("/session/heartbeat"))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(server)
                .await;
        }

        /// Mounts a `session/token-request` (renew) handler answering with `body`.
        async fn mount_renew(server: &MockServer, body: serde_json::Value) {
            Mock::given(method("POST"))
                .and(path("/session/token-request"))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(server)
                .await;
        }

        fn ok_body() -> serde_json::Value {
            json!({ "success": true, "data": {} })
        }

        fn renew_ok_body() -> serde_json::Value {
            json!({
                "success": true,
                "data": { "sessionToken": "fresh", "validityInSecondsST": 3600 }
            })
        }

        fn expired_body() -> serde_json::Value {
            json!({ "success": false, "code": "390112", "message": "expired", "data": {} })
        }

        #[tokio::test]
        async fn keep_session_alive_heartbeats_a_healthy_session() {
            let server = MockServer::start().await;
            mount_heartbeat(&server, ok_body()).await;
            let session = client::test_session(&server.uri(), Duration::from_secs(5));
            assert!(keep_session_alive(&session, INTERVAL).await);
            let reqs = server.received_requests().await.unwrap();
            assert_eq!(reqs.len(), 1, "one heartbeat, no renew");
        }

        #[tokio::test]
        async fn keep_session_alive_renews_when_the_heartbeat_reports_expiry() {
            let server = MockServer::start().await;
            mount_heartbeat(&server, expired_body()).await;
            mount_renew(&server, renew_ok_body()).await;
            let session = client::test_session(&server.uri(), Duration::from_secs(5));
            assert!(keep_session_alive(&session, INTERVAL).await);
            let reqs = server.received_requests().await.unwrap();
            assert!(
                reqs.iter()
                    .any(|r| r.url.path() == "/session/token-request"),
                "renewed after the expired heartbeat"
            );
        }

        #[tokio::test]
        async fn keep_session_alive_discards_when_the_master_token_is_dead() {
            let server = MockServer::start().await;
            mount_heartbeat(&server, expired_body()).await;
            mount_renew(&server, expired_body()).await;
            let session = client::test_session(&server.uri(), Duration::from_secs(5));
            assert!(!keep_session_alive(&session, INTERVAL).await);
        }

        #[tokio::test]
        async fn keep_session_alive_keeps_the_session_on_transient_errors() {
            let server = MockServer::start().await;
            mount_heartbeat(
                &server,
                json!({ "success": false, "code": "390001", "message": "hiccup", "data": {} }),
            )
            .await;
            let session = client::test_session(&server.uri(), Duration::from_secs(5));
            assert!(keep_session_alive(&session, INTERVAL).await);
        }

        #[tokio::test]
        async fn keep_session_alive_proactively_renews_a_token_expiring_before_the_next_tick() {
            let server = MockServer::start().await;
            mount_heartbeat(&server, ok_body()).await;
            mount_renew(&server, renew_ok_body()).await;
            let session = client::test_session(&server.uri(), Duration::from_secs(5));
            // interval + margin exceeds the fresh 3600s validity → renew first.
            assert!(keep_session_alive(&session, Duration::from_secs(7200)).await);
            let reqs = server.received_requests().await.unwrap();
            assert_eq!(
                reqs[0].url.path(),
                "/session/token-request",
                "renew ran first"
            );
            assert_eq!(reqs[1].url.path(), "/session/heartbeat");
        }

        #[tokio::test]
        async fn engine_heartbeat_loop_beats_idle_sessions_and_stops_on_shutdown() {
            let server = MockServer::start().await;
            mount_heartbeat(&server, ok_body()).await;

            let engine = SnowflakeEngine::new(SnowflakeEngineConfig {
                heartbeat_interval: Duration::from_millis(50),
                ..SnowflakeEngineConfig::default()
            });
            // Park one idle session in a pool, bypassing the (live-only) SSO.
            let pool = engine
                .registry
                .get_or_create(&SessionKey::new("ACCT", "user"), 2);
            let uri = server.uri();
            let checkout = pool
                .checkout(|| async {
                    Ok::<_, std::convert::Infallible>((
                        client::test_session(&uri, Duration::from_secs(5)),
                        QueryContext::default(),
                    ))
                })
                .await
                .unwrap();
            pool.checkin(checkout, QueryContext::default());

            engine.start_heartbeat();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while server.received_requests().await.unwrap().is_empty() {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "no heartbeat within the deadline"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            // The borrowed session was restored, not consumed.
            assert_eq!(pool.live(), 1);

            engine.shutdown().await;
            let after = server.received_requests().await.unwrap().len();
            tokio::time::sleep(Duration::from_millis(150)).await;
            assert_eq!(
                server.received_requests().await.unwrap().len(),
                after,
                "no heartbeats after shutdown"
            );
            assert_eq!(engine.pool_count(), 0, "pools drained on shutdown");
        }
    }
}
