//! Gmail Drafts API wrapper.
//!
//! Same cursor-pagination shape as [`crate::gmail::messages_api`], and
//! deliberately the same caps: [`MAX_PAGE_LIMIT`], [`HARD_CAP`] and
//! [`DEFAULT_SEARCH_LIMIT`] are imported from there rather than redeclared, so
//! `gmail draft list` and `gmail search` can't drift apart (#1921).
//!
//! **`drafts.send` and `drafts.delete` are deliberately absent** (#1920):
//! `send` delivers mail that can't be recalled, and `delete` skips Trash, so
//! a deleted draft can't be recovered. omni-dev only ever *stages* drafts;
//! sending and discarding stay in Gmail, done by a person. The
//! `drafts_api_exposes_no_send_or_delete` test below pins this.
//!
//! [`DEFAULT_SEARCH_LIMIT`]: crate::gmail::messages_api::DEFAULT_SEARCH_LIMIT
//! [`HARD_CAP`]: crate::gmail::messages_api::HARD_CAP

use anyhow::Result;
use serde::Serialize;
use url::Url;

use crate::gmail::client::GmailClient;
use crate::gmail::messages_api::{
    effective_cap, header_value, hydrate_in_order, MessageFormat, MessagesApi, MAX_PAGE_LIMIT,
};
use crate::gmail::types::{Draft, DraftListResponse, Message};

/// The headers [`DraftsApi::list_summaries`] asks `messages.get` for.
const SUMMARY_HEADERS: [&str; 5] = ["To", "Cc", "Bcc", "Subject", "Date"];

/// A draft enriched with the headers a draft-list row needs.
///
/// Not a Gmail wire type: `drafts.list` only returns `{id, message: {id,
/// threadId}}`, so this is assembled client-side by
/// [`DraftsApi::list_summaries`] from one `messages.get(format=metadata)` per
/// draft. The two ids are named `draft_id` and `message_id` rather than a
/// bare `id` so machine output can't confuse them: the draft id is what
/// `gmail draft show`/`update` take, and the message id changes every time
/// the draft is updated.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub struct DraftSummary {
    /// Gmail draft id.
    pub draft_id: String,
    /// Id of the draft's current message.
    pub message_id: String,
    /// Id of the thread the draft's message belongs to.
    pub thread_id: String,
    /// The `To` header, or empty if absent.
    pub to: String,
    /// The `Cc` header, or empty if absent.
    pub cc: String,
    /// The `Bcc` header, or empty if absent.
    pub bcc: String,
    /// The `Subject` header, or empty if absent.
    pub subject: String,
    /// The `Date` header, or empty if absent.
    pub date: String,
    /// A short, plain-text snippet of the draft's body.
    pub snippet: String,
}

impl DraftSummary {
    /// Builds a row from a listed draft and its hydrated message.
    ///
    /// The message and thread ids come from the hydrated message when it
    /// carries them, falling back to the listing's.
    #[must_use]
    pub fn from_draft(draft: &Draft, message: &Message) -> Self {
        let payload = message.payload.as_ref();
        let header = |name| header_value(payload, name).unwrap_or_default();
        let message_id = if message.id.is_empty() {
            draft.message.id.clone()
        } else {
            message.id.clone()
        };
        Self {
            draft_id: draft.id.clone(),
            message_id,
            thread_id: message
                .thread_id
                .clone()
                .unwrap_or_else(|| draft.message.thread_id.clone()),
            to: header("To"),
            cc: header("Cc"),
            bcc: header("Bcc"),
            subject: header("Subject"),
            date: header("Date"),
            snippet: message.snippet.clone().unwrap_or_default(),
        }
    }
}

/// Drafts API façade.
#[derive(Debug)]
pub struct DraftsApi<'a> {
    client: &'a GmailClient,
}

impl<'a> DraftsApi<'a> {
    /// Wraps an existing [`GmailClient`] for draft operations.
    #[must_use]
    pub fn new(client: &'a GmailClient) -> Self {
        Self { client }
    }

    /// Lists drafts matching `query`, returning a single page.
    ///
    /// `query` uses the Gmail search-box syntax. `limit` is rejected
    /// client-side when it exceeds [`MAX_PAGE_LIMIT`]; use
    /// [`Self::list_all`] to auto-paginate across pages. Needs only the
    /// `gmail.readonly` scope.
    pub async fn list(
        &self,
        query: Option<&str>,
        limit: usize,
        page_token: Option<&str>,
    ) -> Result<DraftListResponse> {
        if limit > MAX_PAGE_LIMIT {
            return Err(anyhow::anyhow!(
                "`limit` must be <= {MAX_PAGE_LIMIT} (Gmail drafts.list per-page cap; use \
                 `list_all` to auto-paginate)"
            ));
        }
        let url = build_drafts_list_url(self.client.base_url(), query, limit, page_token)?;
        self.client
            .get_parsed(url.as_str(), "Failed to parse drafts.list response")
            .await
    }

