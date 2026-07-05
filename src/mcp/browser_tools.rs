//! MCP tool handler for driving a request through the browser bridge.
//!
//! `browser_bridge_request` sends one HTTP request through a running,
//! authenticated browser tab — the same path as `omni-dev browser bridge
//! request` (`src/cli/browser/request.rs`) — via the shared
//! [`BridgeClient::send`], and returns the buffered response envelope as YAML.
//! Streaming (`--stream`) has no MCP form (it writes chunks to stdout as they
//! arrive) and is forced off; `serve` and `harvest` are not MCP-appropriate.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use rmcp::{
    handler::server::wrapper::Parameters, model::CallToolResult, schemars, tool, tool_router,
    ErrorData as McpError,
};
use serde::Deserialize;

use super::error::tool_error;
use super::git_tools::build_truncated_result;
use super::server::OmniDevServer;
use crate::browser::auth;
use crate::browser::bridge::DEFAULT_CONTROL_PORT;
use crate::browser::client::BridgeClient;
use crate::browser::protocol::ControlRequest;

/// Fetch credentials mode forwarded to the browser `fetch()` (MCP mirror of the
/// CLI's `Credentials`).
#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum BridgeCredentials {
    /// Send cookies/auth on every request (default; correct for same-origin APIs).
    Include,
    /// Send no credentials; required for wildcard-CORS cross-origin assets.
    Omit,
    /// Send credentials only on same-origin requests (browser `fetch` default).
    SameOrigin,
}

impl BridgeCredentials {
    /// The wire value the browser snippet passes to `fetch()`'s `credentials`.
    fn to_fetch_value(self) -> String {
        match self {
            Self::Include => "include",
            Self::Omit => "omit",
            Self::SameOrigin => "same-origin",
        }
        .to_string()
    }
}

/// Parameters for the `browser_bridge_request` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BrowserBridgeRequestParams {
    /// Request URL, relative to the browser's page origin (e.g. `/api/foo`).
    pub url: String,
    /// HTTP method. Defaults to `GET`.
    #[serde(default = "default_method")]
    pub method: String,
    /// Request headers as a `{ "Name": "Value" }` map. Validated for safety.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Request body, sent verbatim (no `@file` expansion — pass content inline).
    #[serde(default)]
    pub body: Option<String>,
    /// Fetch credentials mode. Defaults to `include` (cookies/auth sent). Use
    /// `omit` to read a wildcard-CORS cross-origin response.
    #[serde(default)]
    pub credentials: Option<BridgeCredentials>,
    /// Control-plane port of the running bridge. Defaults to the standard port.
    #[serde(default)]
    pub control_port: Option<u16>,
    /// Read the session token from this `0600` file instead of the
    /// `OMNI_BRIDGE_TOKEN` environment variable.
    #[serde(default)]
    pub token_file: Option<String>,
    /// Route to a specific connected tab: a connection id (from
    /// `/__bridge/status`) or an `Origin` that uniquely matches one tab.
    /// Required when more than one tab is connected.
    #[serde(default)]
    pub target: Option<String>,
    /// Permit a cross-origin outbound URL for this request only. Omit for
    /// same-origin (relative) requests.
    #[serde(default)]
    pub allow_origin: Option<String>,
}

fn default_method() -> String {
    "GET".to_string()
}

#[allow(missing_docs)] // #[tool_router] generates a pub `browser_tool_router` fn.
#[tool_router(router = browser_tool_router, vis = "pub")]
impl OmniDevServer {
    /// Tool: drive a request through a running browser bridge tab.
    #[tool(
        description = "Send one HTTP request through a running, authenticated browser-bridge tab \
                       and return the buffered response envelope (status, headers, body) as YAML. \
                       Mirrors `omni-dev browser bridge request`. Requires a running bridge \
                       (`omni-dev browser bridge serve` or the daemon's bridge service) and a \
                       session token from `OMNI_BRIDGE_TOKEN` or `token_file`. NOT read-only — the \
                       request runs with the tab's session, so a non-GET `method` can mutate remote \
                       state. `url` is relative to the tab's page origin unless `allow_origin` \
                       permits a cross-origin target. Streaming responses are not supported here."
    )]
    pub async fn browser_bridge_request(
        &self,
        Parameters(params): Parameters<BrowserBridgeRequestParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = run_bridge_request(params).await.map_err(tool_error)?;
        Ok(build_truncated_result(yaml))
    }
}

