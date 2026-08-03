//! Gmail Threads API wrapper.
//!
//! Same cursor-pagination shape as [`crate::gmail::messages_api`].
//! Read-only in Phase 1 — `threads.modify`/`.trash` are real Gmail
//! endpoints but outside this issue's stated surface, an explicit non-goal
//! rather than an oversight.

use anyhow::Result;
use url::Url;

use crate::gmail::client::GmailClient;
use crate::gmail::types::{Thread, ThreadListResponse};

/// Maximum page size accepted by `GET /gmail/v1/users/{userId}/threads`.
pub const MAX_PAGE_LIMIT: usize = 500;

/// Per-call upper bound on the number of threads returned by
/// [`ThreadsApi::search_all`], even when the caller passes `limit = 0`.
pub const HARD_CAP: usize = 10_000;

/// The `format` query parameter accepted by `threads.get`.
///
/// Deliberately has no `Raw` variant — meaningless for a thread (a thread's
/// whole point is showing the conversation's parsed messages), unlike
/// [`crate::gmail::messages_api::MessageFormat`].
#[derive(Debug, Clone, Copy, Default)]
pub enum ThreadFormat {
    /// Only `id`/`historyId` per message — no headers or body.
    Minimal,
    /// The full parsed MIME structure for every message. Default.
    #[default]
    Full,
    /// Headers and snippet only per message, no body.
    Metadata,
}

impl ThreadFormat {
    fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Full => "full",
            Self::Metadata => "metadata",
        }
    }
}

/// Threads API façade.
#[derive(Debug)]
pub struct ThreadsApi<'a> {
    client: &'a GmailClient,
}

impl<'a> ThreadsApi<'a> {
    /// Wraps an existing [`GmailClient`] for thread operations.
    #[must_use]
    pub fn new(client: &'a GmailClient) -> Self {
        Self { client }
    }

    /// Searches threads matching `query`, returning a single page.
    ///
    /// `limit` is rejected client-side when it exceeds [`MAX_PAGE_LIMIT`];
    /// use [`Self::search_all`] to auto-paginate across pages.
    pub async fn search(
        &self,
        query: Option<&str>,
        label_ids: &[&str],
        limit: usize,
        page_token: Option<&str>,
    ) -> Result<ThreadListResponse> {
        if limit > MAX_PAGE_LIMIT {
            return Err(anyhow::anyhow!(
                "`limit` must be <= {MAX_PAGE_LIMIT} (Gmail threads.list per-page cap; use \
                 `search_all` to auto-paginate)"
            ));
        }
        let url =
            build_threads_list_url(self.client.base_url(), query, label_ids, limit, page_token)?;
        self.client
            .get_parsed(url.as_str(), "Failed to parse threads.list response")
            .await
    }

    /// Searches threads, auto-paginating via cursor as needed.
    ///
    /// Same "cursor-only, never a short page" termination rule as
    /// [`crate::gmail::messages_api::MessagesApi::search_all`].
    pub async fn search_all(
        &self,
        query: Option<&str>,
        label_ids: &[&str],
        limit: usize,
    ) -> Result<ThreadListResponse> {
        let cap = effective_cap(limit);
        let mut acc: Option<ThreadListResponse> = None;
        let mut page_token: Option<String> = None;
        loop {
            let collected = acc.as_ref().map_or(0, |r| r.threads.len());
            let page_size = (cap - collected).min(MAX_PAGE_LIMIT);
            let page = self
                .search(query, label_ids, page_size, page_token.as_deref())
                .await?;
            let next_token = page.next_page_token.clone();
            match acc.as_mut() {
                Some(existing) => {
                    existing.threads.extend(page.threads);
                    existing.next_page_token = page.next_page_token;
                    existing.result_size_estimate = page.result_size_estimate;
                }
                None => acc = Some(page),
            }
            let collected = acc.as_ref().map_or(0, |r| r.threads.len());
            if collected >= cap || next_token.is_none() {
                break;
            }
            page_token = next_token;
        }
        let mut result = acc.unwrap_or_default();
        result.threads.truncate(cap);
        Ok(result)
    }

    /// Fetches a single thread (with its messages) by id.
    pub async fn get(&self, id: &str, format: ThreadFormat) -> Result<Thread> {
        let url = build_thread_get_url(self.client.base_url(), id, format)?;
        self.client
            .get_parsed(url.as_str(), "Failed to parse threads.get response")
            .await
    }
}

