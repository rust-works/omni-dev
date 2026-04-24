//! Datadog Metrics API wrapper.
//!
//! Exposes a thin façade over [`DatadogClient`] for the metrics endpoints
//! needed by the CLI. Currently covers the point-in-time timeseries query
//! (`GET /api/v1/query`); subsequent slices will add scalar and multi-query
//! variants.

use anyhow::{Context, Result};
use url::Url;

use crate::datadog::client::DatadogClient;
use crate::datadog::types::MetricQueryResponse;

/// Metrics API façade.
#[derive(Debug)]
pub struct MetricsApi<'a> {
    client: &'a DatadogClient,
}

impl<'a> MetricsApi<'a> {
    /// Wraps an existing [`DatadogClient`] for metrics operations.
    #[must_use]
    pub fn new(client: &'a DatadogClient) -> Self {
        Self { client }
    }

    /// Executes a point-in-time metrics timeseries query.
    ///
    /// `from` / `to` are Unix epoch **seconds** — the unit expected by
    /// Datadog's v1 `query` parameters. The response from Datadog uses
    /// milliseconds for its own `from_date` / `to_date` / pointlist
    /// timestamps; we pass those through unmodified.
    pub async fn point_query(
        &self,
        query: &str,
        from: i64,
        to: i64,
    ) -> Result<MetricQueryResponse> {
        let url = build_query_url(self.client.base_url(), query, from, to)?;
        let response = self.client.get_json(url.as_str()).await?;

        if !response.status().is_success() {
            return Err(DatadogClient::response_to_error(response).await.into());
        }

        response
            .json::<MetricQueryResponse>()
            .await
            .context("Failed to parse /api/v1/query response")
    }
}

/// Builds `{base_url}/api/v1/query?from=…&to=…&query=…` with proper
/// percent-encoding for the query string.
fn build_query_url(base_url: &str, query: &str, from: i64, to: i64) -> Result<Url> {
    let mut url =
        Url::parse(&format!("{base_url}/api/v1/query")).context("Invalid Datadog base URL")?;
    url.query_pairs_mut()
        .append_pair("from", &from.to_string())
        .append_pair("to", &to.to_string())
        .append_pair("query", query);
    Ok(url)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // ── build_query_url ────────────────────────────────────────────

    #[test]
    fn build_query_url_encodes_special_chars() {
        let url = build_query_url(
            "https://api.datadoghq.com",
            "avg:system.cpu.user{host:web-01}",
            100,
            200,
        )
        .unwrap();
        let qs = url.query().unwrap();
        assert!(qs.contains("from=100"));
        assert!(qs.contains("to=200"));
        // Braces must be percent-encoded to pass through Datadog's URL parser.
        assert!(qs.contains("query=avg%3Asystem.cpu.user%7Bhost%3Aweb-01%7D"));
    }

    #[test]
    fn build_query_url_strips_trailing_slash_on_base() {
        // The client normalises the base URL at construction time; double-check
        // we don't produce a duplicate slash.
        let url = build_query_url("https://api.datadoghq.com", "m", 0, 1).unwrap();
        assert_eq!(url.path(), "/api/v1/query");
    }

    #[test]
    fn build_query_url_rejects_invalid_base() {
        let err = build_query_url("not a url", "m", 0, 1).unwrap_err();
        assert!(err.to_string().contains("Invalid Datadog base URL"));
    }

    // ── point_query happy path ─────────────────────────────────────

    fn sample_body() -> serde_json::Value {
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
                    ]
                }
            ]
        })
    }

    #[tokio::test]
    async fn point_query_returns_parsed_response() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/query"))
            .and(wiremock::matchers::query_param("from", "100"))
            .and(wiremock::matchers::query_param("to", "200"))
            .and(wiremock::matchers::query_param(
                "query",
                "avg:system.cpu.user{*}",
            ))
            .and(wiremock::matchers::header("DD-API-KEY", "api"))
            .and(wiremock::matchers::header("DD-APPLICATION-KEY", "app"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(sample_body()))
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let resp = MetricsApi::new(&client)
            .point_query("avg:system.cpu.user{*}", 100, 200)
            .await
            .unwrap();

        assert_eq!(resp.status, "ok");
        assert_eq!(resp.series.len(), 1);
        assert_eq!(resp.series[0].pointlist.len(), 3);
        assert_eq!(resp.series[0].pointlist[1].1, None);
    }

    #[tokio::test]
    async fn point_query_propagates_api_errors_with_body() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/query"))
            .respond_with(
                wiremock::ResponseTemplate::new(400).set_body_string(r#"{"errors":["Bad query"]}"#),
            )
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = MetricsApi::new(&client)
            .point_query("bad!!", 0, 1)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("400"));
        assert!(msg.contains("Bad query"));
    }

    #[tokio::test]
    async fn point_query_propagates_invalid_base_url_error() {
        // `DatadogClient::new` doesn't validate its URL, so the error only
        // surfaces when `build_query_url` tries to parse it.
        let client = DatadogClient::new("not a url", "api", "app").unwrap();
        let err = MetricsApi::new(&client)
            .point_query("m", 0, 1)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Invalid Datadog base URL"));
    }

    #[tokio::test]
    async fn point_query_propagates_network_errors() {
        // Point at a port that refuses connection; `get_json` surfaces the
        // reqwest send failure via anyhow context.
        let client = DatadogClient::new("http://127.0.0.1:1", "api", "app").unwrap();
        let err = MetricsApi::new(&client)
            .point_query("m", 0, 1)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Failed to send"));
    }

    #[tokio::test]
    async fn point_query_errors_on_malformed_response() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/query"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = MetricsApi::new(&client)
            .point_query("m", 0, 1)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Failed to parse"));
    }
}
