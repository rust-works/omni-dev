//! Shared response types for the Datadog API.
//!
//! Populated in subsequent slices as endpoint families land. Kept as a
//! module now so `src/datadog.rs` declares a stable set of children.

use std::io::Write;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::cli::datadog::format::{write_items_jsonl, write_scalar_jsonl, JsonlSerialize};

/// A single `(timestamp_ms, value)` sample returned by Datadog.
///
/// Datadog returns `pointlist` as a JSON array of two-element arrays where
/// the timestamp is milliseconds since the Unix epoch and the value may be
/// `null` for gaps in the series.
pub type MetricPoint = (f64, Option<f64>);

/// A single series within a Datadog metrics query response.
///
/// Only the fields used by the CLI renderer are retained; additional
/// fields Datadog may emit (e.g. `length`, `start`, `end`, `aggr`,
/// `unit`, `attributes`, `query_index`) are ignored by the deserializer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MetricSeries {
    /// Metric identifier as returned by Datadog (e.g. `avg:system.cpu.user{*}`).
    pub metric: String,

    /// Human-friendly name suitable as a column header; when Datadog omits
    /// it we fall back to the `expression` or `metric` field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,

    /// Scope that the points apply to (e.g. `host:*`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,

    /// Original query expression for this series.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expression: Option<String>,

    /// Sample points as `(timestamp_ms, value)` pairs.
    #[serde(default)]
    pub pointlist: Vec<MetricPoint>,
}

impl MetricSeries {
    /// Returns the best available column label for this series.
    ///
    /// Prefers `display_name`, then `expression`, then `metric`.
    #[must_use]
    pub fn label(&self) -> &str {
        self.display_name
            .as_deref()
            .or(self.expression.as_deref())
            .unwrap_or(&self.metric)
    }
}

/// Response from `GET /api/v1/query`.
///
/// `from_date` / `to_date` are in milliseconds since the Unix epoch — the
/// native unit Datadog emits.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MetricQueryResponse {
    /// Query status (`ok` or `error`).
    pub status: String,

    /// Start of the returned window in epoch milliseconds.
    pub from_date: i64,

    /// End of the returned window in epoch milliseconds.
    pub to_date: i64,

    /// One entry per series returned by Datadog.
    #[serde(default)]
    pub series: Vec<MetricSeries>,
}

impl JsonlSerialize for MetricQueryResponse {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// A Datadog monitor as returned by `GET /api/v1/monitor` and
/// `GET /api/v1/monitor/{id}`.
///
/// Only the fields exposed by the CLI are retained; additional fields
/// Datadog may emit (e.g. `creator`, `options`) are surfaced through
/// `serde_json::Value` so JSON / YAML output preserves them while the
/// table renderer ignores them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Monitor {
    /// Datadog monitor identifier.
    pub id: i64,

    /// Human-readable monitor name.
    pub name: String,

    /// Monitor type (e.g. `metric alert`, `service check`, `log alert`).
    #[serde(rename = "type")]
    pub monitor_type: String,

    /// The monitor query expression.
    pub query: String,

    /// Optional notification message body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,

    /// Tags applied to the monitor.
    #[serde(default)]
    pub tags: Vec<String>,

    /// Aggregated state across all groups (e.g. `OK`, `Alert`, `No Data`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overall_state: Option<String>,

    /// ISO 8601 creation timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,

    /// ISO 8601 last-modified timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,

    /// Optional priority (1 highest – 5 lowest).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i64>,

    /// Whether the monitor evaluates as multi-alert across groups.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multi: Option<bool>,

    /// Creator of the monitor (raw object).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creator: Option<serde_json::Value>,

    /// Monitor configuration options (raw object).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<serde_json::Value>,
}

impl Monitor {
    /// Status string suitable for table output. Falls back to `-` when
    /// Datadog omits `overall_state`.
    #[must_use]
    pub fn status(&self) -> &str {
        self.overall_state.as_deref().unwrap_or("-")
    }
}

