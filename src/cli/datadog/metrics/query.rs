//! CLI command for Datadog metrics timeseries queries.

use std::collections::BTreeMap;
use std::io::Write;

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use clap::Parser;

use crate::cli::datadog::format::{output_as, OutputFormat};
use crate::datadog::client::DatadogClient;
use crate::datadog::metrics_api::MetricsApi;
use crate::datadog::time::parse_time_range;
use crate::datadog::types::{MetricQueryResponse, MetricSeries};

/// Executes a point-in-time metrics timeseries query.
#[derive(Parser)]
pub struct QueryCommand {
    /// Datadog query string (e.g. `avg:system.cpu.user{*}`).
    #[arg(long)]
    pub query: String,

    /// Start of the query window.
    ///
    /// Accepts relative shorthand (`15m`, `1h`, `7d`), the literal `now`,
    /// an RFC 3339 timestamp with timezone, or Unix epoch seconds.
    #[arg(long)]
    pub from: String,

    /// End of the query window. Defaults to `now` when omitted.
    ///
    /// Accepts the same forms as `--from`.
    #[arg(long)]
    pub to: Option<String>,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl QueryCommand {
    /// Runs the command against the shared client resolved by the parent
    /// `DatadogCommand::execute`. Taking the client as a parameter keeps this
    /// entry point free of process env and fully testable (issue #1030).
    pub async fn execute(self, client: &DatadogClient) -> Result<()> {
        let (from_ts, to_ts) = parse_time_range(&self.from, self.to.as_deref())?;
        run_query(client, &self.query, from_ts, to_ts, &self.output).await
    }
}

/// Fetches the query response and emits it in the requested format.
///
/// Split from [`QueryCommand::execute`] so tests can inject a wiremock
/// client without going through the credential-loading path.
async fn run_query(
    client: &DatadogClient,
    query: &str,
    from: i64,
    to: i64,
    output: &OutputFormat,
) -> Result<()> {
    let response = MetricsApi::new(client).point_query(query, from, to).await?;
    if output_as(&response, output)? {
        return Ok(());
    }
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    render_table(&response, &mut handle)
}

/// Renders the timeseries response as an aligned text table.
///
/// Column layout: a `TIMESTAMP` column followed by one column per series.
/// Timestamps are the union of every series' pointlist, sorted
/// ascending. Missing samples render as `-`.
pub(crate) fn render_table(response: &MetricQueryResponse, out: &mut dyn Write) -> Result<()> {
    if response.series.is_empty() {
        writeln!(out, "No series returned.").context("Failed to write empty-table message")?;
        return Ok(());
    }

    let timestamps = collect_timestamps(&response.series);
    let labels: Vec<String> = response
        .series
        .iter()
        .map(|s| s.label().to_string())
        .collect();
    let rendered_values = render_values(&response.series, &timestamps);

    let ts_label = format_timestamp(*timestamps.first().unwrap_or(&0.0));
    let ts_width = ts_label.len().max("TIMESTAMP".len());
    let col_widths: Vec<usize> = labels
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let data_max = rendered_values
                .iter()
                .map(|row| row[i].len())
                .max()
                .unwrap_or(0);
            label.len().max(data_max)
        })
        .collect();

    let header_cells: Vec<&str> = labels.iter().map(String::as_str).collect();
    write_row(out, "TIMESTAMP", ts_width, &header_cells, &col_widths)?;
    let ts_sep = "-".repeat(ts_width);
    let seps: Vec<String> = col_widths.iter().map(|w| "-".repeat(*w)).collect();
    let sep_cells: Vec<&str> = seps.iter().map(String::as_str).collect();
    write_row(out, &ts_sep, ts_width, &sep_cells, &col_widths)?;
    for (i, ts) in timestamps.iter().enumerate() {
        let ts_str = format_timestamp(*ts);
        let row: Vec<&str> = rendered_values[i].iter().map(String::as_str).collect();
        write_row(out, &ts_str, ts_width, &row, &col_widths)?;
    }
    Ok(())
}

/// Collects every unique timestamp across all series, sorted ascending.
///
/// Uses [`BTreeMap`] keyed by the bit-representation of each `f64` so that
/// we get ordered-unique behaviour without needing `Ord` on `f64`.
fn collect_timestamps(series: &[MetricSeries]) -> Vec<f64> {
    let mut seen: BTreeMap<u64, f64> = BTreeMap::new();
    for s in series {
        for (ts, _) in &s.pointlist {
            seen.entry(ts.to_bits()).or_insert(*ts);
        }
    }
    let mut ts: Vec<f64> = seen.into_values().collect();
    ts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    ts
}

