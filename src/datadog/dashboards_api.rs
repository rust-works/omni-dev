//! Datadog Dashboards API wrapper.
//!
//! Exposes a thin façade over [`DatadogClient`] for the read-only dashboard
//! endpoints needed by the CLI: list and get.
//!
//! Unlike the monitor endpoints, `GET /api/v1/dashboard` returns *all*
//! dashboards in a single response — no server-side pagination — so the
//! list façade does not loop. Any client-side `--limit` truncation belongs
//! in the CLI layer.

use anyhow::Result;
use url::Url;

use crate::datadog::client::DatadogClient;
use crate::datadog::types::{Dashboard, DashboardListResponse, DashboardSummary};

/// Filters accepted by `GET /api/v1/dashboard`.
///
/// Datadog accepts `filter_shared` as a boolean query parameter; the
/// builder appends it only when the field is `Some(_)` so callers can
/// distinguish "unset" from "explicitly set to false".
#[derive(Debug, Default, Clone)]
pub struct DashboardListFilter {
    /// When `Some`, restricts the response to shared (or non-shared)
    /// dashboards depending on the boolean value.
    pub filter_shared: Option<bool>,
}

/// Dashboards API façade.
#[derive(Debug)]
pub struct DashboardsApi<'a> {
    client: &'a DatadogClient,
}

impl<'a> DashboardsApi<'a> {
    /// Wraps an existing [`DatadogClient`] for dashboard operations.
    #[must_use]
    pub fn new(client: &'a DatadogClient) -> Self {
        Self { client }
    }

    /// Lists dashboards matching `filter`.
    ///
    /// Datadog returns every dashboard in one response; this method
    /// makes a single HTTP call and returns the parsed `dashboards`
    /// array. There is no auto-pagination because the API does not
    /// page this endpoint.
    pub async fn list(&self, filter: &DashboardListFilter) -> Result<Vec<DashboardSummary>> {
        let url = build_list_url(self.client.base_url(), filter)?;
        let parsed: DashboardListResponse = self
            .client
            .get_parsed(url.as_str(), "Failed to parse /api/v1/dashboard response")
            .await?;
        Ok(parsed.dashboards)
    }

    /// Fetches a single dashboard definition by id.
    pub async fn get(&self, id: &str) -> Result<Dashboard> {
        let url = build_get_url(self.client.base_url(), id)?;
        self.client
            .get_parsed(
                url.as_str(),
                "Failed to parse /api/v1/dashboard/<id> response",
            )
            .await
    }
}

/// Builds `{base_url}/api/v1/dashboard?{filters}`.
fn build_list_url(base_url: &str, filter: &DashboardListFilter) -> Result<Url> {
    let mut url = DatadogClient::api_url(base_url, "/api/v1/dashboard")?;
    if let Some(shared) = filter.filter_shared {
        url.query_pairs_mut()
            .append_pair("filter_shared", if shared { "true" } else { "false" });
    }
    Ok(url)
}

