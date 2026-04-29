//! Datadog Hosts API wrapper.
//!
//! Exposes a thin façade over [`DatadogClient`] for the read-only hosts
//! endpoint (`GET /api/v1/hosts`). Auto-paginates via `start` / `count`
//! query parameters, capped at [`HARD_CAP`] hosts per invocation.

use anyhow::{Context, Result};
use url::Url;

use crate::datadog::client::DatadogClient;
use crate::datadog::types::{Host, HostsResponse};

/// Per-call upper bound on the number of hosts returned.
pub const HARD_CAP: usize = 10_000;

/// Default page size. Datadog accepts up to 1000 per page; 100 keeps
/// individual responses small while still being efficient for the
/// auto-pagination loop.
pub const LIST_PAGE_SIZE: usize = 100;

/// Filters accepted by `GET /api/v1/hosts`.
#[derive(Debug, Default, Clone)]
pub struct HostsListFilter {
    /// Free-text query (Datadog's `filter` parameter).
    pub filter: Option<String>,
    /// Cutoff in Unix epoch seconds; hosts last reporting before this
    /// are filtered out.
    pub from: Option<i64>,
    /// `up` / `tags` — sort field (rarely used; kept for completeness).
    pub sort_field: Option<String>,
    /// `asc` / `desc`.
    pub sort_dir: Option<String>,
    /// Whether to include muted hosts (default: yes).
    pub include_muted_hosts_data: Option<bool>,
    /// Whether to include host metadata blob (default: yes).
    pub include_hosts_metadata: Option<bool>,
}

/// Hosts API façade.
#[derive(Debug)]
pub struct HostsApi<'a> {
    client: &'a DatadogClient,
}

impl<'a> HostsApi<'a> {
    /// Wraps an existing [`DatadogClient`] for hosts operations.
    #[must_use]
    pub fn new(client: &'a DatadogClient) -> Self {
        Self { client }
    }

    /// Lists hosts matching `filter`, auto-paginating as needed.
    ///
    /// `limit == 0` means "fetch every match up to [`HARD_CAP`]". The
    /// `total_returned` and `total_matching` fields on the returned
    /// envelope reflect the aggregate across all pages issued.
    pub async fn list(&self, filter: &HostsListFilter, limit: usize) -> Result<HostsResponse> {
        let cap = effective_cap(limit);
        let mut hosts: Vec<Host> = Vec::new();
        let mut start: usize = 0;
        let mut total_matching: Option<i64> = None;
        loop {
            let remaining = cap - hosts.len();
            let count = remaining.min(LIST_PAGE_SIZE);
            let url = build_list_url(self.client.base_url(), filter, start, count)?;
            let response = self.client.get_json(url.as_str()).await?;
            if !response.status().is_success() {
                return Err(DatadogClient::response_to_error(response).await.into());
            }
            let parsed: HostsResponse = response
                .json()
                .await
                .context("Failed to parse /api/v1/hosts response")?;
            let batch_len = parsed.host_list.len();
            let exhausted = batch_len < count;
            if total_matching.is_none() {
                total_matching = parsed.total_matching;
            }
            hosts.extend(parsed.host_list);
            if hosts.len() >= cap || exhausted || batch_len == 0 {
                break;
            }
            start += batch_len;
        }
        hosts.truncate(cap);
        let returned = i64::try_from(hosts.len()).unwrap_or(i64::MAX);
        Ok(HostsResponse {
            host_list: hosts,
            total_returned: Some(returned),
            total_matching,
        })
    }
}

fn effective_cap(limit: usize) -> usize {
    if limit == 0 {
        HARD_CAP
    } else {
        limit.min(HARD_CAP)
    }
}

