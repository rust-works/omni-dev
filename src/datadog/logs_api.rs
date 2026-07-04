//! Datadog Logs API wrapper.
//!
//! Exposes a thin façade over [`DatadogClient`] for the v2 logs search
//! endpoint. Datadog v2 logs search uses **cursor pagination**
//! (`meta.page.after`), not offset. [`LogsApi::search`] issues a single
//! request optionally seeded with an `after` cursor token;
//! [`LogsApi::search_all`] auto-paginates up to a caller-supplied limit
//! (or [`HARD_CAP`] when the limit is `0`), mirroring [`MonitorsApi::list`].
//!
//! [`MonitorsApi::list`]: crate::datadog::monitors_api::MonitorsApi::list

use anyhow::Result;
use serde::Serialize;

use crate::datadog::client::DatadogClient;
use crate::datadog::types::{LogSearchResult, SortOrder};

/// Maximum page size accepted by `POST /api/v2/logs/events/search`.
///
/// Datadog rejects page sizes above 1000; the API client surfaces a
/// clearer error than the server's HTTP 400 by validating before the
/// request is sent.
pub const MAX_PAGE_LIMIT: usize = 1000;

/// Per-call upper bound on the number of log events returned by
/// [`LogsApi::search_all`], even when the caller passes `limit = 0`.
pub const HARD_CAP: usize = 10_000;

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
    /// Returns a single page only. When `after` is `Some`, Datadog
    /// resumes pagination at that cursor token (`page.cursor` in the
    /// request body). The next-page token is preserved on
    /// `meta.page.after` of the response so the caller (or
    /// [`LogsApi::search_all`]) can iterate.
    ///
    /// `limit` is rejected client-side when it exceeds [`MAX_PAGE_LIMIT`].
    pub async fn search(
        &self,
        query: &str,
        from: &str,
        to: &str,
        limit: usize,
        sort: SortOrder,
        after: Option<&str>,
    ) -> Result<LogSearchResult> {
        if limit > MAX_PAGE_LIMIT {
            return Err(anyhow::anyhow!(
                "`limit` must be <= {MAX_PAGE_LIMIT} (Datadog v2 logs search per-page cap; use `LogsApi::search_all` to auto-paginate across pages)"
            ));
        }
        let body = SearchRequest {
            filter: Filter { query, from, to },
            page: Page {
                limit,
                cursor: after,
            },
            sort,
        };
        let url = format!("{}/api/v2/logs/events/search", self.client.base_url());
        let response = self.client.post_json(&url, &body).await?;
        self.client
            .parse_response(
                response,
                "Failed to parse /api/v2/logs/events/search response",
            )
            .await
    }

    /// Searches log events, auto-paginating via cursor as needed.
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
    /// The returned envelope keeps the `meta` block from the *last*
    /// successful page so the response's cursor reflects the iterator's
    /// final position (typically `None` when the API is exhausted).
    pub async fn search_all(
        &self,
        query: &str,
        from: &str,
        to: &str,
        limit: usize,
        sort: SortOrder,
    ) -> Result<LogSearchResult> {
        let cap = effective_cap(limit);
        let mut acc: Option<LogSearchResult> = None;
        let mut cursor: Option<String> = None;
        loop {
            let collected = acc.as_ref().map_or(0, |r| r.data.len());
            let remaining = cap - collected;
            let page_size = remaining.min(MAX_PAGE_LIMIT);
            let page = self
                .search(query, from, to, page_size, sort, cursor.as_deref())
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

#[derive(Debug, Serialize)]
struct SearchRequest<'a> {
    filter: Filter<'a>,
    page: Page<'a>,
    sort: SortOrder,
}

#[derive(Debug, Serialize)]
struct Filter<'a> {
    query: &'a str,
    from: &'a str,
    to: &'a str,
}

