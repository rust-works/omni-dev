//! Gmail History API wrapper.
//!
//! Same cursor-pagination shape as
//! [`crate::gmail::messages_api::MessagesApi`], but `startHistoryId` is a
//! required parameter rather than an optional filter. Recovered (and
//! adapted — see [`crate::gmail::types::HistoryMessageRef`]) from Phase 1's
//! `0db5a605` deletion; `gmail sync`'s incremental path
//! (`src/cli/gmail/sync/engine.rs`) is the intended, and first, caller.

use anyhow::Result;
use url::Url;

use crate::gmail::client::GmailClient;
use crate::gmail::types::HistoryListResponse;

/// Maximum page size accepted by `GET /gmail/v1/users/{userId}/history`.
pub const MAX_PAGE_LIMIT: usize = 500;

/// Per-call upper bound on the number of history records returned by
/// [`HistoryApi::list_all`], even when the caller passes `limit = 0`.
pub const HARD_CAP: usize = 10_000;

/// History API façade.
#[derive(Debug)]
pub struct HistoryApi<'a> {
    client: &'a GmailClient,
}

impl<'a> HistoryApi<'a> {
    /// Wraps an existing [`GmailClient`] for history operations.
    #[must_use]
    pub fn new(client: &'a GmailClient) -> Self {
        Self { client }
    }

    /// Lists mailbox changes since `start_history_id`, returning a single
    /// page.
    ///
    /// Google returns 404 `notFound` when `start_history_id` is older than
    /// the mailbox's retention window (about a week); `gmail sync` catches
    /// that specific case and falls back to a full reconciliation pass.
    pub async fn list(
        &self,
        start_history_id: &str,
        history_types: &[&str],
        limit: usize,
        page_token: Option<&str>,
    ) -> Result<HistoryListResponse> {
        if limit > MAX_PAGE_LIMIT {
            return Err(anyhow::anyhow!(
                "`limit` must be <= {MAX_PAGE_LIMIT} (Gmail history.list per-page cap; use \
                 `list_all` to auto-paginate)"
            ));
        }
        let url = build_history_list_url(
            self.client.base_url(),
            start_history_id,
            history_types,
            limit,
            page_token,
        )?;
        self.client
            .get_parsed(url.as_str(), "Failed to parse history.list response")
            .await
    }

    /// Lists mailbox changes since `start_history_id`, auto-paginating via
    /// cursor as needed. Same "cursor-only, never a short page" termination
    /// rule as the other façades.
    pub async fn list_all(
        &self,
        start_history_id: &str,
        history_types: &[&str],
        limit: usize,
    ) -> Result<HistoryListResponse> {
        let cap = effective_cap(limit);
        let mut acc: Option<HistoryListResponse> = None;
        let mut page_token: Option<String> = None;
        loop {
            let collected = acc.as_ref().map_or(0, |r| r.history.len());
            let page_size = (cap - collected).min(MAX_PAGE_LIMIT);
            let page = self
                .list(
                    start_history_id,
                    history_types,
                    page_size,
                    page_token.as_deref(),
                )
                .await?;
            let next_token = page.next_page_token.clone();
            match acc.as_mut() {
                Some(existing) => {
                    existing.history.extend(page.history);
                    existing.next_page_token = page.next_page_token;
                    existing.history_id = page.history_id;
                }
                None => acc = Some(page),
            }
            let collected = acc.as_ref().map_or(0, |r| r.history.len());
            if collected >= cap || next_token.is_none() {
                break;
            }
            page_token = next_token;
        }
        let mut result = acc.unwrap_or_default();
        result.history.truncate(cap);
        Ok(result)
    }
}