/// Returns a `rows × cols` grid of pre-formatted cell strings aligned to
/// the union timestamp list.
fn render_values(series: &[MetricSeries], timestamps: &[f64]) -> Vec<Vec<String>> {
    timestamps
        .iter()
        .map(|ts| {
            series
                .iter()
                .map(|s| match find_value(s, *ts) {
                    Some(v) => format_value(v),
                    None => "-".to_string(),
                })
                .collect()
        })
        .collect()
}

/// Looks up a series' numeric value for a given timestamp.
///
/// Returns `None` when either the series has no point at this timestamp
/// or Datadog reported a null (gap) value — both render identically so
/// the distinction is collapsed at lookup time.
fn find_value(series: &MetricSeries, ts: f64) -> Option<f64> {
    series
        .pointlist
        .iter()
        .find(|(t, _)| t.to_bits() == ts.to_bits())
        .and_then(|(_, v)| *v)
}

/// Formats a numeric sample. Uses enough precision to distinguish small
/// deltas without bloating integer-valued rows.
fn format_value(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{v:.0}")
    } else {
        format!("{v:.6}")
    }
}

/// Formats a Datadog millisecond timestamp as RFC 3339 UTC.
fn format_timestamp(ms: f64) -> String {
    let secs = (ms / 1_000.0) as i64;
    let nsec = ((ms.rem_euclid(1_000.0)) * 1_000_000.0) as u32;
    let dt: DateTime<Utc> = Utc
        .timestamp_opt(secs, nsec)
        .single()
        .unwrap_or_else(Utc::now);
    dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Writes a single table row with consistent 2-space gutter between columns.
fn write_row(
    out: &mut dyn Write,
    ts_cell: &str,
    ts_width: usize,
    cells: &[&str],
    col_widths: &[usize],
) -> Result<()> {
    write!(out, "{ts_cell:<ts_width$}").context("Failed to write timestamp cell")?;
    for (cell, width) in cells.iter().zip(col_widths.iter()) {
        write!(out, "  {cell:<width$}").context("Failed to write series cell")?;
    }
    writeln!(out).context("Failed to terminate row")?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::cli::datadog::format::write_output;
    use crate::datadog::types::MetricSeries;

    /// `Write` impl that always fails, used to exercise the `?`-propagation
    /// error paths inside [`render_table`] and [`write_row`].
    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("test forced write failure"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    // ── fixtures ───────────────────────────────────────────────────

    fn sample_body() -> serde_json::Value {
        serde_json::json!({
            "status": "ok",
            "from_date": 1_700_000_000_000_i64,
            "to_date":   1_700_000_060_000_i64,
            "series": [
                {
                    "metric": "avg:system.cpu.user{*}",
                    "display_name": "avg:system.cpu.user{*}",
                    "expression": "avg:system.cpu.user{*}",
                    "pointlist": [
                        [1_700_000_000_000_i64, 0.5_f64],
                        [1_700_000_030_000_i64, 0.6_f64]
                    ]
                },
                {
                    "metric": "avg:system.cpu.idle{*}",
                    "display_name": "avg:system.cpu.idle{*}",
                    "expression": "avg:system.cpu.idle{*}",
                    "pointlist": [
                        [1_700_000_030_000_i64, 99.4_f64],
                        [1_700_000_060_000_i64, null]
                    ]
                }
            ]
        })
    }

    fn sample_response() -> MetricQueryResponse {
        serde_json::from_value(sample_body()).unwrap()
    }

    // ── helpers ────────────────────────────────────────────────────

    #[test]
    fn format_value_integers_drop_fraction() {
        assert_eq!(format_value(1.0), "1");
        assert_eq!(format_value(-3.0), "-3");
    }

    #[test]
    fn format_value_fractional_has_six_places() {
        assert_eq!(format_value(1.5), "1.500000");
    }

    #[test]
    fn format_value_large_integer_falls_back_to_fractional() {
        // Above 1e15 the `.0` branch is skipped to avoid surprising precision.
        let s = format_value(1e16);
        assert!(s.contains('.'));
    }

    #[test]
    fn format_value_negative_fractional_kept_at_six_places() {
        // Exercises the `fract != 0` branch with a negative value.
        assert_eq!(format_value(-0.25), "-0.250000");
    }

    #[test]
    fn format_timestamp_is_rfc3339_utc() {
        // 1_700_000_000_000 ms == 2023-11-14T22:13:20Z
        let s = format_timestamp(1_700_000_000_000.0);
        assert_eq!(s, "2023-11-14T22:13:20Z");
    }

    #[test]
    fn format_timestamp_out_of_range_falls_back_to_now() {
        // chrono rejects timestamps beyond i64 seconds; the `unwrap_or_else`
        // fallback clamps to `Utc::now`. We only assert that it returns a
        // plausibly-formatted RFC 3339 string rather than panicking.
        let s = format_timestamp(f64::MAX);
        assert!(s.ends_with('Z'));
        assert!(s.contains('T'));
    }

    #[test]
    fn collect_timestamps_dedups_and_sorts() {
        let series = vec![
            MetricSeries {
                metric: "a".into(),
                display_name: None,
                scope: None,
                expression: None,
                pointlist: vec![(2.0, None), (1.0, None)],
            },
            MetricSeries {
                metric: "b".into(),
                display_name: None,
                scope: None,
                expression: None,
                pointlist: vec![(1.0, None), (3.0, None)],
            },
        ];
        assert_eq!(collect_timestamps(&series), vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn collect_timestamps_tolerates_nan() {
        // NaN makes `partial_cmp` return `None`, exercising the
        // `Ordering::Equal` fallback in the sort comparator.
        let series = vec![MetricSeries {
            metric: "a".into(),
            display_name: None,
            scope: None,
            expression: None,
            pointlist: vec![(1.0, None), (f64::NAN, None), (2.0, None)],
        }];
        let ts = collect_timestamps(&series);
        assert_eq!(ts.len(), 3);
        // `NaN` is not equal to itself — just confirm we didn't lose values.
        assert!(ts.iter().any(|t| t.is_nan()));
    }

    #[test]
    fn find_value_collapses_gap_and_missing_point() {
        let s = MetricSeries {
            metric: "m".into(),
            display_name: None,
            scope: None,
            expression: None,
            pointlist: vec![(10.0, None), (20.0, Some(0.5))],
        };
        // Gap (null) and missing-point both surface as `None`.
        assert_eq!(find_value(&s, 10.0), None);
        assert_eq!(find_value(&s, 20.0), Some(0.5));
        assert_eq!(find_value(&s, 30.0), None);
    }

    // ── render_table ───────────────────────────────────────────────

    #[test]
    fn render_table_includes_both_series_and_union_timestamps() {
        let resp = sample_response();
        let mut buf = Vec::new();
        render_table(&resp, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();

        // Header has a column per series.
        assert!(out.contains("TIMESTAMP"));
        assert!(out.contains("avg:system.cpu.user{*}"));
        assert!(out.contains("avg:system.cpu.idle{*}"));

        // Union of timestamps produces three rows (000/030/060 seconds past epoch).
        assert!(out.contains("2023-11-14T22:13:20Z"));
        assert!(out.contains("2023-11-14T22:13:50Z"));
        assert!(out.contains("2023-11-14T22:14:20Z"));

        // Null value renders as `-`.
        let last_line = out.lines().last().unwrap();
        assert!(last_line.contains("  -"));
    }

    #[test]
    fn render_table_fills_missing_series_cells_with_dash() {
        let resp = sample_response();
        let mut buf = Vec::new();
        render_table(&resp, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        // The first row only has a value for the user series, idle is missing.
        let first_data_line = out.lines().nth(2).unwrap();
        assert!(first_data_line.contains("2023-11-14T22:13:20Z"));
        assert!(first_data_line.contains("0.500000"));
        assert!(first_data_line.contains(" -"));
    }

    #[test]
    fn render_table_on_empty_series_prints_message() {
        let resp = MetricQueryResponse {
            status: "ok".into(),
            from_date: 0,
            to_date: 0,
            series: vec![],
        };
        let mut buf = Vec::new();
        render_table(&resp, &mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "No series returned.\n");
    }

    #[test]
    fn render_table_propagates_write_errors() {
        let resp = sample_response();
        let err = render_table(&resp, &mut FailingWriter).unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    #[test]
    fn render_table_empty_series_propagates_write_errors() {
        let resp = MetricQueryResponse {
            status: "ok".into(),
            from_date: 0,
            to_date: 0,
            series: vec![],
        };
        let err = render_table(&resp, &mut FailingWriter).unwrap_err();
        assert!(err.to_string().contains("empty-table message"));
    }

    #[test]
    fn render_table_with_series_but_no_points_still_renders_headers() {
        // Exercises the `timestamps.first().unwrap_or(&0.0)` fallback and
        // the `max().unwrap_or(0)` column-width fallback — both trigger
        // when a series exists but has an empty pointlist.
        let resp = MetricQueryResponse {
            status: "ok".into(),
            from_date: 0,
            to_date: 0,
            series: vec![MetricSeries {
                metric: "m".into(),
                display_name: Some("label".into()),
                scope: None,
                expression: None,
                pointlist: vec![],
            }],
        };
        let mut buf = Vec::new();
        render_table(&resp, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("TIMESTAMP"));
        assert!(out.contains("label"));
        // Header + separator only, no data rows.
        assert_eq!(out.lines().count(), 2);
    }

    // ── write_output integration ───────────────────────────────────

    #[test]
    fn write_output_json_emits_full_response() {
        let resp = sample_response();
        let mut buf = Vec::new();
        let wrote = write_output(&resp, &OutputFormat::Json, &mut buf).unwrap();
        assert!(wrote);
        let out = String::from_utf8(buf).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["status"], "ok");
        assert_eq!(parsed["series"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn write_output_yaml_emits_document() {
        let resp = sample_response();
        let mut buf = Vec::new();
        write_output(&resp, &OutputFormat::Yaml, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("status: ok"));
        assert!(out.contains("series:"));
    }

    #[test]
    fn write_output_jsonl_emits_single_object_line() {
        let resp = sample_response();
        let mut buf = Vec::new();
        write_output(&resp, &OutputFormat::Jsonl, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(out.matches('\n').count(), 1);
        let parsed: serde_json::Value = serde_json::from_str(out.trim_end()).unwrap();
        assert_eq!(parsed["series"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn write_output_yamls_emits_single_document() {
        let resp = sample_response();
        let mut buf = Vec::new();
        write_output(&resp, &OutputFormat::Yamls, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.starts_with("---\n"));
        assert!(out.contains("status: ok"));
    }

    #[test]
    fn write_output_table_returns_false() {
        let resp = sample_response();
        let mut buf = Vec::new();
        let wrote = write_output(&resp, &OutputFormat::Table, &mut buf).unwrap();
        assert!(!wrote);
        assert!(buf.is_empty());
    }

    // ── run_query dispatch ─────────────────────────────────────────

    #[tokio::test]
    async fn run_query_table_path_writes_headers_to_stdout_returns_ok() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/query"))
            .and(wiremock::matchers::query_param("from", "100"))
            .and(wiremock::matchers::query_param("to", "200"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(sample_body()))
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        run_query(
            &client,
            "avg:system.cpu.user{*}",
            100,
            200,
            &OutputFormat::Table,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_query_json_path_returns_ok() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/query"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(sample_body()))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        run_query(&client, "m", 0, 1, &OutputFormat::Json)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_query_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/query"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = run_query(&client, "m", 0, 1, &OutputFormat::Table)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    // ── QueryCommand::execute glue ─────────────────────────────────
    //
    // Tests inject a wiremock-backed client into `execute`, covering the
    // execute-level time-range parsing and query glue without touching
    // credentials or the environment. (Credential resolution itself is covered
    // by the credential-loading tests.)

    #[tokio::test]
    async fn execute_rejects_invalid_time_range() {
        // `parse_time_range` fails before any HTTP call, so the client's URL
        // is never contacted.
        let client = DatadogClient::new("http://127.0.0.1:1", "api", "app").unwrap();
        let cmd = QueryCommand {
            query: "m".into(),
            from: "garbage".into(),
            to: None,
            output: OutputFormat::Table,
        };
        let err = cmd.execute(&client).await.unwrap_err();
        assert!(err.to_string().contains("Invalid time range"));
    }

    #[tokio::test]
    async fn execute_end_to_end() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/query"))
            .and(wiremock::matchers::query_param(
                "query",
                "avg:system.cpu.user{*}",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(sample_body()))
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let cmd = QueryCommand {
            query: "avg:system.cpu.user{*}".into(),
            from: "2023-11-14T22:00:00Z".into(),
            to: Some("2023-11-14T23:00:00Z".into()),
            output: OutputFormat::Json,
        };
        cmd.execute(&client).await.unwrap();
    }
}
