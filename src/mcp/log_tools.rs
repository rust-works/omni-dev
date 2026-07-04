//! MCP tool handler for searching the local request log.
//!
//! The MCP server already writes the request log on every tool call (see
//! `src/mcp/server.rs`), so `log_search` lets an agent introspect its own
//! invocation and HTTP history. It reuses the CLI's filter matrix and renderers
//! (`crate::cli::log::run_search_capture`) — the same code behind `omni-dev
//! log` — capturing the output into a string instead of streaming to stdout.
//! `--follow` has no MCP analogue (it never returns) and is intentionally absent.

use rmcp::{
    handler::server::wrapper::Parameters, model::CallToolResult, schemars, tool, tool_router,
    ErrorData as McpError,
};
use serde::Deserialize;

use super::error::tool_error;
use super::git_tools::build_truncated_result;
use super::server::OmniDevServer;
use crate::cli::log::{Format, SearchRequest};

/// Output rendering for `log_search`.
#[derive(Debug, Clone, Copy, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// One compact line per record (default).
    #[default]
    Oneline,
    /// The on-disk NDJSON line, verbatim.
    Json,
    /// A labelled, multi-line block per record.
    Full,
}

impl From<LogFormat> for Format {
    fn from(value: LogFormat) -> Self {
        match value {
            LogFormat::Oneline => Self::Oneline,
            LogFormat::Json => Self::Json,
            LogFormat::Full => Self::Full,
        }
    }
}

/// Parameters for the `log_search` tool.
///
/// Every field is optional — with none set, the most recent records are
/// returned (bounded by `limit`). Filters are AND-ed together.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct LogSearchParams {
    /// Lower time bound: a relative window (`30m`, `2h`, `1d`), a date
    /// (`2026-07-01`), or an RFC3339 timestamp.
    #[serde(default)]
    pub since: Option<String>,
    /// Upper time bound: same forms as `since` (a relative value means that long
    /// ago). Pair with `since` for a bounded window.
    #[serde(default)]
    pub until: Option<String>,
    /// Match the HTTP method (case-insensitive), e.g. `GET`.
    #[serde(default)]
    pub method: Option<String>,
    /// Match the status: exact (`200`), class (`5xx`), or list (`4xx,5xx`).
    #[serde(default)]
    pub status: Option<String>,
    /// Match the service tag, e.g. `jira`, `datadog`, `browser-bridge`.
    #[serde(default)]
    pub service: Option<String>,
    /// Match the resolved command-path prefix, e.g. `jira read`.
    #[serde(default)]
    pub command: Option<String>,
    /// Match a substring of the request URL.
    #[serde(default)]
    pub url: Option<String>,
    /// Match a regular expression against the raw JSON line.
    #[serde(default)]
    pub grep: Option<String>,
    /// Fuzzy tokens (substrings of the raw line); AND-ed together.
    #[serde(default)]
    pub fuzzy: Vec<String>,
    /// Query expressions (AND/OR/NOT, `field:value`, bare tokens); AND-ed.
    #[serde(default)]
    pub query: Vec<String>,
    /// Match this record `id` or `invocation_id` (pulls a run and its requests).
    #[serde(default)]
    pub id: Option<String>,
    /// Output rendering. Defaults to `oneline`.
    #[serde(default)]
    pub format: LogFormat,
    /// Show at most N (most recent) matching records. Unbounded when omitted.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[allow(missing_docs)] // #[tool_router] generates a pub `log_tool_router` fn.
#[tool_router(router = log_tool_router, vis = "pub")]
impl OmniDevServer {
    /// Tool: search the local invocation + HTTP request log.
    #[tool(
        description = "Search the local omni-dev request log — every CLI/MCP invocation and the \
                       HTTP requests it issued — and return matching records as text. Read-only. \
                       Mirrors `omni-dev log` (the `--follow` tail has no MCP form). All filters \
                       are optional and AND-ed: `since`/`until` (`30m`/`2h`, a date, or an \
                       RFC3339 timestamp), `method`, `status` \
                       (`200`/`5xx`/`4xx,5xx`), `service`, `command`, `url`, `grep` (regex), \
                       `fuzzy`/`query` (mini-language), `id`. `format` is `oneline` (default), \
                       `json` (verbatim NDJSON), or `full`; `limit` keeps the most recent N."
    )]
    pub async fn log_search(
        &self,
        Parameters(params): Parameters<LogSearchParams>,
    ) -> Result<CallToolResult, McpError> {
        let text = crate::cli::log::run_search_capture(SearchRequest {
            since: params.since.as_deref(),
            until: params.until.as_deref(),
            method: params.method.as_deref(),
            status: params.status.as_deref(),
            service: params.service.as_deref(),
            command: params.command.as_deref(),
            url: params.url.as_deref(),
            grep: params.grep.as_deref(),
            fuzzy: &params.fuzzy,
            query: &params.query,
            id: params.id.as_deref(),
            format: params.format.into(),
            limit: params.limit,
        })
        .map_err(tool_error)?;

        Ok(build_truncated_result(text))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn params_default_ok() {
        let p: LogSearchParams = serde_json::from_str("{}").unwrap();
        assert!(p.since.is_none());
        assert!(p.fuzzy.is_empty());
        assert!(matches!(p.format, LogFormat::Oneline));
    }

    #[test]
    fn format_maps_to_cli_format() {
        assert!(matches!(Format::from(LogFormat::Json), Format::Json));
        assert!(matches!(Format::from(LogFormat::Full), Format::Full));
        assert!(matches!(Format::from(LogFormat::Oneline), Format::Oneline));
    }

    #[test]
    fn format_parses_lowercase() {
        let p: LogSearchParams = serde_json::from_str(r#"{"format": "json"}"#).unwrap();
        assert!(matches!(p.format, LogFormat::Json));
    }

    #[test]
    fn format_maps_all_variants() {
        assert!(matches!(Format::from(LogFormat::Oneline), Format::Oneline));
        assert!(matches!(Format::from(LogFormat::Json), Format::Json));
        assert!(matches!(Format::from(LogFormat::Full), Format::Full));
    }

    #[tokio::test]
    async fn log_search_handler_returns_result_for_ambient_log() {
        use crate::mcp::server::OmniDevServer;
        use rmcp::handler::server::wrapper::Parameters;

        // Read-only against whatever the ambient log path resolves to (a missing
        // file yields an empty result). Exercises the handler + the CLI
        // `run_search_capture` wiring without mutating any env. A `status` filter
        // that parses ensures the filter-build path runs.
        let server = OmniDevServer::new();
        let params = LogSearchParams {
            status: Some("5xx".to_string()),
            limit: Some(1),
            ..Default::default()
        };
        let result = server.log_search(Parameters(params)).await;
        assert!(result.is_ok(), "log_search should not error: {result:?}");
        let ok = result.unwrap();
        assert!(!ok.content.is_empty());
    }

    #[tokio::test]
    async fn log_search_handler_reports_invalid_query() {
        use crate::mcp::server::OmniDevServer;
        use rmcp::handler::server::wrapper::Parameters;

        // An unbalanced query expression fails at Filter::build, surfacing a
        // tool error before any file read.
        let server = OmniDevServer::new();
        let params = LogSearchParams {
            query: vec!["(status:5xx AND".to_string()],
            ..Default::default()
        };
        let err = server.log_search(Parameters(params)).await.unwrap_err();
        assert!(!err.message.is_empty());
    }
}
