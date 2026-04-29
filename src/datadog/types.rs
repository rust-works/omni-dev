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

/// Sort order for `POST /api/v2/logs/events/search`.
///
/// Datadog encodes the order on the wire as the field name optionally
/// prefixed with `-` for descending. The CLI exposes the friendlier
/// `timestamp-asc` / `timestamp-desc` value names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortOrder {
    /// Oldest first — wire form `timestamp`.
    TimestampAsc,
    /// Newest first — wire form `-timestamp`.
    TimestampDesc,
}

impl SortOrder {
    /// Returns the wire representation used by the v2 logs API.
    #[must_use]
    pub fn as_api_str(self) -> &'static str {
        match self {
            Self::TimestampAsc => "timestamp",
            Self::TimestampDesc => "-timestamp",
        }
    }
}

impl Serialize for SortOrder {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_api_str())
    }
}

impl<'de> Deserialize<'de> for SortOrder {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "timestamp" => Ok(Self::TimestampAsc),
            "-timestamp" => Ok(Self::TimestampDesc),
            other => Err(serde::de::Error::custom(format!(
                "unknown sort order: {other}"
            ))),
        }
    }
}

/// Attributes payload of a log event returned by `POST /api/v2/logs/events/search`.
///
/// Datadog wraps each event in a `{ id, type, attributes }` envelope. Only
/// the fields needed by the table renderer are surfaced as named fields;
/// callers that need the full event attribute map can re-fetch through
/// `-o json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogEventAttributes {
    /// Event timestamp as Datadog returns it (typically RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,

    /// Service name reported by the log producer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,

    /// Log status (e.g. `info`, `warn`, `error`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,

    /// Originating host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,

    /// Free-form log message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,

    /// Tags applied to the event.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// A single log event hit returned by `POST /api/v2/logs/events/search`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogEvent {
    /// Datadog event identifier.
    pub id: String,

    /// Resource type marker — Datadog returns the literal string `"log"`.
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub event_type: Option<String>,

    /// Event payload.
    #[serde(default)]
    pub attributes: LogEventAttributes,
}

impl LogEvent {
    /// Timestamp string suitable for table output. Falls back to `-`
    /// when Datadog omits the field.
    #[must_use]
    pub fn timestamp_label(&self) -> &str {
        self.attributes.timestamp.as_deref().unwrap_or("-")
    }

    /// Service string suitable for table output. Falls back to `-`
    /// when Datadog omits the field.
    #[must_use]
    pub fn service_label(&self) -> &str {
        self.attributes.service.as_deref().unwrap_or("-")
    }

    /// Status string suitable for table output. Falls back to `-`
    /// when Datadog omits the field.
    #[must_use]
    pub fn status_label(&self) -> &str {
        self.attributes.status.as_deref().unwrap_or("-")
    }

    /// Message string suitable for table output. Falls back to an
    /// empty string when Datadog omits the field.
    #[must_use]
    pub fn message_label(&self) -> &str {
        self.attributes.message.as_deref().unwrap_or("")
    }
}

impl JsonlSerialize for LogEvent {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Cursor pagination block returned by `POST /api/v2/logs/events/search`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogSearchPage {
    /// Cursor token for the next page; absent when no further pages exist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
}

/// Search-level metadata returned by `POST /api/v2/logs/events/search`.
///
/// Datadog returns additional fields (`elapsed`, `request_id`, `warnings`,
/// `status`) whose shapes vary; they're preserved as raw `serde_json::Value`
/// so JSON / YAML output round-trips unchanged.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogSearchMeta {
    /// Cursor pagination block (absent when no further pages exist).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<LogSearchPage>,

    /// Search status reported by Datadog (e.g. `done`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,

    /// Elapsed query time as reported by Datadog, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed: Option<i64>,

    /// Datadog request id; useful for support escalations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,

    /// Optional non-fatal warnings emitted by the search.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warnings: Option<serde_json::Value>,
}

/// Response from `POST /api/v2/logs/events/search`.
///
/// Phase 1 ships single-page only; the cursor token is preserved on
/// `meta.page.after` so a future Phase 2 follow-up can iterate without
/// changing the wire types.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogSearchResult {
    /// Events returned by the API.
    #[serde(default)]
    pub data: Vec<LogEvent>,

    /// Pagination + status metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<LogSearchMeta>,
}

impl JsonlSerialize for LogSearchResult {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_items_jsonl(self.data.iter(), out)
    }
}

// ── Phase 2: events ────────────────────────────────────────────────

