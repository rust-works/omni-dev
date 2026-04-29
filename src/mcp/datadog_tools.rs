//! MCP tool handlers for Datadog read-only operations.
//!
//! Each tool builds a fresh [`DatadogClient`] via
//! [`crate::cli::datadog::helpers::create_client`] and then delegates to the
//! same API façade (`MetricsApi`, `MonitorsApi`, `DashboardsApi`, `LogsApi`)
//! that the CLI uses under `src/cli/datadog/`. The MCP surface and the CLI
//! therefore share a single implementation; tool outputs are YAML
//! serialisations of the typed response structs, matching the CLI `-o yaml`
//! output.
//!
//! Per-call client construction (rather than a shared client cached on
//! `OmniDevServer`) mirrors the Atlassian tools and lets credential changes
//! take effect without restarting the MCP server.

use anyhow::{Context, Result};
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    schemars, tool, tool_router, ErrorData as McpError,
};
use serde::Deserialize;

use crate::cli::datadog::helpers::create_client;
use crate::datadog::auth;
use crate::datadog::client::DatadogClient;
use crate::datadog::dashboards_api::{DashboardListFilter, DashboardsApi};
use crate::datadog::events_api::{EventsApi, EventsListFilter};
use crate::datadog::logs_api::LogsApi;
use crate::datadog::metrics_api::MetricsApi;
use crate::datadog::monitors_api::{MonitorListFilter, MonitorsApi};
use crate::datadog::time::parse_time_range;
use crate::datadog::types::SortOrder;

use super::error::tool_error;
use super::server::OmniDevServer;

// ── Parameter structs ───────────────────────────────────────────────

/// Parameters for `datadog_auth_status` (none).
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct DatadogAuthStatusParams {}

/// Parameters for the `datadog_metrics_query` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DatadogMetricsQueryParams {
    /// Datadog query string (e.g. `avg:system.cpu.user{*}`).
    pub query: String,
    /// Start of the query window. Accepts relative shorthand (`15m`,
    /// `1h`, `7d`), the literal `now`, an RFC 3339 timestamp with
    /// timezone, or Unix epoch seconds.
    pub from: String,
    /// End of the query window. Defaults to `now` when omitted.
    #[serde(default)]
    pub to: Option<String>,
}

/// Parameters for the `datadog_monitor_list` tool.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct DatadogMonitorListParams {
    /// Substring match on the monitor name.
    #[serde(default)]
    pub name: Option<String>,
    /// Comma-separated `key:value` tags applied to the monitor.
    #[serde(default)]
    pub tags: Option<String>,
    /// Comma-separated `key:value` tags applied via `monitor_tags`.
    #[serde(default)]
    pub monitor_tags: Option<String>,
    /// Maximum monitors to return. `0` (or omitted) means "fetch every
    /// match", capped at 10000.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Parameters for the `datadog_monitor_get` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DatadogMonitorGetParams {
    /// Datadog monitor identifier.
    pub monitor_id: i64,
}

/// Parameters for the `datadog_monitor_search` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DatadogMonitorSearchParams {
    /// Free-text / faceted search query (e.g. `status:alert`).
    pub query: String,
    /// Maximum monitors to return. `0` (or omitted) means "fetch every
    /// match", capped at 10000.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Parameters for the `datadog_dashboard_list` tool.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct DatadogDashboardListParams {
    /// When set, restricts the response to shared (or non-shared)
    /// dashboards depending on the boolean.
    #[serde(default)]
    pub filter_shared: Option<bool>,
}

/// Parameters for the `datadog_dashboard_get` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DatadogDashboardGetParams {
    /// Datadog dashboard identifier (e.g. `abc-def-ghi`).
    pub dashboard_id: String,
}

/// Parameters for the `datadog_events_list` tool.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct DatadogEventsListParams {
    /// Datadog events query (e.g. `service:api`).
    #[serde(default)]
    pub filter: Option<String>,
    /// Start of the time range. Defaults to `1h`.
    #[serde(default)]
    pub from: Option<String>,
    /// End of the time range. Defaults to `now`.
    #[serde(default)]
    pub to: Option<String>,
    /// Comma-separated list of source names.
    #[serde(default)]
    pub sources: Option<String>,
    /// Comma-separated list of `key:value` tags.
    #[serde(default)]
    pub tags: Option<String>,
    /// Per-page cap; defaults to 100. Max 1000.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Parameters for the `datadog_logs_search` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DatadogLogsSearchParams {
    /// Datadog logs query (e.g. `service:api status:error`).
    pub filter: String,
    /// Start of the time range. Accepts relative shorthand (`15m`,
    /// `1h`), `now`, RFC 3339, or Unix epoch seconds. Defaults to
    /// `15m` when omitted.
    #[serde(default)]
    pub from: Option<String>,
    /// End of the time range. Defaults to `now` when omitted.
    #[serde(default)]
    pub to: Option<String>,
    /// Maximum events to return (Datadog v2 logs search per-page cap is
    /// 1000; cursor pagination across pages is a Phase 2 follow-up).
    /// Defaults to 100 when omitted.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Sort order: `"timestamp-asc"` (oldest first) or `"timestamp-desc"`
    /// (newest first; default).
    #[serde(default)]
    pub sort: Option<String>,
}

// ── Tool handlers ────────────────────────────────────────────────────