fn build_history_list_url(
    base_url: &str,
    start_history_id: &str,
    history_types: &[&str],
    limit: usize,
    page_token: Option<&str>,
) -> Result<Url> {
    let mut url = GmailClient::api_url(base_url, "/gmail/v1/users/me/history")?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("startHistoryId", start_history_id);
        for history_type in history_types {
            pairs.append_pair("historyTypes", history_type);
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

    fn history_record_json(id: &str) -> serde_json::Value {
        serde_json::json!({"id": id})
    }

    fn page_body(
        ids: &[&str],
        next_token: Option<&str>,
        final_history_id: Option<&str>,
    ) -> serde_json::Value {
        let history: Vec<serde_json::Value> =
            ids.iter().map(|id| history_record_json(id)).collect();
        let mut body = serde_json::json!({"history": history});
        if let Some(token) = next_token {
            body["nextPageToken"] = serde_json::json!(token);
        }
        if let Some(hid) = final_history_id {
            body["historyId"] = serde_json::json!(hid);
        }
        body
    }

    // ── URL builders (pure) ──────────────────────────────────────────

    #[test]
    fn build_history_list_url_always_includes_start_history_id() {
        let url =
            build_history_list_url("https://gmail.googleapis.com", "1000", &[], 0, None).unwrap();
        assert!(url
            .query_pairs()
            .any(|pair| pair == ("startHistoryId".into(), "1000".into())));
    }

    #[test]
    fn build_history_list_url_repeats_history_types() {
        let url = build_history_list_url(
            "https://gmail.googleapis.com",
            "1000",
            &["messageAdded", "labelAdded"],
            0,
            None,
        )
        .unwrap();
        let query: Vec<_> = url.query_pairs().collect();
        assert!(query.contains(&("historyTypes".into(), "messageAdded".into())));
        assert!(query.contains(&("historyTypes".into(), "labelAdded".into())));
    }

    #[test]
    fn build_history_list_url_rejects_invalid_base_url() {
        let err = build_history_list_url("not a url", "1000", &[], 0, None).unwrap_err();
        assert!(err.to_string().contains("Invalid Gmail base URL"));
    }

    // ── Standard error paths ─────────────────────────────────────────

    #[tokio::test]
    async fn list_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/history"))
            .respond_with(wiremock::ResponseTemplate::new(400).set_body_string("bad request"))
            .mount(&server)
            .await;

        let err = HistoryApi::new(&client)
            .list("1000", &[], 10, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("400"));
    }

    #[tokio::test]
    async fn list_propagates_404_not_found_for_expired_start_history_id() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/history"))
            .respond_with(
                wiremock::ResponseTemplate::new(404).set_body_json(serde_json::json!({
                    "error": {"message": "Not Found", "errors": [{"reason": "notFound"}]}
                })),
            )
            .mount(&server)
            .await;

        let err = HistoryApi::new(&client)
            .list("1", &[], 10, None)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("404"));
        assert!(msg.contains("notFound"));
    }

    #[tokio::test]
    async fn list_rejects_limit_above_max_page_limit_client_side() {
        let client = dead_client();
        let err = HistoryApi::new(&client)
            .list("1000", &[], MAX_PAGE_LIMIT + 1, None)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("limit"));
        assert!(msg.contains("list_all"));
    }

    #[tokio::test]
    async fn list_propagates_network_errors() {
        // `dead_client()` also points the session's token endpoint at the
        // dead address, so the failure surfaces during token acquisition
        // before the history.list request is ever attempted.
        let client = dead_client();
        let err = HistoryApi::new(&client)
            .list("1000", &[], 10, None)
            .await
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("Failed to obtain a Gmail access token"));
    }

    #[tokio::test]
    async fn list_errors_on_malformed_response() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/history"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let err = HistoryApi::new(&client)
            .list("1000", &[], 10, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Failed to parse"));
    }

    // ── Pagination ────────────────────────────────────────────────────

    #[tokio::test]
    async fn list_all_single_page_when_no_next_token() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/history"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(page_body(
                    &["a", "b"],
                    None,
                    Some("2000"),
                )),
            )
            .expect(1)
            .mount(&server)
            .await;

        let result = HistoryApi::new(&client)
            .list_all("1000", &[], 100)
            .await
            .unwrap();
        assert_eq!(result.history.len(), 2);
        assert_eq!(result.history_id.as_deref(), Some("2000"));
    }

    #[tokio::test]
    async fn list_all_follows_next_page_token_to_exhaustion() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/history"))
            .and(wiremock::matchers::query_param_is_missing("pageToken"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(page_body(
                    &["a", "b"],
                    Some("c1"),
                    None,
                )),
            )
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/history"))
            .and(wiremock::matchers::query_param("pageToken", "c1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(page_body(
                    &["c"],
                    None,
                    Some("3000"),
                )),
            )
            .expect(1)
            .mount(&server)
            .await;

        let result = HistoryApi::new(&client)
            .list_all("1000", &[], 0)
            .await
            .unwrap();
        let ids: Vec<&str> = result.history.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
        assert_eq!(result.history_id.as_deref(), Some("3000"));
    }

    #[tokio::test]
    async fn list_all_stops_at_explicit_limit() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/history"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(page_body(
                    &["a", "b", "c"],
                    Some("more"),
                    None,
                )),
            )
            .expect(1)
            .mount(&server)
            .await;

        let result = HistoryApi::new(&client)
            .list_all("1000", &[], 3)
            .await
            .unwrap();
        assert_eq!(result.history.len(), 3);
    }

    #[tokio::test]
    async fn list_all_truncates_to_hard_cap() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let full_page: Vec<serde_json::Value> = (0..MAX_PAGE_LIMIT)
            .map(|i| history_record_json(&format!("h{i}")))
            .collect();
        let body = serde_json::json!({"history": full_page, "nextPageToken": "always-more"});
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/history"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let result = HistoryApi::new(&client)
            .list_all("1000", &[], 0)
            .await
            .unwrap();
        assert_eq!(result.history.len(), HARD_CAP);
    }

    #[tokio::test]
    async fn list_all_continues_past_empty_page_with_a_valid_next_page_token() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/history"))
            .and(wiremock::matchers::query_param_is_missing("pageToken"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(page_body(
                    &[],
                    Some("p2"),
                    None,
                )),
            )
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/history"))
            .and(wiremock::matchers::query_param("pageToken", "p2"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(page_body(
                    &["a"],
                    None,
                    Some("9"),
                )),
            )
            .expect(1)
            .mount(&server)
            .await;

        let result = HistoryApi::new(&client)
            .list_all("1000", &[], 0)
            .await
            .unwrap();
        assert_eq!(result.history.len(), 1);
    }

    #[tokio::test]
    async fn list_all_propagates_api_errors_on_first_page() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/history"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("nope"))
            .mount(&server)
            .await;

        let err = HistoryApi::new(&client)
            .list_all("1000", &[], 0)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    // ── History record shapes (added/deleted/label changes) ──────────

    #[tokio::test]
    async fn list_parses_messages_added_with_label_ids() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/history"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "history": [{
                    "id": "10",
                    "messagesAdded": [{
                        "message": {"id": "m1", "threadId": "t1", "labelIds": ["INBOX", "UNREAD"]}
                    }]
                }]
            })))
            .mount(&server)
            .await;

        let result = HistoryApi::new(&client)
            .list("1", &[], 10, None)
            .await
            .unwrap();
        let added = &result.history[0].messages_added[0].message;
        assert_eq!(added.id, "m1");
        assert_eq!(added.thread_id, "t1");
        assert_eq!(added.label_ids, vec!["INBOX", "UNREAD"]);
    }

    #[tokio::test]
    async fn list_parses_messages_deleted_and_label_changes() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/history"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "history": [{
                    "id": "10",
                    "messagesDeleted": [{"message": {"id": "m2", "threadId": "t2"}}],
                    "labelsAdded": [{"message": {"id": "m3", "threadId": "t3"}, "labelIds": ["IMPORTANT"]}],
                    "labelsRemoved": [{"message": {"id": "m3", "threadId": "t3"}, "labelIds": ["UNREAD"]}]
                }]
            })))
            .mount(&server)
            .await;

        let result = HistoryApi::new(&client)
            .list("1", &[], 10, None)
            .await
            .unwrap();
        let record = &result.history[0];
        assert_eq!(record.messages_deleted[0].message.id, "m2");
        assert_eq!(record.labels_added[0].label_ids, vec!["IMPORTANT"]);
        assert_eq!(record.labels_removed[0].label_ids, vec!["UNREAD"]);
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