/// Attributes payload of an event returned by `GET /api/v2/events`.
///
/// Datadog wraps each event in a `{ id, type, attributes }` envelope. The
/// `attributes` block in turn contains a nested `attributes` map plus
/// flat fields like `tags`, `timestamp`, and `service`. Only the fields
/// the CLI uses for table rendering are surfaced as named fields; the
/// rest round-trips through `extra` so JSON / YAML output stays lossless.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventAttributes {
    /// Event timestamp as Datadog returns it (RFC 3339 string).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,

    /// Event title (often the headline shown in the Datadog UI).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,

    /// Free-form event body / description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,

    /// Source name reported by the event producer (e.g. `aws`, `kubernetes`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,

    /// Service emitting the event (when applicable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,

    /// Originating host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,

    /// Event status (`info`, `warning`, `error`, `success`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,

    /// Aggregation key — events sharing one collapse in the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aggregation_key: Option<String>,

    /// Tags applied to the event.
    #[serde(default)]
    pub tags: Vec<String>,

    /// Nested per-source attributes Datadog returns under `attributes.attributes`.
    /// Preserved as raw JSON because the schema varies by event type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attributes: Option<serde_json::Value>,
}

/// A single event hit returned by `GET /api/v2/events`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Event {
    /// Datadog event identifier.
    pub id: String,

    /// Resource type marker — Datadog returns the literal string `"event"`.
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub event_type: Option<String>,

    /// Event payload.
    #[serde(default)]
    pub attributes: EventAttributes,
}

impl Event {
    /// Timestamp string for table output. Falls back to `-` when unset.
    #[must_use]
    pub fn timestamp_label(&self) -> &str {
        self.attributes.timestamp.as_deref().unwrap_or("-")
    }

    /// Title string for table output. Falls back to `-` when unset.
    #[must_use]
    pub fn title_label(&self) -> &str {
        self.attributes.title.as_deref().unwrap_or("-")
    }

    /// Source string for table output. Falls back to `-` when unset.
    #[must_use]
    pub fn source_label(&self) -> &str {
        self.attributes.source.as_deref().unwrap_or("-")
    }

    /// Host string for table output. Falls back to `-` when unset.
    #[must_use]
    pub fn host_label(&self) -> &str {
        self.attributes.host.as_deref().unwrap_or("-")
    }
}

impl JsonlSerialize for Event {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Cursor pagination block returned by `GET /api/v2/events`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventsPage {
    /// Cursor token for the next page; absent when no further pages exist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
}

/// Search-level metadata returned by `GET /api/v2/events`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventsMeta {
    /// Cursor pagination block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<EventsPage>,

    /// Search status reported by Datadog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,

    /// Datadog request id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,

    /// Elapsed query time as reported by Datadog, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed: Option<i64>,

    /// Optional non-fatal warnings emitted by the search.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warnings: Option<serde_json::Value>,
}

/// Response from `GET /api/v2/events`.
///
/// Phase 2 ships single-page only; the cursor token is preserved on
/// `meta.page.after` for callers that need to iterate manually.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventsResponse {
    /// Events returned by the API.
    #[serde(default)]
    pub data: Vec<Event>,

    /// Pagination + status metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<EventsMeta>,

    /// Cursor / self link block (preserved as raw JSON for round-trip).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub links: Option<serde_json::Value>,
}

impl JsonlSerialize for EventsResponse {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_items_jsonl(self.data.iter(), out)
    }
}

// ── Phase 2: SLOs ──────────────────────────────────────────────────

/// A Datadog Service Level Objective as returned by `GET /api/v1/slo`
/// and `GET /api/v1/slo/{id}`.
///
/// The `query` and `thresholds` shapes vary by SLO type (metric / monitor
/// / time-slice), so they're preserved as raw `serde_json::Value` to keep
/// JSON / YAML output lossless without pulling Datadog's variant schemas
/// into the type surface.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Slo {
    /// Datadog SLO identifier.
    pub id: String,

    /// Human-readable name.
    pub name: String,

    /// SLO type as reported by Datadog (`metric`, `monitor`, `time_slice`).
    #[serde(rename = "type")]
    pub slo_type: String,

    /// Query specification (raw — schema differs per SLO type).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<serde_json::Value>,

    /// Target threshold definitions (raw — list of objects).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thresholds: Option<serde_json::Value>,

    /// Tags applied to the SLO.
    #[serde(default)]
    pub tags: Vec<String>,

    /// Underlying monitor ids (for monitor SLOs).
    #[serde(default)]
    pub monitor_ids: Vec<i64>,

    /// Tags propagated from underlying monitors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor_tags: Option<Vec<String>>,

    /// Optional description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Creation timestamp (Datadog returns Unix epoch seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,

    /// Last-modified timestamp (Datadog returns Unix epoch seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_at: Option<i64>,

    /// Creator (raw object).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creator: Option<serde_json::Value>,

    /// Optional grouping facets (raw — present for multi-group SLOs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub groups: Option<serde_json::Value>,

    /// Configured alert ids (raw — list of integers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configured_alert_ids: Option<serde_json::Value>,
}

