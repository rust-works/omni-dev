//! Datadog Events API wrapper.
//!
//! Exposes a thin façade over [`DatadogClient`] for the read-only events
//! stream (`GET /api/v2/events`) needed by the CLI.
//!
//! Datadog v2 events use cursor pagination via `meta.page.after`; Phase 2
//! ships single-page only and preserves the cursor on the response so a
//! future iteration can extend without changing the wire types.

use anyhow::{Context, Result};
use url::Url;

use crate::datadog::client::DatadogClient;
use crate::datadog::types::EventsResponse;

/// Per-page upper bound enforced by Datadog's v2 events API.
pub const MAX_PAGE_LIMIT: usize = 1000;

/// Filters accepted by `GET /api/v2/events`.
///
/// Each field is optional: the URL builder appends a query parameter
/// only when the field is `Some(_)`. `from` / `to` are Unix epoch
/// **seconds** — the `EventsApi::list` method converts them to RFC 3339
/// before sending, matching the Datadog v2 API expectations.
#[derive(Debug, Default, Clone)]
pub struct EventsListFilter {
    /// Datadog events query string (e.g. `service:api`).
    pub query: Option<String>,
    /// Comma-separated list of source names (e.g. `aws,kubernetes`).
    pub sources: Option<String>,
    /// Comma-separated list of `key:value` tags.
    pub tags: Option<String>,
}

/// Events API façade.
#[derive(Debug)]
pub struct EventsApi<'a> {
    client: &'a DatadogClient,
}

impl<'a> EventsApi<'a> {
    /// Wraps an existing [`DatadogClient`] for events operations.
    #[must_use]
    pub fn new(client: &'a DatadogClient) -> Self {
        Self { client }
    }

    /// Lists events matching `filter` between `from` and `to` (RFC 3339
    /// strings) capped at `limit` per page.
    ///
    /// Single-page only; cursor pagination across pages is left to the
    /// caller via the `meta.page.after` field on the response.
    pub async fn list(
        &self,
        filter: &EventsListFilter,
        from: &str,
        to: &str,
        limit: usize,
    ) -> Result<EventsResponse> {
        if limit > MAX_PAGE_LIMIT {
            return Err(anyhow::anyhow!(
                "--limit must be <= {MAX_PAGE_LIMIT} (Datadog v2 events per-page cap; cursor pagination across pages is a follow-up)"
            ));
        }
        let url = build_list_url(self.client.base_url(), filter, from, to, limit)?;
        let response = self.client.get_json(url.as_str()).await?;
        if !response.status().is_success() {
            return Err(DatadogClient::response_to_error(response).await.into());
        }
        response
            .json::<EventsResponse>()
            .await
            .context("Failed to parse /api/v2/events response")
    }
}

