//! Datadog SLO API wrapper.
//!
//! Exposes a thin façade over [`DatadogClient`] for the read-only Service
//! Level Objective endpoints needed by the CLI: list and get. List
//! auto-paginates when called with `limit == 0`, capped at [`HARD_CAP`].

use anyhow::{Context, Result};
use url::Url;

use crate::datadog::client::DatadogClient;
use crate::datadog::types::{Slo, SloGetResponse, SloListResponse};

/// Per-call upper bound on the number of SLOs returned.
pub const HARD_CAP: usize = 10_000;

/// Default page size. Datadog's `/api/v1/slo` accepts up to 1000 per
/// page; 50 keeps payloads small and matches the UI's default.
pub const LIST_PAGE_SIZE: usize = 50;

/// Filters accepted by `GET /api/v1/slo`.
#[derive(Debug, Default, Clone)]
pub struct SloListFilter {
    /// Comma-separated list of `key:value` tags applied to the SLO.
    pub tags: Option<String>,
    /// Free-text query (Datadog's `query` parameter — substring match).
    pub query: Option<String>,
    /// Comma-separated list of SLO ids.
    pub ids: Option<String>,
    /// Comma-separated list of metric names referenced by the SLO.
    pub metrics: Option<String>,
}

/// SLO API façade.
#[derive(Debug)]
pub struct SloApi<'a> {
    client: &'a DatadogClient,
}

impl<'a> SloApi<'a> {
    /// Wraps an existing [`DatadogClient`] for SLO operations.
    #[must_use]
    pub fn new(client: &'a DatadogClient) -> Self {
        Self { client }
    }

    /// Lists SLOs matching `filter`, auto-paginating as needed.
    ///
    /// `limit == 0` means "fetch every match up to [`HARD_CAP`]".
    pub async fn list(&self, filter: &SloListFilter, limit: usize) -> Result<Vec<Slo>> {
        let cap = effective_cap(limit);
        let mut out: Vec<Slo> = Vec::new();
        let mut offset: usize = 0;
        loop {
            let remaining = cap - out.len();
            let page_size = remaining.min(LIST_PAGE_SIZE);
            let url = build_list_url(self.client.base_url(), filter, offset, page_size)?;
            let response = self.client.get_json(url.as_str()).await?;
            if !response.status().is_success() {
                return Err(DatadogClient::response_to_error(response).await.into());
            }
            let parsed: SloListResponse = response
                .json()
                .await
                .context("Failed to parse /api/v1/slo response")?;
            let exhausted = parsed.data.len() < page_size;
            let batch_len = parsed.data.len();
            out.extend(parsed.data);
            if out.len() >= cap || exhausted || batch_len == 0 {
                break;
            }
            offset += batch_len;
        }
        out.truncate(cap);
        Ok(out)
    }

    /// Fetches a single SLO definition by id.
    pub async fn get(&self, id: &str) -> Result<Slo> {
        let url = build_get_url(self.client.base_url(), id)?;
        let response = self.client.get_json(url.as_str()).await?;
        if !response.status().is_success() {
            return Err(DatadogClient::response_to_error(response).await.into());
        }
        let parsed: SloGetResponse = response
            .json()
            .await
            .context("Failed to parse /api/v1/slo/<id> response")?;
        Ok(parsed.data)
    }
}

/// Clamps a caller-supplied limit to [`HARD_CAP`], treating `0` as
/// "fetch as many as the cap allows".
fn effective_cap(limit: usize) -> usize {
    if limit == 0 {
        HARD_CAP
    } else {
        limit.min(HARD_CAP)
    }
}

fn build_list_url(
    base_url: &str,
    filter: &SloListFilter,
    offset: usize,
    limit: usize,
) -> Result<Url> {
    let mut url =
        Url::parse(&format!("{base_url}/api/v1/slo")).context("Invalid Datadog base URL")?;
    {
        let mut q = url.query_pairs_mut();
        if let Some(tags) = filter.tags.as_deref() {
            q.append_pair("tags_query", tags);
        }
        if let Some(query) = filter.query.as_deref() {
            q.append_pair("query", query);
        }
        if let Some(ids) = filter.ids.as_deref() {
            q.append_pair("ids", ids);
        }
        if let Some(metrics) = filter.metrics.as_deref() {
            q.append_pair("metrics_query", metrics);
        }
        q.append_pair("offset", &offset.to_string());
        q.append_pair("limit", &limit.to_string());
    }
    Ok(url)
}