impl JsonlSerialize for Slo {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Response envelope for `GET /api/v1/slo`.
///
/// `errors` is populated when Datadog rejects part of the request; the
/// list façade surfaces those as a hard failure.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SloListResponse {
    /// SLOs returned by the API.
    #[serde(default)]
    pub data: Vec<Slo>,

    /// Optional non-fatal error list (raw — populated under partial failure).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<serde_json::Value>,

    /// Optional non-fatal error list (per Datadog's plural variant).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errors: Option<Vec<String>>,
}

/// Response envelope for `GET /api/v1/slo/{id}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SloGetResponse {
    /// The SLO returned by the API.
    pub data: Slo,

    /// Optional non-fatal error block (raw).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<serde_json::Value>,

    /// Optional non-fatal error list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errors: Option<Vec<String>>,
}

// ── Phase 2: hosts ─────────────────────────────────────────────────

/// A reporting host as returned by `GET /api/v1/hosts`.
///
/// Datadog returns dozens of fields per host; only the ones surfaced by
/// the table renderer are typed. `meta`, `metrics`, and `tags_by_source`
/// are preserved as raw JSON for `-o json/yaml` output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Host {
    /// Hostname as reported by the agent.
    pub name: String,

    /// Alternate names (e.g. EC2 instance id, FQDN).
    #[serde(default)]
    pub aliases: Vec<String>,

    /// Apps (integrations) reporting on this host.
    #[serde(default)]
    pub apps: Vec<String>,

    /// Tag map keyed by source (raw — schema is `{ source: [tag, ...] }`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags_by_source: Option<serde_json::Value>,

    /// Whether the host is currently reporting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub up: Option<bool>,

    /// Last time the host reported, in Unix epoch seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reported_time: Option<i64>,

    /// Sources Datadog has data from (e.g. `agent`, `aws`).
    #[serde(default)]
    pub sources: Vec<String>,

    /// Whether the host is currently muted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_muted: Option<bool>,

    /// Optional mute timeout (epoch seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mute_timeout: Option<i64>,

    /// Datadog-internal numeric host id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,

    /// Reporting hostname (occasionally distinct from `name`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_name: Option<String>,

    /// Per-source meta block (raw).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,

    /// Per-host metrics block (raw).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<serde_json::Value>,
}

impl Host {
    /// `up` rendered as `yes` / `no` / `-` for the bespoke table view.
    #[must_use]
    pub fn up_label(&self) -> &'static str {
        match self.up {
            Some(true) => "yes",
            Some(false) => "no",
            None => "-",
        }
    }
}

impl JsonlSerialize for Host {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Response envelope for `GET /api/v1/hosts`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostsResponse {
    /// Hosts returned in the current page.
    #[serde(default)]
    pub host_list: Vec<Host>,

    /// Number of hosts in the current response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_returned: Option<i64>,

    /// Total number of hosts matching the query across all pages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_matching: Option<i64>,
}

impl JsonlSerialize for HostsResponse {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_items_jsonl(self.host_list.iter(), out)
    }
}

// ── Phase 2: downtimes ─────────────────────────────────────────────

/// A scheduled downtime as returned by `GET /api/v1/downtime`.
///
/// `recurrence` is preserved as raw JSON because Datadog encodes it as
/// either `null`, an object, or an array of objects depending on the
/// downtime kind.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Downtime {
    /// Datadog downtime identifier.
    pub id: i64,

    /// Scope tags the downtime applies to (e.g. `["env:prod"]`).
    #[serde(default)]
    pub scope: Vec<String>,

    /// Optional monitor tags filter.
    #[serde(default)]
    pub monitor_tags: Vec<String>,

    /// Start time (epoch seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<i64>,

    /// End time (epoch seconds). Absent for indefinite downtimes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<i64>,

    /// Notification message body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,

    /// Whether the downtime is currently active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<bool>,

    /// Whether the downtime has been disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,

    /// Underlying monitor id (for single-monitor downtimes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor_id: Option<i64>,

    /// Recurrence rule (raw — null / object / array).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recurrence: Option<serde_json::Value>,

    /// Creation timestamp (epoch seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<i64>,

    /// Last-modified timestamp (epoch seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<i64>,

    /// Creator user id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creator_id: Option<i64>,

    /// Parent downtime id (for child downtimes generated from a recurrence).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<i64>,

    /// Timezone for the downtime schedule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

impl Downtime {
    /// Joins `scope` tags with commas, falling back to `*` for empty
    /// scope (the convention Datadog's UI uses for "all").
    #[must_use]
    pub fn scope_label(&self) -> String {
        if self.scope.is_empty() {
            "*".to_string()
        } else {
            self.scope.join(",")
        }
    }