fn build_list_url(
    base_url: &str,
    filter: &HostsListFilter,
    start: usize,
    count: usize,
) -> Result<Url> {
    let mut url =
        Url::parse(&format!("{base_url}/api/v1/hosts")).context("Invalid Datadog base URL")?;
    {
        let mut q = url.query_pairs_mut();
        if let Some(f) = filter.filter.as_deref() {
            q.append_pair("filter", f);
        }
        if let Some(from) = filter.from {
            q.append_pair("from", &from.to_string());
        }
        if let Some(field) = filter.sort_field.as_deref() {
            q.append_pair("sort_field", field);
        }
        if let Some(dir) = filter.sort_dir.as_deref() {
            q.append_pair("sort_dir", dir);
        }
        if let Some(b) = filter.include_muted_hosts_data {
            q.append_pair("include_muted_hosts_data", if b { "true" } else { "false" });
        }
        if let Some(b) = filter.include_hosts_metadata {
            q.append_pair("include_hosts_metadata", if b { "true" } else { "false" });
        }
        q.append_pair("start", &start.to_string());
        q.append_pair("count", &count.to_string());
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
        assert_eq!(effective_cap(13), 13);
    }

    // ── URL builder ────────────────────────────────────────────────

    #[test]
    fn build_list_url_appends_only_provided_filters() {
        let filter = HostsListFilter {
            filter: Some("env:prod".into()),
            ..HostsListFilter::default()
        };
        let url = build_list_url("https://api.datadoghq.com", &filter, 0, 100).unwrap();
        let qs = url.query().unwrap();
        assert!(qs.contains("filter=env%3Aprod"));
        assert!(qs.contains("start=0"));
        assert!(qs.contains("count=100"));
        assert!(!qs.contains("from="));
        assert!(!qs.contains("sort_field="));
        assert!(!qs.contains("include_muted_hosts_data="));
    }

    #[test]
    fn build_list_url_appends_full_filter_set() {
        let filter = HostsListFilter {
            filter: Some("apps:nginx".into()),
            from: Some(1_700_000_000),
            sort_field: Some("up".into()),
            sort_dir: Some("desc".into()),
            include_muted_hosts_data: Some(false),
            include_hosts_metadata: Some(true),
        };
        let url = build_list_url("https://api.datadoghq.com", &filter, 100, 50).unwrap();
        let qs = url.query().unwrap();
        assert!(qs.contains("filter=apps%3Anginx"));
        assert!(qs.contains("from=1700000000"));
        assert!(qs.contains("sort_field=up"));
        assert!(qs.contains("sort_dir=desc"));
        assert!(qs.contains("include_muted_hosts_data=false"));
        assert!(qs.contains("include_hosts_metadata=true"));
        assert!(qs.contains("start=100"));
        assert!(qs.contains("count=50"));
    }

    #[test]
    fn build_list_url_inverted_booleans_take_other_arms() {
        // `build_list_url_appends_full_filter_set` covers
        // `include_muted_hosts_data=false` and `include_hosts_metadata=true`;
        // this case exercises the reciprocal arms of both ternaries.
        let filter = HostsListFilter {
            include_muted_hosts_data: Some(true),
            include_hosts_metadata: Some(false),
            ..HostsListFilter::default()
        };
        let url = build_list_url("https://api.datadoghq.com", &filter, 0, 10).unwrap();
        let qs = url.query().unwrap();
        assert!(qs.contains("include_muted_hosts_data=true"));
        assert!(qs.contains("include_hosts_metadata=false"));
    }

    #[test]
    fn build_list_url_rejects_invalid_base() {
        let err = build_list_url("not a url", &HostsListFilter::default(), 0, 100).unwrap_err();
        assert!(err.to_string().contains("Invalid Datadog base URL"));
    }

    // ── fixtures ───────────────────────────────────────────────────

    fn host_json(name: &str) -> serde_json::Value {
        serde_json::json!({
            "name": name,
            "aliases": [],
            "apps": ["nginx"],
            "up": true,
            "last_reported_time": 1_700_000_000_i64,
            "sources": ["agent"]
        })
    }

    // ── happy path / pagination ────────────────────────────────────

    #[tokio::test]
    async fn list_single_page_returns_envelope() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/hosts"))
            .and(wiremock::matchers::query_param("filter", "env:prod"))
            .and(wiremock::matchers::query_param("start", "0"))
            .and(wiremock::matchers::query_param("count", "5"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "host_list": [host_json("web-01"), host_json("web-02")],
                    "total_returned": 2_i64,
                    "total_matching": 5_i64
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let result = HostsApi::new(&client)
            .list(
                &HostsListFilter {
                    filter: Some("env:prod".into()),
                    ..HostsListFilter::default()
                },
                5,
            )
            .await
            .unwrap();
        assert_eq!(result.host_list.len(), 2);
        assert_eq!(result.total_returned, Some(2));
        assert_eq!(result.total_matching, Some(5));
    }

    #[tokio::test]
    async fn list_auto_paginates_until_short_page() {
        let server = wiremock::MockServer::start().await;
        let body0: Vec<serde_json::Value> = (0..LIST_PAGE_SIZE)
            .map(|i| host_json(&format!("h{i}")))
            .collect();
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/hosts"))
            .and(wiremock::matchers::query_param("start", "0"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "host_list": body0,
                    "total_matching": 137_i64
                })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let body1: Vec<serde_json::Value> =
            (0..37).map(|i| host_json(&format!("h-late-{i}"))).collect();
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/hosts"))
            .and(wiremock::matchers::query_param(
                "start",
                LIST_PAGE_SIZE.to_string(),
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"host_list": body1})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let result = HostsApi::new(&client)
            .list(&HostsListFilter::default(), 0)
            .await
            .unwrap();
        assert_eq!(result.host_list.len(), LIST_PAGE_SIZE + 37);
        assert_eq!(result.total_matching, Some(137));
        assert_eq!(result.total_returned, Some((LIST_PAGE_SIZE + 37) as i64));
    }

    #[tokio::test]
    async fn list_caps_at_explicit_limit() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/hosts"))
            .and(wiremock::matchers::query_param("count", "3"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "host_list": [host_json("a"), host_json("b"), host_json("c")]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let result = HostsApi::new(&client)
            .list(&HostsListFilter::default(), 3)
            .await
            .unwrap();
        assert_eq!(result.host_list.len(), 3);
    }

    #[tokio::test]
    async fn list_stops_on_empty_page() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/hosts"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"host_list": []})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let result = HostsApi::new(&client)
            .list(&HostsListFilter::default(), 0)
            .await
            .unwrap();
        assert!(result.host_list.is_empty());
        assert_eq!(result.total_returned, Some(0));
    }

    #[tokio::test]
    async fn list_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/hosts"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = HostsApi::new(&client)
            .list(&HostsListFilter::default(), 5)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    #[tokio::test]
    async fn list_propagates_invalid_base_url_error() {
        let client = DatadogClient::new("not a url", "api", "app").unwrap();
        let err = HostsApi::new(&client)
            .list(&HostsListFilter::default(), 5)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Invalid Datadog base URL"));
    }

    #[tokio::test]
    async fn list_propagates_network_errors() {
        let client = DatadogClient::new("http://127.0.0.1:1", "api", "app").unwrap();
        let err = HostsApi::new(&client)
            .list(&HostsListFilter::default(), 5)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Failed to send"));
    }

    #[tokio::test]
    async fn list_errors_on_malformed_response() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/hosts"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = HostsApi::new(&client)
            .list(&HostsListFilter::default(), 5)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Failed to parse"));
    }
}