fn build_get_url(base_url: &str, id: &str) -> Result<Url> {
    let mut url =
        Url::parse(&format!("{base_url}/api/v1/slo")).context("Invalid Datadog base URL")?;
    url.path_segments_mut()
        .map_err(|()| anyhow::anyhow!("Invalid Datadog base URL: cannot append path segment"))?
        .push(id);
    Ok(url)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // ── effective_cap ──────────────────────────────────────────────

    #[test]
    fn effective_cap_zero_means_hard_cap() {
        assert_eq!(effective_cap(0), HARD_CAP);
    }

    #[test]
    fn effective_cap_clamps_to_hard_cap() {
        assert_eq!(effective_cap(HARD_CAP + 5), HARD_CAP);
    }

    #[test]
    fn effective_cap_passes_through_small_limits() {
        assert_eq!(effective_cap(7), 7);
    }

    // ── URL builders ───────────────────────────────────────────────

    #[test]
    fn build_list_url_appends_only_provided_filters() {
        let filter = SloListFilter {
            tags: Some("team:sre".into()),
            query: None,
            ids: None,
            metrics: None,
        };
        let url = build_list_url("https://api.datadoghq.com", &filter, 0, 50).unwrap();
        let qs = url.query().unwrap();
        assert!(qs.contains("tags_query=team%3Asre"));
        assert!(qs.contains("offset=0"));
        assert!(qs.contains("limit=50"));
        // Param keys absent when their `Option` is `None`. Use anchored
        // substrings so `tags_query` doesn't satisfy a bare `query=` match.
        let keys: Vec<String> = url.query_pairs().map(|(k, _)| k.into_owned()).collect();
        assert!(!keys.iter().any(|k| k == "query"));
        assert!(!keys.iter().any(|k| k == "ids"));
        assert!(!keys.iter().any(|k| k == "metrics_query"));
    }

    #[test]
    fn build_list_url_appends_all_filters_when_present() {
        let filter = SloListFilter {
            tags: Some("env:prod".into()),
            query: Some("latency".into()),
            ids: Some("a,b".into()),
            metrics: Some("system.cpu".into()),
        };
        let url = build_list_url("https://api.datadoghq.com", &filter, 25, 10).unwrap();
        let qs = url.query().unwrap();
        assert!(qs.contains("tags_query=env%3Aprod"));
        assert!(qs.contains("query=latency"));
        assert!(qs.contains("ids=a%2Cb"));
        assert!(qs.contains("metrics_query=system.cpu"));
        assert!(qs.contains("offset=25"));
        assert!(qs.contains("limit=10"));
    }

    #[test]
    fn build_list_url_rejects_invalid_base() {
        let err = build_list_url("not a url", &SloListFilter::default(), 0, 50).unwrap_err();
        assert!(err.to_string().contains("Invalid Datadog base URL"));
    }

    #[test]
    fn build_get_url_includes_id_path_segment() {
        let url = build_get_url("https://api.datadoghq.com", "abc-def").unwrap();
        assert_eq!(url.path(), "/api/v1/slo/abc-def");
    }

    #[test]
    fn build_get_url_percent_encodes_reserved_chars_in_id() {
        let url = build_get_url("https://api.datadoghq.com", "weird/id").unwrap();
        assert_eq!(url.path(), "/api/v1/slo/weird%2Fid");
    }

    #[test]
    fn build_get_url_rejects_invalid_base() {
        let err = build_get_url("not a url", "id").unwrap_err();
        assert!(err.to_string().contains("Invalid Datadog base URL"));
    }

    #[test]
    fn build_get_url_rejects_cannot_be_a_base_scheme() {
        let err = build_get_url("mailto:test@example.com", "id").unwrap_err();
        assert!(err.to_string().contains("cannot append path segment"));
    }

    // ── fixtures ───────────────────────────────────────────────────

    fn slo_json(id: &str, name: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "name": name,
            "type": "metric",
            "tags": ["team:sre"],
            "monitor_ids": []
        })
    }

    // ── list happy / pagination ────────────────────────────────────

    #[tokio::test]
    async fn list_single_page_returns_parsed_slos() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/slo"))
            .and(wiremock::matchers::query_param("tags_query", "team:sre"))
            .and(wiremock::matchers::query_param("offset", "0"))
            .and(wiremock::matchers::query_param("limit", "5"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"data": [slo_json("a", "A"), slo_json("b", "B")]}),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let slos = SloApi::new(&client)
            .list(
                &SloListFilter {
                    tags: Some("team:sre".into()),
                    query: None,
                    ids: None,
                    metrics: None,
                },
                5,
            )
            .await
            .unwrap();
        assert_eq!(slos.len(), 2);
        assert_eq!(slos[0].id, "a");
    }

    #[tokio::test]
    async fn list_auto_paginates_across_pages() {
        let server = wiremock::MockServer::start().await;
        // Page 0 returns LIST_PAGE_SIZE (full page).
        let body0: Vec<serde_json::Value> = (0..LIST_PAGE_SIZE)
            .map(|i| slo_json(&format!("p0-{i}"), "x"))
            .collect();
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/slo"))
            .and(wiremock::matchers::query_param("offset", "0"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"data": body0})),
            )
            .expect(1)
            .mount(&server)
            .await;
        // Page 1 returns a short page → loop stops.
        let body1: Vec<serde_json::Value> =
            (0..7).map(|i| slo_json(&format!("p1-{i}"), "y")).collect();
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/slo"))
            .and(wiremock::matchers::query_param(
                "offset",
                LIST_PAGE_SIZE.to_string(),
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"data": body1})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let slos = SloApi::new(&client)
            .list(&SloListFilter::default(), 0)
            .await
            .unwrap();
        assert_eq!(slos.len(), LIST_PAGE_SIZE + 7);
        assert_eq!(slos[0].id, "p0-0");
    }

    #[tokio::test]
    async fn list_caps_at_explicit_limit() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/slo"))
            .and(wiremock::matchers::query_param("limit", "3"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "data": [slo_json("a", "A"), slo_json("b", "B"), slo_json("c", "C")]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let slos = SloApi::new(&client)
            .list(&SloListFilter::default(), 3)
            .await
            .unwrap();
        assert_eq!(slos.len(), 3);
    }

    #[tokio::test]
    async fn list_stops_on_empty_page() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/slo"))
            .and(wiremock::matchers::query_param("offset", "0"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": []})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let slos = SloApi::new(&client)
            .list(&SloListFilter::default(), 0)
            .await
            .unwrap();
        assert!(slos.is_empty());
    }

    #[tokio::test]
    async fn list_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/slo"))
            .respond_with(
                wiremock::ResponseTemplate::new(403).set_body_string(r#"{"errors":["nope"]}"#),
            )
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = SloApi::new(&client)
            .list(&SloListFilter::default(), 5)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("403"));
        assert!(msg.contains("nope"));
    }

    #[tokio::test]
    async fn list_propagates_invalid_base_url_error() {
        let client = DatadogClient::new("not a url", "api", "app").unwrap();
        let err = SloApi::new(&client)
            .list(&SloListFilter::default(), 5)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Invalid Datadog base URL"));
    }

    #[tokio::test]
    async fn list_propagates_network_errors() {
        let client = DatadogClient::new("http://127.0.0.1:1", "api", "app").unwrap();
        let err = SloApi::new(&client)
            .list(&SloListFilter::default(), 5)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Failed to send"));
    }

    #[tokio::test]
    async fn list_errors_on_malformed_response() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/slo"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = SloApi::new(&client)
            .list(&SloListFilter::default(), 5)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Failed to parse"));
    }

    // ── get happy / errors ─────────────────────────────────────────

    #[tokio::test]
    async fn get_returns_unwrapped_slo() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/slo/abc-def"))
            .and(wiremock::matchers::header("DD-API-KEY", "api"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"data": slo_json("abc-def", "Latency")})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let s = SloApi::new(&client).get("abc-def").await.unwrap();
        assert_eq!(s.id, "abc-def");
        assert_eq!(s.name, "Latency");
    }

    #[tokio::test]
    async fn get_propagates_404() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/slo/missing"))
            .respond_with(
                wiremock::ResponseTemplate::new(404).set_body_string(r#"{"errors":["nope"]}"#),
            )
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = SloApi::new(&client).get("missing").await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("404"));
        assert!(msg.contains("nope"));
    }

    #[tokio::test]
    async fn get_propagates_invalid_base_url_error() {
        let client = DatadogClient::new("not a url", "api", "app").unwrap();
        let err = SloApi::new(&client).get("x").await.unwrap_err();
        assert!(err.to_string().contains("Invalid Datadog base URL"));
    }

    #[tokio::test]
    async fn get_propagates_network_errors() {
        let client = DatadogClient::new("http://127.0.0.1:1", "api", "app").unwrap();
        let err = SloApi::new(&client).get("x").await.unwrap_err();
        assert!(err.to_string().contains("Failed to send"));
    }

    #[tokio::test]
    async fn get_errors_on_malformed_response() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/slo/x"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = SloApi::new(&client).get("x").await.unwrap_err();
        assert!(err.to_string().contains("Failed to parse"));
    }
}