/// Builds `{base_url}/api/v1/dashboard/{id}`.
///
/// `id` is percent-encoded as a path segment so dashboard ids that
/// contain reserved characters round-trip correctly.
fn build_get_url(base_url: &str, id: &str) -> Result<Url> {
    let mut url = DatadogClient::api_url(base_url, "/api/v1/dashboard")?;
    url.path_segments_mut()
        .map_err(|()| anyhow::anyhow!("Invalid Datadog base URL: cannot append path segment"))?
        .push(id);
    Ok(url)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // ── URL builders ───────────────────────────────────────────────

    #[test]
    fn build_list_url_omits_filter_when_unset() {
        let url =
            build_list_url("https://api.datadoghq.com", &DashboardListFilter::default()).unwrap();
        assert_eq!(url.path(), "/api/v1/dashboard");
        assert!(url.query().is_none());
    }

    #[test]
    fn build_list_url_appends_filter_shared_true() {
        let url = build_list_url(
            "https://api.datadoghq.com",
            &DashboardListFilter {
                filter_shared: Some(true),
            },
        )
        .unwrap();
        assert_eq!(url.query(), Some("filter_shared=true"));
    }

    #[test]
    fn build_list_url_appends_filter_shared_false() {
        let url = build_list_url(
            "https://api.datadoghq.com",
            &DashboardListFilter {
                filter_shared: Some(false),
            },
        )
        .unwrap();
        assert_eq!(url.query(), Some("filter_shared=false"));
    }

    #[test]
    fn build_list_url_rejects_invalid_base() {
        let err = build_list_url("not a url", &DashboardListFilter::default()).unwrap_err();
        assert!(err.to_string().contains("Invalid Datadog base URL"));
    }

    #[test]
    fn build_get_url_includes_id_path_segment() {
        let url = build_get_url("https://api.datadoghq.com", "abc-def-ghi").unwrap();
        assert_eq!(url.path(), "/api/v1/dashboard/abc-def-ghi");
    }

    #[test]
    fn build_get_url_percent_encodes_reserved_chars_in_id() {
        let url = build_get_url("https://api.datadoghq.com", "weird/id").unwrap();
        // `/` in a single path segment is percent-encoded; the resulting
        // path therefore stays under /api/v1/dashboard with one segment.
        assert_eq!(url.path(), "/api/v1/dashboard/weird%2Fid");
    }

    #[test]
    fn build_get_url_rejects_invalid_base() {
        let err = build_get_url("not a url", "id").unwrap_err();
        assert!(err.to_string().contains("Invalid Datadog base URL"));
    }

    #[test]
    fn build_get_url_rejects_cannot_be_a_base_scheme() {
        // `mailto:` parses successfully via `Url::parse` but is a
        // cannot-be-a-base URL, so `path_segments_mut` returns Err(()).
        // This exercises the `map_err` arm that's otherwise unreachable
        // from the production base-URL inputs.
        let err = build_get_url("mailto:test@example.com", "id").unwrap_err();
        assert!(err.to_string().contains("cannot append path segment"));
    }

    // ── fixtures ───────────────────────────────────────────────────

    fn dashboard_summary_json(id: &str, title: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "title": title,
            "author_handle": "alice@example.com",
            "url": format!("/dashboard/{id}"),
            "modified_at": "2024-02-01T00:00:00.000Z",
            "is_shared": true
        })
    }

    fn dashboard_full_json(id: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "title": "Service Overview",
            "description": "Top-level service health.",
            "url": format!("/dashboard/{id}"),
            "author_handle": "alice@example.com",
            "layout_type": "ordered",
            "widgets": [
                {"id": 1, "definition": {"type": "note", "content": "hello"}}
            ]
        })
    }

    // ── list happy path / errors ───────────────────────────────────

    #[tokio::test]
    async fn list_returns_parsed_dashboards() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/dashboard"))
            .and(wiremock::matchers::header("DD-API-KEY", "api"))
            .and(wiremock::matchers::header("DD-APPLICATION-KEY", "app"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "dashboards": [
                        dashboard_summary_json("abc", "Service A"),
                        dashboard_summary_json("def", "Service B")
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let dashboards = DashboardsApi::new(&client)
            .list(&DashboardListFilter::default())
            .await
            .unwrap();
        assert_eq!(dashboards.len(), 2);
        assert_eq!(dashboards[0].id, "abc");
        assert_eq!(dashboards[1].title, "Service B");
    }

    #[tokio::test]
    async fn list_passes_filter_shared_query_param() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/dashboard"))
            .and(wiremock::matchers::query_param("filter_shared", "true"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "dashboards": [dashboard_summary_json("abc", "Service A")]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let dashboards = DashboardsApi::new(&client)
            .list(&DashboardListFilter {
                filter_shared: Some(true),
            })
            .await
            .unwrap();
        assert_eq!(dashboards.len(), 1);
    }

    #[tokio::test]
    async fn list_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/dashboard"))
            .respond_with(
                wiremock::ResponseTemplate::new(403).set_body_string(r#"{"errors":["nope"]}"#),
            )
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = DashboardsApi::new(&client)
            .list(&DashboardListFilter::default())
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("403"));
        assert!(msg.contains("nope"));
    }

    #[tokio::test]
    async fn list_propagates_invalid_base_url_error() {
        let client = DatadogClient::new("not a url", "api", "app").unwrap();
        let err = DashboardsApi::new(&client)
            .list(&DashboardListFilter::default())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Invalid Datadog base URL"));
    }

    #[tokio::test]
    async fn list_propagates_network_errors() {
        let client = DatadogClient::new("http://127.0.0.1:1", "api", "app").unwrap();
        let err = DashboardsApi::new(&client)
            .list(&DashboardListFilter::default())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Failed to send"));
    }

    #[tokio::test]
    async fn list_errors_on_malformed_response() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/dashboard"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = DashboardsApi::new(&client)
            .list(&DashboardListFilter::default())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Failed to parse"));
    }

    // ── get happy path / errors ────────────────────────────────────

    #[tokio::test]
    async fn get_returns_parsed_dashboard() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/dashboard/abc-def-ghi"))
            .and(wiremock::matchers::header("DD-API-KEY", "api"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(dashboard_full_json("abc-def-ghi")),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let d = DashboardsApi::new(&client)
            .get("abc-def-ghi")
            .await
            .unwrap();
        assert_eq!(d.id, "abc-def-ghi");
        assert_eq!(d.title, "Service Overview");
        assert!(d.widgets.is_some());
    }

    #[tokio::test]
    async fn get_propagates_404() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/dashboard/missing"))
            .respond_with(
                wiremock::ResponseTemplate::new(404).set_body_string(r#"{"errors":["Not found"]}"#),
            )
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = DashboardsApi::new(&client)
            .get("missing")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
        assert!(err.to_string().contains("Not found"));
    }

    #[tokio::test]
    async fn get_propagates_invalid_base_url_error() {
        let client = DatadogClient::new("not a url", "api", "app").unwrap();
        let err = DashboardsApi::new(&client).get("x").await.unwrap_err();
        assert!(err.to_string().contains("Invalid Datadog base URL"));
    }

    #[tokio::test]
    async fn get_propagates_network_errors() {
        let client = DatadogClient::new("http://127.0.0.1:1", "api", "app").unwrap();
        let err = DashboardsApi::new(&client).get("x").await.unwrap_err();
        assert!(err.to_string().contains("Failed to send"));
    }

    #[tokio::test]
    async fn get_errors_on_malformed_response() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/dashboard/x"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = DashboardsApi::new(&client).get("x").await.unwrap_err();
        assert!(err.to_string().contains("Failed to parse"));
    }
}