#[allow(missing_docs)] // #[tool_router] generates a pub `datadog_tool_router` fn.
#[tool_router(router = datadog_tool_router, vis = "pub")]
impl OmniDevServer {
    /// Reports which Datadog credential scopes have credentials configured.
    ///
    /// Boolean presence flags only — never returns secret values.
    #[tool(
        description = "Report which Datadog credential scopes have credentials configured. \
                       Returns boolean presence flags only — NEVER includes the API key, \
                       application key, or any other secret. The site (non-secret) is \
                       returned verbatim. Read-only. Output is YAML."
    )]
    pub async fn datadog_auth_status(
        &self,
        Parameters(_params): Parameters<DatadogAuthStatusParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = run_auth_status().map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: execute a point-in-time Datadog metrics timeseries query.
    #[tool(
        description = "Execute a point-in-time Datadog metrics timeseries query. \
                       Mirrors `omni-dev datadog metrics query`. Returns YAML matching \
                       the CLI `-o yaml` output (status, from_date, to_date, series)."
    )]
    pub async fn datadog_metrics_query(
        &self,
        Parameters(params): Parameters<DatadogMetricsQueryParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = run_metrics_query(&params).await.map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: list Datadog monitors with optional filters.
    #[tool(
        description = "List Datadog monitors with optional name / tags filters. \
                       `limit` of 0 (or omitted) auto-paginates up to 10000. \
                       Mirrors `omni-dev datadog monitor list`. Output is YAML."
    )]
    pub async fn datadog_monitor_list(
        &self,
        Parameters(params): Parameters<DatadogMonitorListParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = run_monitor_list(&params).await.map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: fetch a single Datadog monitor by id.
    #[tool(description = "Fetch a single Datadog monitor by numeric id. \
                       Mirrors `omni-dev datadog monitor get`. Output is YAML.")]
    pub async fn datadog_monitor_get(
        &self,
        Parameters(params): Parameters<DatadogMonitorGetParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = run_monitor_get(params.monitor_id)
            .await
            .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: free-text / faceted search across Datadog monitors.
    #[tool(description = "Free-text / faceted search across Datadog monitors. \
                       `limit` of 0 (or omitted) auto-paginates up to 10000. \
                       Mirrors `omni-dev datadog monitor search`. Output is YAML.")]
    pub async fn datadog_monitor_search(
        &self,
        Parameters(params): Parameters<DatadogMonitorSearchParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = run_monitor_search(&params).await.map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: list Datadog dashboards.
    #[tool(
        description = "List Datadog dashboards. `filter_shared` (boolean, optional) \
                       restricts to shared / non-shared dashboards. \
                       Mirrors `omni-dev datadog dashboard list`. Output is YAML."
    )]
    pub async fn datadog_dashboard_list(
        &self,
        Parameters(params): Parameters<DatadogDashboardListParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = run_dashboard_list(&params).await.map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: fetch a single Datadog dashboard definition by id.
    #[tool(description = "Fetch a single Datadog dashboard by id (string). \
                       Returns the full definition including widgets. \
                       Mirrors `omni-dev datadog dashboard get`. Output is YAML.")]
    pub async fn datadog_dashboard_get(
        &self,
        Parameters(params): Parameters<DatadogDashboardGetParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = run_dashboard_get(&params.dashboard_id)
            .await
            .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: search Datadog log events (single page).
    #[tool(
        description = "Search Datadog log events. Single page only — Datadog v2 logs \
                       search uses cursor pagination; `limit` is the per-page cap (max 1000). \
                       `sort` is `timestamp-asc` or `timestamp-desc` (default). \
                       Mirrors `omni-dev datadog logs search`. Output is YAML."
    )]
    pub async fn datadog_logs_search(
        &self,
        Parameters(params): Parameters<DatadogLogsSearchParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = run_logs_search(&params).await.map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: list Datadog events (single page).
    #[tool(
        description = "List Datadog events. Single page only — Datadog v2 events use \
                       cursor pagination; `limit` is the per-page cap (max 1000). \
                       `from` / `to` accept relative shorthand (`15m`, `1h`), `now`, \
                       RFC 3339, or Unix epoch seconds. Mirrors \
                       `omni-dev datadog events list`. Output is YAML."
    )]
    pub async fn datadog_events_list(
        &self,
        Parameters(params): Parameters<DatadogEventsListParams>,
    ) -> Result<CallToolResult, McpError> {
        let yaml = run_events_list(&params).await.map_err(tool_error)?;
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }
}

// ── Internal run_* helpers ──────────────────────────────────────────

/// Renders the credential-presence summary as YAML.
///
/// Pure: never touches the network and never reads any secret values.
fn run_auth_status() -> Result<String> {
    let status = auth::status();
    serde_yaml::to_string(&status).context("Failed to serialize Datadog auth status")
}

async fn run_metrics_query(params: &DatadogMetricsQueryParams) -> Result<String> {
    let (from_ts, to_ts) = parse_time_range(&params.from, params.to.as_deref())?;
    let (client, _site) = create_client()?;
    let resp = MetricsApi::new(&client)
        .point_query(&params.query, from_ts, to_ts)
        .await?;
    serde_yaml::to_string(&resp).context("Failed to serialize metrics query response")
}

async fn run_monitor_list(params: &DatadogMonitorListParams) -> Result<String> {
    let (client, _site) = create_client()?;
    let monitors = monitor_list_with(&client, params).await?;
    serde_yaml::to_string(&monitors).context("Failed to serialize monitor list")
}