fn build_threads_list_url(
    base_url: &str,
    query: Option<&str>,
    label_ids: &[&str],
    limit: usize,
    page_token: Option<&str>,
) -> Result<Url> {
    let mut url = GmailClient::api_url(base_url, "/gmail/v1/users/me/threads")?;
    let query = query.filter(|q| !q.is_empty());
    // Only touch `query_pairs_mut()` when there's something to append —
    // calling it unconditionally leaves a bare trailing `?` even with zero
    // pairs appended.
    if query.is_some() || !label_ids.is_empty() || limit > 0 || page_token.is_some() {
        let mut pairs = url.query_pairs_mut();
        if let Some(q) = query {
            pairs.append_pair("q", q);
        }
        for label in label_ids {
            pairs.append_pair("labelIds", label);
        }
        if limit > 0 {
            pairs.append_pair("maxResults", &limit.to_string());
        }
        if let Some(token) = page_token {
            pairs.append_pair("pageToken", token);
        }
    }
    Ok(url)
}

fn build_thread_get_url(base_url: &str, id: &str, format: ThreadFormat) -> Result<Url> {
    let mut url = GmailClient::api_url(base_url, &format!("/gmail/v1/users/me/threads/{id}"))?;
    url.query_pairs_mut().append_pair("format", format.as_str());
    Ok(url)
}