impl JsonlSerialize for Monitor {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Pagination metadata returned by `GET /api/v1/monitor/search`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MonitorSearchMetadata {
    /// Zero-indexed page number returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<i64>,

    /// Number of items per page (Datadog calls this `per_page`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_page: Option<i64>,

    /// Total number of pages available for the query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_count: Option<i64>,

    /// Total number of monitors matching the query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_count: Option<i64>,
}

/// A single hit in `GET /api/v1/monitor/search`.
///
/// Schema differs from a full [`Monitor`] (notably `status` is uppercase
/// like `ALERT` rather than the mixed-case `overall_state`); the search
/// envelope is intentionally a separate type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MonitorSearchItem {
    /// Datadog monitor identifier.
    pub id: i64,

    /// Human-readable monitor name.
    pub name: String,

    /// Aggregated state (e.g. `OK`, `ALERT`, `NO DATA`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,

    /// Tags applied to the monitor.
    #[serde(default)]
    pub tags: Vec<String>,

    /// Monitor type.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub monitor_type: Option<String>,

    /// Monitor query expression.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,

    /// Most recent trigger time, in epoch milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_triggered_ts: Option<i64>,

    /// Creator of the monitor (raw object).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creator: Option<serde_json::Value>,
}

impl MonitorSearchItem {
    /// Status string suitable for table output. Falls back to `-` when
    /// Datadog omits `status`.
    #[must_use]
    pub fn status_label(&self) -> &str {
        self.status.as_deref().unwrap_or("-")
    }
}

/// Response from `GET /api/v1/monitor/search`.
///
/// `counts` is opaque: Datadog returns nested faceted counters whose
/// shape varies by query, so it's preserved as `serde_json::Value`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MonitorSearchResult {
    /// Monitors matching the search query.
    #[serde(default)]
    pub monitors: Vec<MonitorSearchItem>,

    /// Faceted counters returned by Datadog (raw object, optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counts: Option<serde_json::Value>,

    /// Pagination metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<MonitorSearchMetadata>,
}

impl JsonlSerialize for MonitorSearchResult {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_items_jsonl(self.monitors.iter(), out)
    }
}

/// One row of `GET /api/v1/dashboard`'s `dashboards` array.
///
/// Datadog identifies dashboards by an opaque string (e.g. `abc-def-ghi`),
/// not a numeric id. Optional fields are preserved as `Option<_>` so
/// JSON / YAML output never invents a value the API didn't return.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DashboardSummary {
    /// Datadog dashboard identifier.
    pub id: String,

    /// Human-readable title.
    pub title: String,

    /// Login of the dashboard's author.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_handle: Option<String>,

    /// Web UI URL relative to the Datadog site (e.g. `/dashboard/abc-def-ghi`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    /// ISO 8601 last-modified timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_at: Option<String>,

    /// ISO 8601 creation timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,

    /// Optional dashboard description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Whether the dashboard is shared with the wider organisation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_shared: Option<bool>,

    /// Whether the dashboard cannot be edited via the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_read_only: Option<bool>,

    /// Layout type as reported by Datadog (`ordered` or `free`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout_type: Option<String>,
}

impl DashboardSummary {
    /// Author handle for table output. Falls back to `-` when Datadog
    /// omits the field.
    #[must_use]
    pub fn author_label(&self) -> &str {
        self.author_handle.as_deref().unwrap_or("-")
    }

    /// URL string for table output. Falls back to `-` when Datadog
    /// omits the field.
    #[must_use]
    pub fn url_label(&self) -> &str {
        self.url.as_deref().unwrap_or("-")
    }
}

impl JsonlSerialize for DashboardSummary {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Envelope returned by `GET /api/v1/dashboard`.
///
/// Datadog returns *all* dashboards in this single response — no
/// server-side pagination — so the wrapper is a thin newtype kept only
/// to mirror the on-the-wire shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DashboardListResponse {
    /// Dashboards returned by the API.
    #[serde(default)]
    pub dashboards: Vec<DashboardSummary>,
}