#[derive(Debug, Serialize)]
struct Page<'a> {
    limit: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    cursor: Option<&'a str>,
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

    fn log_event_json(id: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "type": "log",
            "attributes": {
                "timestamp": "2026-04-22T10:00:00.000Z",
                "service": "api",
                "status": "info",
                "message": id,
                "tags": []
            }
        })
    }

    fn page_body(ids: &[&str], next_cursor: Option<&str>) -> serde_json::Value {
        let data: Vec<serde_json::Value> = ids.iter().map(|id| log_event_json(id)).collect();
        let meta = match next_cursor {
            Some(c) => serde_json::json!({ "page": { "after": c }, "status": "done" }),
            None => serde_json::json!({ "page": {}, "status": "done" }),
        };
        serde_json::json!({ "data": data, "meta": meta })
    }

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

    // ── search ─────────────────────────────────────────────────────

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
                None,
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
    async fn search_includes_cursor_in_body_when_after_is_some() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/v2/logs/events/search"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "filter": { "query": "*", "from": "now-1h", "to": "now" },
                "page": { "limit": 50, "cursor": "tok-2" },
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
                50,
                SortOrder::TimestampDesc,
                Some("tok-2"),
            )
            .await
            .unwrap();
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
            .search("*", "now-1h", "now", 50, SortOrder::TimestampAsc, None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn search_rejects_limit_above_max_page_limit_client_side() {
        let client = DatadogClient::new("http://127.0.0.1:1", "api", "app").unwrap();
        let err = LogsApi::new(&client)
            .search(
                "*",
                "now-1h",
                "now",
                MAX_PAGE_LIMIT + 1,
                SortOrder::TimestampDesc,
                None,
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("limit"));
        assert!(msg.contains("1000"));
        assert!(msg.contains("search_all"));
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
                None,
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
            .search("???", "now-1h", "now", 10, SortOrder::TimestampDesc, None)
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
            .search("*", "now-1h", "now", 10, SortOrder::TimestampDesc, None)
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
            .search("*", "now-1h", "now", 10, SortOrder::TimestampDesc, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Failed to parse"));
    }

    // ── search_all ─────────────────────────────────────────────────

    #[tokio::test]
    async fn search_all_single_page_when_response_has_no_cursor() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/v2/logs/events/search"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "filter": { "query": "*", "from": "now-1h", "to": "now" },
                "page": { "limit": 100 },
                "sort": "-timestamp"
            })))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(page_body(&["a", "b"], None)),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let result = LogsApi::new(&client)
            .search_all("*", "now-1h", "now", 100, SortOrder::TimestampDesc)
            .await
            .unwrap();
        assert_eq!(result.data.len(), 2);
    }

    #[tokio::test]
    async fn search_all_follows_cursor_until_no_more_pages() {
        // Page 1 returns 2 items + cursor "c1", page 2 returns 1 item + no
        // cursor. With `limit == 0`, the loop should issue both requests
        // and concatenate their data.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/v2/logs/events/search"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "filter": { "query": "*", "from": "now-1h", "to": "now" },
                "page": { "limit": MAX_PAGE_LIMIT },
                "sort": "-timestamp"
            })))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(page_body(&["a", "b"], Some("c1"))),
            )
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/v2/logs/events/search"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "filter": { "query": "*", "from": "now-1h", "to": "now" },
                "page": { "limit": MAX_PAGE_LIMIT, "cursor": "c1" },
                "sort": "-timestamp"
            })))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(page_body(&["c"], None)),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let result = LogsApi::new(&client)
            .search_all("*", "now-1h", "now", 0, SortOrder::TimestampDesc)
            .await
            .unwrap();
        let ids: Vec<&str> = result.data.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
        // The final response had no cursor — meta.page.after is None.
        assert!(result
            .meta
            .as_ref()
            .and_then(|m| m.page.as_ref())
            .and_then(|p| p.after.as_deref())
            .is_none());
    }

    #[tokio::test]
    async fn search_all_stops_at_explicit_limit_within_first_page() {
        // limit=5 → page_size=5; the API returns exactly 5 (a "short
        // page" by user's request) so we stop without a second call.
        let server = wiremock::MockServer::start().await;
        let ids = ["a", "b", "c", "d", "e"];
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/v2/logs/events/search"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "filter": { "query": "*", "from": "now-1h", "to": "now" },
                "page": { "limit": 5 },
                "sort": "-timestamp"
            })))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(page_body(&ids, Some("c1"))),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let result = LogsApi::new(&client)
            .search_all("*", "now-1h", "now", 5, SortOrder::TimestampDesc)
            .await
            .unwrap();
        assert_eq!(result.data.len(), 5);
    }

    #[tokio::test]
    async fn search_all_truncates_to_hard_cap_when_unbounded() {
        // The mock returns a full MAX_PAGE_LIMIT page + cursor on every
        // request, so the only stopping condition is HARD_CAP.
        let server = wiremock::MockServer::start().await;
        let full_page: Vec<serde_json::Value> = (0..MAX_PAGE_LIMIT)
            .map(|i| log_event_json(&format!("e{i}")))
            .collect();
        let body = serde_json::json!({
            "data": full_page,
            "meta": { "page": { "after": "always-more" } }
        });
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/v2/logs/events/search"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let result = LogsApi::new(&client)
            .search_all("*", "now-1h", "now", 0, SortOrder::TimestampDesc)
            .await
            .unwrap();
        assert_eq!(result.data.len(), HARD_CAP);
    }

    #[tokio::test]
    async fn search_all_propagates_api_errors_on_first_page() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/v2/logs/events/search"))
            .respond_with(
                wiremock::ResponseTemplate::new(403).set_body_string(r#"{"errors":["nope"]}"#),
            )
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = LogsApi::new(&client)
            .search_all("*", "now-1h", "now", 0, SortOrder::TimestampDesc)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("403"));
        assert!(msg.contains("nope"));
    }

    #[tokio::test]
    async fn search_all_caps_explicit_limit_at_hard_cap() {
        // Caller asked for HARD_CAP + 50; per-page mock returns full pages
        // forever. We stop at HARD_CAP without honouring the surplus.
        let server = wiremock::MockServer::start().await;
        let full_page: Vec<serde_json::Value> = (0..MAX_PAGE_LIMIT)
            .map(|i| log_event_json(&format!("e{i}")))
            .collect();
        let body = serde_json::json!({
            "data": full_page,
            "meta": { "page": { "after": "always-more" } }
        });
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/v2/logs/events/search"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let result = LogsApi::new(&client)
            .search_all(
                "*",
                "now-1h",
                "now",
                HARD_CAP + 50,
                SortOrder::TimestampDesc,
            )
            .await
            .unwrap();
        assert_eq!(result.data.len(), HARD_CAP);
    }
}
