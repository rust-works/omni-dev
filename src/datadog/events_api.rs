//! Datadog Events API wrapper.
//!
//! Exposes a thin façade over [`DatadogClient`] for the read-only events
//! stream (`GET /api/v2/events`).
//!
//! Datadog v2 events use cursor pagination via `meta.page.after`.
//! [`EventsApi::list`] issues a single request optionally seeded with an
//! `after` cursor token; [`EventsApi::list_all`] auto-paginates up to a
//! caller-supplied limit (or [`HARD_CAP`] when the limit is `0`),
//! mirroring [`MonitorsApi::list`].
//!
//! [`MonitorsApi::list`]: crate::datadog::monitors_api::MonitorsApi::list

use anyhow::{Context, Result};
use url::Url;

use crate::datadog::client::DatadogClient;
use crate::datadog::types::EventsResponse;

/// Per-page upper bound enforced by Datadog's v2 events API.
pub const MAX_PAGE_LIMIT: usize = 1000;

/// Per-call upper bound on the number of events returned by
/// [`EventsApi::list_all`], even when the caller passes `limit = 0`.
pub const HARD_CAP: usize = 10_000;

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
    /// Single-page only. When `after` is `Some`, Datadog resumes
    /// pagination at that cursor token (`page[cursor]` in the query
    /// string). The next-page token is preserved on `meta.page.after`
    /// of the response so callers (or [`EventsApi::list_all`]) can
    /// iterate.
    pub async fn list(
        &self,
        filter: &EventsListFilter,
        from: &str,
        to: &str,
        limit: usize,
        after: Option<&str>,
    ) -> Result<EventsResponse> {
        if limit > MAX_PAGE_LIMIT {
            return Err(anyhow::anyhow!(
                "`limit` must be <= {MAX_PAGE_LIMIT} (Datadog v2 events per-page cap; use `EventsApi::list_all` to auto-paginate across pages)"
            ));
        }
        let url = build_list_url(self.client.base_url(), filter, from, to, limit, after)?;
        let response = self.client.get_json(url.as_str()).await?;
        if !response.status().is_success() {
            return Err(DatadogClient::response_to_error(response).await.into());
        }
        response
            .json::<EventsResponse>()
            .await
            .context("Failed to parse /api/v2/events response")
    }

    /// Lists events, auto-paginating via cursor as needed.
    ///
    /// `limit == 0` means "fetch every match up to [`HARD_CAP`]". Any
    /// non-zero `limit` is upper-bounded by [`HARD_CAP`] to keep a single
    /// invocation from issuing more than 10k items' worth of requests.
    /// Per-request page size is clamped to [`MAX_PAGE_LIMIT`].
    ///
    /// Termination follows cursor-pagination semantics: the loop stops
    /// when the response omits `meta.page.after` (Datadog signals "no
    /// more pages" only via the absent cursor — a short page on its own
    /// is *not* a terminator) or when `cap` items have been collected.
    ///
    /// The returned envelope keeps the `meta` and `links` blocks from the
    /// *last* successful page so the response's cursor reflects the
    /// iterator's final position (typically `None` when the API is
    /// exhausted).
    pub async fn list_all(
        &self,
        filter: &EventsListFilter,
        from: &str,
        to: &str,
        limit: usize,
    ) -> Result<EventsResponse> {
        let cap = effective_cap(limit);
        let mut acc: Option<EventsResponse> = None;
        let mut cursor: Option<String> = None;
        loop {
            let collected = acc.as_ref().map_or(0, |r| r.data.len());
            let remaining = cap - collected;
            let page_size = remaining.min(MAX_PAGE_LIMIT);
            let page = self
                .list(filter, from, to, page_size, cursor.as_deref())
                .await?;
            let next_cursor = page
                .meta
                .as_ref()
                .and_then(|m| m.page.as_ref())
                .and_then(|p| p.after.clone());
            match acc.as_mut() {
                Some(existing) => {
                    existing.data.extend(page.data);
                    existing.meta = page.meta;
                    existing.links = page.links;
                }
                None => acc = Some(page),
            }
            let collected = acc.as_ref().map_or(0, |r| r.data.len());
            if collected >= cap || next_cursor.is_none() {
                break;
            }
            cursor = next_cursor;
        }
        let mut result = acc.unwrap_or_default();
        result.data.truncate(cap);
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

/// Builds `{base_url}/api/v2/events?filter[query]=…&filter[from]=…&filter[to]=…&page[limit]=N&page[cursor]=…`.
fn build_list_url(
    base_url: &str,
    filter: &EventsListFilter,
    from: &str,
    to: &str,
    limit: usize,
    after: Option<&str>,
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
        if let Some(cursor) = after {
            q.append_pair("page[cursor]", cursor);
        }
    }
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
            None,
        )
        .unwrap();
        let qs = url.query().unwrap();
        assert!(qs.contains("filter%5Bquery%5D=service%3Aapi"));
        assert!(qs.contains("filter%5Bfrom%5D=2026-04-22T09%3A00%3A00Z"));
        assert!(qs.contains("filter%5Bto%5D=2026-04-22T10%3A00%3A00Z"));
        assert!(qs.contains("page%5Blimit%5D=50"));
        assert!(!qs.contains("filter%5Bsources%5D"));
        assert!(!qs.contains("filter%5Btags%5D"));
        assert!(!qs.contains("page%5Bcursor%5D"));
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
            None,
        )
        .unwrap();
        let qs = url.query().unwrap();
        assert!(qs.contains("filter%5Bsources%5D=aws%2Ckubernetes"));
        assert!(qs.contains("filter%5Btags%5D=env%3Aprod%2Cteam%3Asre"));
    }

    #[test]
    fn build_list_url_appends_cursor_when_provided() {
        let url = build_list_url(
            "https://api.datadoghq.com",
            &EventsListFilter::default(),
            "2026-04-22T09:00:00Z",
            "2026-04-22T10:00:00Z",
            10,
            Some("tok-2"),
        )
        .unwrap();
        let qs = url.query().unwrap();
        assert!(qs.contains("page%5Bcursor%5D=tok-2"));
    }

    #[test]
    fn build_list_url_rejects_invalid_base() {
        let err = build_list_url(
            "not a url",
            &EventsListFilter::default(),
            "2026-04-22T09:00:00Z",
            "2026-04-22T10:00:00Z",
            10,
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("Invalid Datadog base URL"));
    }

    // ── fixtures ───────────────────────────────────────────────────

    fn event_json(id: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "type": "event",
            "attributes": {
                "timestamp": "2026-04-22T10:00:00.000Z",
                "title": "Deploy",
                "source": "github",
                "tags": ["env:prod"]
            }
        })
    }

    fn page_body(ids: &[&str], next_cursor: Option<&str>) -> serde_json::Value {
        let data: Vec<serde_json::Value> = ids.iter().map(|id| event_json(id)).collect();
        let meta = match next_cursor {
            Some(c) => serde_json::json!({ "page": { "after": c }, "status": "done" }),
            None => serde_json::json!({ "page": {}, "status": "done" }),
        };
        serde_json::json!({ "data": data, "meta": meta })
    }

    fn sample_body() -> serde_json::Value {
        serde_json::json!({
            "data": [event_json("EV1")],
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
                None,
            )
            .await
            .unwrap();
        assert_eq!(result.data.len(), 1);
        assert_eq!(result.data[0].id, "EV1");
    }

    #[tokio::test]
    async fn list_includes_cursor_in_query_when_after_is_some() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v2/events"))
            .and(wiremock::matchers::query_param("page[cursor]", "tok-2"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(sample_body()))
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        EventsApi::new(&client)
            .list(
                &EventsListFilter::default(),
                "2026-04-22T09:00:00Z",
                "2026-04-22T10:00:00Z",
                10,
                Some("tok-2"),
            )
            .await
            .unwrap();
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
                None,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("limit"));
        assert!(err.to_string().contains(&MAX_PAGE_LIMIT.to_string()));
        assert!(err.to_string().contains("list_all"));
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
                None,
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
                None,
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
                None,
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
                None,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Failed to parse"));
    }

    // ── list_all ───────────────────────────────────────────────────

    #[tokio::test]
    async fn list_all_single_page_when_response_has_no_cursor() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v2/events"))
            .and(wiremock::matchers::query_param("page[limit]", "100"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(page_body(&["a", "b"], None)),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let result = EventsApi::new(&client)
            .list_all(
                &EventsListFilter::default(),
                "2026-04-22T09:00:00Z",
                "2026-04-22T10:00:00Z",
                100,
            )
            .await
            .unwrap();
        assert_eq!(result.data.len(), 2);
    }

    #[tokio::test]
    async fn list_all_follows_cursor_until_no_more_pages() {
        // Page 1 returns 2 events + cursor "c1"; page 2 returns 1 event +
        // no cursor. With `limit == 0`, the loop should issue both
        // requests and concatenate their data.
        let server = wiremock::MockServer::start().await;
        let limit_str = MAX_PAGE_LIMIT.to_string();
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v2/events"))
            .and(wiremock::matchers::query_param(
                "page[limit]",
                limit_str.as_str(),
            ))
            .and(wiremock::matchers::query_param_is_missing("page[cursor]"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(page_body(&["a", "b"], Some("c1"))),
            )
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v2/events"))
            .and(wiremock::matchers::query_param("page[cursor]", "c1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(page_body(&["c"], None)),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let result = EventsApi::new(&client)
            .list_all(
                &EventsListFilter::default(),
                "2026-04-22T09:00:00Z",
                "2026-04-22T10:00:00Z",
                0,
            )
            .await
            .unwrap();
        let ids: Vec<&str> = result.data.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
        assert!(result
            .meta
            .as_ref()
            .and_then(|m| m.page.as_ref())
            .and_then(|p| p.after.as_deref())
            .is_none());
    }

    #[tokio::test]
    async fn list_all_stops_at_explicit_limit_within_first_page() {
        let server = wiremock::MockServer::start().await;
        let ids = ["a", "b", "c"];
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v2/events"))
            .and(wiremock::matchers::query_param("page[limit]", "3"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(page_body(&ids, Some("c1"))),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let result = EventsApi::new(&client)
            .list_all(
                &EventsListFilter::default(),
                "2026-04-22T09:00:00Z",
                "2026-04-22T10:00:00Z",
                3,
            )
            .await
            .unwrap();
        assert_eq!(result.data.len(), 3);
    }

    #[tokio::test]
    async fn list_all_truncates_to_hard_cap_when_unbounded() {
        let server = wiremock::MockServer::start().await;
        let full_page: Vec<serde_json::Value> = (0..MAX_PAGE_LIMIT)
            .map(|i| event_json(&format!("e{i}")))
            .collect();
        let body = serde_json::json!({
            "data": full_page,
            "meta": { "page": { "after": "always-more" } }
        });
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v2/events"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let result = EventsApi::new(&client)
            .list_all(
                &EventsListFilter::default(),
                "2026-04-22T09:00:00Z",
                "2026-04-22T10:00:00Z",
                0,
            )
            .await
            .unwrap();
        assert_eq!(result.data.len(), HARD_CAP);
    }

    #[tokio::test]
    async fn list_all_propagates_api_errors_on_first_page() {
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
            .list_all(
                &EventsListFilter::default(),
                "2026-04-22T09:00:00Z",
                "2026-04-22T10:00:00Z",
                0,
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("403"));
        assert!(msg.contains("nope"));
    }

    #[tokio::test]
    async fn list_all_caps_explicit_limit_at_hard_cap() {
        let server = wiremock::MockServer::start().await;
        let full_page: Vec<serde_json::Value> = (0..MAX_PAGE_LIMIT)
            .map(|i| event_json(&format!("e{i}")))
            .collect();
        let body = serde_json::json!({
            "data": full_page,
            "meta": { "page": { "after": "always-more" } }
        });
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v2/events"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let result = EventsApi::new(&client)
            .list_all(
                &EventsListFilter::default(),
                "2026-04-22T09:00:00Z",
                "2026-04-22T10:00:00Z",
                HARD_CAP + 50,
            )
            .await
            .unwrap();
        assert_eq!(result.data.len(), HARD_CAP);
    }
}
