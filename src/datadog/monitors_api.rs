//! Datadog Monitors API wrapper.
//!
//! Exposes a thin façade over [`DatadogClient`] for the read-only monitor
//! endpoints needed by the CLI: list, get, and search. List and search
//! auto-paginate when called with `limit == 0`, capped at [`HARD_CAP`]
//! per the Phase 1 decisions on [#619].
//!
//! [#619]: https://github.com/rust-works/omni-dev/issues/619

use anyhow::{Context, Result};
use url::Url;

use crate::datadog::client::DatadogClient;
use crate::datadog::types::{Monitor, MonitorSearchResult};

/// Per-call upper bound on the number of monitors returned, even when
/// the caller passes `limit = 0` (fetch-all).
pub const HARD_CAP: usize = 10_000;

/// Default page size used when paginating list / search responses.
///
/// Datadog's `/api/v1/monitor` defaults to 100 and accepts up to 1000;
/// `/api/v1/monitor/search` defaults to 30 (which we keep so search
/// requests stay well within Datadog's per-query budget).
pub const LIST_PAGE_SIZE: usize = 100;
const SEARCH_PAGE_SIZE: usize = 30;

/// Filters accepted by `GET /api/v1/monitor`.
///
/// Each field is optional: the URL builder appends a query parameter
/// only when the field is `Some(_)`. `tags` and `monitor_tags` are
/// passed through verbatim — Datadog expects a comma-separated
/// `key:value` string.
#[derive(Debug, Default, Clone)]
pub struct MonitorListFilter {
    /// Substring match on the monitor name.
    pub name: Option<String>,
    /// Comma-separated `key:value` tags applied to the monitor.
    pub tags: Option<String>,
    /// Comma-separated `key:value` tags applied via `monitor_tags`.
    pub monitor_tags: Option<String>,
}

/// Monitors API façade.
#[derive(Debug)]
pub struct MonitorsApi<'a> {
    client: &'a DatadogClient,
}

impl<'a> MonitorsApi<'a> {
    /// Wraps an existing [`DatadogClient`] for monitor operations.
    #[must_use]
    pub fn new(client: &'a DatadogClient) -> Self {
        Self { client }
    }

    /// Lists monitors matching `filter`, auto-paginating as needed.
    ///
    /// `limit == 0` means "fetch every monitor up to [`HARD_CAP`]".
    /// Any non-zero `limit` is upper-bounded by [`HARD_CAP`] to keep a
    /// single CLI invocation from issuing more than 10k items' worth of
    /// requests.
    pub async fn list(&self, filter: &MonitorListFilter, limit: usize) -> Result<Vec<Monitor>> {
        let cap = effective_cap(limit);
        let mut out: Vec<Monitor> = Vec::new();
        let mut page: u32 = 0;
        loop {
            let remaining = cap - out.len();
            let page_size = remaining.min(LIST_PAGE_SIZE);
            let url = build_list_url(self.client.base_url(), filter, page, page_size)?;
            let response = self.client.get_json(url.as_str()).await?;
            if !response.status().is_success() {
                return Err(DatadogClient::response_to_error(response).await.into());
            }
            let batch: Vec<Monitor> = response
                .json()
                .await
                .context("Failed to parse /api/v1/monitor response")?;
            let exhausted = batch.len() < page_size;
            out.extend(batch);
            if out.len() >= cap || exhausted {
                break;
            }
            page += 1;
        }
        out.truncate(cap);
        Ok(out)
    }

    /// Fetches a single monitor by id.
    pub async fn get(&self, id: i64) -> Result<Monitor> {
        let url = build_get_url(self.client.base_url(), id)?;
        let response = self.client.get_json(url.as_str()).await?;
        if !response.status().is_success() {
            return Err(DatadogClient::response_to_error(response).await.into());
        }
        response
            .json::<Monitor>()
            .await
            .context("Failed to parse /api/v1/monitor/<id> response")
    }