/// Clamps a caller-supplied limit to [`HARD_CAP`], treating `0` as "fetch
/// as many as the cap allows".
fn effective_cap(limit: usize) -> usize {
    if limit == 0 {
        HARD_CAP
    } else {
        limit.min(HARD_CAP)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::gmail::auth::{GmailCredentials, GmailScope};
    use crate::utils::secret::Secret;

    fn test_credentials() -> GmailCredentials {
        GmailCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: GmailScope::ReadOnly,
        }
    }

    fn dead_client() -> GmailClient {
        // Routes the session's token endpoint to the same dead address —
        // otherwise `GmailSession` would try to refresh against the real
        // Google token endpoint before the API call is ever attempted.
        let mut client = GmailClient::new("http://127.0.0.1:1", &test_credentials()).unwrap();
        crate::gmail::client::test_support::replace_session(
            &mut client,
            &test_credentials(),
            "http://127.0.0.1:1",
        );
        client
    }

    async fn client_with_bootstrapped_token(server: &wiremock::MockServer) -> GmailClient {
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/token"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "test-token",
                    "expires_in": 3600,
                })),
            )
            .mount(server)
            .await;

        let mut client = GmailClient::new(&server.uri(), &test_credentials()).unwrap();
        crate::gmail::client::test_support::replace_session(
            &mut client,
            &test_credentials(),
            &format!("{}/token", server.uri()),
        );
        client
    }

    fn thread_ref_json(id: &str) -> serde_json::Value {
        serde_json::json!({"id": id})
    }

    fn page_body(ids: &[&str], next_token: Option<&str>) -> serde_json::Value {
        let threads: Vec<serde_json::Value> = ids.iter().map(|id| thread_ref_json(id)).collect();
        let mut body = serde_json::json!({"threads": threads});
        if let Some(token) = next_token {
            body["nextPageToken"] = serde_json::json!(token);
        }
        body
    }

    // ── URL builders (pure) ──────────────────────────────────────────

    #[test]
    fn build_threads_list_url_with_only_provided_filters() {
        let url =
            build_threads_list_url("https://gmail.googleapis.com", None, &[], 0, None).unwrap();
        assert_eq!(
            url.as_str(),
            "https://gmail.googleapis.com/gmail/v1/users/me/threads"
        );
    }

    #[test]
    fn build_threads_list_url_with_full_filter_set() {
        let url = build_threads_list_url(
            "https://gmail.googleapis.com",
            Some("label:finance"),
            &["INBOX"],
            25,
            Some("cursor-1"),
        )
        .unwrap();
        let query: Vec<_> = url.query_pairs().collect();
        assert!(query.contains(&("q".into(), "label:finance".into())));
        assert!(query.contains(&("labelIds".into(), "INBOX".into())));
        assert!(query.contains(&("maxResults".into(), "25".into())));
        assert!(query.contains(&("pageToken".into(), "cursor-1".into())));
    }

    #[test]
    fn build_threads_list_url_rejects_invalid_base_url() {
        let err = build_threads_list_url("not a url", None, &[], 0, None).unwrap_err();
        assert!(err.to_string().contains("Invalid Gmail base URL"));
    }

    #[test]
    fn build_thread_get_url_uses_thread_format_query_param_not_message_format() {
        let url =
            build_thread_get_url("https://gmail.googleapis.com", "t1", ThreadFormat::Metadata)
                .unwrap();
        assert!(url
            .query_pairs()
            .any(|pair| pair == ("format".into(), "metadata".into())));
    }

    // ── Standard error paths ─────────────────────────────────────────

    #[tokio::test]
    async fn search_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/threads"))
            .respond_with(wiremock::ResponseTemplate::new(400).set_body_string("bad query"))
            .mount(&server)
            .await;

        let err = ThreadsApi::new(&client)
            .search(Some("???"), &[], 10, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("400"));
    }

    #[tokio::test]
    async fn search_rejects_limit_above_max_page_limit_client_side() {
        let client = dead_client();
        let err = ThreadsApi::new(&client)
            .search(None, &[], MAX_PAGE_LIMIT + 1, None)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("limit"));
        assert!(msg.contains("search_all"));
    }

    #[tokio::test]
    async fn search_propagates_network_errors() {
        // `dead_client()` also points the session's token endpoint at the
        // dead address, so the failure surfaces during token acquisition
        // before the threads.list request is ever attempted.
        let client = dead_client();
        let err = ThreadsApi::new(&client)
            .search(None, &[], 10, None)
            .await
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("Failed to obtain a Gmail access token"));
    }

    #[tokio::test]
    async fn search_errors_on_malformed_response() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/threads"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let err = ThreadsApi::new(&client)
            .search(None, &[], 10, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Failed to parse"));
    }

    // ── Pagination ────────────────────────────────────────────────────

    #[tokio::test]
    async fn search_all_single_page_when_no_next_token() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/threads"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(page_body(&["a", "b"], None)),
            )
            .expect(1)
            .mount(&server)
            .await;

        let result = ThreadsApi::new(&client)
            .search_all(None, &[], 100)
            .await
            .unwrap();
        assert_eq!(result.threads.len(), 2);
    }

    #[tokio::test]
    async fn search_all_follows_next_page_token_to_exhaustion() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/threads"))
            .and(wiremock::matchers::query_param_is_missing("pageToken"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(page_body(&["a", "b"], Some("c1"))),
            )
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/threads"))
            .and(wiremock::matchers::query_param("pageToken", "c1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(page_body(&["c"], None)),
            )
            .expect(1)
            .mount(&server)
            .await;

        let result = ThreadsApi::new(&client)
            .search_all(None, &[], 0)
            .await
            .unwrap();
        let ids: Vec<&str> = result.threads.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
    }

    #[tokio::test]
    async fn search_all_stops_at_explicit_limit() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/threads"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(page_body(&["a", "b", "c"], Some("more"))),
            )
            .expect(1)
            .mount(&server)
            .await;

        let result = ThreadsApi::new(&client)
            .search_all(None, &[], 3)
            .await
            .unwrap();
        assert_eq!(result.threads.len(), 3);
    }

    #[tokio::test]
    async fn search_all_truncates_to_hard_cap() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let full_page: Vec<serde_json::Value> = (0..MAX_PAGE_LIMIT)
            .map(|i| thread_ref_json(&format!("t{i}")))
            .collect();
        let body = serde_json::json!({"threads": full_page, "nextPageToken": "always-more"});
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/threads"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let result = ThreadsApi::new(&client)
            .search_all(None, &[], 0)
            .await
            .unwrap();
        assert_eq!(result.threads.len(), HARD_CAP);
    }

    #[tokio::test]
    async fn search_all_continues_past_empty_page_with_a_valid_next_page_token() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/threads"))
            .and(wiremock::matchers::query_param_is_missing("pageToken"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(page_body(&[], Some("p2"))),
            )
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/threads"))
            .and(wiremock::matchers::query_param("pageToken", "p2"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(page_body(&["a"], None)),
            )
            .expect(1)
            .mount(&server)
            .await;

        let result = ThreadsApi::new(&client)
            .search_all(Some("rare-query"), &[], 0)
            .await
            .unwrap();
        assert_eq!(result.threads.len(), 1);
    }

    #[tokio::test]
    async fn search_all_propagates_api_errors_on_first_page() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/threads"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("nope"))
            .mount(&server)
            .await;

        let err = ThreadsApi::new(&client)
            .search_all(None, &[], 0)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    // ── get ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn get_returns_thread_with_messages() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/threads/t1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "t1",
                    "messages": [{"id": "m1", "threadId": "t1"}],
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let thread = ThreadsApi::new(&client)
            .get("t1", ThreadFormat::Full)
            .await
            .unwrap();
        assert_eq!(thread.id, "t1");
        assert_eq!(thread.messages.len(), 1);
    }

    #[tokio::test]
    async fn get_sends_minimal_format_query_param() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/threads/t1"))
            .and(wiremock::matchers::query_param("format", "minimal"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "t1"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let thread = ThreadsApi::new(&client)
            .get("t1", ThreadFormat::Minimal)
            .await
            .unwrap();
        assert_eq!(thread.id, "t1");
    }

    // ── effective_cap ─────────────────────────────────────────────────

    #[test]
    fn effective_cap_zero_is_hard_cap() {
        assert_eq!(effective_cap(0), HARD_CAP);
    }

    #[test]
    fn effective_cap_clamps_above_hard_cap() {
        assert_eq!(effective_cap(HARD_CAP + 5), HARD_CAP);
    }
}
