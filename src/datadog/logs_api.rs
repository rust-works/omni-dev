//! Datadog Logs API wrapper.
//!
//! Exposes a thin façade over [`DatadogClient`] for the Phase 1 logs
//! search endpoint. Datadog v2 logs search uses **cursor pagination**
//! (`meta.page.after`), not offset; Phase 1 ships single-page only and
//! caps `--limit` at [`MAX_PAGE_LIMIT`] per the decisions on [#619].
//! Multi-page cursor iteration is a Phase 2 follow-up.
//!
//! [#619]: https://github.com/rust-works/omni-dev/issues/619

use anyhow::{Context, Result};
use serde::Serialize;

use crate::datadog::client::DatadogClient;
use crate::datadog::types::{LogSearchResult, SortOrder};

/// Maximum page size accepted by `POST /api/v2/logs/events/search`.
///
/// Datadog rejects page sizes above 1000; the API client surfaces a
/// clearer error than the server's HTTP 400 by validating before the
/// request is sent.
pub const MAX_PAGE_LIMIT: usize = 1000;

/// Logs API façade.
#[derive(Debug)]
pub struct LogsApi<'a> {
    client: &'a DatadogClient,
}

impl<'a> LogsApi<'a> {
    /// Wraps an existing [`DatadogClient`] for log operations.
    #[must_use]
    pub fn new(client: &'a DatadogClient) -> Self {
        Self { client }
    }

    /// Searches log events.
    ///
    /// `from` and `to` are passed through to Datadog as strings. Datadog
    /// accepts ISO 8601 timestamps, epoch milliseconds, and relative
    /// shorthand like `now-15m` / `now`; callers are expected to convert
    /// CLI-level inputs into a form Datadog understands before calling.
    ///
    /// Phase 1 returns a single page only — cursor pagination via
    /// `meta.page.after` is not auto-iterated. `limit` is rejected
    /// client-side when it exceeds [`MAX_PAGE_LIMIT`].
    pub async fn search(
        &self,
        query: &str,
        from: &str,
        to: &str,
        limit: usize,
        sort: SortOrder,
    ) -> Result<LogSearchResult> {
        if limit > MAX_PAGE_LIMIT {
            return Err(anyhow::anyhow!(
                "--limit must be <= {MAX_PAGE_LIMIT} (Datadog v2 logs search per-page cap; cursor pagination across pages is a Phase 2 follow-up)"
            ));
        }
        let body = SearchRequest {
            filter: Filter { query, from, to },
            page: Page { limit },
            sort,
        };
        let url = format!("{}/api/v2/logs/events/search", self.client.base_url());
        let response = self.client.post_json(&url, &body).await?;
        if !response.status().is_success() {
            return Err(DatadogClient::response_to_error(response).await.into());
        }
        response
            .json::<LogSearchResult>()
            .await
            .context("Failed to parse /api/v2/logs/events/search response")
    }
}

#[derive(Debug, Serialize)]
struct SearchRequest<'a> {
    filter: Filter<'a>,
    page: Page,
    sort: SortOrder,
}

#[derive(Debug, Serialize)]
struct Filter<'a> {
    query: &'a str,
    from: &'a str,
    to: &'a str,
}

#[derive(Debug, Serialize)]
struct Page {
    limit: usize,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn sample_search_body() -> serde_json::Value {
        serde_json::json!({
            "data": [
                {
                    "id": "AAAA",
                    "type": "log",
                    "attributes": {
                        "timestamp": "2026-04-22T10:00:00.000Z",
                        "service": "api",
                        "status": "info",
                        "message": "hello",
                        "tags": ["env:prod"]
                    }
                }
            ],
            "meta": {
                "page": { "after": "next-cursor" },
                "status": "done",
                "elapsed": 12
            }
        })
    }

    #[tokio::test]
    async fn search_posts_exact_body_shape_and_parses_response() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/v2/logs/events/search"))
            .and(wiremock::matchers::header("DD-API-KEY", "api"))
            .and(wiremock::matchers::header("DD-APPLICATION-KEY", "app"))
            .and(wiremock::matchers::header(
                "Content-Type",
                "application/json",
            ))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "filter": {
                    "query": "service:api status:error",
                    "from": "now-15m",
                    "to": "now"
                },
                "page": { "limit": 100 },
                "sort": "-timestamp"
            })))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(sample_search_body()))
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let result = LogsApi::new(&client)
            .search(
                "service:api status:error",
                "now-15m",
                "now",
                100,
                SortOrder::TimestampDesc,
            )
            .await
            .unwrap();
        assert_eq!(result.data.len(), 1);
        assert_eq!(result.data[0].id, "AAAA");
        assert_eq!(
            result
                .meta
                .as_ref()
                .and_then(|m| m.page.as_ref())
                .and_then(|p| p.after.as_deref()),
            Some("next-cursor")
        );
    }

    #[tokio::test]
    async fn search_serializes_ascending_sort_without_minus_prefix() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/v2/logs/events/search"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "filter": { "query": "*", "from": "now-1h", "to": "now" },
                "page": { "limit": 50 },
                "sort": "timestamp"
            })))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(sample_search_body()))
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        LogsApi::new(&client)
            .search("*", "now-1h", "now", 50, SortOrder::TimestampAsc)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn search_rejects_limit_above_max_page_limit_client_side() {
        // No server is started — the validation must fire before any
        // network call.
        let client = DatadogClient::new("http://127.0.0.1:1", "api", "app").unwrap();
        let err = LogsApi::new(&client)
            .search(
                "*",
                "now-1h",
                "now",
                MAX_PAGE_LIMIT + 1,
                SortOrder::TimestampDesc,
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("--limit"));
        assert!(msg.contains("1000"));
        assert!(msg.contains("Phase 2"));
    }

    #[tokio::test]
    async fn search_accepts_limit_at_max_page_limit_boundary() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/v2/logs/events/search"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "filter": { "query": "*", "from": "now-1h", "to": "now" },
                "page": { "limit": MAX_PAGE_LIMIT },
                "sort": "-timestamp"
            })))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(sample_search_body()))
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        LogsApi::new(&client)
            .search(
                "*",
                "now-1h",
                "now",
                MAX_PAGE_LIMIT,
                SortOrder::TimestampDesc,
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn search_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/v2/logs/events/search"))
            .respond_with(
                wiremock::ResponseTemplate::new(400).set_body_string(r#"{"errors":["bad query"]}"#),
            )
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = LogsApi::new(&client)
            .search("???", "now-1h", "now", 10, SortOrder::TimestampDesc)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("400"));
        assert!(msg.contains("bad query"));
    }

    #[tokio::test]
    async fn search_propagates_network_errors() {
        let client = DatadogClient::new("http://127.0.0.1:1", "api", "app").unwrap();
        let err = LogsApi::new(&client)
            .search("*", "now-1h", "now", 10, SortOrder::TimestampDesc)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Failed to send"));
    }

    #[tokio::test]
    async fn search_errors_on_malformed_response() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/v2/logs/events/search"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = LogsApi::new(&client)
            .search("*", "now-1h", "now", 10, SortOrder::TimestampDesc)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Failed to parse"));
    }
}