    /// Searches monitors against the free-text / faceted query string,
    /// auto-paginating as needed.
    ///
    /// `limit == 0` means "fetch every match up to [`HARD_CAP`]". The
    /// returned envelope keeps `counts` and `metadata` from the first
    /// successful page; only the `monitors` list is concatenated across
    /// pages.
    pub async fn search(&self, query: &str, limit: usize) -> Result<MonitorSearchResult> {
        let cap = effective_cap(limit);
        let mut acc: Option<MonitorSearchResult> = None;
        let mut page: u32 = 0;
        loop {
            let collected = acc.as_ref().map_or(0, |r| r.monitors.len());
            let remaining = cap - collected;
            let per_page = remaining.min(SEARCH_PAGE_SIZE);
            let url = build_search_url(self.client.base_url(), query, page, per_page)?;
            let response = self.client.get_json(url.as_str()).await?;
            if !response.status().is_success() {
                return Err(DatadogClient::response_to_error(response).await.into());
            }
            let batch: MonitorSearchResult = response
                .json()
                .await
                .context("Failed to parse /api/v1/monitor/search response")?;
            let batch_len = batch.monitors.len();
            let page_count = batch.metadata.as_ref().and_then(|m| m.page_count);
            let exhausted_by_size = batch_len < per_page;
            let exhausted_by_metadata = page_count.is_some_and(|pc| i64::from(page) + 1 >= pc);

            match acc.as_mut() {
                Some(existing) => existing.monitors.extend(batch.monitors),
                None => acc = Some(batch),
            }

            let collected = acc.as_ref().map_or(0, |r| r.monitors.len());
            if collected >= cap || exhausted_by_size || exhausted_by_metadata {
                break;
            }
            page += 1;
        }
        let mut result = acc.unwrap_or_default();
        result.monitors.truncate(cap);
        Ok(result)
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

/// Builds `{base_url}/api/v1/monitor?{filters}&page=…&page_size=…`.
fn build_list_url(
    base_url: &str,
    filter: &MonitorListFilter,
    page: u32,
    page_size: usize,
) -> Result<Url> {
    let mut url =
        Url::parse(&format!("{base_url}/api/v1/monitor")).context("Invalid Datadog base URL")?;
    {
        let mut q = url.query_pairs_mut();
        if let Some(name) = filter.name.as_deref() {
            q.append_pair("name", name);
        }
        if let Some(tags) = filter.tags.as_deref() {
            q.append_pair("tags", tags);
        }
        if let Some(monitor_tags) = filter.monitor_tags.as_deref() {
            q.append_pair("monitor_tags", monitor_tags);
        }
        q.append_pair("page", &page.to_string());
        q.append_pair("page_size", &page_size.to_string());
    }
    Ok(url)
}

/// Builds `{base_url}/api/v1/monitor/{id}`.
fn build_get_url(base_url: &str, id: i64) -> Result<Url> {
    Url::parse(&format!("{base_url}/api/v1/monitor/{id}")).context("Invalid Datadog base URL")
}

/// Builds `{base_url}/api/v1/monitor/search?query=…&page=…&per_page=…`.
fn build_search_url(base_url: &str, query: &str, page: u32, per_page: usize) -> Result<Url> {
    let mut url = Url::parse(&format!("{base_url}/api/v1/monitor/search"))
        .context("Invalid Datadog base URL")?;
    url.query_pairs_mut()
        .append_pair("query", query)
        .append_pair("page", &page.to_string())
        .append_pair("per_page", &per_page.to_string());
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
        assert_eq!(effective_cap(42), 42);
    }

    // ── URL builders ───────────────────────────────────────────────

    #[test]
    fn build_list_url_appends_only_provided_filters() {
        let filter = MonitorListFilter {
            name: Some("cpu".into()),
            tags: None,
            monitor_tags: None,
        };
        let url = build_list_url("https://api.datadoghq.com", &filter, 0, 100).unwrap();
        let qs = url.query().unwrap();
        assert!(qs.contains("name=cpu"));
        assert!(qs.contains("page=0"));
        assert!(qs.contains("page_size=100"));
        assert!(!qs.contains("tags="));
        assert!(!qs.contains("monitor_tags="));
    }

    #[test]
    fn build_list_url_encodes_tags_and_monitor_tags() {
        let filter = MonitorListFilter {
            name: None,
            tags: Some("team:sre,env:prod".into()),
            monitor_tags: Some("severity:high".into()),
        };
        let url = build_list_url("https://api.datadoghq.com", &filter, 2, 50).unwrap();
        let qs = url.query().unwrap();
        // `:` and `,` get percent-encoded by url's form-encoder.
        assert!(qs.contains("tags=team%3Asre%2Cenv%3Aprod"));
        assert!(qs.contains("monitor_tags=severity%3Ahigh"));
        assert!(qs.contains("page=2"));
        assert!(qs.contains("page_size=50"));
    }

    #[test]
    fn build_list_url_rejects_invalid_base() {
        let err = build_list_url("not a url", &MonitorListFilter::default(), 0, 100).unwrap_err();
        assert!(err.to_string().contains("Invalid Datadog base URL"));
    }

    #[test]
    fn build_get_url_includes_id_path_segment() {
        let url = build_get_url("https://api.datadoghq.com", 12345).unwrap();
        assert_eq!(url.path(), "/api/v1/monitor/12345");
    }

    #[test]
    fn build_get_url_rejects_invalid_base() {
        let err = build_get_url("not a url", 1).unwrap_err();
        assert!(err.to_string().contains("Invalid Datadog base URL"));
    }

    #[test]
    fn build_search_url_encodes_query() {
        let url = build_search_url(
            "https://api.datadoghq.com",
            "status:alert AND env:prod",
            0,
            30,
        )
        .unwrap();
        let qs = url.query().unwrap();
        assert!(qs.contains("query=status%3Aalert+AND+env%3Aprod"));
        assert!(qs.contains("page=0"));
        assert!(qs.contains("per_page=30"));
    }

    #[test]
    fn build_search_url_rejects_invalid_base() {
        let err = build_search_url("not a url", "q", 0, 30).unwrap_err();
        assert!(err.to_string().contains("Invalid Datadog base URL"));
    }

    // ── fixtures ───────────────────────────────────────────────────

    fn monitor_json(id: i64, name: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "name": name,
            "type": "metric alert",
            "query": "avg(last_5m):avg:system.cpu.user{*} > 90",
            "tags": ["team:sre"],
            "overall_state": "OK"
        })
    }

    fn search_page(items: usize, page: i64, page_count: i64, total: i64) -> serde_json::Value {
        let monitors: Vec<serde_json::Value> = (0..items)
            .map(|i| {
                serde_json::json!({
                    "id": (page * 100) + i as i64,
                    "name": format!("Monitor {i}"),
                    "status": "ALERT",
                    "tags": ["env:prod"]
                })
            })
            .collect();
        serde_json::json!({
            "monitors": monitors,
            "counts": {},
            "metadata": {
                "page": page,
                "per_page": items as i64,
                "page_count": page_count,
                "total_count": total
            }
        })
    }

    // ── list happy path / pagination ───────────────────────────────

    #[tokio::test]
    async fn list_single_page_returns_parsed_monitors() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/monitor"))
            .and(wiremock::matchers::query_param("name", "cpu"))
            .and(wiremock::matchers::query_param("page", "0"))
            .and(wiremock::matchers::query_param("page_size", "5"))
            .and(wiremock::matchers::header("DD-API-KEY", "api"))
            .and(wiremock::matchers::header("DD-APPLICATION-KEY", "app"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([
                    monitor_json(1, "Disk full"),
                    monitor_json(2, "CPU high")
                ])),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let filter = MonitorListFilter {
            name: Some("cpu".into()),
            tags: None,
            monitor_tags: None,
        };
        let monitors = MonitorsApi::new(&client).list(&filter, 5).await.unwrap();
        assert_eq!(monitors.len(), 2);
        assert_eq!(monitors[0].id, 1);
        assert_eq!(monitors[1].name, "CPU high");
    }

    #[tokio::test]
    async fn list_auto_paginates_until_short_page() {
        // Three pages: [page 0]=100 items, [page 1]=100 items, [page 2]=37 items.
        let server = wiremock::MockServer::start().await;
        for page in 0..2 {
            let body: Vec<serde_json::Value> = (0..LIST_PAGE_SIZE as i64)
                .map(|i| monitor_json(page * 100 + i, "m"))
                .collect();
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/api/v1/monitor"))
                .and(wiremock::matchers::query_param("page", page.to_string()))
                .and(wiremock::matchers::query_param(
                    "page_size",
                    LIST_PAGE_SIZE.to_string(),
                ))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
                .expect(1)
                .mount(&server)
                .await;
        }
        let last_page: Vec<serde_json::Value> =
            (0..37_i64).map(|i| monitor_json(200 + i, "m")).collect();
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/monitor"))
            .and(wiremock::matchers::query_param("page", "2"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(last_page))
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let monitors = MonitorsApi::new(&client)
            .list(&MonitorListFilter::default(), 0)
            .await
            .unwrap();
        assert_eq!(monitors.len(), LIST_PAGE_SIZE * 2 + 37);
        // First and last ids check ordering preserved across pages.
        assert_eq!(monitors[0].id, 0);
        assert_eq!(monitors.last().unwrap().id, 236);
    }

    #[tokio::test]
    async fn list_caps_explicit_limit_at_hard_cap() {
        // The user asked for more than HARD_CAP — the API never sees a
        // request with page_size > LIST_PAGE_SIZE because we clamp first,
        // and we stop after HARD_CAP items.
        let server = wiremock::MockServer::start().await;
        let body: Vec<serde_json::Value> = (0..LIST_PAGE_SIZE as i64)
            .map(|i| monitor_json(i, "m"))
            .collect();
        // Mount a single mock that responds to *any* page request with a
        // full page; the loop will stop when it hits HARD_CAP.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/monitor"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let monitors = MonitorsApi::new(&client)
            .list(&MonitorListFilter::default(), HARD_CAP + 50)
            .await
            .unwrap();
        assert_eq!(monitors.len(), HARD_CAP);
    }

    #[tokio::test]
    async fn list_stops_when_explicit_limit_reached_within_first_page() {
        let server = wiremock::MockServer::start().await;
        // Limit 3 → page_size becomes 3; the API returns exactly 3 (a
        // "short page" by user's request), so we stop without page 1.
        let body: Vec<serde_json::Value> = (0..3_i64).map(|i| monitor_json(i, "m")).collect();
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/monitor"))
            .and(wiremock::matchers::query_param("page", "0"))
            .and(wiremock::matchers::query_param("page_size", "3"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let monitors = MonitorsApi::new(&client)
            .list(&MonitorListFilter::default(), 3)
            .await
            .unwrap();
        assert_eq!(monitors.len(), 3);
    }

    #[tokio::test]
    async fn list_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/monitor"))
            .respond_with(
                wiremock::ResponseTemplate::new(403).set_body_string(r#"{"errors":["nope"]}"#),
            )
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = MonitorsApi::new(&client)
            .list(&MonitorListFilter::default(), 5)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("403"));
        assert!(msg.contains("nope"));
    }

    #[tokio::test]
    async fn list_propagates_invalid_base_url_error() {
        let client = DatadogClient::new("not a url", "api", "app").unwrap();
        let err = MonitorsApi::new(&client)
            .list(&MonitorListFilter::default(), 5)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Invalid Datadog base URL"));
    }

    #[tokio::test]
    async fn list_propagates_network_errors() {
        // Point at a port that refuses connection; `get_json` surfaces the
        // reqwest send failure via anyhow context.
        let client = DatadogClient::new("http://127.0.0.1:1", "api", "app").unwrap();
        let err = MonitorsApi::new(&client)
            .list(&MonitorListFilter::default(), 5)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Failed to send"));
    }

    #[tokio::test]
    async fn list_errors_on_malformed_response() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/monitor"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = MonitorsApi::new(&client)
            .list(&MonitorListFilter::default(), 5)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Failed to parse"));
    }

    // ── get happy path ─────────────────────────────────────────────

    #[tokio::test]
    async fn get_returns_parsed_monitor() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/monitor/12345"))
            .and(wiremock::matchers::header("DD-API-KEY", "api"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(monitor_json(12345, "CPU high")),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let m = MonitorsApi::new(&client).get(12345).await.unwrap();
        assert_eq!(m.id, 12345);
        assert_eq!(m.name, "CPU high");
    }

    #[tokio::test]
    async fn get_propagates_404() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/monitor/9"))
            .respond_with(
                wiremock::ResponseTemplate::new(404).set_body_string(r#"{"errors":["Not found"]}"#),
            )
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = MonitorsApi::new(&client).get(9).await.unwrap_err();
        assert!(err.to_string().contains("404"));
        assert!(err.to_string().contains("Not found"));
    }

    #[tokio::test]
    async fn get_propagates_invalid_base_url_error() {
        let client = DatadogClient::new("not a url", "api", "app").unwrap();
        let err = MonitorsApi::new(&client).get(1).await.unwrap_err();
        assert!(err.to_string().contains("Invalid Datadog base URL"));
    }

    #[tokio::test]
    async fn get_propagates_network_errors() {
        let client = DatadogClient::new("http://127.0.0.1:1", "api", "app").unwrap();
        let err = MonitorsApi::new(&client).get(1).await.unwrap_err();
        assert!(err.to_string().contains("Failed to send"));
    }

    #[tokio::test]
    async fn get_errors_on_malformed_response() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/monitor/1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = MonitorsApi::new(&client).get(1).await.unwrap_err();
        assert!(err.to_string().contains("Failed to parse"));
    }

    // ── search happy path / pagination ─────────────────────────────

    #[tokio::test]
    async fn search_single_page_returns_envelope() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/monitor/search"))
            .and(wiremock::matchers::query_param("query", "status:alert"))
            .and(wiremock::matchers::query_param("page", "0"))
            .and(wiremock::matchers::query_param("per_page", "30"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(search_page(2, 0, 1, 2)),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let result = MonitorsApi::new(&client)
            .search("status:alert", 30)
            .await
            .unwrap();
        assert_eq!(result.monitors.len(), 2);
        assert_eq!(result.metadata.unwrap().total_count, Some(2));
    }

    #[tokio::test]
    async fn search_auto_paginates_with_unbounded_limit() {
        // page 0 + page 1 each return SEARCH_PAGE_SIZE items; metadata
        // says page_count = 2 so we stop without issuing page 2.
        let server = wiremock::MockServer::start().await;
        for page in 0..2_i64 {
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/api/v1/monitor/search"))
                .and(wiremock::matchers::query_param("page", page.to_string()))
                .respond_with(
                    wiremock::ResponseTemplate::new(200).set_body_json(search_page(
                        SEARCH_PAGE_SIZE,
                        page,
                        2,
                        60,
                    )),
                )
                .expect(1)
                .mount(&server)
                .await;
        }

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let result = MonitorsApi::new(&client).search("q", 0).await.unwrap();
        assert_eq!(result.monitors.len(), SEARCH_PAGE_SIZE * 2);
    }

    #[tokio::test]
    async fn search_stops_on_short_page_when_metadata_missing() {
        // page 0 returns fewer items than per_page → exhausted_by_size path.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/monitor/search"))
            .and(wiremock::matchers::query_param("page", "0"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "monitors": [
                        { "id": 1_i64, "name": "Only" }
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let result = MonitorsApi::new(&client).search("q", 0).await.unwrap();
        assert_eq!(result.monitors.len(), 1);
        assert!(result.metadata.is_none());
    }

    #[tokio::test]
    async fn search_caps_at_explicit_limit_within_full_page() {
        let server = wiremock::MockServer::start().await;
        // Limit 5 → per_page becomes 5; if Datadog returns 5 we stop.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/monitor/search"))
            .and(wiremock::matchers::query_param("page", "0"))
            .and(wiremock::matchers::query_param("per_page", "5"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(search_page(5, 0, 10, 100)),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let result = MonitorsApi::new(&client).search("q", 5).await.unwrap();
        assert_eq!(result.monitors.len(), 5);
    }

    #[tokio::test]
    async fn search_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/monitor/search"))
            .respond_with(
                wiremock::ResponseTemplate::new(400).set_body_string(r#"{"errors":["bad query"]}"#),
            )
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = MonitorsApi::new(&client)
            .search("???", 5)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("400"));
        assert!(msg.contains("bad query"));
    }

    #[tokio::test]
    async fn search_propagates_invalid_base_url_error() {
        let client = DatadogClient::new("not a url", "api", "app").unwrap();
        let err = MonitorsApi::new(&client).search("q", 5).await.unwrap_err();
        assert!(err.to_string().contains("Invalid Datadog base URL"));
    }

    #[tokio::test]
    async fn search_propagates_network_errors() {
        let client = DatadogClient::new("http://127.0.0.1:1", "api", "app").unwrap();
        let err = MonitorsApi::new(&client).search("q", 5).await.unwrap_err();
        assert!(err.to_string().contains("Failed to send"));
    }

    #[tokio::test]
    async fn search_errors_on_malformed_response() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/monitor/search"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = MonitorsApi::new(&client).search("q", 5).await.unwrap_err();
        assert!(err.to_string().contains("Failed to parse"));
    }
}
