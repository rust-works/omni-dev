//! Shared response types for the Datadog API.
//!
//! Populated in subsequent slices as endpoint families land. Kept as a
//! module now so `src/datadog.rs` declares a stable set of children.

use std::io::Write;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::cli::datadog::format::{write_scalar_jsonl, JsonlSerialize};

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
}