/// Splits the API call away from credential loading so tests can inject a
/// wiremock-backed [`DatadogClient`].
async fn monitor_list_with(
    client: &DatadogClient,
    params: &DatadogMonitorListParams,
) -> Result<Vec<crate::datadog::types::Monitor>> {
    let filter = MonitorListFilter {
        name: params.name.clone(),
        tags: params.tags.clone(),
        monitor_tags: params.monitor_tags.clone(),
    };
    MonitorsApi::new(client)
        .list(&filter, params.limit.unwrap_or(0))
        .await
}

async fn run_monitor_get(id: i64) -> Result<String> {
    let (client, _site) = create_client()?;
    let monitor = MonitorsApi::new(&client).get(id).await?;
    serde_yaml::to_string(&monitor).context("Failed to serialize monitor")
}

async fn run_monitor_search(params: &DatadogMonitorSearchParams) -> Result<String> {
    let (client, _site) = create_client()?;
    let result = MonitorsApi::new(&client)
        .search(&params.query, params.limit.unwrap_or(0))
        .await?;
    serde_yaml::to_string(&result).context("Failed to serialize monitor search results")
}

async fn run_dashboard_list(params: &DatadogDashboardListParams) -> Result<String> {
    let (client, _site) = create_client()?;
    let filter = DashboardListFilter {
        filter_shared: params.filter_shared,
    };
    let dashboards = DashboardsApi::new(&client).list(&filter).await?;
    serde_yaml::to_string(&dashboards).context("Failed to serialize dashboard list")
}

async fn run_dashboard_get(id: &str) -> Result<String> {
    let (client, _site) = create_client()?;
    let dashboard = DashboardsApi::new(&client).get(id).await?;
    serde_yaml::to_string(&dashboard).context("Failed to serialize dashboard")
}

async fn run_logs_search(params: &DatadogLogsSearchParams) -> Result<String> {
    let from = params.from.as_deref().unwrap_or("15m");
    let to = params.to.as_deref().unwrap_or("now");
    let (from_str, to_str) = resolve_logs_time_range(from, to)?;
    let limit = params.limit.unwrap_or(100);
    let sort = parse_sort_order(params.sort.as_deref())?;

    let (client, _site) = create_client()?;
    let result = LogsApi::new(&client)
        .search(&params.filter, &from_str, &to_str, limit, sort)
        .await?;
    serde_yaml::to_string(&result).context("Failed to serialize logs search results")
}

async fn run_events_list(params: &DatadogEventsListParams) -> Result<String> {
    let from = params.from.as_deref().unwrap_or("1h");
    let to = params.to.as_deref().unwrap_or("now");
    let (from_str, to_str) = resolve_logs_time_range(from, to)?;
    let limit = params.limit.unwrap_or(100);

    let (client, _site) = create_client()?;
    let filter = EventsListFilter {
        query: params.filter.clone(),
        sources: params.sources.clone(),
        tags: params.tags.clone(),
    };
    let result = EventsApi::new(&client)
        .list(&filter, &from_str, &to_str, limit)
        .await?;
    serde_yaml::to_string(&result).context("Failed to serialize events list")
}

/// Resolves `--from` / `--to` strings into RFC 3339 timestamps suitable
/// for the Datadog v2 logs search body.
///
/// Mirrors the CLI helper in `cli/datadog/logs/search.rs::resolve_time_range`
/// so MCP and CLI submit identical wire bodies for the same inputs.
fn resolve_logs_time_range(from: &str, to: &str) -> Result<(String, String)> {
    let (from_secs, to_secs) =
        parse_time_range(from, Some(to)).context("Failed to parse from / to")?;
    Ok((epoch_to_rfc3339(from_secs), epoch_to_rfc3339(to_secs)))
}

fn epoch_to_rfc3339(secs: i64) -> String {
    use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
    let dt: DateTime<Utc> = Utc.timestamp_opt(secs, 0).single().unwrap_or_else(|| {
        Utc.timestamp_opt(0, 0)
            .single()
            .unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).unwrap_or_default())
    });
    dt.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Parses an MCP-supplied sort order string.