    /// Lists drafts, auto-paginating via cursor as needed.
    ///
    /// `limit == 0` means "fetch every draft up to
    /// [`HARD_CAP`](crate::gmail::messages_api::HARD_CAP)", exactly as
    /// [`MessagesApi::search_all`] does.
    pub async fn list_all(&self, query: Option<&str>, limit: usize) -> Result<DraftListResponse> {
        let cap = effective_cap(limit);
        let mut acc: Option<DraftListResponse> = None;
        let mut page_token: Option<String> = None;
        loop {
            let collected = acc.as_ref().map_or(0, |r| r.drafts.len());
            let page_size = (cap - collected).min(MAX_PAGE_LIMIT);
            let page = self.list(query, page_size, page_token.as_deref()).await?;
            let next_token = page.next_page_token.clone();
            match acc.as_mut() {
                Some(existing) => {
                    existing.drafts.extend(page.drafts);
                    existing.next_page_token = page.next_page_token;
                    existing.result_size_estimate = page.result_size_estimate;
                }
                None => acc = Some(page),
            }
            let collected = acc.as_ref().map_or(0, |r| r.drafts.len());
            if collected >= cap || next_token.is_none() {
                break;
            }
            page_token = next_token;
        }
        let mut result = acc.unwrap_or_default();
        result.drafts.truncate(cap);
        Ok(result)
    }

    /// Lists drafts and enriches each with its recipients, subject, date
    /// and snippet via one `messages.get(format=metadata)` call per draft.
    ///
    /// The hydration fan-out is [`MessagesApi::search_summaries`]' own
    /// (`hydrate_in_order`): at most `concurrency` calls in flight (clamped
    /// to 1 through
    /// [`MAX_CONCURRENCY`](crate::gmail::messages_api::MAX_CONCURRENCY)),
    /// results in listing order, and any one failure aborts the whole call.
    /// Both endpoints accept `gmail.readonly`.
    pub async fn list_summaries(
        &self,
        query: Option<&str>,
        limit: usize,
        concurrency: usize,
    ) -> Result<Vec<DraftSummary>> {
        let list = self.list_all(query, limit).await?;
        let messages = MessagesApi::new(self.client);
        let messages = &messages;
        hydrate_in_order(list.drafts, concurrency, |draft| async move {
            let message = messages
                .get(&draft.message.id, MessageFormat::Metadata, &SUMMARY_HEADERS)
                .await?;
            Ok(DraftSummary::from_draft(&draft, &message))
        })
        .await
    }
}

