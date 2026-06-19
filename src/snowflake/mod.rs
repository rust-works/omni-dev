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
//! This is the standalone engine, analogous to [`crate::browser`]; the daemon
//! adapter lives in [`crate::daemon::services::snowflake`].

pub mod client;
pub mod session;

use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use chrono::TimeDelta;
use serde::Deserialize;
use serde_json::Value;

use crate::utils::settings::Settings;
use client::{Error as ClientError, Row, SnowflakeClient, SnowflakeClientConfig, SnowflakeSession};
use session::{PoolRegistry, QueryContext, SessionInfo, SessionKey};

/// Env var (with `~/.omni-dev/settings.json` fallback) for the default account.
const ENV_ACCOUNT: &str = "SNOWFLAKE_ACCOUNT";
/// Env var for the default user.
const ENV_USER: &str = "SNOWFLAKE_USER";
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

/// Default pool size when `SNOWFLAKE_POOL_SIZE` is unset: the max concurrent
/// queries (and max browser auths) per `(account, user)`.
const DEFAULT_POOL_SIZE: usize = 4;
/// Default overall sign-in deadline: comfortably over the SSO callback wait so a
/// genuine sign-in completes, but bounded so a hung auth can't hold the gate.
const DEFAULT_AUTH_TIMEOUT: Duration = Duration::from_secs(150);
/// Max characters of SQL shown in the "running" preview for a busy session.
const SQL_PREVIEW_MAX: usize = 60;

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
    /// Default warehouse applied at session creation.
    pub default_warehouse: Option<String>,
    /// Default role applied at session creation.
    pub default_role: Option<String>,
    /// Default database applied at session creation.
    pub default_database: Option<String>,
    /// Default schema applied at session creation.
    pub default_schema: Option<String>,
    /// Max concurrent sessions (and browser auths) per `(account, user)`.
    pub pool_size: usize,
    /// Per-request HTTP timeout for REST calls.
    pub http_timeout: Duration,
    /// Overall deadline for one sign-in (SSO + login) so a hung auth can't hold
    /// the shared auth gate indefinitely.
    pub auth_timeout: Duration,
    /// Overall deadline for one query (submit + async-result polling).
    pub query_timeout: Duration,
}

impl Default for SnowflakeEngineConfig {
    fn default() -> Self {
        Self {
            default_account: None,
            default_user: None,
            default_warehouse: None,
            default_role: None,
            default_database: None,
            default_schema: None,
            pool_size: DEFAULT_POOL_SIZE,
            http_timeout: client::config::DEFAULT_HTTP_TIMEOUT,
            auth_timeout: DEFAULT_AUTH_TIMEOUT,
            query_timeout: client::config::DEFAULT_QUERY_TIMEOUT,
        }
    }
}

impl SnowflakeEngineConfig {
    /// Resolves defaults from env vars (then settings.json). Cheap and
    /// side-effect-free; never authenticates.
    ///
    /// # Errors
    ///
    /// Currently infallible, but returns `Result` so the daemon registry wiring
    /// can `?` it and future validation can surface errors.
    pub fn from_env_and_settings() -> Result<Self> {
        let settings = Settings::load().unwrap_or_else(|_| Settings {
            env: std::collections::HashMap::new(),
        });
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
        Ok(Self {
            default_account: settings.get_env_var(ENV_ACCOUNT),
            default_user: settings.get_env_var(ENV_USER),
            default_warehouse: settings.get_env_var(ENV_WAREHOUSE),
            default_role: settings.get_env_var(ENV_ROLE),
            default_database: settings.get_env_var(ENV_DATABASE),
            default_schema: settings.get_env_var(ENV_SCHEMA),
            pool_size,
            http_timeout: secs(ENV_HTTP_TIMEOUT).unwrap_or(client::config::DEFAULT_HTTP_TIMEOUT),
            auth_timeout: secs(ENV_AUTH_TIMEOUT).unwrap_or(DEFAULT_AUTH_TIMEOUT),
            query_timeout: secs(ENV_QUERY_TIMEOUT).unwrap_or(client::config::DEFAULT_QUERY_TIMEOUT),
        })
    }
}

/// A single arbitrary-SQL query request routed to the engine.
///
/// `account`/`user` and the per-query context default to the engine config when
/// omitted. Deserialized from the daemon `query` op payload.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct QueryRequest {
    /// Target account; falls back to `SNOWFLAKE_ACCOUNT`.
    pub account: Option<String>,
    /// Authenticating user; falls back to `SNOWFLAKE_USER`.
    pub user: Option<String>,
    /// Per-query `USE WAREHOUSE` override.
    pub warehouse: Option<String>,
    /// Per-query `USE ROLE` override.
    pub role: Option<String>,
    /// Per-query `USE DATABASE` override.
    pub database: Option<String>,
    /// Per-query `USE SCHEMA` override.
    pub schema: Option<String>,
    /// The SQL to execute.
    pub sql: String,
}

impl QueryRequest {
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

/// The account-agnostic Snowflake query engine: lazy multiplexed auth, bounded
/// per-identity session pools, and concurrent arbitrary-SQL execution.
pub struct SnowflakeEngine {
    config: SnowflakeEngineConfig,
    registry: PoolRegistry,
}

impl SnowflakeEngine {
    /// Builds an engine. Cheap — no eager auth or I/O.
    #[must_use]
    pub fn new(config: SnowflakeEngineConfig) -> Self {
        Self {
            config,
            registry: PoolRegistry::new(),
        }
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

    /// Drops every pool (and its sessions).
    pub async fn shutdown(&self) {
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

        // Record what this member is now running, so menus/status show it.
        pool.start_query(checkout.id(), sql_preview(&req.sql, SQL_PREVIEW_MAX));

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

/// Authenticates a session (external-browser SSO), enables keep-alive, and
/// captures its base (account/user default) context.
async fn create_session_with_base(
    key: &SessionKey,
    config: &SnowflakeEngineConfig,
) -> client::Result<(SnowflakeSession, QueryContext)> {
    let mut cfg = SnowflakeClientConfig::external_browser(&key.account, &key.user);
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

    #[test]
    fn default_config_has_a_nonzero_pool_size() {
        assert!(SnowflakeEngineConfig::default().pool_size >= 1);
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
}