/// A Datadog dashboard returned by `GET /api/v1/dashboard/{id}`.
///
/// `widgets` is preserved as a raw `serde_json::Value` because the per-
/// widget schemas are deeply heterogeneous (timeseries, query value,
/// note, group, log stream, …) — modelling each variant would explode
/// the type surface for no CLI gain. This mirrors how the Atlassian
/// integration treats ADF documents.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Dashboard {
    /// Datadog dashboard identifier.
    pub id: String,

    /// Human-readable title.
    pub title: String,

    /// Optional dashboard description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Web UI URL relative to the Datadog site.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    /// Login of the dashboard's author.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_handle: Option<String>,

    /// ISO 8601 creation timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,

    /// ISO 8601 last-modified timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_at: Option<String>,

    /// Layout type as reported by Datadog (`ordered` or `free`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout_type: Option<String>,

    /// Whether the dashboard cannot be edited via the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_read_only: Option<bool>,

    /// Reflow type for `ordered` dashboards (`auto` or `fixed`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reflow_type: Option<String>,

    /// Notification list for the dashboard (raw value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_list: Option<serde_json::Value>,

    /// Template variables (raw value — schemas vary by variable type).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_variables: Option<serde_json::Value>,

    /// Widget definitions. Preserved as raw JSON; see type docs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub widgets: Option<serde_json::Value>,
}

impl Dashboard {
    /// Author handle for table output. Falls back to `-` when Datadog
    /// omits the field.
    #[must_use]
    pub fn author_label(&self) -> &str {
        self.author_handle.as_deref().unwrap_or("-")
    }

    /// URL string for table output. Falls back to `-` when Datadog
    /// omits the field.
    #[must_use]
    pub fn url_label(&self) -> &str {
        self.url.as_deref().unwrap_or("-")
    }
}

impl JsonlSerialize for Dashboard {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_scalar_jsonl(self, out)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn sample_response_json() -> serde_json::Value {
        serde_json::json!({
            "status": "ok",
            "from_date": 1_700_000_000_000_i64,
            "to_date":   1_700_000_030_000_i64,
            "series": [
                {
                    "metric": "avg:system.cpu.user{*}",
                    "display_name": "avg:system.cpu.user{*}",
                    "scope": "host:*",
                    "expression": "avg:system.cpu.user{*}",
                    "pointlist": [
                        [1_700_000_000_000_i64, 0.5_f64],
                        [1_700_000_015_000_i64, null],
                        [1_700_000_030_000_i64, 0.6_f64]
                    ],
                    "length": 3,
                    "unit": [],
                    "attributes": {}
                }
            ]
        })
    }

    #[test]
    fn deserialize_strips_unknown_fields() {
        let resp: MetricQueryResponse = serde_json::from_value(sample_response_json()).unwrap();
        assert_eq!(resp.status, "ok");
        assert_eq!(resp.series.len(), 1);
        let series = &resp.series[0];
        assert_eq!(series.metric, "avg:system.cpu.user{*}");
        assert_eq!(series.pointlist.len(), 3);
        assert_eq!(series.pointlist[1].1, None);
        assert_eq!(series.pointlist[2].1, Some(0.6));
    }

    #[test]
    fn series_defaults_are_applied() {
        let value = serde_json::json!({
            "status": "ok",
            "from_date": 0_i64,
            "to_date":   0_i64,
            "series": [{"metric": "m"}]
        });
        let resp: MetricQueryResponse = serde_json::from_value(value).unwrap();
        assert_eq!(resp.series[0].metric, "m");
        assert!(resp.series[0].pointlist.is_empty());
        assert_eq!(resp.series[0].display_name, None);
    }

