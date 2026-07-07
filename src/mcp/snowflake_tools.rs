//! MCP tool handlers for Snowflake SQL access.
//!
//! A thin transport over the daemon's `snowflake` service: each tool sends one
//! `query`/`sessions`/`disconnect` op to the daemon control socket — the same
//! path the `omni-dev snowflake` CLI uses (`src/cli/snowflake.rs`) — and returns
//! the reply as YAML. The daemon holds the authenticated, multiplexed sessions;
//! these tools never authenticate or hold a session themselves, so they require
//! a running daemon.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock as Content},
    schemars, tool, tool_router, ErrorData as McpError,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::error::tool_error;
use super::git_tools::build_truncated_result;
use super::server::OmniDevServer;
use crate::daemon::client::DaemonClient;
use crate::daemon::protocol::DaemonEnvelope;
use crate::daemon::server::resolve_socket;
use crate::snowflake::QueryRequest;
use crate::utils::settings::SettingsEnv;

/// The `snowflake` service routing key on the daemon control socket.
const SERVICE: &str = "snowflake";

/// Parameters for the `snowflake_query` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SnowflakeQueryParams {
    /// SQL to execute. Required.
    pub sql: String,
    /// Target account. Falls back to `SNOWFLAKE_ACCOUNT` / settings.json.
    #[serde(default)]
    pub account: Option<String>,
    /// Authenticating user. Falls back to `SNOWFLAKE_USER` / settings.json.
    #[serde(default)]
    pub user: Option<String>,
    /// Per-query warehouse (`USE WAREHOUSE`).
    #[serde(default)]
    pub warehouse: Option<String>,
    /// Per-query role (`USE ROLE`).
    #[serde(default)]
    pub role: Option<String>,
    /// Per-query database (`USE DATABASE`).
    #[serde(default)]
    pub database: Option<String>,
    /// Per-query schema (`USE SCHEMA`).
    #[serde(default)]
    pub schema: Option<String>,
    /// Control-socket path override. Defaults to the per-user runtime location.
    #[serde(default)]
    pub socket: Option<String>,
}

/// Parameters for the `snowflake_sessions` tool.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct SnowflakeSessionsParams {
    /// Control-socket path override. Defaults to the per-user runtime location.
    #[serde(default)]
    pub socket: Option<String>,
}

/// Parameters for the `snowflake_disconnect` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SnowflakeDisconnectParams {
    /// Account of the session to evict.
    pub account: String,
    /// User of the session to evict.
    pub user: String,
    /// Control-socket path override. Defaults to the per-user runtime location.
    #[serde(default)]
    pub socket: Option<String>,
}

#[allow(missing_docs)] // #[tool_router] generates a pub `snowflake_tool_router` fn.
#[tool_router(router = snowflake_tool_router, vis = "pub")]
impl OmniDevServer {
    /// Tool: run SQL through a multiplexed Snowflake session.
    #[tool(
        description = "Run SQL against Snowflake and return the result set as YAML. Mirrors \
                       `omni-dev snowflake query`. The query runs through the omni-dev daemon, \
                       which multiplexes authenticated (account, user) sessions — so the daemon \
                       must be running (`omni-dev daemon start`). First-time use of an \
                       (account, user) authenticates via external-browser SSO on the daemon host \
                       (a browser may open there). `account`/`user` default to \
                       `SNOWFLAKE_ACCOUNT`/`SNOWFLAKE_USER` (and settings.json) when omitted; \
                       `warehouse`/`role`/`database`/`schema` are optional per-query context."
    )]
    pub async fn snowflake_query(
        &self,
        Parameters(params): Parameters<SnowflakeQueryParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = run_snowflake_query(params).await.map_err(tool_error)?;
        Ok(build_truncated_result(yaml))
    }

    /// Tool: list the daemon's active multiplexed Snowflake sessions.
    #[tool(
        description = "List the omni-dev daemon's active multiplexed Snowflake sessions as YAML \
                       (per-pool account/user, live/max sessions, query counts). Read-only. \
                       Mirrors `omni-dev snowflake sessions`. Requires a running daemon."
    )]
    pub async fn snowflake_sessions(
        &self,
        Parameters(params): Parameters<SnowflakeSessionsParams>,
    ) -> Result<CallToolResult, McpError> {
        let socket = resolve_socket(params.socket.map(PathBuf::from)).map_err(tool_error)?;
        let result = call(&socket, "sessions", Value::Null)
            .await
            .map_err(tool_error)?;
        let yaml = to_yaml(&result).map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: evict a single multiplexed Snowflake session.
    #[tool(
        description = "Disconnect (evict) one multiplexed Snowflake session identified by its \
                       `account` and `user`. Mirrors `omni-dev snowflake disconnect`. Returns \
                       whether a session was actually evicted. Requires a running daemon."
    )]
    pub async fn snowflake_disconnect(
        &self,
        Parameters(params): Parameters<SnowflakeDisconnectParams>,
    ) -> Result<CallToolResult, McpError> {
        let socket = resolve_socket(params.socket.map(PathBuf::from)).map_err(tool_error)?;
        let payload = json!({ "account": params.account, "user": params.user });
        let result = call(&socket, "disconnect", payload)
            .await
            .map_err(tool_error)?;
        let yaml = to_yaml(&result).map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }
}