fn build_drafts_list_url(
    base_url: &str,
    query: Option<&str>,
    limit: usize,
    page_token: Option<&str>,
) -> Result<Url> {
    let mut url = GmailClient::api_url(base_url, "/gmail/v1/users/me/drafts")?;
    let query = query.filter(|q| !q.is_empty());
    // Only touch `query_pairs_mut()` when there's something to append —
    // calling it unconditionally leaves a bare trailing `?` even with zero
    // pairs appended.
    if query.is_some() || limit > 0 || page_token.is_some() {
        let mut pairs = url.query_pairs_mut();
        if let Some(q) = query {
            pairs.append_pair("q", q);
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::gmail::auth::{GmailCredentials, GmailScope};
    use crate::utils::secret::Secret;

    /// Read-only credentials throughout: both endpoints `draft list` calls
    /// accept `gmail.readonly`, and nothing client-side gates on scope.
    fn test_credentials() -> GmailCredentials {
        GmailCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: GmailScope::ReadOnly,
        }
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

    /// A `drafts.list` page whose draft `rN` points at message `mN` in
    /// thread `tN`.
    fn page_body(ids: &[&str], next: Option<&str>) -> serde_json::Value {
        let drafts: Vec<_> = ids
            .iter()
            .map(|n| {
                serde_json::json!({
                    "id": format!("r{n}"),
                    "message": {"id": format!("m{n}"), "threadId": format!("t{n}")},
                })
            })
            .collect();
        let mut body = serde_json::json!({ "drafts": drafts });
        if let Some(token) = next {
            body["nextPageToken"] = serde_json::json!(token);
        }
        body
    }

    async fn mount_message(server: &wiremock::MockServer, n: &str, subject: &str) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!(
                "/gmail/v1/users/me/messages/m{n}"
            )))
            .and(wiremock::matchers::query_param("format", "metadata"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": format!("m{n}"),
                    "threadId": format!("t{n}"),
                    "snippet": "Draft body",
                    "payload": {
                        "headers": [
                            {"name": "To", "value": "a@example.com, b@example.com"},
                            {"name": "Cc", "value": "c@example.com"},
                            {"name": "Subject", "value": subject},
                            {"name": "Date", "value": "Mon, 1 Jan 2026 00:00:00 +0000"},
                        ]
                    }
                })),
            )
            .expect(1)
            .mount(server)
            .await;
    }

    // ── URL builder (pure) ───────────────────────────────────────────

    #[test]
    fn build_drafts_list_url_has_no_trailing_question_mark_when_bare() {
        let url = build_drafts_list_url("https://gmail.googleapis.com", None, 0, None).unwrap();
        assert_eq!(
            url.as_str(),
            "https://gmail.googleapis.com/gmail/v1/users/me/drafts"
        );
    }

    #[test]
    fn build_drafts_list_url_encodes_query_limit_and_page_token() {
        let url = build_drafts_list_url(
            "https://gmail.googleapis.com",
            Some("to:bob subject:\"q3 plan\""),
            25,
            Some("tok"),
        )
        .unwrap();
        let pairs: Vec<(String, String)> = url.query_pairs().into_owned().collect();
        assert_eq!(
            pairs,
            [
                ("q".to_string(), "to:bob subject:\"q3 plan\"".to_string()),
                ("maxResults".to_string(), "25".to_string()),
                ("pageToken".to_string(), "tok".to_string()),
            ]
        );
    }

    #[test]
    fn build_drafts_list_url_omits_an_empty_query() {
        let url = build_drafts_list_url("https://gmail.googleapis.com", Some(""), 0, None).unwrap();
        assert!(url.query().is_none());
    }

    #[test]
    fn build_drafts_list_url_rejects_invalid_base_url() {
        assert!(build_drafts_list_url("not a url", None, 0, None).is_err());
    }

    // ── wire types ───────────────────────────────────────────────────

    #[test]
    fn draft_list_response_without_drafts_key_parses_as_empty() {
        let parsed: DraftListResponse =
            serde_json::from_value(serde_json::json!({"resultSizeEstimate": 0})).unwrap();
        assert!(parsed.drafts.is_empty());
        assert!(parsed.next_page_token.is_none());
    }

    // ── list ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn list_rejects_a_limit_above_the_page_cap() {
        let client = GmailClient::new("http://127.0.0.1:1", &test_credentials()).unwrap();
        let err = DraftsApi::new(&client)
            .list(None, MAX_PAGE_LIMIT + 1, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("drafts.list per-page cap"));
    }

    #[tokio::test]
    async fn list_sends_the_query_and_parses_drafts() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/drafts"))
            .and(wiremock::matchers::query_param("q", "to:bob"))
            .and(wiremock::matchers::query_param("maxResults", "10"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(page_body(&["1"], None)),
            )
            .expect(1)
            .mount(&server)
            .await;

        let page = DraftsApi::new(&client)
            .list(Some("to:bob"), 10, None)
            .await
            .unwrap();
        assert_eq!(page.drafts.len(), 1);
        assert_eq!(page.drafts[0].id, "r1");
        assert_eq!(page.drafts[0].message.id, "m1");
        assert_eq!(page.drafts[0].message.thread_id, "t1");
    }

    #[tokio::test]
    async fn list_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/drafts"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("nope"))
            .mount(&server)
            .await;

        let err = DraftsApi::new(&client)
            .list(None, 10, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    // ── list_all ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn list_all_follows_the_cursor_across_pages() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        // Page 2 is mounted first and matched on its token; page 1 matches
        // any request without one.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/drafts"))
            .and(wiremock::matchers::query_param("pageToken", "p2"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(page_body(&["3"], None)),
            )
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/drafts"))
            .and(wiremock::matchers::query_param_is_missing("pageToken"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(page_body(&["1", "2"], Some("p2"))),
            )
            .expect(1)
            .mount(&server)
            .await;

        let all = DraftsApi::new(&client).list_all(None, 0).await.unwrap();
        let ids: Vec<&str> = all.drafts.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, ["r1", "r2", "r3"]);
        assert!(all.next_page_token.is_none());
    }

    #[tokio::test]
    async fn list_all_stops_at_the_limit_and_shrinks_the_last_page() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/drafts"))
            .and(wiremock::matchers::query_param("pageToken", "p2"))
            .and(wiremock::matchers::query_param("maxResults", "1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(page_body(&["3"], Some("p3"))),
            )
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/drafts"))
            .and(wiremock::matchers::query_param_is_missing("pageToken"))
            .and(wiremock::matchers::query_param("maxResults", "3"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(page_body(&["1", "2"], Some("p2"))),
            )
            .expect(1)
            .mount(&server)
            .await;

        // `p3` is never requested: the cap is reached after page 2.
        let all = DraftsApi::new(&client).list_all(None, 3).await.unwrap();
        assert_eq!(all.drafts.len(), 3);
    }

    #[tokio::test]
    async fn list_all_caps_zero_at_the_page_limit_per_request() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/drafts"))
            .and(wiremock::matchers::query_param(
                "maxResults",
                MAX_PAGE_LIMIT.to_string(),
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(&server)
            .await;

        let all = DraftsApi::new(&client).list_all(None, 0).await.unwrap();
        assert!(all.drafts.is_empty());
    }

    // ── list_summaries ───────────────────────────────────────────────

    #[tokio::test]
    async fn list_summaries_hydrates_each_draft_with_both_ids_and_headers() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/drafts"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(page_body(&["1"], None)),
            )
            .expect(1)
            .mount(&server)
            .await;
        mount_message(&server, "1", "Q3 plan").await;

        let summaries = DraftsApi::new(&client)
            .list_summaries(None, 10, 4)
            .await
            .unwrap();
        assert_eq!(
            summaries,
            [DraftSummary {
                draft_id: "r1".to_string(),
                message_id: "m1".to_string(),
                thread_id: "t1".to_string(),
                to: "a@example.com, b@example.com".to_string(),
                cc: "c@example.com".to_string(),
                bcc: String::new(),
                subject: "Q3 plan".to_string(),
                date: "Mon, 1 Jan 2026 00:00:00 +0000".to_string(),
                snippet: "Draft body".to_string(),
            }]
        );

        let requests = server.received_requests().await.unwrap();
        let hydration = requests
            .iter()
            .find(|r| r.url.path() == "/gmail/v1/users/me/messages/m1")
            .unwrap();
        let headers: Vec<String> = hydration
            .url
            .query_pairs()
            .filter(|(k, _)| k == "metadataHeaders")
            .map(|(_, v)| v.into_owned())
            .collect();
        assert_eq!(headers, SUMMARY_HEADERS);
    }

    #[tokio::test]
    async fn list_summaries_preserves_listing_order_under_concurrency() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/drafts"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(page_body(&["1", "2", "3"], None)),
            )
            .mount(&server)
            .await;
        for n in ["1", "2", "3"] {
            mount_message(&server, n, &format!("subject {n}")).await;
        }

        let summaries = DraftsApi::new(&client)
            .list_summaries(None, 10, 4)
            .await
            .unwrap();
        let ids: Vec<&str> = summaries.iter().map(|s| s.draft_id.as_str()).collect();
        assert_eq!(ids, ["r1", "r2", "r3"]);
    }

    #[tokio::test]
    async fn list_summaries_of_an_empty_mailbox_issues_no_hydration() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/drafts"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"resultSizeEstimate": 0})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let summaries = DraftsApi::new(&client)
            .list_summaries(None, 10, 4)
            .await
            .unwrap();
        assert!(summaries.is_empty());
        let requests = server.received_requests().await.unwrap();
        assert!(requests
            .iter()
            .all(|r| !r.url.path().starts_with("/gmail/v1/users/me/messages")));
    }

    #[tokio::test]
    async fn list_summaries_propagates_a_hydration_error() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/drafts"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(page_body(&["1"], None)),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages/m1"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let err = DraftsApi::new(&client)
            .list_summaries(None, 10, 4)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    // ── DraftSummary::from_draft ─────────────────────────────────────

    #[test]
    fn from_draft_falls_back_to_the_listing_ids() {
        let draft = Draft {
            id: "r1".to_string(),
            message: crate::gmail::types::MessageRef {
                id: "m1".to_string(),
                thread_id: "t1".to_string(),
            },
        };
        let summary = DraftSummary::from_draft(&draft, &Message::default());
        assert_eq!(summary.draft_id, "r1");
        assert_eq!(summary.message_id, "m1");
        assert_eq!(summary.thread_id, "t1");
        assert!(summary.to.is_empty());
    }

    // ── read-only surface ────────────────────────────────────────────

    /// #1920 excludes `drafts.send` (irrecoverable delivery) and
    /// `drafts.delete` (skips Trash) on purpose. This fails the build if
    /// either ever lands here, so the exclusion is a decision someone has
    /// to revisit rather than an omission someone can fill in.
    #[test]
    fn drafts_api_exposes_no_send_or_delete() {
        let source = include_str!("drafts_api.rs");
        // Only production code: this test has to name what it forbids.
        let code_only = source.split("#[cfg(test)]").next().unwrap_or(source);
        for (number, line) in code_only.lines().enumerate() {
            let code = line.trim_start();
            if code.starts_with("//") {
                continue; // prose may discuss send/delete; code may not
            }
            let forbidden = code.contains("fn send")
                || code.contains("fn delete")
                || code.contains("/send")
                || code.contains(".delete(")
                || code.contains("Method::DELETE");
            assert!(
                !forbidden,
                "drafts_api.rs:{}: DraftsApi must never send or delete a draft (#1920): {line}",
                number + 1
            );
        }
    }
}