    /// Message for table output (single-line, falling back to `-`).
    #[must_use]
    pub fn message_label(&self) -> &str {
        self.message.as_deref().unwrap_or("-")
    }

    /// Monitor id for table output (formatted, fallback `-`).
    #[must_use]
    pub fn monitor_label(&self) -> String {
        self.monitor_id
            .map_or_else(|| "-".to_string(), |id| id.to_string())
    }
}

impl JsonlSerialize for Downtime {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_scalar_jsonl(self, out)
    }
}

// ── Phase 2: metric catalog ────────────────────────────────────────

/// Response from `GET /api/v1/metrics`.
///
/// Datadog returns a flat array of metric names. The optional `from`
/// echoes back the requested time anchor.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MetricCatalogResponse {
    /// Echo of the requested `from` (Unix epoch seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<i64>,

    /// Metric names returned by the API.
    #[serde(default)]
    pub metrics: Vec<String>,
}

impl JsonlSerialize for MetricCatalogResponse {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        for m in &self.metrics {
            write_scalar_jsonl(m, out)?;
        }
        Ok(())
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

    // ── SortOrder ──────────────────────────────────────────────────

    #[test]
    fn sort_order_as_api_str_uses_minus_for_desc() {
        assert_eq!(SortOrder::TimestampAsc.as_api_str(), "timestamp");
        assert_eq!(SortOrder::TimestampDesc.as_api_str(), "-timestamp");
    }

    #[test]
    fn sort_order_serializes_to_api_string() {
        assert_eq!(
            serde_json::to_value(SortOrder::TimestampAsc).unwrap(),
            serde_json::Value::String("timestamp".into())
        );
        assert_eq!(
            serde_json::to_value(SortOrder::TimestampDesc).unwrap(),
            serde_json::Value::String("-timestamp".into())
        );
    }

    #[test]
    fn sort_order_deserializes_known_values() {
        let asc: SortOrder = serde_json::from_value(serde_json::json!("timestamp")).unwrap();
        assert_eq!(asc, SortOrder::TimestampAsc);
        let desc: SortOrder = serde_json::from_value(serde_json::json!("-timestamp")).unwrap();
        assert_eq!(desc, SortOrder::TimestampDesc);
    }

    #[test]
    fn sort_order_rejects_unknown_value() {
        let err = serde_json::from_value::<SortOrder>(serde_json::json!("nope")).unwrap_err();
        assert!(err.to_string().contains("unknown sort order"));
    }

    #[test]
    fn sort_order_rejects_non_string_value() {
        // Exercises the `String::deserialize(...)?` propagation: a JSON
        // number can't be deserialised as a `String`, so the error short-
        // circuits before the match-on-content arm runs.
        let err = serde_json::from_value::<SortOrder>(serde_json::json!(42)).unwrap_err();
        // serde_json's error for "expected string, got number" mentions
        // the type name; we don't pin the exact wording.
        assert!(err.to_string().to_lowercase().contains("string"));
    }

    // ── LogEvent / LogSearchResult ─────────────────────────────────

    fn sample_log_search_json() -> serde_json::Value {
        serde_json::json!({
            "data": [
                {
                    "id": "AAAAAA",
                    "type": "log",
                    "attributes": {
                        "timestamp": "2026-04-22T10:00:00.000Z",
                        "service": "api",
                        "status": "info",
                        "host": "web-01",
                        "message": "request handled",
                        "tags": ["env:prod"]
                    }
                },
                {
                    "id": "BBBBBB",
                    "type": "log",
                    "attributes": {}
                }
            ],
            "meta": {
                "page": { "after": "next-cursor" },
                "status": "done",
                "elapsed": 23,
                "request_id": "req-1",
                "warnings": []
            }
        })
    }

    #[test]
    fn log_search_result_deserializes_full_envelope() {
        let r: LogSearchResult = serde_json::from_value(sample_log_search_json()).unwrap();
        assert_eq!(r.data.len(), 2);
        assert_eq!(r.data[0].id, "AAAAAA");
        assert_eq!(r.data[0].event_type.as_deref(), Some("log"));
        assert_eq!(
            r.data[0].attributes.timestamp.as_deref(),
            Some("2026-04-22T10:00:00.000Z")
        );
        assert_eq!(r.data[0].attributes.tags, vec!["env:prod"]);
        assert!(r.data[1].attributes.tags.is_empty());
        let meta = r.meta.as_ref().unwrap();
        assert_eq!(
            meta.page.as_ref().and_then(|p| p.after.as_deref()),
            Some("next-cursor")
        );
        assert_eq!(meta.status.as_deref(), Some("done"));
        assert_eq!(meta.elapsed, Some(23));
        assert_eq!(meta.request_id.as_deref(), Some("req-1"));
    }

