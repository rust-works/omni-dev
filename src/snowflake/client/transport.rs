//! HTTP transport for the Snowflake v1 REST endpoints.
//!
//! Every endpoint speaks the same envelope: `{ "data": …, "success": bool,
//! "code": "…", "message": "…" }`. Control calls use [`Transport::post`];
//! statements use [`Transport::post_statement`], which submits the query and —
//! because the v1 endpoint returns an "in progress" code with a result URL for
//! anything slower than the server's synchronous window — **polls** that URL
//! until the result is ready.
//!
//! Timeouts: a single request is bounded by a `tokio` deadline (so a hung
//! request fails without being mistaken for a retryable read-timeout that would
//! re-run a heavy query); the connection is bounded by `connect_timeout`; and a
//! whole statement (submit + polling) is bounded by the caller's deadline.
//! Transient failures (connection errors, `429`/`502`/`503`/`504`) are retried
//! with backoff, reusing the caller's `requestId` (an idempotency key).

use std::time::{Duration, Instant};

use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use serde_json::Value;
use url::Url;

use super::error::{Error, Result};
use crate::request_log;

/// Response codes that mean the session token is no longer valid.
const SESSION_EXPIRED_CODES: &[&str] = &["390112", "390114", "390108"];
/// Response codes that mean the query is still running and must be polled.
const IN_PROGRESS_CODES: &[&str] = &["333333", "333334"];
/// HTTP statuses worth retrying (transient/server-side).
const RETRYABLE_STATUSES: &[&str] = &["429", "502", "503", "504"];
/// Connection establishment timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Total send attempts (1 initial + retries) for one request.
const MAX_ATTEMPTS: u32 = 3;
/// Base backoff between attempts (doubled each retry).
const BACKOFF_BASE: Duration = Duration::from_millis(200);

/// A parsed Snowflake response envelope.
struct RawResponse {
    success: bool,
    code: String,
    message: String,
    data: Value,
}

/// A thin HTTP transport bound to one account's API host.
pub(crate) struct Transport {
    http: reqwest::Client,
    base_url: Url,
    /// Per-request deadline (covers connect + send + body of one request).
    request_timeout: Duration,
}

impl Transport {
    /// Builds a transport for `host` (e.g. `acct.snowflakecomputing.com`).
    pub(crate) fn new(host: &str, request_timeout: Duration) -> Result<Self> {
        let base_url = Url::parse(&format!("https://{host}/"))
            .map_err(|e| Error::Protocol(format!("invalid Snowflake host '{host}': {e}")))?;
        Self::with_base_url(base_url, request_timeout)
    }

