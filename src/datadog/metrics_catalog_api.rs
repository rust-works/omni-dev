//! Datadog Metric Catalog API wrapper.
//!
//! Exposes a thin façade over [`DatadogClient`] for the read-only metric
//! catalog endpoint (`GET /api/v1/metrics`). This is distinct from the
//! Phase 1 metrics *query* endpoint (`/api/v1/query`); the catalog
//! returns the names of metrics ingested since `from`.

use anyhow::{Context, Result};
use url::Url;

use crate::datadog::client::DatadogClient;
use crate::datadog::types::MetricCatalogResponse;

/// Metric Catalog API façade.
#[derive(Debug)]
pub struct MetricsCatalogApi<'a> {
    client: &'a DatadogClient,
}

impl<'a> MetricsCatalogApi<'a> {
    /// Wraps an existing [`DatadogClient`] for metric catalog operations.
    #[must_use]
    pub fn new(client: &'a DatadogClient) -> Self {
        Self { client }
    }

    /// Lists metrics ingested since `from` (Unix epoch seconds), filtered
    /// optionally by `host`.
    pub async fn list(
        &self,
        host: Option<&str>,
        from: Option<i64>,
    ) -> Result<MetricCatalogResponse> {
        let url = build_list_url(self.client.base_url(), host, from)?;
        let response = self.client.get_json(url.as_str()).await?;
        if !response.status().is_success() {
            return Err(DatadogClient::response_to_error(response).await.into());
        }
        response
            .json::<MetricCatalogResponse>()
            .await
            .context("Failed to parse /api/v1/metrics response")
    }
}

fn build_list_url(base_url: &str, host: Option<&str>, from: Option<i64>) -> Result<Url> {
    let mut url =
        Url::parse(&format!("{base_url}/api/v1/metrics")).context("Invalid Datadog base URL")?;
    {
        let mut q = url.query_pairs_mut();
        if let Some(host) = host {
            q.append_pair("host", host);
        }
        if let Some(from) = from {
            q.append_pair("from", &from.to_string());
        }
    }
    Ok(url)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // ── URL builder ────────────────────────────────────────────────

    #[test]
    fn build_list_url_omits_filters_when_unset() {
        let url = build_list_url("https://api.datadoghq.com", None, None).unwrap();
        assert_eq!(url.path(), "/api/v1/metrics");
        // Either no query at all, or an empty one — both mean "no params".
        assert!(url.query().unwrap_or("").is_empty());
    }

    #[test]
    fn build_list_url_appends_host_filter() {
        let url = build_list_url("https://api.datadoghq.com", Some("web-01"), None).unwrap();
        assert_eq!(url.query(), Some("host=web-01"));
    }

    #[test]
    fn build_list_url_appends_from_filter() {
        let url = build_list_url("https://api.datadoghq.com", None, Some(1_700_000_000)).unwrap();
        assert_eq!(url.query(), Some("from=1700000000"));
    }

    #[test]
    fn build_list_url_appends_both_when_set() {
        let url = build_list_url(
            "https://api.datadoghq.com",
            Some("web-01"),
            Some(1_700_000_000),
        )
        .unwrap();
        let qs = url.query().unwrap();
        assert!(qs.contains("host=web-01"));
        assert!(qs.contains("from=1700000000"));
    }

    #[test]
    fn build_list_url_rejects_invalid_base() {
        let err = build_list_url("not a url", None, None).unwrap_err();
        assert!(err.to_string().contains("Invalid Datadog base URL"));
    }

    // ── happy / error paths ────────────────────────────────────────

    #[tokio::test]
    async fn list_returns_parsed_response() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/metrics"))
            .and(wiremock::matchers::query_param("host", "web-01"))
            .and(wiremock::matchers::query_param("from", "1700000000"))
            .and(wiremock::matchers::header("DD-API-KEY", "api"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "from": 1_700_000_000_i64,
                    "metrics": ["system.cpu.user", "system.cpu.idle"]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let result = MetricsCatalogApi::new(&client)
            .list(Some("web-01"), Some(1_700_000_000))
            .await
            .unwrap();
        assert_eq!(result.from, Some(1_700_000_000));
        assert_eq!(result.metrics.len(), 2);
        assert_eq!(result.metrics[0], "system.cpu.user");
    }

    #[tokio::test]
    async fn list_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/metrics"))
            .respond_with(
                wiremock::ResponseTemplate::new(400).set_body_string(r#"{"errors":["bad from"]}"#),
            )
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = MetricsCatalogApi::new(&client)
            .list(None, None)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("400"));
        assert!(msg.contains("bad from"));
    }

    #[tokio::test]
    async fn list_propagates_invalid_base_url_error() {
        let client = DatadogClient::new("not a url", "api", "app").unwrap();
        let err = MetricsCatalogApi::new(&client)
            .list(None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Invalid Datadog base URL"));
    }

    #[tokio::test]
    async fn list_propagates_network_errors() {
        let client = DatadogClient::new("http://127.0.0.1:1", "api", "app").unwrap();
        let err = MetricsCatalogApi::new(&client)
            .list(None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Failed to send"));
    }

    #[tokio::test]
    async fn list_errors_on_malformed_response() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/metrics"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = MetricsCatalogApi::new(&client)
            .list(None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Failed to parse"));
    }
}
