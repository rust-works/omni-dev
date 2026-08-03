//! Gmail Messages API wrapper.
//!
//! `messages.list` uses **cursor pagination** (`nextPageToken`), like
//! Datadog's v2 logs search — [`MessagesApi::search`] issues a single page,
//! [`MessagesApi::search_all`] auto-paginates up to a caller-supplied limit
//! (or [`HARD_CAP`] when the limit is `0`). Gmail's list endpoint is
//! GET-with-query-params (not POST-with-body like Datadog's logs search),
//! so URL construction follows the free `build_*_url` pattern from
//! `src/datadog/monitors_api.rs` instead.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use futures::stream::StreamExt as _;
use serde::Serialize;
use url::Url;

use crate::gmail::client::GmailClient;
use crate::gmail::types::{Message, MessageListResponse};

/// Maximum page size accepted by `GET /gmail/v1/users/{userId}/messages`.
pub const MAX_PAGE_LIMIT: usize = 500;

/// Per-call upper bound on the number of messages returned by
/// [`MessagesApi::search_all`], even when the caller passes `limit = 0`.
pub const HARD_CAP: usize = 10_000;

/// Upper bound on [`MessagesApi::search_summaries`]'s `concurrency`
/// parameter, regardless of what the caller (CLI `--concurrency` or the MCP
/// `concurrency` param) requests.
///
/// Gmail's quota is 250 units/user/second and `messages.get` costs 5 units,
/// so more than 50 requests in flight at once already assumes every one
/// completes within a second — the flag exists to bound the fan-out against
/// that quota, so it shouldn't itself accept a value that can blow past it.
pub const MAX_CONCURRENCY: usize = 50;

/// Gmail's documented quota ceiling, in quota units per user per second.
///
/// The load-bearing constraint behind [`MAX_CONCURRENCY`] above and behind
/// `gmail sync`'s proactive token-bucket limiter
/// (`src/cli/gmail/sync/engine.rs`) — the single biggest determinant of
/// whether a bulk sync is pleasant or infuriating (#1467).
pub const GMAIL_QUOTA_UNITS_PER_SECOND: u32 = 250;

/// Quota-unit cost of one `messages.get` call, regardless of `format`.
pub const MESSAGES_GET_COST_UNITS: u32 = 5;

/// Default `limit` for a search when the caller doesn't specify one.
///
/// Shared between the CLI (`gmail search`'s `--limit` default) and the MCP
/// `gmail_search` tool (its `limit` param default when omitted), so an
/// unset limit means the same "quota-safe 50" thing in both surfaces rather
/// than silently falling back to `0` (fetch-to-[`HARD_CAP`]) in one of them.
pub const DEFAULT_SEARCH_LIMIT: usize = 50;

/// The `format` query parameter accepted by `messages.get`.
#[derive(Debug, Clone, Copy, Default)]
pub enum MessageFormat {
    /// Only `id`/`threadId`/`labelIds`/`sizeEstimate` — no headers or body.
    Minimal,
    /// The full parsed MIME structure. Default.
    #[default]
    Full,
    /// Headers and snippet only, no body.
    Metadata,
    /// The full RFC 2822 message, base64url-encoded.
    Raw,
}

impl MessageFormat {
    fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Full => "full",
            Self::Metadata => "metadata",
            Self::Raw => "raw",
        }
    }
}

/// A search hit enriched with the headers a search-result table/list needs.
///
/// Not a Gmail wire type — `messages.list` only returns `{id, threadId}`;
/// this is assembled client-side by [`MessagesApi::search_summaries`] from a
/// follow-up `messages.get(format=metadata)` call per hit.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub struct MessageSummary {
    /// Gmail message id.
    pub id: String,
    /// Id of the thread this message belongs to.
    pub thread_id: String,
    /// The `From` header, or empty if absent.
    pub from: String,
    /// The `Subject` header, or empty if absent.
    pub subject: String,
    /// The `Date` header, or empty if absent.
    pub date: String,
    /// A short, plain-text snippet of the message body.
    pub snippet: String,
}