/// Builds `{base_url}/api/v2/events?filter[query]=…&filter[from]=…&filter[to]=…&page[limit]=N`.
fn build_list_url(
    base_url: &str,
    filter: &EventsListFilter,
    from: &str,
    to: &str,
    limit: usize,
) -> Result<Url> {
    let mut url =
        Url::parse(&format!("{base_url}/api/v2/events")).context("Invalid Datadog base URL")?;
    {
        let mut q = url.query_pairs_mut();
        if let Some(query) = filter.query.as_deref() {
            q.append_pair("filter[query]", query);
        }
        if let Some(sources) = filter.sources.as_deref() {
            q.append_pair("filter[sources]", sources);
        }
        if let Some(tags) = filter.tags.as_deref() {
            q.append_pair("filter[tags]", tags);
        }
        q.append_pair("filter[from]", from);
        q.append_pair("filter[to]", to);
        q.append_pair("page[limit]", &limit.to_string());
    }
    Ok(url)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // ── URL builder ────────────────────────────────────────────────

    #[test]
    fn build_list_url_appends_only_provided_filters() {
        let filter = EventsListFilter {
            query: Some("service:api".into()),
            sources: None,
            tags: None,
        };
        let url = build_list_url(
            "https://api.datadoghq.com",
            &filter,
            "2026-04-22T09:00:00Z",
            "2026-04-22T10:00:00Z",
            50,
        )
        .unwrap();
        let qs = url.query().unwrap();
        assert!(qs.contains("filter%5Bquery%5D=service%3Aapi"));
        assert!(qs.contains("filter%5Bfrom%5D=2026-04-22T09%3A00%3A00Z"));
        assert!(qs.contains("filter%5Bto%5D=2026-04-22T10%3A00%3A00Z"));
        assert!(qs.contains("page%5Blimit%5D=50"));
        assert!(!qs.contains("filter%5Bsources%5D"));
        assert!(!qs.contains("filter%5Btags%5D"));
    }

    #[test]
    fn build_list_url_encodes_sources_and_tags() {
        let filter = EventsListFilter {
            query: None,
            sources: Some("aws,kubernetes".into()),
            tags: Some("env:prod,team:sre".into()),
        };
        let url = build_list_url(
            "https://api.datadoghq.com",
            &filter,
            "2026-04-22T09:00:00Z",
            "2026-04-22T10:00:00Z",
            10,
        )
        .unwrap();
        let qs = url.query().unwrap();
        assert!(qs.contains("filter%5Bsources%5D=aws%2Ckubernetes"));
        assert!(qs.contains("filter%5Btags%5D=env%3Aprod%2Cteam%3Asre"));
    }

    #[test]
    fn build_list_url_rejects_invalid_base() {
        let err = build_list_url(
            "not a url",
            &EventsListFilter::default(),
            "2026-04-22T09:00:00Z",
            "2026-04-22T10:00:00Z",
            10,
        )
        .unwrap_err();
        assert!(err.to_string().contains("Invalid Datadog base URL"));
    }

    // ── fixtures ───────────────────────────────────────────────────

    fn sample_body() -> serde_json::Value {
        serde_json::json!({
            "data": [
                {
                    "id": "EV1",
                    "type": "event",
                    "attributes": {
                        "timestamp": "2026-04-22T10:00:00.000Z",
                        "title": "Deploy",
                        "source": "github",
                        "tags": ["env:prod"]
                    }
                }
            ],
            "meta": {"page": {"after": "next"}, "status": "done"}
        })
    }

    // ── happy path ─────────────────────────────────────────────────

    #[tokio::test]
    async fn list_sends_filters_and_returns_parsed_response() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v2/events"))
            .and(wiremock::matchers::query_param(
                "filter[query]",
                "service:api",
            ))
            .and(wiremock::matchers::query_param(
                "filter[from]",
                "2026-04-22T09:00:00Z",
            ))
            .and(wiremock::matchers::query_param(
                "filter[to]",
                "2026-04-22T10:00:00Z",
            ))
            .and(wiremock::matchers::query_param("page[limit]", "10"))
            .and(wiremock::matchers::header("DD-API-KEY", "api"))
            .and(wiremock::matchers::header("DD-APPLICATION-KEY", "app"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(sample_body()))
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let result = EventsApi::new(&client)
            .list(
                &EventsListFilter {
                    query: Some("service:api".into()),
                    sources: None,
                    tags: None,
                },
                "2026-04-22T09:00:00Z",
                "2026-04-22T10:00:00Z",
                10,
            )
            .await
            .unwrap();
        assert_eq!(result.data.len(), 1);
        assert_eq!(result.data[0].id, "EV1");
    }

    // ── client-side / API errors ───────────────────────────────────

    #[tokio::test]
    async fn list_rejects_limit_above_max_page_limit_client_side() {
        let client = DatadogClient::new("http://127.0.0.1:1", "api", "app").unwrap();
        let err = EventsApi::new(&client)
            .list(
                &EventsListFilter::default(),
                "2026-04-22T09:00:00Z",
                "2026-04-22T10:00:00Z",
                MAX_PAGE_LIMIT + 1,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("--limit"));
        assert!(err.to_string().contains(&MAX_PAGE_LIMIT.to_string()));
    }

    #[tokio::test]
    async fn list_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v2/events"))
            .respond_with(
                wiremock::ResponseTemplate::new(403).set_body_string(r#"{"errors":["nope"]}"#),
            )
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = EventsApi::new(&client)
            .list(
                &EventsListFilter::default(),
                "2026-04-22T09:00:00Z",
                "2026-04-22T10:00:00Z",
                10,
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("403"));
        assert!(msg.contains("nope"));
    }

    #[tokio::test]
    async fn list_propagates_invalid_base_url_error() {
        let client = DatadogClient::new("not a url", "api", "app").unwrap();
        let err = EventsApi::new(&client)
            .list(
                &EventsListFilter::default(),
                "2026-04-22T09:00:00Z",
                "2026-04-22T10:00:00Z",
                10,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Invalid Datadog base URL"));
    }

    #[tokio::test]
    async fn list_propagates_network_errors() {
        let client = DatadogClient::new("http://127.0.0.1:1", "api", "app").unwrap();
        let err = EventsApi::new(&client)
            .list(
                &EventsListFilter::default(),
                "2026-04-22T09:00:00Z",
                "2026-04-22T10:00:00Z",
                10,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Failed to send"));
    }

    #[tokio::test]
    async fn list_errors_on_malformed_response() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v2/events"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = EventsApi::new(&client)
            .list(
                &EventsListFilter::default(),
                "2026-04-22T09:00:00Z",
                "2026-04-22T10:00:00Z",
                10,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Failed to parse"));
    }
}
