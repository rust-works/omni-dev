//! Datadog Downtimes API wrapper.
//!
//! Exposes a thin façade over [`DatadogClient`] for the read-only
//! downtimes endpoint (`GET /api/v1/downtime`).
//!
//! Datadog returns the full downtime list in a single response — no
//! server-side pagination — so the façade does not loop. The optional
//! `current_only` filter restricts the response to active downtimes.

use anyhow::{Context, Result};
use url::Url;

use crate::datadog::client::DatadogClient;
use crate::datadog::types::Downtime;

/// Downtimes API façade.
#[derive(Debug)]
pub struct DowntimesApi<'a> {
    client: &'a DatadogClient,
}

impl<'a> DowntimesApi<'a> {
    /// Wraps an existing [`DatadogClient`] for downtime operations.
    #[must_use]
    pub fn new(client: &'a DatadogClient) -> Self {
        Self { client }
    }

    /// Lists all downtimes. When `current_only` is true, only active
    /// downtimes are returned.
    pub async fn list(&self, current_only: bool) -> Result<Vec<Downtime>> {
        let url = build_list_url(self.client.base_url(), current_only)?;
        let response = self.client.get_json(url.as_str()).await?;
        if !response.status().is_success() {
            return Err(DatadogClient::response_to_error(response).await.into());
        }
        response
            .json::<Vec<Downtime>>()
            .await
            .context("Failed to parse /api/v1/downtime response")
    }
}

fn build_list_url(base_url: &str, current_only: bool) -> Result<Url> {
    let mut url =
        Url::parse(&format!("{base_url}/api/v1/downtime")).context("Invalid Datadog base URL")?;
    if current_only {
        url.query_pairs_mut().append_pair("current_only", "true");
    }
    Ok(url)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // ── URL builder ────────────────────────────────────────────────

    #[test]
    fn build_list_url_omits_current_only_when_false() {
        let url = build_list_url("https://api.datadoghq.com", false).unwrap();
        assert_eq!(url.path(), "/api/v1/downtime");
        assert!(url.query().is_none());
    }

    #[test]
    fn build_list_url_appends_current_only_when_true() {
        let url = build_list_url("https://api.datadoghq.com", true).unwrap();
        assert_eq!(url.query(), Some("current_only=true"));
    }

    #[test]
    fn build_list_url_rejects_invalid_base() {
        let err = build_list_url("not a url", false).unwrap_err();
        assert!(err.to_string().contains("Invalid Datadog base URL"));
    }

    // ── fixtures ───────────────────────────────────────────────────

    fn downtime_json(id: i64, scope: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "scope": scope,
            "start": 1_700_000_000_i64,
            "end": 1_700_000_300_i64,
            "message": format!("dt {id}"),
            "active": true,
            "disabled": false
        })
    }

    // ── happy / error paths ────────────────────────────────────────

    #[tokio::test]
    async fn list_returns_parsed_downtimes() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/downtime"))
            .and(wiremock::matchers::header("DD-API-KEY", "api"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([
                    downtime_json(1, &["env:prod"]),
                    downtime_json(2, &["env:staging"])
                ])),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let dts = DowntimesApi::new(&client).list(false).await.unwrap();
        assert_eq!(dts.len(), 2);
        assert_eq!(dts[0].id, 1);
        assert_eq!(dts[1].scope, vec!["env:staging"]);
    }

    #[tokio::test]
    async fn list_passes_current_only_param() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/downtime"))
            .and(wiremock::matchers::query_param("current_only", "true"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!([downtime_json(1, &["env:prod"])])),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let dts = DowntimesApi::new(&client).list(true).await.unwrap();
        assert_eq!(dts.len(), 1);
    }

    #[tokio::test]
    async fn list_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/downtime"))
            .respond_with(
                wiremock::ResponseTemplate::new(403).set_body_string(r#"{"errors":["nope"]}"#),
            )
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = DowntimesApi::new(&client).list(false).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("403"));
        assert!(msg.contains("nope"));
    }

    #[tokio::test]
    async fn list_propagates_invalid_base_url_error() {
        let client = DatadogClient::new("not a url", "api", "app").unwrap();
        let err = DowntimesApi::new(&client).list(false).await.unwrap_err();
        assert!(err.to_string().contains("Invalid Datadog base URL"));
    }

    #[tokio::test]
    async fn list_propagates_network_errors() {
        let client = DatadogClient::new("http://127.0.0.1:1", "api", "app").unwrap();
        let err = DowntimesApi::new(&client).list(false).await.unwrap_err();
        assert!(err.to_string().contains("Failed to send"));
    }

    #[tokio::test]
    async fn list_errors_on_malformed_response() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/downtime"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = DowntimesApi::new(&client).list(false).await.unwrap_err();
        assert!(err.to_string().contains("Failed to parse"));
    }
}