    #[test]
    fn log_search_result_defaults_when_optional_fields_missing() {
        let r: LogSearchResult = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(r.data.is_empty());
        assert!(r.meta.is_none());
    }

    #[test]
    fn log_event_labels_fall_back_to_dash_or_empty() {
        let e = LogEvent {
            id: "x".into(),
            event_type: None,
            attributes: LogEventAttributes::default(),
        };
        assert_eq!(e.timestamp_label(), "-");
        assert_eq!(e.service_label(), "-");
        assert_eq!(e.status_label(), "-");
        assert_eq!(e.message_label(), "");
    }

    #[test]
    fn log_event_labels_use_present_fields() {
        let e = LogEvent {
            id: "x".into(),
            event_type: Some("log".into()),
            attributes: LogEventAttributes {
                timestamp: Some("t".into()),
                service: Some("s".into()),
                status: Some("info".into()),
                host: None,
                message: Some("hello".into()),
                tags: vec![],
            },
        };
        assert_eq!(e.timestamp_label(), "t");
        assert_eq!(e.service_label(), "s");
        assert_eq!(e.status_label(), "info");
        assert_eq!(e.message_label(), "hello");
    }

    #[test]
    fn log_search_result_jsonl_emits_one_line_per_event() {
        let r: LogSearchResult = serde_json::from_value(sample_log_search_json()).unwrap();
        let mut buf = Vec::new();
        r.write_jsonl(&mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(out.matches('\n').count(), 2);
        let lines: Vec<&str> = out.lines().collect();
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["id"], "AAAAAA");
    }

    #[test]
    fn log_search_result_jsonl_empty_data_emits_nothing() {
        let r = LogSearchResult::default();
        let mut buf = Vec::new();
        r.write_jsonl(&mut buf).unwrap();
        assert!(buf.is_empty());
    }