/// Validates headers, resolves the session token, and sends a non-streaming
/// request via [`BridgeClient::send`], returning the response envelope as YAML.
async fn run_bridge_request(params: BrowserBridgeRequestParams) -> Result<String> {
    for (name, value) in &params.headers {
        if !auth::header_is_safe(name, value) {
            bail!("Invalid header name or value: {name}: {value}");
        }
    }

    let token =
        crate::cli::browser::resolve_client_token(params.token_file.as_deref().map(Path::new))?;
    let control_port = params.control_port.unwrap_or(DEFAULT_CONTROL_PORT);

    let payload = ControlRequest {
        url: params.url,
        method: params.method,
        headers: params.headers,
        body: params.body,
        stream: false,
        target: params.target,
        allow_origin: params.allow_origin,
        credentials: params.credentials.map(BridgeCredentials::to_fetch_value),
        // The body is always inline text here (no `@file`/binary path), so it is
        // never base64-encoded — the CLI only sets this for `--body-file`.
        encoding: None,
    };

    let client = BridgeClient::new(control_port, token);
    let endpoint = client.endpoint();
    let resp = client
        .send(&payload)
        .await
        .with_context(|| format!("Failed to reach bridge at {endpoint} (is it running?)"))?;

    serde_yaml::to_string(&resp).context("Failed to serialize bridge response as YAML")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn params_require_url_and_default_method() {
        assert!(serde_json::from_str::<BrowserBridgeRequestParams>("{}").is_err());
        let p: BrowserBridgeRequestParams = serde_json::from_str(r#"{"url": "/api/foo"}"#).unwrap();
        assert_eq!(p.url, "/api/foo");
        assert_eq!(p.method, "GET");
        assert!(p.headers.is_empty());
    }

    #[test]
    fn credentials_parse_kebab_case() {
        let p: BrowserBridgeRequestParams =
            serde_json::from_str(r#"{"url": "/x", "credentials": "same-origin"}"#).unwrap();
        assert!(matches!(p.credentials, Some(BridgeCredentials::SameOrigin)));
        assert_eq!(
            p.credentials.unwrap().to_fetch_value(),
            "same-origin".to_string()
        );
    }

    #[test]
    fn headers_parse_as_map() {
        let p: BrowserBridgeRequestParams =
            serde_json::from_str(r#"{"url": "/x", "headers": {"Accept": "application/json"}}"#)
                .unwrap();
        assert_eq!(
            p.headers.get("Accept").map(String::as_str),
            Some("application/json")
        );
    }

    #[test]
    fn credentials_to_fetch_value_covers_all_variants() {
        assert_eq!(BridgeCredentials::Include.to_fetch_value(), "include");
        assert_eq!(BridgeCredentials::Omit.to_fetch_value(), "omit");
        assert_eq!(
            BridgeCredentials::SameOrigin.to_fetch_value(),
            "same-origin"
        );
    }

    #[tokio::test]
    async fn browser_bridge_request_handler_bad_token_file_returns_tool_error() {
        use crate::mcp::server::OmniDevServer;
        use rmcp::handler::server::wrapper::Parameters;

        // A non-existent token file makes token resolution fail deterministically,
        // before any network is attempted — exercising the handler + validation.
        let server = OmniDevServer::new();
        let params = BrowserBridgeRequestParams {
            url: "/api/foo".to_string(),
            method: "GET".to_string(),
            headers: std::collections::BTreeMap::new(),
            body: None,
            credentials: None,
            control_port: None,
            token_file: Some("/no/such/bridge/token/for/mcp/test".to_string()),
            target: None,
            allow_origin: None,
        };
        let err = server
            .browser_bridge_request(Parameters(params))
            .await
            .unwrap_err();
        assert!(!err.message.is_empty());
    }

    #[tokio::test]
    async fn browser_bridge_request_handler_rejects_unsafe_header() {
        use crate::mcp::server::OmniDevServer;
        use rmcp::handler::server::wrapper::Parameters;

        // An unsafe header value (newline injection) is rejected before token
        // resolution or any network call.
        let mut headers = std::collections::BTreeMap::new();
        headers.insert("X-Bad".to_string(), "a\r\nInjected: 1".to_string());
        let server = OmniDevServer::new();
        let params = BrowserBridgeRequestParams {
            url: "/api/foo".to_string(),
            method: "GET".to_string(),
            headers,
            body: None,
            credentials: None,
            control_port: None,
            token_file: None,
            target: None,
            allow_origin: None,
        };
        let err = server
            .browser_bridge_request(Parameters(params))
            .await
            .unwrap_err();
        assert!(err.message.contains("Invalid header"));
    }
}