impl MessageSummary {
    /// Builds a summary row from an already-fetched [`Message`] — the seam
    /// `omni-dev gmail thread` reuses to render its per-message rows with
    /// the same table renderer `search` uses, without a second API call.
    #[must_use]
    pub fn from_message(message: &Message) -> Self {
        Self {
            id: message.id.clone(),
            thread_id: message.thread_id.clone().unwrap_or_default(),
            from: header_value(message.payload.as_ref(), "From").unwrap_or_default(),
            subject: header_value(message.payload.as_ref(), "Subject").unwrap_or_default(),
            date: header_value(message.payload.as_ref(), "Date").unwrap_or_default(),
            snippet: message.snippet.clone().unwrap_or_default(),
        }
    }
}

/// Looks up a header's value from a message's raw `payload.headers` array
/// (`[{"name": "...", "value": "..."}]`), matching `name` case-insensitively
/// — Gmail's `metadataHeaders` filter matches case-insensitively too.
fn header_value(payload: Option<&serde_json::Value>, name: &str) -> Option<String> {
    payload?
        .get("headers")?
        .as_array()?
        .iter()
        .find(|header| {
            header
                .get("name")
                .and_then(|n| n.as_str())
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
        })
        .and_then(|header| header.get("value"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Messages API façade.
#[derive(Debug)]
pub struct MessagesApi<'a> {
    client: &'a GmailClient,
}

impl<'a> MessagesApi<'a> {
    /// Wraps an existing [`GmailClient`] for message operations.
    #[must_use]
    pub fn new(client: &'a GmailClient) -> Self {
        Self { client }
    }

    /// Searches messages matching `query`, returning a single page.
    ///
    /// `limit` is rejected client-side when it exceeds [`MAX_PAGE_LIMIT`];
    /// use [`Self::search_all`] to auto-paginate across pages.
    pub async fn search(
        &self,
        query: Option<&str>,
        label_ids: &[&str],
        limit: usize,
        page_token: Option<&str>,
    ) -> Result<MessageListResponse> {
        if limit > MAX_PAGE_LIMIT {
            return Err(anyhow::anyhow!(
                "`limit` must be <= {MAX_PAGE_LIMIT} (Gmail messages.list per-page cap; use \
                 `search_all` to auto-paginate)"
            ));
        }
        let url =
            build_messages_list_url(self.client.base_url(), query, label_ids, limit, page_token)?;
        self.client
            .get_parsed(url.as_str(), "Failed to parse messages.list response")
            .await
    }

    /// Searches messages, auto-paginating via cursor as needed.
    ///
    /// `limit == 0` means "fetch every match up to [`HARD_CAP`]".
    /// Termination is cursor-only, never a short page: a `q`-filtered list
    /// can return zero messages on a page alongside a valid
    /// `nextPageToken` — a scan that hasn't found a match on this page yet.
    /// Only an absent token stops the loop.
    pub async fn search_all(
        &self,
        query: Option<&str>,
        label_ids: &[&str],
        limit: usize,
    ) -> Result<MessageListResponse> {
        let cap = effective_cap(limit);
        let mut acc: Option<MessageListResponse> = None;
        let mut page_token: Option<String> = None;
        loop {
            let collected = acc.as_ref().map_or(0, |r| r.messages.len());
            let page_size = (cap - collected).min(MAX_PAGE_LIMIT);
            let page = self
                .search(query, label_ids, page_size, page_token.as_deref())
                .await?;
            let next_token = page.next_page_token.clone();
            match acc.as_mut() {
                Some(existing) => {
                    existing.messages.extend(page.messages);
                    existing.next_page_token = page.next_page_token;
                    existing.result_size_estimate = page.result_size_estimate;
                }
                None => acc = Some(page),
            }
            let collected = acc.as_ref().map_or(0, |r| r.messages.len());
            if collected >= cap || next_token.is_none() {
                break;
            }
            page_token = next_token;
        }
        let mut result = acc.unwrap_or_default();
        result.messages.truncate(cap);
        Ok(result)
    }

    /// Fetches a single message by id.
    pub async fn get(
        &self,
        id: &str,
        format: MessageFormat,
        metadata_headers: &[&str],
    ) -> Result<Message> {
        let url = build_message_get_url(self.client.base_url(), id, format, metadata_headers)?;
        self.client
            .get_parsed(url.as_str(), "Failed to parse messages.get response")
            .await
    }

    /// Searches messages and enriches each hit with `From`/`Subject`/`Date`
    /// via one `messages.get(format=metadata)` call per hit.
    ///
    /// Gmail's list endpoint only returns `{id, threadId}` per hit — a
    /// `search`-shaped CLI/MCP surface needs more than bare ids to be
    /// useful, so this costs one extra request per result. Gmail's quota is
    /// **250 units/user/second** and `messages.get` costs **5 units**, so
    /// this is a genuinely expensive operation — callers are expected to
    /// treat it as opt-in (the CLI's `--enrich` flag) rather than a default,
    /// and `concurrency` bounds the fan-out (modelled on
    /// `src/cli/atlassian/confluence/download.rs`'s
    /// `Semaphore::new(params.concurrency)` list-then-hydrate shape) so a
    /// large `limit` can't burst past the quota in an uncontrolled way.
    /// Order is preserved (`buffered`, not `buffer_unordered`) so results
    /// match `search_all`'s ordering. A hydration failure on any one id
    /// aborts the whole call with that error, once every already-in-flight
    /// fetch in its concurrency batch completes — it is never silently
    /// dropped from the results.
    pub async fn search_summaries(
        &self,
        query: Option<&str>,
        label_ids: &[&str],
        limit: usize,
        concurrency: usize,
    ) -> Result<Vec<MessageSummary>> {
        let list = self.search_all(query, label_ids, limit).await?;
        let concurrency = effective_concurrency(concurrency);
        // Collect owned ids first: a closure borrowing `list.messages`
        // directly ties its returned future to that borrow's lifetime,
        // which `buffered` then can't unify into a `for<'a> FnMut(&'a _)`
        // shape — this is what the `implementation of FnOnce is not
        // general enough` error was pointing at.
        let ids: Vec<String> = list.messages.into_iter().map(|m| m.id).collect();
        // `buffered` refills its concurrency window from `ids` as each slot
        // frees, regardless of whether the item that just freed it errored —
        // left unchecked, one failed hydration wouldn't stop the remaining
        // fetches from firing, defeating the point of bounding concurrency
        // against Gmail's per-second quota. `failed` is checked once per
        // item before its network call: only fetches not yet dispatched at
        // the time of the first failure are skipped, so already in-flight
        // ones (up to `concurrency` many) still run to completion.
        let failed = Arc::new(AtomicBool::new(false));
        futures::stream::iter(ids)
            .map(|id| {
                let failed = Arc::clone(&failed);
                async move {
                    if failed.load(Ordering::Acquire) {
                        return Err(anyhow::anyhow!(
                            "skipped hydrating message {id}: an earlier hydration request failed"
                        ));
                    }
                    let result = self
                        .get(&id, MessageFormat::Metadata, &["From", "Subject", "Date"])
                        .await
                        .map(|message| MessageSummary::from_message(&message));
                    if result.is_err() {
                        failed.store(true, Ordering::Release);
                    }
                    result
                }
            })
            .buffered(concurrency)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect()
    }

    /// Adds/removes labels on up to 1000 messages in one call.
    ///
    /// Requires the `gmail.modify` scope — no client-side scope gating is
    /// performed (matches this client's posture elsewhere of letting the
    /// server enforce authorization): a `gmail.readonly`-only token simply
    /// gets a 403 back from Google, surfaced via
    /// [`GmailClient::response_to_error`].
    pub async fn batch_modify(
        &self,
        ids: &[&str],
        add_label_ids: &[&str],
        remove_label_ids: &[&str],
    ) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        if ids.len() > 1000 {
            return Err(anyhow::anyhow!(
                "batchModify accepts at most 1000 message ids per call; got {}",
                ids.len()
            ));
        }
        let url = GmailClient::api_url(
            self.client.base_url(),
            "/gmail/v1/users/me/messages/batchModify",
        )?;
        let body = BatchModifyRequest {
            ids,
            add_label_ids,
            remove_label_ids,
        };
        let response = self.client.post_json(url.as_str(), &body).await?;
        if !response.status().is_success() {
            return Err(GmailClient::response_to_error(response).await.into());
        }
        Ok(())
    }
}

fn build_messages_list_url(
    base_url: &str,
    query: Option<&str>,
    label_ids: &[&str],
    limit: usize,
    page_token: Option<&str>,
) -> Result<Url> {
    let mut url = GmailClient::api_url(base_url, "/gmail/v1/users/me/messages")?;
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

fn build_message_get_url(
    base_url: &str,
    id: &str,
    format: MessageFormat,
    metadata_headers: &[&str],
) -> Result<Url> {
    let mut url = GmailClient::api_url(base_url, &format!("/gmail/v1/users/me/messages/{id}"))?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("format", format.as_str());
        for header in metadata_headers {
            pairs.append_pair("metadataHeaders", header);
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

/// Clamps a requested hydration `concurrency` into `1..=MAX_CONCURRENCY`:
/// `0` (which would otherwise stall the stream forever) is raised to `1`,
/// and anything past [`MAX_CONCURRENCY`] is capped rather than allowed to
/// burst past Gmail's per-second quota.
fn effective_concurrency(concurrency: usize) -> usize {
    concurrency.clamp(1, MAX_CONCURRENCY)
}

#[derive(Debug, Serialize)]
struct BatchModifyRequest<'a> {
    ids: &'a [&'a str],
    #[serde(rename = "addLabelIds", skip_serializing_if = "<[_]>::is_empty")]
    add_label_ids: &'a [&'a str],
    #[serde(rename = "removeLabelIds", skip_serializing_if = "<[_]>::is_empty")]
    remove_label_ids: &'a [&'a str],
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

    fn message_ref_json(id: &str) -> serde_json::Value {
        serde_json::json!({"id": id, "threadId": "thread-1"})
    }

    fn page_body(ids: &[&str], next_token: Option<&str>) -> serde_json::Value {
        let messages: Vec<serde_json::Value> = ids.iter().map(|id| message_ref_json(id)).collect();
        let mut body = serde_json::json!({"messages": messages});
        if let Some(token) = next_token {
            body["nextPageToken"] = serde_json::json!(token);
        }
        body
    }

    // ── URL builders (pure) ──────────────────────────────────────────

    #[test]
    fn build_messages_list_url_with_only_provided_filters() {
        let url =
            build_messages_list_url("https://gmail.googleapis.com", None, &[], 0, None).unwrap();
        assert_eq!(
            url.as_str(),
            "https://gmail.googleapis.com/gmail/v1/users/me/messages"
        );
    }

    #[test]
    fn build_messages_list_url_with_full_filter_set() {
        let url = build_messages_list_url(
            "https://gmail.googleapis.com",
            Some("label:finance"),
            &["INBOX", "IMPORTANT"],
            50,
            Some("cursor-1"),
        )
        .unwrap();
        let query: Vec<_> = url.query_pairs().collect();
        assert!(query.contains(&("q".into(), "label:finance".into())));
        assert!(query.contains(&("labelIds".into(), "INBOX".into())));
        assert!(query.contains(&("labelIds".into(), "IMPORTANT".into())));
        assert!(query.contains(&("maxResults".into(), "50".into())));
        assert!(query.contains(&("pageToken".into(), "cursor-1".into())));
    }

    #[test]
    fn build_messages_list_url_percent_encodes_query_operators() {
        let url = build_messages_list_url(
            "https://gmail.googleapis.com",
            Some("label:finance"),
            &[],
            0,
            None,
        )
        .unwrap();
        assert!(url.query().unwrap().contains("q=label%3Afinance"));
    }

    #[test]
    fn build_messages_list_url_rejects_invalid_base_url() {
        let err = build_messages_list_url("not a url", None, &[], 0, None).unwrap_err();
        assert!(err.to_string().contains("Invalid Gmail base URL"));
    }

    #[test]
    fn build_message_get_url_includes_format_and_metadata_headers() {
        let url = build_message_get_url(
            "https://gmail.googleapis.com",
            "msg1",
            MessageFormat::Metadata,
            &["From", "Subject"],
        )
        .unwrap();
        assert!(url
            .as_str()
            .starts_with("https://gmail.googleapis.com/gmail/v1/users/me/messages/msg1"));
        let query: Vec<_> = url.query_pairs().collect();
        assert!(query.contains(&("format".into(), "metadata".into())));
        assert!(query.contains(&("metadataHeaders".into(), "From".into())));
        assert!(query.contains(&("metadataHeaders".into(), "Subject".into())));
    }

    // ── Standard error paths ─────────────────────────────────────────

    #[tokio::test]
    async fn search_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages"))
            .respond_with(wiremock::ResponseTemplate::new(400).set_body_string("bad query"))
            .mount(&server)
            .await;

        let err = MessagesApi::new(&client)
            .search(Some("???"), &[], 10, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("400"));
    }

    #[tokio::test]
    async fn search_rejects_limit_above_max_page_limit_client_side() {
        // Pure client-side check, no network needed.
        let client = dead_client();
        let err = MessagesApi::new(&client)
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
        // (before the messages.list request is ever attempted) —
        // `client.rs`'s own tests cover a network failure on the API call
        // itself once a token is already held.
        let client = dead_client();
        let err = MessagesApi::new(&client)
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
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let err = MessagesApi::new(&client)
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
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(page_body(&["a", "b"], None)),
            )
            .expect(1)
            .mount(&server)
            .await;

        let result = MessagesApi::new(&client)
            .search_all(None, &[], 100)
            .await
            .unwrap();
        assert_eq!(result.messages.len(), 2);
    }

    #[tokio::test]
    async fn search_all_follows_next_page_token_to_exhaustion() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages"))
            .and(wiremock::matchers::query_param_is_missing("pageToken"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(page_body(&["a", "b"], Some("c1"))),
            )
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages"))
            .and(wiremock::matchers::query_param("pageToken", "c1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(page_body(&["c"], None)),
            )
            .expect(1)
            .mount(&server)
            .await;

        let result = MessagesApi::new(&client)
            .search_all(None, &[], 0)
            .await
            .unwrap();
        let ids: Vec<&str> = result.messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
    }

    #[tokio::test]
    async fn search_all_stops_at_explicit_limit() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(page_body(&["a", "b", "c", "d", "e"], Some("more"))),
            )
            .expect(1)
            .mount(&server)
            .await;

        let result = MessagesApi::new(&client)
            .search_all(None, &[], 5)
            .await
            .unwrap();
        assert_eq!(result.messages.len(), 5);
    }

    #[tokio::test]
    async fn search_all_truncates_to_hard_cap() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let full_page: Vec<serde_json::Value> = (0..MAX_PAGE_LIMIT)
            .map(|i| message_ref_json(&format!("m{i}")))
            .collect();
        let body = serde_json::json!({"messages": full_page, "nextPageToken": "always-more"});
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let result = MessagesApi::new(&client)
            .search_all(None, &[], 0)
            .await
            .unwrap();
        assert_eq!(result.messages.len(), HARD_CAP);
    }

    #[tokio::test]
    async fn search_all_continues_past_empty_page_with_a_valid_next_page_token() {
        // The Gmail-specific case: a filtered scan can return zero results
        // on a page while still signalling more pages exist.
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages"))
            .and(wiremock::matchers::query_param_is_missing("pageToken"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(page_body(&[], Some("p2"))),
            )
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages"))
            .and(wiremock::matchers::query_param("pageToken", "p2"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(page_body(&["a"], None)),
            )
            .expect(1)
            .mount(&server)
            .await;

        let result = MessagesApi::new(&client)
            .search_all(Some("rare-query"), &[], 0)
            .await
            .unwrap();
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.messages[0].id, "a");
    }

    #[tokio::test]
    async fn search_all_propagates_api_errors_on_first_page() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("nope"))
            .mount(&server)
            .await;

        let err = MessagesApi::new(&client)
            .search_all(None, &[], 0)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    // ── get ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn get_sends_correct_format_and_metadata_headers_query_params() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages/msg1"))
            .and(wiremock::matchers::query_param("format", "metadata"))
            .and(wiremock::matchers::query_param("metadataHeaders", "From"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "msg1"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let message = MessagesApi::new(&client)
            .get("msg1", MessageFormat::Metadata, &["From"])
            .await
            .unwrap();
        assert_eq!(message.id, "msg1");
    }

    #[tokio::test]
    async fn get_sends_raw_format_query_param() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages/msg1"))
            .and(wiremock::matchers::query_param("format", "raw"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "msg1"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let message = MessagesApi::new(&client)
            .get("msg1", MessageFormat::Raw, &[])
            .await
            .unwrap();
        assert_eq!(message.id, "msg1");
    }

    #[tokio::test]
    async fn get_parses_message_with_mime_payload_value_preserved() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let payload = serde_json::json!({"mimeType": "text/plain", "body": {"data": "aGk"}});
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages/msg1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "msg1",
                    "payload": payload,
                })),
            )
            .mount(&server)
            .await;

        let message = MessagesApi::new(&client)
            .get("msg1", MessageFormat::Full, &[])
            .await
            .unwrap();
        assert_eq!(message.payload, Some(payload));
    }

    // ── search_summaries / header_value ─────────────────────────────

    #[test]
    fn header_value_matches_case_insensitively() {
        let payload = serde_json::json!({
            "headers": [{"name": "subject", "value": "Hello"}],
        });
        assert_eq!(
            header_value(Some(&payload), "Subject").as_deref(),
            Some("Hello")
        );
    }

    #[test]
    fn header_value_is_none_when_absent() {
        let payload = serde_json::json!({"headers": []});
        assert_eq!(header_value(Some(&payload), "From"), None);
        assert_eq!(header_value(None, "From"), None);
    }

    #[tokio::test]
    async fn search_summaries_enriches_each_hit_with_headers_and_snippet() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(page_body(&["m1"], None)),
            )
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages/m1"))
            .and(wiremock::matchers::query_param("format", "metadata"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "m1",
                    "threadId": "thread-1",
                    "snippet": "Hi there",
                    "payload": {
                        "headers": [
                            {"name": "From", "value": "a@example.com"},
                            {"name": "Subject", "value": "Hello"},
                            {"name": "Date", "value": "Mon, 1 Jan 2026 00:00:00 +0000"},
                        ]
                    }
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let summaries = MessagesApi::new(&client)
            .search_summaries(None, &[], 10, 4)
            .await
            .unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, "m1");
        assert_eq!(summaries[0].thread_id, "thread-1");
        assert_eq!(summaries[0].from, "a@example.com");
        assert_eq!(summaries[0].subject, "Hello");
        assert_eq!(summaries[0].snippet, "Hi there");
    }

    #[tokio::test]
    async fn search_summaries_preserves_original_order_under_concurrency() {
        // `buffered` (not `buffer_unordered`) is load-bearing here: with
        // concurrency > 1, an unordered combinator could return hydrated
        // results in completion order rather than search order.
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(page_body(&["m1", "m2", "m3"], None)),
            )
            .mount(&server)
            .await;
        for id in ["m1", "m2", "m3"] {
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path(format!(
                    "/gmail/v1/users/me/messages/{id}"
                )))
                .respond_with(
                    wiremock::ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({"id": id})),
                )
                .mount(&server)
                .await;
        }

        let summaries = MessagesApi::new(&client)
            .search_summaries(None, &[], 10, 4)
            .await
            .unwrap();
        let ids: Vec<&str> = summaries.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["m1", "m2", "m3"]);
    }

    #[tokio::test]
    async fn search_summaries_clamps_zero_concurrency_to_one() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(page_body(&["m1"], None)),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages/m1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "m1"})),
            )
            .mount(&server)
            .await;

        // concurrency = 0 must not panic or deadlock; it's clamped to 1.
        let summaries = MessagesApi::new(&client)
            .search_summaries(None, &[], 10, 0)
            .await
            .unwrap();
        assert_eq!(summaries.len(), 1);
    }

    #[tokio::test]
    async fn search_summaries_propagates_get_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(page_body(&["m1"], None)),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages/m1"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let err = MessagesApi::new(&client)
            .search_summaries(None, &[], 10, 4)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    #[tokio::test]
    async fn search_summaries_stops_dispatching_new_fetches_after_a_failure() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(page_body(&["m1", "m2", "m3", "m4", "m5"], None)),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages/m1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "m1"})),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages/m2"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        // m3/m4/m5 have no mounted mock. With concurrency = 1, `buffered`
        // only creates the next fetch once the previous one completes, so
        // m3 is only dispatched after m2's failure has set the flag — if
        // the short-circuit regresses, m3 (and m4/m5) would hit the server
        // with no matching mock and this MockServer would panic on drop.

        let err = MessagesApi::new(&client)
            .search_summaries(None, &[], 10, 1)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("500") || err.to_string().contains("boom"));

        let requests = server.received_requests().await.unwrap();
        let hydration_requests = requests
            .iter()
            .filter(|r| r.url.path().starts_with("/gmail/v1/users/me/messages/"))
            .count();
        assert_eq!(
            hydration_requests, 2,
            "only m1 and m2 should have been fetched before the failure stopped further dispatch"
        );
    }

    // ── batch_modify ──────────────────────────────────────────────────

    #[tokio::test]
    async fn batch_modify_posts_ids_and_label_deltas_and_treats_204_as_success() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/gmail/v1/users/me/messages/batchModify",
            ))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "ids": ["m1", "m2"],
                "addLabelIds": ["IMPORTANT"],
                "removeLabelIds": ["UNREAD"],
            })))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        MessagesApi::new(&client)
            .batch_modify(&["m1", "m2"], &["IMPORTANT"], &["UNREAD"])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn batch_modify_no_op_on_empty_ids_makes_zero_requests() {
        let client = dead_client();
        // No mounted mocks and no token bootstrap — a real request would
        // fail on connection refused, proving no call was attempted.
        MessagesApi::new(&client)
            .batch_modify(&[], &["IMPORTANT"], &[])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn batch_modify_rejects_more_than_1000_ids_client_side() {
        let client = dead_client();
        let ids: Vec<String> = (0..1001).map(|i| format!("m{i}")).collect();
        let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let err = MessagesApi::new(&client)
            .batch_modify(&id_refs, &[], &[])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("1000"));
    }

    #[tokio::test]
    async fn batch_modify_rejects_invalid_base_url() {
        let client = GmailClient::new("not a url", &test_credentials()).unwrap();
        let err = MessagesApi::new(&client)
            .batch_modify(&["m1"], &[], &["UNREAD"])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Invalid Gmail base URL"));
    }

    #[tokio::test]
    async fn batch_modify_surfaces_insufficient_scope_403_with_reason() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/gmail/v1/users/me/messages/batchModify",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(403).set_body_json(serde_json::json!({
                    "error": {
                        "message": "Insufficient Permission",
                        "errors": [{"reason": "insufficientPermissions"}],
                    }
                })),
            )
            .mount(&server)
            .await;

        let err = MessagesApi::new(&client)
            .batch_modify(&["m1"], &["IMPORTANT"], &[])
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Insufficient Permission"));
        assert!(msg.contains("insufficientPermissions"));
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

    #[test]
    fn effective_cap_passes_through_small_limits() {
        assert_eq!(effective_cap(42), 42);
    }

    // ── effective_concurrency ────────────────────────────────────────

    #[test]
    fn effective_concurrency_raises_zero_to_one() {
        assert_eq!(effective_concurrency(0), 1);
    }

    #[test]
    fn effective_concurrency_clamps_above_max_concurrency() {
        assert_eq!(
            effective_concurrency(MAX_CONCURRENCY + 1000),
            MAX_CONCURRENCY
        );
    }

    #[test]
    fn effective_concurrency_passes_through_small_values() {
        assert_eq!(effective_concurrency(4), 4);
    }
}