    #[test]
    fn log_event_jsonl_emits_one_line_per_call() {
        let r: LogSearchResult = serde_json::from_value(sample_log_search_json()).unwrap();
        let mut buf = Vec::new();
        r.data[0].write_jsonl(&mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(out.matches('\n').count(), 1);
        let v: serde_json::Value = serde_json::from_str(out.trim_end()).unwrap();
        assert_eq!(v["id"], "AAAAAA");
    }

    #[test]
    fn log_search_result_roundtrips_through_json() {
        let r: LogSearchResult = serde_json::from_value(sample_log_search_json()).unwrap();
        let json = serde_json::to_string(&r).unwrap();
        let r2: LogSearchResult = serde_json::from_str(&json).unwrap();
        assert_eq!(r, r2);
    }

    // ── Phase 2: Event / EventsResponse ────────────────────────────

    fn sample_events_json() -> serde_json::Value {
        serde_json::json!({
            "data": [
                {
                    "id": "EV1",
                    "type": "event",
                    "attributes": {
                        "timestamp": "2026-04-22T10:00:00.000Z",
                        "title": "Deploy",
                        "text": "shipped v1.2.3",
                        "source": "github",
                        "service": "api",
                        "host": "web-01",
                        "status": "success",
                        "aggregation_key": "deploy-1",
                        "tags": ["env:prod"],
                        "attributes": {"sha": "abc123"}
                    }
                },
                {
                    "id": "EV2",
                    "type": "event",
                    "attributes": {}
                }
            ],
            "meta": {
                "page": {"after": "next-cursor"},
                "status": "done",
                "request_id": "r-1",
                "elapsed": 7,
                "warnings": []
            },
            "links": {"self": "/api/v2/events"}
        })
    }

    #[test]
    fn events_response_deserializes_full_envelope() {
        let r: EventsResponse = serde_json::from_value(sample_events_json()).unwrap();
        assert_eq!(r.data.len(), 2);
        assert_eq!(r.data[0].id, "EV1");
        assert_eq!(r.data[0].event_type.as_deref(), Some("event"));
        assert_eq!(r.data[0].attributes.title.as_deref(), Some("Deploy"));
        assert_eq!(r.data[0].attributes.tags, vec!["env:prod"]);
        assert!(r.data[0].attributes.attributes.is_some());
        let meta = r.meta.as_ref().unwrap();
        assert_eq!(
            meta.page.as_ref().and_then(|p| p.after.as_deref()),
            Some("next-cursor")
        );
        assert_eq!(meta.elapsed, Some(7));
        assert_eq!(meta.request_id.as_deref(), Some("r-1"));
        assert!(r.links.is_some());
    }

    #[test]
    fn events_response_defaults_when_optional_fields_missing() {
        let r: EventsResponse = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(r.data.is_empty());
        assert!(r.meta.is_none());
        assert!(r.links.is_none());
    }

    #[test]
    fn event_labels_fall_back_to_dash() {
        let e = Event {
            id: "x".into(),
            event_type: None,
            attributes: EventAttributes::default(),
        };
        assert_eq!(e.timestamp_label(), "-");
        assert_eq!(e.title_label(), "-");
        assert_eq!(e.source_label(), "-");
        assert_eq!(e.host_label(), "-");
    }

    #[test]
    fn event_labels_use_present_fields() {
        let r: EventsResponse = serde_json::from_value(sample_events_json()).unwrap();
        let e = &r.data[0];
        assert_eq!(e.timestamp_label(), "2026-04-22T10:00:00.000Z");
        assert_eq!(e.title_label(), "Deploy");
        assert_eq!(e.source_label(), "github");
        assert_eq!(e.host_label(), "web-01");
    }

    #[test]
    fn events_response_jsonl_emits_one_line_per_event() {
        let r: EventsResponse = serde_json::from_value(sample_events_json()).unwrap();
        let mut buf = Vec::new();
        r.write_jsonl(&mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(out.matches('\n').count(), 2);
        let first: serde_json::Value = serde_json::from_str(out.lines().next().unwrap()).unwrap();
        assert_eq!(first["id"], "EV1");
    }

    #[test]
    fn event_jsonl_emits_one_line_per_call() {
        let r: EventsResponse = serde_json::from_value(sample_events_json()).unwrap();
        let mut buf = Vec::new();
        r.data[0].write_jsonl(&mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(out.matches('\n').count(), 1);
    }

    #[test]
    fn events_response_roundtrips_through_json() {
        let r: EventsResponse = serde_json::from_value(sample_events_json()).unwrap();
        let json = serde_json::to_string(&r).unwrap();
        let r2: EventsResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(r, r2);
    }

    // ── Phase 2: Slo / Slo*Response ────────────────────────────────

    fn sample_slo_json() -> serde_json::Value {
        serde_json::json!({
            "id": "abc-def",
            "name": "API latency p95",
            "type": "metric",
            "query": {"numerator": "sum:requests.success{*}.as_count()", "denominator": "sum:requests{*}.as_count()"},
            "thresholds": [{"timeframe": "30d", "target": 99.9}],
            "tags": ["team:sre"],
            "monitor_ids": [1_i64, 2_i64],
            "monitor_tags": ["severity:high"],
            "description": "Latency under 200ms",
            "created_at": 1_700_000_000_i64,
            "modified_at": 1_700_000_500_i64,
            "creator": {"name": "Alice"},
            "configured_alert_ids": [10_i64],
            "groups": ["env:prod"],
            "extra_unknown": "ignored"
        })
    }

    #[test]
    fn slo_deserializes_full_payload_and_strips_unknowns() {
        let s: Slo = serde_json::from_value(sample_slo_json()).unwrap();
        assert_eq!(s.id, "abc-def");
        assert_eq!(s.name, "API latency p95");
        assert_eq!(s.slo_type, "metric");
        assert_eq!(s.tags, vec!["team:sre"]);
        assert_eq!(s.monitor_ids, vec![1, 2]);
        assert_eq!(
            s.monitor_tags.as_deref(),
            Some(&["severity:high".to_string()][..])
        );
        assert!(s.query.is_some());
        assert!(s.thresholds.is_some());
        assert!(s.creator.is_some());
        assert!(s.configured_alert_ids.is_some());
        assert!(s.groups.is_some());
    }

    #[test]
    fn slo_defaults_when_optional_fields_missing() {
        let s: Slo = serde_json::from_value(serde_json::json!({
            "id": "x", "name": "y", "type": "monitor"
        }))
        .unwrap();
        assert!(s.tags.is_empty());
        assert!(s.monitor_ids.is_empty());
        assert!(s.query.is_none());
        assert!(s.thresholds.is_none());
        assert!(s.monitor_tags.is_none());
    }

    #[test]
    fn slo_jsonl_emits_one_line_per_call() {
        let s: Slo = serde_json::from_value(sample_slo_json()).unwrap();
        let mut buf = Vec::new();
        s.write_jsonl(&mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(out.matches('\n').count(), 1);
        let v: serde_json::Value = serde_json::from_str(out.trim_end()).unwrap();
        assert_eq!(v["id"], "abc-def");
        assert_eq!(v["type"], "metric");
    }

    #[test]
    fn slo_list_response_deserializes_envelope() {
        let r: SloListResponse = serde_json::from_value(serde_json::json!({
            "data": [sample_slo_json()],
            "errors": ["soft warning"]
        }))
        .unwrap();
        assert_eq!(r.data.len(), 1);
        assert_eq!(r.errors.as_deref(), Some(&["soft warning".to_string()][..]));
    }

    #[test]
    fn slo_list_response_defaults_to_empty() {
        let r: SloListResponse = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(r.data.is_empty());
        assert!(r.errors.is_none());
        assert!(r.error.is_none());
    }

    #[test]
    fn slo_get_response_deserializes_envelope() {
        let r: SloGetResponse = serde_json::from_value(serde_json::json!({
            "data": sample_slo_json()
        }))
        .unwrap();
        assert_eq!(r.data.id, "abc-def");
        assert!(r.errors.is_none());
    }

    #[test]
    fn slo_roundtrips_through_json() {
        let s: Slo = serde_json::from_value(sample_slo_json()).unwrap();
        let json = serde_json::to_string(&s).unwrap();
        let s2: Slo = serde_json::from_str(&json).unwrap();
        assert_eq!(s, s2);
    }

    // ── Phase 2: Host / HostsResponse ──────────────────────────────

    fn sample_host_json() -> serde_json::Value {
        serde_json::json!({
            "name": "web-01",
            "aliases": ["i-1234abcd", "web-01.example"],
            "apps": ["nginx", "ntp"],
            "tags_by_source": {"Datadog": ["env:prod"]},
            "up": true,
            "last_reported_time": 1_700_000_000_i64,
            "sources": ["agent", "aws"],
            "is_muted": false,
            "mute_timeout": null,
            "id": 99_i64,
            "host_name": "web-01.example",
            "meta": {"platform": "linux"},
            "metrics": {"load": 0.5}
        })
    }

    #[test]
    fn host_deserializes_full_payload() {
        let h: Host = serde_json::from_value(sample_host_json()).unwrap();
        assert_eq!(h.name, "web-01");
        assert_eq!(h.aliases.len(), 2);
        assert_eq!(h.apps, vec!["nginx", "ntp"]);
        assert_eq!(h.up, Some(true));
        assert_eq!(h.last_reported_time, Some(1_700_000_000));
        assert_eq!(h.sources, vec!["agent", "aws"]);
        assert_eq!(h.is_muted, Some(false));
        assert_eq!(h.id, Some(99));
        assert!(h.meta.is_some());
    }

    #[test]
    fn host_up_label_renders_yes_no_dash() {
        let mut h: Host = serde_json::from_value(sample_host_json()).unwrap();
        assert_eq!(h.up_label(), "yes");
        h.up = Some(false);
        assert_eq!(h.up_label(), "no");
        h.up = None;
        assert_eq!(h.up_label(), "-");
    }

    #[test]
    fn host_defaults_when_optional_fields_missing() {
        let h: Host = serde_json::from_value(serde_json::json!({"name": "x"})).unwrap();
        assert!(h.aliases.is_empty());
        assert!(h.apps.is_empty());
        assert!(h.up.is_none());
        assert_eq!(h.up_label(), "-");
    }

    #[test]
    fn host_jsonl_emits_one_line_per_call() {
        let h: Host = serde_json::from_value(sample_host_json()).unwrap();
        let mut buf = Vec::new();
        h.write_jsonl(&mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap().matches('\n').count(), 1);
    }

    #[test]
    fn hosts_response_deserializes_envelope() {
        let r: HostsResponse = serde_json::from_value(serde_json::json!({
            "host_list": [sample_host_json()],
            "total_returned": 1_i64,
            "total_matching": 1_i64
        }))
        .unwrap();
        assert_eq!(r.host_list.len(), 1);
        assert_eq!(r.total_returned, Some(1));
        assert_eq!(r.total_matching, Some(1));
    }

    #[test]
    fn hosts_response_defaults_to_empty() {
        let r: HostsResponse = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(r.host_list.is_empty());
        assert!(r.total_returned.is_none());
    }

    #[test]
    fn hosts_response_jsonl_emits_one_line_per_host() {
        let r: HostsResponse = serde_json::from_value(serde_json::json!({
            "host_list": [sample_host_json(), sample_host_json()],
            "total_returned": 2_i64,
            "total_matching": 2_i64
        }))
        .unwrap();
        let mut buf = Vec::new();
        r.write_jsonl(&mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap().matches('\n').count(), 2);
    }

    #[test]
    fn hosts_response_jsonl_empty_emits_nothing() {
        let r = HostsResponse::default();
        let mut buf = Vec::new();
        r.write_jsonl(&mut buf).unwrap();
        assert!(buf.is_empty());
    }

    // ── Phase 2: Downtime ──────────────────────────────────────────

    fn sample_downtime_json() -> serde_json::Value {
        serde_json::json!({
            "id": 12345_i64,
            "scope": ["env:prod", "team:sre"],
            "monitor_tags": ["severity:high"],
            "start": 1_700_000_000_i64,
            "end": 1_700_000_300_i64,
            "message": "Maintenance window",
            "active": true,
            "disabled": false,
            "monitor_id": 6789_i64,
            "recurrence": {"type": "weeks", "period": 1},
            "created": 1_699_999_000_i64,
            "modified": 1_699_999_500_i64,
            "creator_id": 42_i64,
            "parent_id": null,
            "timezone": "UTC"
        })
    }

    #[test]
    fn downtime_deserializes_full_payload() {
        let d: Downtime = serde_json::from_value(sample_downtime_json()).unwrap();
        assert_eq!(d.id, 12345);
        assert_eq!(d.scope, vec!["env:prod", "team:sre"]);
        assert_eq!(d.monitor_tags, vec!["severity:high"]);
        assert_eq!(d.message.as_deref(), Some("Maintenance window"));
        assert_eq!(d.active, Some(true));
        assert_eq!(d.monitor_id, Some(6789));
        assert!(d.recurrence.is_some());
        assert_eq!(d.timezone.as_deref(), Some("UTC"));
    }

    #[test]
    fn downtime_defaults_when_optional_fields_missing() {
        let d: Downtime = serde_json::from_value(serde_json::json!({
            "id": 1_i64
        }))
        .unwrap();
        assert!(d.scope.is_empty());
        assert!(d.monitor_tags.is_empty());
        assert!(d.message.is_none());
        assert!(d.recurrence.is_none());
    }

    #[test]
    fn downtime_scope_label_joins_or_falls_back_to_star() {
        let d1: Downtime = serde_json::from_value(sample_downtime_json()).unwrap();
        assert_eq!(d1.scope_label(), "env:prod,team:sre");
        let d2: Downtime = serde_json::from_value(serde_json::json!({"id": 1_i64})).unwrap();
        assert_eq!(d2.scope_label(), "*");
    }

    #[test]
    fn downtime_message_label_falls_back_to_dash() {
        let d: Downtime = serde_json::from_value(serde_json::json!({"id": 1_i64})).unwrap();
        assert_eq!(d.message_label(), "-");
        let d_with: Downtime =
            serde_json::from_value(serde_json::json!({"id": 1_i64, "message": "m"})).unwrap();
        assert_eq!(d_with.message_label(), "m");
    }

    #[test]
    fn downtime_monitor_label_renders_id_or_dash() {
        let d_with: Downtime =
            serde_json::from_value(serde_json::json!({"id": 1_i64, "monitor_id": 99_i64})).unwrap();
        assert_eq!(d_with.monitor_label(), "99");
        let d_without: Downtime = serde_json::from_value(serde_json::json!({"id": 1_i64})).unwrap();
        assert_eq!(d_without.monitor_label(), "-");
    }

    #[test]
    fn downtime_jsonl_emits_one_line_per_call() {
        let d: Downtime = serde_json::from_value(sample_downtime_json()).unwrap();
        let mut buf = Vec::new();
        d.write_jsonl(&mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap().matches('\n').count(), 1);
    }

    #[test]
    fn downtime_roundtrips_through_json() {
        let d: Downtime = serde_json::from_value(sample_downtime_json()).unwrap();
        let json = serde_json::to_string(&d).unwrap();
        let d2: Downtime = serde_json::from_str(&json).unwrap();
        assert_eq!(d, d2);
    }

    // ── Phase 2: MetricCatalogResponse ─────────────────────────────

    #[test]
    fn metric_catalog_response_deserializes_full_payload() {
        let r: MetricCatalogResponse = serde_json::from_value(serde_json::json!({
            "from": 1_700_000_000_i64,
            "metrics": ["system.cpu.user", "system.cpu.idle"]
        }))
        .unwrap();
        assert_eq!(r.from, Some(1_700_000_000));
        assert_eq!(r.metrics, vec!["system.cpu.user", "system.cpu.idle"]);
    }

    #[test]
    fn metric_catalog_response_defaults_to_empty() {
        let r: MetricCatalogResponse = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(r.from.is_none());
        assert!(r.metrics.is_empty());
    }

    #[test]
    fn metric_catalog_response_jsonl_emits_one_line_per_metric() {
        let r = MetricCatalogResponse {
            from: Some(0),
            metrics: vec!["a".into(), "b".into(), "c".into()],
        };
        let mut buf = Vec::new();
        r.write_jsonl(&mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "\"a\"\n\"b\"\n\"c\"\n");
    }

    #[test]
    fn metric_catalog_response_jsonl_empty_emits_nothing() {
        let r = MetricCatalogResponse::default();
        let mut buf = Vec::new();
        r.write_jsonl(&mut buf).unwrap();
        assert!(buf.is_empty());
    }
}