/// Builds the [`QueryRequest`] from tool params (filling `SNOWFLAKE_*` defaults
/// like the CLI), sends the `query` op, and returns the reply as YAML.
async fn run_snowflake_query(params: SnowflakeQueryParams) -> Result<String> {
    if params.sql.trim().is_empty() {
        bail!("no SQL provided");
    }
    let mut req = QueryRequest {
        account: params.account,
        user: params.user,
        warehouse: params.warehouse,
        role: params.role,
        database: params.database,
        schema: params.schema,
        sql: params.sql,
    };
    req.fill_defaults_from(&SettingsEnv::load());

    let payload = serde_json::to_value(&req).context("Failed to serialize query request")?;
    let socket = resolve_socket(params.socket.map(PathBuf::from))?;
    let result = call(&socket, "query", payload).await?;
    to_yaml(&result)
}

/// Sends one `snowflake` service op over the control socket, returning its
/// payload or turning an `ok: false` reply into an error. Mirrors the private
/// helper of the same shape in `src/cli/snowflake.rs`.
async fn call(socket: &Path, op: &str, payload: Value) -> Result<Value> {
    let reply = DaemonClient::new(socket)
        .request(DaemonEnvelope::service(SERVICE, op, payload))
        .await
        .context(
            "Failed to reach the omni-dev daemon — is it running? Start it with \
             `omni-dev daemon start`.",
        )?;
    if reply.ok {
        Ok(reply.payload)
    } else {
        bail!(
            "daemon returned an error: {}",
            reply.error.as_deref().unwrap_or("unknown error")
        )
    }
}

/// Serializes a daemon reply payload as YAML (the MCP output convention).
fn to_yaml(value: &Value) -> Result<String> {
    serde_yaml::to_string(value).context("Failed to serialize Snowflake reply as YAML")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn query_params_require_sql() {
        assert!(serde_json::from_str::<SnowflakeQueryParams>("{}").is_err());
        let ok: SnowflakeQueryParams = serde_json::from_str(r#"{"sql": "select 1"}"#).unwrap();
        assert_eq!(ok.sql, "select 1");
        assert!(ok.account.is_none());
    }

    #[test]
    fn sessions_params_default_ok() {
        let p: SnowflakeSessionsParams = serde_json::from_str("{}").unwrap();
        assert!(p.socket.is_none());
    }

    #[test]
    fn disconnect_params_require_account_and_user() {
        assert!(serde_json::from_str::<SnowflakeDisconnectParams>("{}").is_err());
        let p: SnowflakeDisconnectParams =
            serde_json::from_str(r#"{"account": "a", "user": "u"}"#).unwrap();
        assert_eq!(p.account, "a");
        assert_eq!(p.user, "u");
    }

    #[test]
    fn to_yaml_serializes_reply() {
        let value = json!({"columns": ["n"], "rows": [[1]]});
        let yaml = to_yaml(&value).unwrap();
        assert!(yaml.contains("columns"));
        assert!(yaml.contains("rows"));
    }

    // A bogus socket path fails to connect immediately (ENOENT), exercising the
    // handler + `run_*`/`call` wiring without a running daemon.
    const NO_DAEMON_SOCKET: &str = "/no/such/omni-dev-daemon.sock";

    #[tokio::test]
    async fn snowflake_query_handler_unreachable_daemon_returns_tool_error() {
        use crate::mcp::server::OmniDevServer;
        use rmcp::handler::server::wrapper::Parameters;

        let server = OmniDevServer::new();
        let params = SnowflakeQueryParams {
            sql: "select 1".to_string(),
            account: Some("acct".to_string()),
            user: Some("user".to_string()),
            warehouse: None,
            role: None,
            database: None,
            schema: None,
            socket: Some(NO_DAEMON_SOCKET.to_string()),
        };
        let err = server
            .snowflake_query(Parameters(params))
            .await
            .unwrap_err();
        assert!(!err.message.is_empty());
    }

    #[tokio::test]
    async fn snowflake_query_handler_rejects_empty_sql() {
        use crate::mcp::server::OmniDevServer;
        use rmcp::handler::server::wrapper::Parameters;

        let server = OmniDevServer::new();
        let params = SnowflakeQueryParams {
            sql: "   ".to_string(),
            account: None,
            user: None,
            warehouse: None,
            role: None,
            database: None,
            schema: None,
            socket: Some(NO_DAEMON_SOCKET.to_string()),
        };
        let err = server
            .snowflake_query(Parameters(params))
            .await
            .unwrap_err();
        assert!(err.message.contains("no SQL"));
    }

    #[tokio::test]
    async fn snowflake_sessions_handler_unreachable_daemon_returns_tool_error() {
        use crate::mcp::server::OmniDevServer;
        use rmcp::handler::server::wrapper::Parameters;

        let server = OmniDevServer::new();
        let params = SnowflakeSessionsParams {
            socket: Some(NO_DAEMON_SOCKET.to_string()),
        };
        let err = server
            .snowflake_sessions(Parameters(params))
            .await
            .unwrap_err();
        assert!(!err.message.is_empty());
    }

    #[tokio::test]
    async fn snowflake_disconnect_handler_unreachable_daemon_returns_tool_error() {
        use crate::mcp::server::OmniDevServer;
        use rmcp::handler::server::wrapper::Parameters;

        let server = OmniDevServer::new();
        let params = SnowflakeDisconnectParams {
            account: "acct".to_string(),
            user: "user".to_string(),
            socket: Some(NO_DAEMON_SOCKET.to_string()),
        };
        let err = server
            .snowflake_disconnect(Parameters(params))
            .await
            .unwrap_err();
        assert!(!err.message.is_empty());
    }
}