    /// Builds a transport for an explicit base URL (used by tests).
    pub(crate) fn with_base_url(base_url: Url, request_timeout: Duration) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(Error::Transport)?;
        Ok(Self {
            http,
            base_url,
            request_timeout,
        })
    }

    /// POSTs a control request and returns its `data` (login/renew/heartbeat).
    ///
    /// # Errors
    ///
    /// [`Error::SessionExpired`] for the expiry codes, [`Error::Server`] for any
    /// other `success: false`, and transport/protocol errors otherwise.
    pub(crate) async fn post(
        &self,
        path: &str,
        query: &[(&str, &str)],
        body: &Value,
        token: Option<&str>,
    ) -> Result<Value> {
        let url = self.resolve(path, query)?;
        let raw = self
            .send_with_retry("POST", &url, || self.post_request(url.clone(), body, token))
            .await?;
        finalize(raw)
    }

    /// Submits a SQL statement and returns its `data`, polling the async result
    /// URL until the query completes (or `deadline` elapses).
    ///
    /// # Errors
    ///
    /// As [`post`](Self::post), plus [`Error::Server`] (`code: "timeout"`) if the
    /// query does not finish within `deadline`.
    pub(crate) async fn post_statement(
        &self,
        query: &[(&str, &str)],
        body: &Value,
        token: &str,
        deadline: Duration,
        poll_interval: Duration,
    ) -> Result<Value> {
        let url = self.resolve("queries/v1/query-request", query)?;
        let mut raw = self
            .send_with_retry("POST", &url, || {
                self.post_request(url.clone(), body, Some(token))
            })
            .await?;

        if SESSION_EXPIRED_CODES.contains(&raw.code.as_str()) {
            return Err(Error::SessionExpired);
        }
        if IN_PROGRESS_CODES.contains(&raw.code.as_str()) {
            let result_url = raw
                .data
                .get("getResultUrl")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    Error::Protocol("async query response missing getResultUrl".into())
                })?;
            let url = Url::parse(result_url)
                .or_else(|_| self.base_url.join(result_url))
                .map_err(|e| Error::Protocol(format!("invalid result URL '{result_url}': {e}")))?;
            raw = self
                .poll_until_ready(&url, token, deadline, poll_interval)
                .await?;
        }
        finalize(raw)
    }

    /// Fetches one child statement's result by its query id, polling until it is
    /// no longer in progress. Used by the multi-statement path, where the parent
    /// submission returns a list of child `resultIds` rather than inline rows;
    /// each is retrieved from the `/queries/{id}/result` monitoring endpoint.
    ///
    /// # Errors
    ///
    /// As [`post_statement`](Self::post_statement).
    pub(crate) async fn get_statement_result(
        &self,
        query_id: &str,
        token: &str,
        deadline: Duration,
        poll_interval: Duration,
    ) -> Result<Value> {
        let rid = request_id();
        let url = self.resolve(
            &format!("queries/{query_id}/result"),
            &[("requestId", rid.as_str())],
        )?;
        let mut raw = self
            .send_with_retry("GET", &url, || self.get_request(url.clone(), Some(token)))
            .await?;
        if SESSION_EXPIRED_CODES.contains(&raw.code.as_str()) {
            return Err(Error::SessionExpired);
        }
        if IN_PROGRESS_CODES.contains(&raw.code.as_str()) {
            raw = self
                .poll_until_ready(&url, token, deadline, poll_interval)
                .await?;
        }
        finalize(raw)
    }

    /// Polls `url` (GET) until the query is no longer in progress, or `deadline`
    /// elapses. Sleeps before each poll so the caller's initial request is not
    /// re-issued immediately.
    async fn poll_until_ready(
        &self,
        url: &Url,
        token: &str,
        deadline: Duration,
        poll_interval: Duration,
    ) -> Result<RawResponse> {
        let start = Instant::now();
        loop {
            tokio::time::sleep(poll_interval).await;
            let raw = self
                .send_with_retry("GET", url, || self.get_request(url.clone(), Some(token)))
                .await?;
            if !IN_PROGRESS_CODES.contains(&raw.code.as_str()) {
                return Ok(raw);
            }
            if start.elapsed() >= deadline {
                return Err(Error::Server {
                    code: "timeout".to_string(),
                    message: format!("query did not finish within {deadline:?}"),
                });
            }
        }
    }

    /// Performs a plain GET (no envelope) and returns the body bytes — used to
    /// download result chunks from blob storage with their per-chunk headers.
    ///
    /// # Errors
    ///
    /// [`Error::Server`] on a non-success status, or a transport error.
    pub(crate) async fn get_bytes(
        &self,
        url: &str,
        headers: &[(String, String)],
    ) -> Result<Vec<u8>> {
        let mut request = self.http.get(url);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let send = async {
            let response = request.send().await?;
            let status = response.status();
            let bytes = response.bytes().await?;
            Ok::<_, reqwest::Error>((status, bytes))
        };
        let (status, bytes) = match tokio::time::timeout(self.request_timeout, send).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Err(Error::Transport(e)),
            Err(_) => {
                return Err(Error::Server {
                    code: "timeout".to_string(),
                    message: "result-chunk download timed out".to_string(),
                })
            }
        };
        if !status.is_success() {
            return Err(Error::Server {
                code: status.as_u16().to_string(),
                message: "result-chunk download failed".to_string(),
            });
        }
        Ok(bytes.to_vec())
    }

    /// Resolves `path` + `query` against the base URL.
    fn resolve(&self, path: &str, query: &[(&str, &str)]) -> Result<Url> {
        let mut url = self
            .base_url
            .join(path)
            .map_err(|e| Error::Protocol(format!("invalid path '{path}': {e}")))?;
        {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in query {
                pairs.append_pair(key, value);
            }
        }
        Ok(url)
    }

    /// Builds a POST request (Snowflake headers + optional token auth).
    fn post_request(&self, url: Url, body: &Value, token: Option<&str>) -> reqwest::RequestBuilder {
        let mut request = self
            .http
            .post(url)
            .header(ACCEPT, "application/snowflake")
            .header(CONTENT_TYPE, "application/json")
            .json(body);
        if let Some(token) = token {
            request = request.header(AUTHORIZATION, format!("Snowflake Token=\"{token}\""));
        }
        request
    }

    /// Builds a GET request (Snowflake headers + optional token auth).
    fn get_request(&self, url: Url, token: Option<&str>) -> reqwest::RequestBuilder {
        let mut request = self.http.get(url).header(ACCEPT, "application/snowflake");
        if let Some(token) = token {
            request = request.header(AUTHORIZATION, format!("Snowflake Token=\"{token}\""));
        }
        request
    }

    /// Appends a best-effort HTTP record for one Snowflake request attempt,
    /// flagging `via_daemon` when running inside the daemon process. The pooled
    /// `daemon_session_id` is not visible at this transport boundary (one
    /// `Transport` is shared across pooled sessions); threading it is a follow-up.
    fn log_request(
        &self,
        method: &str,
        url: &Url,
        started: Instant,
        status: Option<u16>,
        error: Option<&str>,
    ) {
        let via_daemon = matches!(
            request_log::current_context().source,
            request_log::Source::Daemon
        );
        request_log::record_http_with(
            "snowflake",
            method,
            url.as_str(),
            started,
            status,
            error,
            request_log::HttpExtra {
                via_daemon,
                ..Default::default()
            },
        );
    }

    /// Sends a freshly built request, retrying transient failures with backoff.
    /// `method`/`url` are passed only for the request log.
    async fn send_with_retry(
        &self,
        method: &str,
        url: &Url,
        build: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<RawResponse> {
        let mut attempt = 1;
        loop {
            match self.send_once(method, url, build()).await {
                Ok(raw) => return Ok(raw),
                Err(err) if attempt < MAX_ATTEMPTS && is_retryable(&err) => {
                    tokio::time::sleep(BACKOFF_BASE * 2u32.pow(attempt - 1)).await;
                    attempt += 1;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// Sends one request (bounded by `request_timeout`) and parses the envelope.
    async fn send_once(
        &self,
        method: &str,
        url: &Url,
        request: reqwest::RequestBuilder,
    ) -> Result<RawResponse> {
        let started = Instant::now();
        let send = async {
            let response = request.send().await?;
            let status = response.status();
            let text = response.text().await?;
            Ok::<_, reqwest::Error>((status, text))
        };
        let (status, text) = match tokio::time::timeout(self.request_timeout, send).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                self.log_request(method, url, started, None, Some(&e.to_string()));
                return Err(Error::Transport(e));
            }
            Err(_) => {
                self.log_request(method, url, started, None, Some("request timed out"));
                return Err(Error::Server {
                    code: "timeout".to_string(),
                    message: format!("request exceeded {:?}", self.request_timeout),
                });
            }
        };
        self.log_request(method, url, started, Some(status.as_u16()), None);
        if !status.is_success() {
            return Err(Error::Server {
                code: status.as_u16().to_string(),
                message: text,
            });
        }
        let envelope: Value = serde_json::from_str(&text)
            .map_err(|e| Error::Protocol(format!("invalid response JSON: {e}")))?;
        Ok(RawResponse {
            success: envelope
                .get("success")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            code: envelope
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            message: envelope
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            data: envelope.get("data").cloned().unwrap_or(Value::Null),
        })
    }
}

/// Maps a final (non-in-progress) envelope to its `data`, or an error.
fn finalize(raw: RawResponse) -> Result<Value> {
    if SESSION_EXPIRED_CODES.contains(&raw.code.as_str()) {
        return Err(Error::SessionExpired);
    }
    if !raw.success {
        return Err(Error::Server {
            code: raw.code,
            message: if raw.message.is_empty() {
                "unknown error".to_string()
            } else {
                raw.message
            },
        });
    }
    Ok(raw.data)
}

/// Whether an error is worth retrying (transient transport / server status).
/// Note: a per-request timeout (`code: "timeout"`) is **not** retried, so a slow
/// query is never re-run.
fn is_retryable(error: &Error) -> bool {
    match error {
        Error::Transport(e) => e.is_connect() || e.is_timeout(),
        Error::Server { code, .. } => RETRYABLE_STATUSES.contains(&code.as_str()),
        _ => false,
    }
}

/// A random request id (uuid-v4 shape) for `requestId` query params.
pub(crate) fn request_id() -> String {
    let hex = format!("{:032x}", rand::random::<u128>());
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32],
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn transport(server: &MockServer) -> Transport {
        let base = Url::parse(&server.uri()).unwrap().join("/").unwrap();
        Transport::with_base_url(base, Duration::from_secs(5)).unwrap()
    }

    #[test]
    fn request_id_has_uuid_shape() {
        let id = request_id();
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'));
        assert_ne!(request_id(), request_id());
    }

    #[tokio::test]
    async fn retries_transient_then_succeeds_reusing_request_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/x"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(2)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/x"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true, "data": { "ok": 1 }
            })))
            .with_priority(2)
            .mount(&server)
            .await;

        let data = transport(&server)
            .post(
                "x",
                &[("requestId", "fixed-id")],
                &serde_json::json!({}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(data, serde_json::json!({ "ok": 1 }));

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 3);
        for req in &requests {
            assert!(req
                .url
                .query()
                .unwrap_or_default()
                .contains("requestId=fixed-id"));
        }
    }

    #[tokio::test]
    async fn non_retryable_server_error_is_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/y"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": false, "code": "001003", "message": "SQL compilation error"
            })))
            .mount(&server)
            .await;

        let err = transport(&server)
            .post("y", &[("requestId", "id")], &serde_json::json!({}), None)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Server { .. }));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn post_statement_polls_until_the_query_completes() {
        let server = MockServer::start().await;
        // Submit returns "in progress" with a result URL.
        Mock::given(method("POST"))
            .and(path("/queries/v1/query-request"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true, "code": "333333", "data": { "getResultUrl": "/poll/123" }
            })))
            .mount(&server)
            .await;
        // First poll: still in progress.
        Mock::given(method("GET"))
            .and(path("/poll/123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true, "code": "333333", "data": {}
            })))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        // Then the result.
        Mock::given(method("GET"))
            .and(path("/poll/123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true,
                "data": { "rowtype": [{ "name": "N", "type": "fixed" }], "rowset": [["1"]] }
            })))
            .with_priority(2)
            .mount(&server)
            .await;

        let data = transport(&server)
            .post_statement(
                &[("requestId", "id")],
                &serde_json::json!({ "sqlText": "select 1" }),
                "tok",
                Duration::from_secs(5),
                Duration::from_millis(10),
            )
            .await
            .unwrap();
        assert_eq!(data["rowtype"][0]["name"], "N");
        assert_eq!(data["rowset"][0][0], "1");
        // submit + 2 polls.
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn post_statement_times_out_if_never_ready() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/queries/v1/query-request"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true, "code": "333334", "data": { "getResultUrl": "/poll/x" }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/poll/x"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true, "code": "333334", "data": {}
            })))
            .mount(&server)
            .await;

        let err = transport(&server)
            .post_statement(
                &[("requestId", "id")],
                &serde_json::json!({ "sqlText": "select 1" }),
                "tok",
                Duration::from_millis(30),
                Duration::from_millis(10),
            )
            .await
            .unwrap_err();
        match err {
            Error::Server { code, .. } => assert_eq!(code, "timeout"),
            other => panic!("expected timeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_statement_result_fetches_a_child_result() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/queries/01ab-cdef/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true,
                "data": { "rowtype": [{ "name": "N", "type": "fixed" }], "rowset": [["7"]] }
            })))
            .mount(&server)
            .await;

        let data = transport(&server)
            .get_statement_result(
                "01ab-cdef",
                "tok",
                Duration::from_secs(5),
                Duration::from_millis(10),
            )
            .await
            .unwrap();
        assert_eq!(data["rowset"][0][0], "7");
        // A single GET, no polling.
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn get_statement_result_polls_an_in_progress_child() {
        let server = MockServer::start().await;
        // First GET: still running.
        Mock::given(method("GET"))
            .and(path("/queries/qid/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true, "code": "333333", "data": {}
            })))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        // Then the result.
        Mock::given(method("GET"))
            .and(path("/queries/qid/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true,
                "data": { "rowtype": [{ "name": "N", "type": "fixed" }], "rowset": [["1"]] }
            })))
            .with_priority(2)
            .mount(&server)
            .await;

        let data = transport(&server)
            .get_statement_result(
                "qid",
                "tok",
                Duration::from_secs(5),
                Duration::from_millis(10),
            )
            .await
            .unwrap();
        assert_eq!(data["rowset"][0][0], "1");
        // initial + one poll.
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }
}