    #[test]
    fn series_label_prefers_display_name() {
        let s = MetricSeries {
            metric: "m".into(),
            display_name: Some("d".into()),
            scope: None,
            expression: Some("e".into()),
            pointlist: vec![],
        };
        assert_eq!(s.label(), "d");
    }

    #[test]
    fn series_label_falls_back_to_expression_then_metric() {
        let s = MetricSeries {
            metric: "m".into(),
            display_name: None,
            scope: None,
            expression: Some("e".into()),
            pointlist: vec![],
        };
        assert_eq!(s.label(), "e");

        let s = MetricSeries {
            metric: "m".into(),
            display_name: None,
            scope: None,
            expression: None,
            pointlist: vec![],
        };
        assert_eq!(s.label(), "m");
    }

    #[test]
    fn metric_query_response_jsonl_emits_one_object_per_call() {
        let resp: MetricQueryResponse = serde_json::from_value(sample_response_json()).unwrap();
        let mut buf = Vec::new();
        resp.write_jsonl(&mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.ends_with('\n'));
        assert_eq!(out.matches('\n').count(), 1);
        let value: serde_json::Value = serde_json::from_str(out.trim_end()).unwrap();
        assert_eq!(value["status"], "ok");
    }

    #[test]
    fn metric_query_response_roundtrips_through_json() {
        let resp: MetricQueryResponse = serde_json::from_value(sample_response_json()).unwrap();
        let json = serde_json::to_string(&resp).unwrap();
        let roundtripped: MetricQueryResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, roundtripped);
    }

    // ── Monitor ────────────────────────────────────────────────────

    fn sample_monitor_json() -> serde_json::Value {
        serde_json::json!({
            "id": 12345_i64,
            "name": "CPU high",
            "type": "metric alert",
            "query": "avg(last_5m):avg:system.cpu.user{*} > 90",
            "message": "Notify @ops",
            "tags": ["team:sre", "env:prod"],
            "overall_state": "OK",
            "created": "2024-01-01T00:00:00.000Z",
            "modified": "2024-02-01T00:00:00.000Z",
            "priority": 2_i64,
            "multi": true,
            "creator": {"name": "Alice", "email": "alice@example.com"},
            "options": {"notify_no_data": true, "no_data_timeframe": 10},
            "deleted": null,
            "matching_downtimes": []
        })
    }

    #[test]
    fn monitor_deserialize_strips_unknown_fields_and_renames_type() {
        let m: Monitor = serde_json::from_value(sample_monitor_json()).unwrap();
        assert_eq!(m.id, 12345);
        assert_eq!(m.name, "CPU high");
        assert_eq!(m.monitor_type, "metric alert");
        assert_eq!(m.tags, vec!["team:sre", "env:prod"]);
        assert_eq!(m.overall_state.as_deref(), Some("OK"));
        assert_eq!(m.priority, Some(2));
        assert_eq!(m.multi, Some(true));
        assert!(m.creator.is_some());
        assert!(m.options.is_some());
    }

    #[test]
    fn monitor_defaults_when_optional_fields_missing() {
        let value = serde_json::json!({
            "id": 1_i64,
            "name": "n",
            "type": "metric alert",
            "query": "q"
        });
        let m: Monitor = serde_json::from_value(value).unwrap();
        assert!(m.tags.is_empty());
        assert_eq!(m.overall_state, None);
        assert_eq!(m.message, None);
        assert_eq!(m.priority, None);
        assert_eq!(m.multi, None);
        assert!(m.creator.is_none());
        assert!(m.options.is_none());
    }

    #[test]
    fn monitor_status_falls_back_to_dash() {
        let m = Monitor {
            id: 1,
            name: "n".into(),
            monitor_type: "metric alert".into(),
            query: "q".into(),
            message: None,
            tags: vec![],
            overall_state: None,
            created: None,
            modified: None,
            priority: None,
            multi: None,
            creator: None,
            options: None,
        };
        assert_eq!(m.status(), "-");
    }

    #[test]
    fn monitor_status_returns_overall_state_when_present() {
        let m = Monitor {
            id: 1,
            name: "n".into(),
            monitor_type: "metric alert".into(),
            query: "q".into(),
            message: None,
            tags: vec![],
            overall_state: Some("Alert".into()),
            created: None,
            modified: None,
            priority: None,
            multi: None,
            creator: None,
            options: None,
        };
        assert_eq!(m.status(), "Alert");
    }

    #[test]
    fn monitor_jsonl_emits_one_line_per_call() {
        let m: Monitor = serde_json::from_value(sample_monitor_json()).unwrap();
        let mut buf = Vec::new();
        m.write_jsonl(&mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(out.matches('\n').count(), 1);
        let v: serde_json::Value = serde_json::from_str(out.trim_end()).unwrap();
        assert_eq!(v["id"], 12345);
        assert_eq!(v["type"], "metric alert");
    }

    #[test]
    fn monitor_roundtrips_through_json() {
        let m: Monitor = serde_json::from_value(sample_monitor_json()).unwrap();
        let json = serde_json::to_string(&m).unwrap();
        let m2: Monitor = serde_json::from_str(&json).unwrap();
        assert_eq!(m, m2);
    }

    // ── MonitorSearchResult / Item ─────────────────────────────────

    fn sample_search_json() -> serde_json::Value {
        serde_json::json!({
            "monitors": [
                {
                    "id": 1_i64,
                    "name": "Disk full",
                    "status": "ALERT",
                    "tags": ["team:sre"],
                    "type": "metric alert",
                    "query": "avg(last_1h):avg:system.disk.in_use{*} > 0.9",
                    "last_triggered_ts": 1_700_000_000_000_i64,
                    "creator": {"name": "Alice"}
                },
                {
                    "id": 2_i64,
                    "name": "Latency",
                    "tags": []
                }
            ],
            "counts": {"status": [{"name": "ALERT", "count": 1}]},
            "metadata": {
                "page": 0,
                "per_page": 30,
                "page_count": 1,
                "total_count": 2
            }
        })
    }

    #[test]
    fn monitor_search_result_deserializes_full_envelope() {
        let r: MonitorSearchResult = serde_json::from_value(sample_search_json()).unwrap();
        assert_eq!(r.monitors.len(), 2);
        assert_eq!(r.monitors[0].id, 1);
        assert_eq!(r.monitors[0].status.as_deref(), Some("ALERT"));
        assert_eq!(r.monitors[0].monitor_type.as_deref(), Some("metric alert"));
        assert_eq!(r.monitors[0].last_triggered_ts, Some(1_700_000_000_000));
        assert_eq!(r.monitors[1].status, None);
        assert!(r.monitors[1].tags.is_empty());
        assert!(r.counts.is_some());
        let meta = r.metadata.as_ref().unwrap();
        assert_eq!(meta.total_count, Some(2));
        assert_eq!(meta.page, Some(0));
    }

    #[test]
    fn monitor_search_result_defaults_when_optional_fields_missing() {
        let r: MonitorSearchResult = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(r.monitors.is_empty());
        assert!(r.counts.is_none());
        assert!(r.metadata.is_none());
    }

    #[test]
    fn monitor_search_item_status_label_falls_back_to_dash() {
        let item = MonitorSearchItem {
            id: 1,
            name: "n".into(),
            status: None,
            tags: vec![],
            monitor_type: None,
            query: None,
            last_triggered_ts: None,
            creator: None,
        };
        assert_eq!(item.status_label(), "-");
    }

    #[test]
    fn monitor_search_item_status_label_returns_status_when_present() {
        let item = MonitorSearchItem {
            id: 1,
            name: "n".into(),
            status: Some("OK".into()),
            tags: vec![],
            monitor_type: None,
            query: None,
            last_triggered_ts: None,
            creator: None,
        };
        assert_eq!(item.status_label(), "OK");
    }

    #[test]
    fn monitor_search_result_jsonl_emits_one_line_per_monitor() {
        let r: MonitorSearchResult = serde_json::from_value(sample_search_json()).unwrap();
        let mut buf = Vec::new();
        r.write_jsonl(&mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(out.matches('\n').count(), 2);
        let lines: Vec<&str> = out.lines().collect();
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["id"], 1);
        assert_eq!(first["status"], "ALERT");
        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["id"], 2);
    }

    #[test]
    fn monitor_search_result_jsonl_empty_monitors_emits_nothing() {
        let r = MonitorSearchResult::default();
        let mut buf = Vec::new();
        r.write_jsonl(&mut buf).unwrap();
        assert!(buf.is_empty());
    }

    #[test]
    fn monitor_search_result_roundtrips_through_json() {
        let r: MonitorSearchResult = serde_json::from_value(sample_search_json()).unwrap();
        let json = serde_json::to_string(&r).unwrap();
        let r2: MonitorSearchResult = serde_json::from_str(&json).unwrap();
        assert_eq!(r, r2);
    }

    // ── Dashboard / DashboardSummary ───────────────────────────────

    fn sample_dashboard_summary_json() -> serde_json::Value {
        serde_json::json!({
            "id": "abc-def-ghi",
            "title": "Service Overview",
            "author_handle": "alice@example.com",
            "url": "/dashboard/abc-def-ghi/service-overview",
            "modified_at": "2024-02-01T00:00:00.000Z",
            "created_at": "2024-01-01T00:00:00.000Z",
            "description": "Top-level service health.",
            "is_shared": true,
            "is_read_only": false,
            "layout_type": "ordered",
            "deleted": null
        })
    }

    fn sample_dashboard_json() -> serde_json::Value {
        serde_json::json!({
            "id": "abc-def-ghi",
            "title": "Service Overview",
            "description": "Top-level service health.",
            "url": "/dashboard/abc-def-ghi",
            "author_handle": "alice@example.com",
            "created_at": "2024-01-01T00:00:00.000Z",
            "modified_at": "2024-02-01T00:00:00.000Z",
            "layout_type": "ordered",
            "is_read_only": false,
            "reflow_type": "auto",
            "notify_list": [],
            "template_variables": [
                {"name": "env", "default": "prod"}
            ],
            "widgets": [
                {"id": 1, "definition": {"type": "note", "content": "hello"}}
            ],
            "extra_unknown": "ignored"
        })
    }

    #[test]
    fn dashboard_summary_deserializes_full_payload() {
        let s: DashboardSummary = serde_json::from_value(sample_dashboard_summary_json()).unwrap();
        assert_eq!(s.id, "abc-def-ghi");
        assert_eq!(s.title, "Service Overview");
        assert_eq!(s.author_handle.as_deref(), Some("alice@example.com"));
        assert_eq!(
            s.url.as_deref(),
            Some("/dashboard/abc-def-ghi/service-overview")
        );
        assert_eq!(s.is_shared, Some(true));
        assert_eq!(s.is_read_only, Some(false));
        assert_eq!(s.layout_type.as_deref(), Some("ordered"));
    }

    #[test]
    fn dashboard_summary_defaults_when_optional_fields_missing() {
        let s: DashboardSummary = serde_json::from_value(serde_json::json!({
            "id": "x",
            "title": "y"
        }))
        .unwrap();
        assert_eq!(s.author_handle, None);
        assert_eq!(s.url, None);
        assert_eq!(s.is_shared, None);
        assert_eq!(s.author_label(), "-");
        assert_eq!(s.url_label(), "-");
    }

    #[test]
    fn dashboard_summary_labels_use_present_fields() {
        let s = DashboardSummary {
            id: "x".into(),
            title: "y".into(),
            author_handle: Some("alice".into()),
            url: Some("/u".into()),
            modified_at: None,
            created_at: None,
            description: None,
            is_shared: None,
            is_read_only: None,
            layout_type: None,
        };
        assert_eq!(s.author_label(), "alice");
        assert_eq!(s.url_label(), "/u");
    }

    #[test]
    fn dashboard_summary_jsonl_emits_one_line_per_call() {
        let s: DashboardSummary = serde_json::from_value(sample_dashboard_summary_json()).unwrap();
        let mut buf = Vec::new();
        s.write_jsonl(&mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(out.matches('\n').count(), 1);
        let v: serde_json::Value = serde_json::from_str(out.trim_end()).unwrap();
        assert_eq!(v["id"], "abc-def-ghi");
    }

    #[test]
    fn dashboard_summary_roundtrips_through_json() {
        let s: DashboardSummary = serde_json::from_value(sample_dashboard_summary_json()).unwrap();
        let json = serde_json::to_string(&s).unwrap();
        let s2: DashboardSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(s, s2);
    }

    #[test]
    fn dashboard_list_response_deserializes_envelope() {
        let r: DashboardListResponse = serde_json::from_value(serde_json::json!({
            "dashboards": [
                sample_dashboard_summary_json(),
                {"id": "zzz", "title": "Other"}
            ]
        }))
        .unwrap();
        assert_eq!(r.dashboards.len(), 2);
        assert_eq!(r.dashboards[1].id, "zzz");
    }

    #[test]
    fn dashboard_list_response_defaults_to_empty() {
        let r: DashboardListResponse = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(r.dashboards.is_empty());
    }

    #[test]
    fn dashboard_deserializes_full_payload_and_strips_unknowns() {
        let d: Dashboard = serde_json::from_value(sample_dashboard_json()).unwrap();
        assert_eq!(d.id, "abc-def-ghi");
        assert_eq!(d.title, "Service Overview");
        assert_eq!(d.layout_type.as_deref(), Some("ordered"));
        assert_eq!(d.reflow_type.as_deref(), Some("auto"));
        assert!(d.widgets.is_some());
        assert!(d.template_variables.is_some());
        assert!(d.notify_list.is_some());
    }

    #[test]
    fn dashboard_defaults_when_optional_fields_missing() {
        let d: Dashboard = serde_json::from_value(serde_json::json!({
            "id": "x",
            "title": "y"
        }))
        .unwrap();
        assert!(d.widgets.is_none());
        assert!(d.notify_list.is_none());
        assert!(d.template_variables.is_none());
        assert_eq!(d.author_label(), "-");
        assert_eq!(d.url_label(), "-");
    }

    #[test]
    fn dashboard_labels_use_present_fields() {
        let d = Dashboard {
            id: "x".into(),
            title: "y".into(),
            description: None,
            url: Some("/u".into()),
            author_handle: Some("alice".into()),
            created_at: None,
            modified_at: None,
            layout_type: None,
            is_read_only: None,
            reflow_type: None,
            notify_list: None,
            template_variables: None,
            widgets: None,
        };
        assert_eq!(d.author_label(), "alice");
        assert_eq!(d.url_label(), "/u");
    }

    #[test]
    fn dashboard_jsonl_emits_one_line_per_call() {
        let d: Dashboard = serde_json::from_value(sample_dashboard_json()).unwrap();
        let mut buf = Vec::new();
        d.write_jsonl(&mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(out.matches('\n').count(), 1);
        let v: serde_json::Value = serde_json::from_str(out.trim_end()).unwrap();
        assert_eq!(v["id"], "abc-def-ghi");
    }

    #[test]
    fn dashboard_roundtrips_through_json() {
        let d: Dashboard = serde_json::from_value(sample_dashboard_json()).unwrap();
        let json = serde_json::to_string(&d).unwrap();
        let d2: Dashboard = serde_json::from_str(&json).unwrap();
        assert_eq!(d, d2);
    }
}