///
/// Accepts the same kebab-case names as the CLI `--sort` arg
/// (`timestamp-asc` / `timestamp-desc`); `None` defaults to descending.
fn parse_sort_order(raw: Option<&str>) -> Result<SortOrder> {
    match raw.map(str::to_ascii_lowercase).as_deref() {
        None | Some("timestamp-desc") => Ok(SortOrder::TimestampDesc),
        Some("timestamp-asc") => Ok(SortOrder::TimestampAsc),
        Some(other) => anyhow::bail!(
            "Invalid sort \"{other}\": must be \"timestamp-asc\" or \"timestamp-desc\""
        ),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::fs;

    use rmcp::handler::server::wrapper::Parameters;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::datadog::auth::{DATADOG_API_KEY, DATADOG_API_URL, DATADOG_APP_KEY, DATADOG_SITE};
    use crate::datadog::test_support::{with_empty_home, EnvGuard};

    /// Writes a minimal Datadog settings.json + sets DATADOG_API_URL so
    /// that every `create_client()` call is routed to the wiremock server.
    fn configure_credentials_and_api_url(home: &std::path::Path, api_url: &str) {
        let omni_dir = home.join(".omni-dev");
        fs::create_dir_all(&omni_dir).unwrap();
        fs::write(
            omni_dir.join("settings.json"),
            r#"{"env":{"DATADOG_API_KEY":"api","DATADOG_APP_KEY":"app","DATADOG_SITE":"datadoghq.com"}}"#,
        )
        .unwrap();
        std::env::set_var(DATADOG_API_KEY, "api");
        std::env::set_var(DATADOG_APP_KEY, "app");
        std::env::set_var(DATADOG_API_URL, api_url);
    }

    // ── parse_sort_order ──────────────────────────────────────────────

    #[test]
    fn parse_sort_order_defaults_to_desc() {
        assert_eq!(parse_sort_order(None).unwrap(), SortOrder::TimestampDesc);
    }

    #[test]
    fn parse_sort_order_accepts_known_kebab_strings() {
        assert_eq!(
            parse_sort_order(Some("timestamp-asc")).unwrap(),
            SortOrder::TimestampAsc
        );
        assert_eq!(
            parse_sort_order(Some("timestamp-desc")).unwrap(),
            SortOrder::TimestampDesc
        );
    }

    #[test]
    fn parse_sort_order_is_case_insensitive() {
        assert_eq!(
            parse_sort_order(Some("Timestamp-ASC")).unwrap(),
            SortOrder::TimestampAsc
        );
    }

    #[test]
    fn parse_sort_order_rejects_unknown_value() {
        let err = parse_sort_order(Some("oldest")).unwrap_err();
        assert!(err.to_string().contains("sort"));
    }

    // ── epoch_to_rfc3339 ──────────────────────────────────────────────

    #[test]
    fn epoch_to_rfc3339_renders_zulu_seconds() {
        // 2023-11-14T22:13:20Z
        assert_eq!(epoch_to_rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn epoch_to_rfc3339_handles_zero() {
        assert_eq!(epoch_to_rfc3339(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn epoch_to_rfc3339_clamps_out_of_range_to_epoch() {
        // chrono rejects timestamps beyond i64 seconds; the fallback
        // path simply returns *some* RFC 3339 string.
        let s = epoch_to_rfc3339(i64::MAX);
        assert!(s.ends_with('Z'));
    }

    // ── resolve_logs_time_range ──────────────────────────────────────

    #[test]
    fn resolve_logs_time_range_returns_two_rfc3339_timestamps() {
        let (from, to) =
            resolve_logs_time_range("2023-11-14T22:00:00Z", "2023-11-14T23:00:00Z").unwrap();
        assert_eq!(from, "2023-11-14T22:00:00Z");
        assert_eq!(to, "2023-11-14T23:00:00Z");
    }

    #[test]
    fn resolve_logs_time_range_propagates_parse_error() {
        let err = resolve_logs_time_range("garbage", "now").unwrap_err();
        assert!(err.to_string().contains("Failed to parse"));
    }

    // ── run_auth_status ───────────────────────────────────────────────

    #[test]
    fn run_auth_status_reports_unconfigured_state() {
        let guard = EnvGuard::take();
        let _dir = with_empty_home(&guard);

        let yaml = run_auth_status().unwrap();
        assert!(yaml.contains("scopes:"));
        assert!(yaml.contains("name: default"));
        assert!(yaml.contains("has_api_key: false"));
        assert!(yaml.contains("has_app_key: false"));
    }

    #[test]
    fn run_auth_status_never_emits_secret_values() {
        let guard = EnvGuard::take();
        let dir = with_empty_home(&guard);
        let omni_dir = dir.path().join(".omni-dev");
        fs::create_dir_all(&omni_dir).unwrap();
        fs::write(
            omni_dir.join("settings.json"),
            r#"{"env":{
                "DATADOG_API_KEY":"sekret-api-do-not-leak",
                "DATADOG_APP_KEY":"sekret-app-do-not-leak",
                "DATADOG_SITE":"datadoghq.com"
            }}"#,
        )
        .unwrap();

        let yaml = run_auth_status().unwrap();
        assert!(yaml.contains("has_api_key: true"));
        assert!(yaml.contains("has_app_key: true"));
        assert!(yaml.contains("datadoghq.com"));
        assert!(!yaml.contains("sekret-api-do-not-leak"));
        assert!(!yaml.contains("sekret-app-do-not-leak"));
    }

    // ── run_metrics_query ─────────────────────────────────────────────

    fn metrics_body() -> serde_json::Value {
        serde_json::json!({
            "status": "ok",
            "from_date": 1_700_000_000_000_i64,
            "to_date":   1_700_000_030_000_i64,
            "series": [{
                "metric": "avg:system.cpu.user{*}",
                "display_name": "avg:system.cpu.user{*}",
                "expression": "avg:system.cpu.user{*}",
                "pointlist": [
                    [1_700_000_000_000_i64, 0.5_f64],
                    [1_700_000_030_000_i64, 0.6_f64]
                ]
            }]
        })
    }

    #[tokio::test]
    async fn run_metrics_query_serialises_response_as_yaml() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/query"))
            .and(query_param("query", "avg:system.cpu.user{*}"))
            .respond_with(ResponseTemplate::new(200).set_body_json(metrics_body()))
            .expect(1)
            .mount(&server)
            .await;

        let guard = EnvGuard::take();
        let dir = with_empty_home(&guard);
        configure_credentials_and_api_url(dir.path(), &server.uri());

        let yaml = run_metrics_query(&DatadogMetricsQueryParams {
            query: "avg:system.cpu.user{*}".to_string(),
            from: "2023-11-14T22:00:00Z".to_string(),
            to: Some("2023-11-14T23:00:00Z".to_string()),
        })
        .await
        .unwrap();
        assert!(yaml.contains("status: ok"));
        assert!(yaml.contains("avg:system.cpu.user"));
    }

    #[tokio::test]
    async fn run_metrics_query_rejects_invalid_time_range() {
        let guard = EnvGuard::take();
        let _dir = with_empty_home(&guard);
        let err = run_metrics_query(&DatadogMetricsQueryParams {
            query: "m".into(),
            from: "garbage".into(),
            to: None,
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("Invalid time range"));
    }

    #[tokio::test]
    async fn run_metrics_query_errors_when_credentials_missing() {
        let guard = EnvGuard::take();
        let _dir = with_empty_home(&guard);
        let err = run_metrics_query(&DatadogMetricsQueryParams {
            query: "m".into(),
            from: "1h".into(),
            to: Some("now".into()),
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not configured"));
    }

    // ── run_monitor_list ──────────────────────────────────────────────

    fn monitor_json(id: i64, name: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "name": name,
            "type": "metric alert",
            "query": "avg(last_5m):avg:system.cpu.user{*} > 90",
            "tags": ["team:sre"],
            "overall_state": "OK"
        })
    }

    #[tokio::test]
    async fn run_monitor_list_returns_yaml_array() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/monitor"))
            .and(query_param("name", "cpu"))
            .and(query_param("tags", "team:sre"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                monitor_json(1, "Disk full"),
                monitor_json(2, "CPU high")
            ])))
            .expect(1)
            .mount(&server)
            .await;

        let guard = EnvGuard::take();
        let dir = with_empty_home(&guard);
        configure_credentials_and_api_url(dir.path(), &server.uri());

        let yaml = run_monitor_list(&DatadogMonitorListParams {
            name: Some("cpu".into()),
            tags: Some("team:sre".into()),
            monitor_tags: None,
            limit: Some(10),
        })
        .await
        .unwrap();
        assert!(yaml.contains("CPU high"));
        assert!(yaml.contains("Disk full"));
    }

    #[tokio::test]
    async fn run_monitor_list_propagates_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/monitor"))
            .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
            .mount(&server)
            .await;

        let guard = EnvGuard::take();
        let dir = with_empty_home(&guard);
        configure_credentials_and_api_url(dir.path(), &server.uri());

        let err = run_monitor_list(&DatadogMonitorListParams {
            name: None,
            tags: None,
            monitor_tags: None,
            limit: Some(5),
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    // ── run_monitor_get ───────────────────────────────────────────────

    #[tokio::test]
    async fn run_monitor_get_returns_yaml_object() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/monitor/12345"))
            .respond_with(ResponseTemplate::new(200).set_body_json(monitor_json(12345, "CPU high")))
            .expect(1)
            .mount(&server)
            .await;

        let guard = EnvGuard::take();
        let dir = with_empty_home(&guard);
        configure_credentials_and_api_url(dir.path(), &server.uri());

        let yaml = run_monitor_get(12345).await.unwrap();
        assert!(yaml.contains("id: 12345"));
        assert!(yaml.contains("CPU high"));
    }

    #[tokio::test]
    async fn run_monitor_get_propagates_404() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/monitor/9"))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;

        let guard = EnvGuard::take();
        let dir = with_empty_home(&guard);
        configure_credentials_and_api_url(dir.path(), &server.uri());

        let err = run_monitor_get(9).await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    // ── run_monitor_search ────────────────────────────────────────────

    #[tokio::test]
    async fn run_monitor_search_returns_yaml_envelope() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/monitor/search"))
            .and(query_param("query", "status:alert"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "monitors": [
                    { "id": 1_i64, "name": "Disk full", "status": "ALERT", "tags": [] }
                ],
                "metadata": {"page": 0, "per_page": 1, "page_count": 1, "total_count": 1}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let guard = EnvGuard::take();
        let dir = with_empty_home(&guard);
        configure_credentials_and_api_url(dir.path(), &server.uri());

        let yaml = run_monitor_search(&DatadogMonitorSearchParams {
            query: "status:alert".to_string(),
            limit: Some(5),
        })
        .await
        .unwrap();
        assert!(yaml.contains("monitors:"));
        assert!(yaml.contains("Disk full"));
        assert!(yaml.contains("ALERT"));
    }

    // ── run_dashboard_list ────────────────────────────────────────────

    #[tokio::test]
    async fn run_dashboard_list_passes_filter_shared_param() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/dashboard"))
            .and(query_param("filter_shared", "true"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "dashboards": [
                    {"id": "abc", "title": "Service A", "is_shared": true}
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let guard = EnvGuard::take();
        let dir = with_empty_home(&guard);
        configure_credentials_and_api_url(dir.path(), &server.uri());

        let yaml = run_dashboard_list(&DatadogDashboardListParams {
            filter_shared: Some(true),
        })
        .await
        .unwrap();
        assert!(yaml.contains("Service A"));
    }

    #[tokio::test]
    async fn run_dashboard_list_omits_filter_when_unset() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/dashboard"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "dashboards": []
            })))
            .expect(1)
            .mount(&server)
            .await;

        let guard = EnvGuard::take();
        let dir = with_empty_home(&guard);
        configure_credentials_and_api_url(dir.path(), &server.uri());

        let yaml = run_dashboard_list(&DatadogDashboardListParams::default())
            .await
            .unwrap();
        assert!(yaml.contains("[]"));
    }

    // ── run_dashboard_get ─────────────────────────────────────────────

    #[tokio::test]
    async fn run_dashboard_get_returns_yaml_with_widgets() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/dashboard/abc-def-ghi"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "abc-def-ghi",
                "title": "Service Overview",
                "layout_type": "ordered",
                "widgets": [
                    {"id": 1, "definition": {"type": "note", "content": "hi"}}
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let guard = EnvGuard::take();
        let dir = with_empty_home(&guard);
        configure_credentials_and_api_url(dir.path(), &server.uri());

        let yaml = run_dashboard_get("abc-def-ghi").await.unwrap();
        assert!(yaml.contains("id: abc-def-ghi"));
        assert!(yaml.contains("Service Overview"));
        assert!(yaml.contains("widgets:"));
    }

    // ── run_logs_search ───────────────────────────────────────────────

    fn logs_body() -> serde_json::Value {
        serde_json::json!({
            "data": [
                {
                    "id": "AAAA",
                    "type": "log",
                    "attributes": {
                        "timestamp": "2026-04-22T10:00:00.000Z",
                        "service": "api",
                        "status": "info",
                        "message": "request handled",
                        "tags": ["env:prod"]
                    }
                }
            ],
            "meta": {"page": {"after": "next"}, "status": "done"}
        })
    }

    #[tokio::test]
    async fn run_logs_search_uses_descending_default_sort_and_default_limit() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v2/logs/events/search"))
            .and(header("DD-API-KEY", "api"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "filter": { "query": "service:api" },
                "page": { "limit": 100 },
                "sort": "-timestamp"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(logs_body()))
            .expect(1)
            .mount(&server)
            .await;

        let guard = EnvGuard::take();
        let dir = with_empty_home(&guard);
        configure_credentials_and_api_url(dir.path(), &server.uri());

        let yaml = run_logs_search(&DatadogLogsSearchParams {
            filter: "service:api".to_string(),
            from: Some("2023-11-14T22:00:00Z".to_string()),
            to: Some("2023-11-14T23:00:00Z".to_string()),
            limit: None,
            sort: None,
        })
        .await
        .unwrap();
        assert!(yaml.contains("data:"));
        assert!(yaml.contains("AAAA"));
    }

    #[tokio::test]
    async fn run_logs_search_threads_explicit_sort_and_limit() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v2/logs/events/search"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "page": { "limit": 25 },
                "sort": "timestamp"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(logs_body()))
            .expect(1)
            .mount(&server)
            .await;

        let guard = EnvGuard::take();
        let dir = with_empty_home(&guard);
        configure_credentials_and_api_url(dir.path(), &server.uri());

        run_logs_search(&DatadogLogsSearchParams {
            filter: "*".to_string(),
            from: Some("2023-11-14T22:00:00Z".to_string()),
            to: Some("2023-11-14T23:00:00Z".to_string()),
            limit: Some(25),
            sort: Some("timestamp-asc".to_string()),
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_logs_search_rejects_unknown_sort_value() {
        let guard = EnvGuard::take();
        let _dir = with_empty_home(&guard);
        let err = run_logs_search(&DatadogLogsSearchParams {
            filter: "*".into(),
            from: None,
            to: None,
            limit: None,
            sort: Some("oldest-first".into()),
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("sort"));
    }

    // ── Tool handler bodies (smoke + auth-status full path) ──────────

    #[tokio::test(flavor = "current_thread")]
    async fn datadog_auth_status_handler_returns_yaml_no_secrets() {
        let guard = EnvGuard::take();
        let dir = with_empty_home(&guard);
        let omni_dir = dir.path().join(".omni-dev");
        fs::create_dir_all(&omni_dir).unwrap();
        fs::write(
            omni_dir.join("settings.json"),
            r#"{"env":{
                "DATADOG_API_KEY":"sekret-api",
                "DATADOG_APP_KEY":"sekret-app",
                "DATADOG_SITE":"us5.datadoghq.com"
            }}"#,
        )
        .unwrap();
        std::env::remove_var(DATADOG_API_KEY);
        std::env::remove_var(DATADOG_APP_KEY);
        std::env::remove_var(DATADOG_SITE);

        let server = OmniDevServer::new();
        let result = server
            .datadog_auth_status(Parameters(DatadogAuthStatusParams::default()))
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false));
        let body = result.content[0]
            .as_text()
            .expect("auth status returns text content")
            .text
            .clone();
        assert!(body.contains("has_api_key: true"));
        assert!(body.contains("has_app_key: true"));
        assert!(body.contains("us5.datadoghq.com"));
        assert!(!body.contains("sekret-api"));
        assert!(!body.contains("sekret-app"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn datadog_metrics_query_handler_propagates_credentials_error() {
        let guard = EnvGuard::take();
        let _dir = with_empty_home(&guard);

        let server = OmniDevServer::new();
        let err = server
            .datadog_metrics_query(Parameters(DatadogMetricsQueryParams {
                query: "m".to_string(),
                from: "1h".to_string(),
                to: Some("now".to_string()),
            }))
            .await
            .unwrap_err();
        assert!(err.message.contains("not configured"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn datadog_logs_search_handler_rejects_invalid_sort() {
        let guard = EnvGuard::take();
        let _dir = with_empty_home(&guard);

        let server = OmniDevServer::new();
        let err = server
            .datadog_logs_search(Parameters(DatadogLogsSearchParams {
                filter: "*".to_string(),
                from: None,
                to: None,
                limit: None,
                sort: Some("nope".to_string()),
            }))
            .await
            .unwrap_err();
        assert!(err.message.contains("sort"));
    }

    /// Pulls the text body out of a successful `CallToolResult`, panicking
    /// with a clear message if the result was an error or non-text payload.
    fn handler_text(result: &rmcp::model::CallToolResult) -> String {
        assert!(!result.is_error.unwrap_or(false), "tool returned error");
        result.content[0]
            .as_text()
            .expect("expected text content")
            .text
            .clone()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn datadog_metrics_query_handler_success_returns_yaml() {
        let server_mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/query"))
            .respond_with(ResponseTemplate::new(200).set_body_json(metrics_body()))
            .expect(1)
            .mount(&server_mock)
            .await;

        let guard = EnvGuard::take();
        let dir = with_empty_home(&guard);
        configure_credentials_and_api_url(dir.path(), &server_mock.uri());

        let server = OmniDevServer::new();
        let result = server
            .datadog_metrics_query(Parameters(DatadogMetricsQueryParams {
                query: "avg:system.cpu.user{*}".to_string(),
                from: "2023-11-14T22:00:00Z".to_string(),
                to: Some("2023-11-14T23:00:00Z".to_string()),
            }))
            .await
            .unwrap();
        let body = handler_text(&result);
        assert!(body.contains("status: ok"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn datadog_monitor_list_handler_success_returns_yaml() {
        let server_mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/monitor"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!([monitor_json(7, "Disk full")])),
            )
            .expect(1)
            .mount(&server_mock)
            .await;

        let guard = EnvGuard::take();
        let dir = with_empty_home(&guard);
        configure_credentials_and_api_url(dir.path(), &server_mock.uri());

        let server = OmniDevServer::new();
        let result = server
            .datadog_monitor_list(Parameters(DatadogMonitorListParams {
                limit: Some(5),
                ..Default::default()
            }))
            .await
            .unwrap();
        let body = handler_text(&result);
        assert!(body.contains("Disk full"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn datadog_monitor_get_handler_success_returns_yaml() {
        let server_mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/monitor/42"))
            .respond_with(ResponseTemplate::new(200).set_body_json(monitor_json(42, "CPU high")))
            .expect(1)
            .mount(&server_mock)
            .await;

        let guard = EnvGuard::take();
        let dir = with_empty_home(&guard);
        configure_credentials_and_api_url(dir.path(), &server_mock.uri());

        let server = OmniDevServer::new();
        let result = server
            .datadog_monitor_get(Parameters(DatadogMonitorGetParams { monitor_id: 42 }))
            .await
            .unwrap();
        let body = handler_text(&result);
        assert!(body.contains("id: 42"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn datadog_monitor_search_handler_success_returns_yaml() {
        let server_mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/monitor/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "monitors": [
                    { "id": 99_i64, "name": "Latency", "status": "OK", "tags": [] }
                ]
            })))
            .expect(1)
            .mount(&server_mock)
            .await;

        let guard = EnvGuard::take();
        let dir = with_empty_home(&guard);
        configure_credentials_and_api_url(dir.path(), &server_mock.uri());

        let server = OmniDevServer::new();
        let result = server
            .datadog_monitor_search(Parameters(DatadogMonitorSearchParams {
                query: "status:ok".to_string(),
                limit: Some(5),
            }))
            .await
            .unwrap();
        let body = handler_text(&result);
        assert!(body.contains("Latency"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn datadog_dashboard_list_handler_success_returns_yaml() {
        let server_mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/dashboard"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "dashboards": [{"id": "abc", "title": "Overview"}]
            })))
            .expect(1)
            .mount(&server_mock)
            .await;

        let guard = EnvGuard::take();
        let dir = with_empty_home(&guard);
        configure_credentials_and_api_url(dir.path(), &server_mock.uri());

        let server = OmniDevServer::new();
        let result = server
            .datadog_dashboard_list(Parameters(DatadogDashboardListParams::default()))
            .await
            .unwrap();
        let body = handler_text(&result);
        assert!(body.contains("Overview"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn datadog_dashboard_get_handler_success_returns_yaml() {
        let server_mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/dashboard/zzz"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "zzz",
                "title": "Detail"
            })))
            .expect(1)
            .mount(&server_mock)
            .await;

        let guard = EnvGuard::take();
        let dir = with_empty_home(&guard);
        configure_credentials_and_api_url(dir.path(), &server_mock.uri());

        let server = OmniDevServer::new();
        let result = server
            .datadog_dashboard_get(Parameters(DatadogDashboardGetParams {
                dashboard_id: "zzz".to_string(),
            }))
            .await
            .unwrap();
        let body = handler_text(&result);
        assert!(body.contains("id: zzz"));
        assert!(body.contains("Detail"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn datadog_logs_search_handler_success_returns_yaml() {
        let server_mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v2/logs/events/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(logs_body()))
            .expect(1)
            .mount(&server_mock)
            .await;

        let guard = EnvGuard::take();
        let dir = with_empty_home(&guard);
        configure_credentials_and_api_url(dir.path(), &server_mock.uri());

        let server = OmniDevServer::new();
        let result = server
            .datadog_logs_search(Parameters(DatadogLogsSearchParams {
                filter: "service:api".to_string(),
                from: Some("2023-11-14T22:00:00Z".to_string()),
                to: Some("2023-11-14T23:00:00Z".to_string()),
                limit: Some(50),
                sort: Some("timestamp-desc".to_string()),
            }))
            .await
            .unwrap();
        let body = handler_text(&result);
        assert!(body.contains("AAAA"));
    }

    // ── Parameter struct sanity checks ────────────────────────────────

    #[test]
    fn auth_status_params_accepts_empty_object() {
        let _: DatadogAuthStatusParams = serde_json::from_str("{}").unwrap();
    }

    #[test]
    fn monitor_list_params_accepts_empty_object() {
        let _: DatadogMonitorListParams = serde_json::from_str("{}").unwrap();
    }

    #[test]
    fn dashboard_list_params_accepts_empty_object() {
        let _: DatadogDashboardListParams = serde_json::from_str("{}").unwrap();
    }

    #[test]
    fn logs_search_params_accepts_minimal_object() {
        let p: DatadogLogsSearchParams = serde_json::from_str(r#"{"filter":"*"}"#).unwrap();
        assert_eq!(p.filter, "*");
        assert!(p.from.is_none());
        assert!(p.sort.is_none());
    }

    #[test]
    fn monitor_get_params_requires_monitor_id() {
        let err = serde_json::from_str::<DatadogMonitorGetParams>("{}").unwrap_err();
        assert!(err.to_string().contains("monitor_id"));
    }

    #[test]
    fn dashboard_get_params_requires_dashboard_id() {
        let err = serde_json::from_str::<DatadogDashboardGetParams>("{}").unwrap_err();
        assert!(err.to_string().contains("dashboard_id"));
    }

    #[test]
    fn metrics_query_params_requires_query_and_from() {
        let err = serde_json::from_str::<DatadogMetricsQueryParams>("{}").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("query") || msg.contains("from"));
    }

    // ── Router registration ───────────────────────────────────────────

    #[test]
    fn datadog_tool_router_registers_all_tools() {
        let router = OmniDevServer::datadog_tool_router();
        for name in [
            // Phase 1
            "datadog_auth_status",
            "datadog_metrics_query",
            "datadog_monitor_list",
            "datadog_monitor_get",
            "datadog_monitor_search",
            "datadog_dashboard_list",
            "datadog_dashboard_get",
            "datadog_logs_search",
            // Phase 2
            "datadog_events_list",
        ] {
            assert!(router.has_route(name), "missing route: {name}");
        }
    }

    // ── Phase 2: events tests ────────────────────────────────────────

    #[test]
    fn events_list_params_accepts_empty_object() {
        let _: DatadogEventsListParams = serde_json::from_str("{}").unwrap();
    }

    fn events_body() -> serde_json::Value {
        serde_json::json!({
            "data": [
                {
                    "id": "EV1",
                    "type": "event",
                    "attributes": {
                        "timestamp": "2026-04-22T10:00:00.000Z",
                        "title": "Deploy",
                        "source": "github"
                    }
                }
            ]
        })
    }

    #[tokio::test]
    async fn run_events_list_returns_yaml() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v2/events"))
            .respond_with(ResponseTemplate::new(200).set_body_json(events_body()))
            .expect(1)
            .mount(&server)
            .await;

        let guard = EnvGuard::take();
        let dir = with_empty_home(&guard);
        configure_credentials_and_api_url(dir.path(), &server.uri());

        let yaml = run_events_list(&DatadogEventsListParams {
            filter: Some("service:api".into()),
            from: Some("2026-04-22T09:00:00Z".into()),
            to: Some("2026-04-22T10:00:00Z".into()),
            sources: None,
            tags: None,
            limit: Some(10),
        })
        .await
        .unwrap();
        assert!(yaml.contains("EV1"));
        assert!(yaml.contains("Deploy"));
    }

    #[tokio::test]
    async fn run_events_list_rejects_invalid_time_range() {
        let guard = EnvGuard::take();
        let _dir = with_empty_home(&guard);
        let err = run_events_list(&DatadogEventsListParams {
            filter: None,
            from: Some("garbage".into()),
            to: None,
            sources: None,
            tags: None,
            limit: None,
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("Failed to parse"));
    }

    #[tokio::test]
    async fn run_events_list_errors_when_credentials_missing() {
        let guard = EnvGuard::take();
        let _dir = with_empty_home(&guard);
        let err = run_events_list(&DatadogEventsListParams {
            filter: None,
            from: Some("2026-04-22T09:00:00Z".into()),
            to: Some("2026-04-22T10:00:00Z".into()),
            sources: None,
            tags: None,
            limit: None,
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not configured"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn datadog_events_list_handler_success_returns_yaml() {
        let server_mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v2/events"))
            .respond_with(ResponseTemplate::new(200).set_body_json(events_body()))
            .expect(1)
            .mount(&server_mock)
            .await;

        let guard = EnvGuard::take();
        let dir = with_empty_home(&guard);
        configure_credentials_and_api_url(dir.path(), &server_mock.uri());

        let server = OmniDevServer::new();
        let result = server
            .datadog_events_list(Parameters(DatadogEventsListParams {
                filter: None,
                from: Some("2026-04-22T09:00:00Z".into()),
                to: Some("2026-04-22T10:00:00Z".into()),
                sources: None,
                tags: None,
                limit: Some(10),
            }))
            .await
            .unwrap();
        let body = handler_text(&result);
        assert!(body.contains("EV1"));
    }
}
