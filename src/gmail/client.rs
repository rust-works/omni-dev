//! Gmail REST API client.
//!
//! Thin `reqwest` wrapper that attaches a Bearer access token (refreshed by
//! an owned [`GmailSession`]) to every request, retries HTTP 429 via the
//! shared [`retry_429`](crate::utils::http::retry_429) driver, and retries
//! exactly once on HTTP 401 by forcing a session refresh. Modelled on
//! [`crate::datadog::client::DatadogClient`]; the difference is Bearer-token
//! auth with in-process refresh instead of two static API keys.

use anyhow::{Context, Result};
use reqwest::{Client, Response};
use url::Url;

use crate::gmail::auth::{GmailCredentials, GmailSession};
use crate::gmail::error::GmailError;
use crate::request_log;
use crate::utils::env::{EnvSource, SystemEnv};
use crate::utils::http::{connect_timeout, read_timeout, retry_if};

/// HTTP client for the Gmail v1 REST API.
pub struct GmailClient {
    client: Client,
    base_url: String,
    session: GmailSession,
}

impl std::fmt::Debug for GmailClient {
    // Hand-written, not derived: omits `session` entirely rather than
    // relying on every nested `Secret` staying wrapped — the safest
    // possible redaction is "not mentioned at all."
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GmailClient")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl GmailClient {
    /// The real Gmail API host. Unlike Datadog, there is no per-tenant
    /// site/region this is *derived* from — [`Self::DEFAULT_BASE_URL`] is
    /// the one real host, overridable wholesale via `GMAIL_API_URL`
    /// (`crate::gmail::auth::GMAIL_API_URL`; see
    /// [`Self::from_credentials_with`]) rather than site-substituted like
    /// Datadog's `DATADOG_API_URL`. [`Self::new`]'s `base_url` parameter is
    /// the lower-level seam both the override and tests go through.
    const DEFAULT_BASE_URL: &'static str = "https://gmail.googleapis.com";

    /// Builds a client against `base_url` with already-loaded credentials.
    ///
    /// For production use, construct via [`Self::from_credentials`]; tests
    /// pass a wiremock URL directly.
    pub fn new(base_url: &str, credentials: &GmailCredentials) -> Result<Self> {
        let client = Client::builder()
            .connect_timeout(connect_timeout())
            .read_timeout(read_timeout())
            .build()
            .context("Failed to build HTTP client")?;
        let session = GmailSession::new(client.clone(), credentials);
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            session,
        })
    }

    /// Creates a client from stored credentials against the real Gmail API
    /// host.
    ///
    /// Respects `GMAIL_API_URL` as an optional override: when set (and
    /// non-empty) in the process environment it replaces
    /// [`Self::DEFAULT_BASE_URL`] wholesale. Added per PR #1466 review —
    /// without it, exercising the CLI's output shapes required a real
    /// Google Cloud project, and there was no way to route through a forced
    /// egress proxy.
    pub fn from_credentials(credentials: &GmailCredentials) -> Result<Self> {
        Self::from_credentials_with(&SystemEnv, credentials)
    }

    /// [`from_credentials`](Self::from_credentials) over an injected
    /// [`EnvSource`], so tests can exercise the `GMAIL_API_URL` override via
    /// `MapEnv` without mutating the process environment.
    pub(crate) fn from_credentials_with(
        env: &impl EnvSource,
        credentials: &GmailCredentials,
    ) -> Result<Self> {
        let base_url = env
            .var(crate::gmail::auth::GMAIL_API_URL)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| Self::DEFAULT_BASE_URL.to_string());
        Self::new(&base_url, credentials)
    }

    /// Returns the API base URL (without trailing slash).
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Builds an absolute API URL by joining `path` onto `base_url`.
    ///
    /// Takes `base_url` (rather than `&self`) so the free `build_*_url`
    /// functions in the API façade modules — and their unit tests, which
    /// pass literal base URLs — can call it without an instance.
    pub(crate) fn api_url(base_url: &str, path: &str) -> Result<Url> {
        Url::parse(&format!("{base_url}{path}")).context("Invalid Gmail base URL")
    }

    /// Checks `response` for success and deserialises its JSON body into `T`.
    pub(crate) async fn parse_response<T: serde::de::DeserializeOwned>(
        &self,
        response: Response,
        context: &'static str,
    ) -> Result<T> {
        if !response.status().is_success() {
            return Err(Self::response_to_error(response).await.into());
        }
        response.json().await.context(context)
    }

    /// Sends an authenticated GET and deserialises the JSON body into `T`.
    pub(crate) async fn get_parsed<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        context: &'static str,
    ) -> Result<T> {
        let response = self.get_json(url).await?;
        self.parse_response(response, context).await
    }

    /// Sends an authenticated GET request and returns the raw response.
    ///
    /// Retries exactly once on HTTP 401 by forcing a session refresh — see
    /// [`Self::send_authorized`] for why both a proactive and a reactive
    /// refresh path exist.
    pub async fn get_json(&self, url: &str) -> Result<Response> {
        self.send_authorized(url, "GET", |client, token| {
            client
                .get(url)
                .bearer_auth(token)
                .header("Accept", "application/json")
        })
        .await
    }

    /// Sends an authenticated POST request with a JSON body and returns the
    /// raw response.
    pub async fn post_json<T: serde::Serialize + Sync + ?Sized>(
        &self,
        url: &str,
        body: &T,
    ) -> Result<Response> {
        self.send_authorized(url, "POST", |client, token| {
            client
                .post(url)
                .bearer_auth(token)
                .header("Content-Type", "application/json")
                .json(body)
        })
        .await
    }

    /// Sends a request built by `build`, retrying exactly once on HTTP 401.
    ///
    /// [`GmailSession::access_token`] already refreshes proactively when the
    /// tracked expiry is near — this reactive path exists for what
    /// proactive tracking can't see: clock skew against Google's clock, or
    /// the token being invalidated server-side mid-run (revoked access). A
    /// second 401 after the retry is authoritative: either the refresh
    /// produced a token that was also rejected, or another caller's
    /// already-current token was reused and still rejected — either way the
    /// problem isn't staleness, so it surfaces as an ordinary
    /// `ApiRequestFailed` rather than retrying again.
    async fn send_authorized<F>(
        &self,
        url: &str,
        method: &'static str,
        build: F,
    ) -> Result<Response>
    where
        F: Fn(&Client, &str) -> reqwest::RequestBuilder + Send + Sync,
    {
        let token = self
            .session
            .access_token()
            .await
            .context("Failed to obtain a Gmail access token")?;
        let response = self
            .send_once(url, method, &build, token.expose_secret())
            .await?;
        if response.status().as_u16() != 401 {
            return Ok(response);
        }
        let refreshed = self
            .session
            .force_refresh(&token)
            .await
            .context("Failed to refresh the Gmail access token after a 401")?;
        self.send_once(url, method, &build, refreshed.expose_secret())
            .await
    }

    async fn send_once<F>(
        &self,
        url: &str,
        method: &'static str,
        build: &F,
        token: &str,
    ) -> Result<Response>
    where
        F: Fn(&Client, &str) -> reqwest::RequestBuilder + Send + Sync,
    {
        retry_if(
            || build(&self.client, token),
            |started, result| {
                request_log::record_http_result("gmail", method, url, started, result);
            },
            |status, body| status == 429 || is_gmail_quota_exceeded(status, body),
        )
        .await
        .with_context(|| format!("Failed to send {method} request to Gmail API"))
    }

    /// Consumes a non-success response into a [`GmailError`].
    ///
    /// Parses Gmail's `{"error":{"message":...,"errors":[{"reason":...}]}}`
    /// envelope into a human message when present (falls back to the raw
    /// body otherwise). Gmail signals quota exhaustion as **403**
    /// `rateLimitExceeded`/`userRateLimitExceeded`, not `429` — unlike plain
    /// 429s, that shape now also drives a retry (see [`is_gmail_quota_exceeded`]
    /// via [`retry_if`](crate::utils::http::retry_if)), so this only sees the
    /// error once retries are exhausted (or the reason didn't match).
    pub async fn response_to_error(response: Response) -> GmailError {
        let status = response.status().as_u16();
        let raw = response.text().await.unwrap_or_default();
        let body = extract_gmail_error_message(&raw).unwrap_or(raw);
        GmailError::ApiRequestFailed { status, body }
    }
}

/// Extracts the `error.errors[0].reason` field from Gmail's JSON error
/// envelope, if present and the body parses as that shape.
fn gmail_error_reason(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    value
        .get("error")
        .and_then(|e| e.get("errors"))
        .and_then(|e| e.as_array())
        .and_then(|a| a.first())
        .and_then(|e| e.get("reason"))
        .and_then(|r| r.as_str())
        .map(str::to_string)
}

fn extract_gmail_error_message(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let message = value.get("error")?.get("message")?.as_str()?;
    let reason = gmail_error_reason(body);
    Some(reason.map_or_else(
        || message.to_string(),
        |r| format!("{message} (reason: {r})"),
    ))
}

/// Whether a response is Gmail's quota-exhaustion signal — **403** with
/// `reason` of `rateLimitExceeded` or `userRateLimitExceeded` specifically,
/// not any 403 with a `reason` field: e.g. `insufficientPermissions` is also
/// a 403 and must never be retried (retrying a scope/permission error just
/// wastes the backoff window before failing anyway).
fn is_gmail_quota_exceeded(status: u16, body: &[u8]) -> bool {
    if status != 403 {
        return false;
    }
    let Ok(text) = std::str::from_utf8(body) else {
        return false;
    };
    matches!(
        gmail_error_reason(text).as_deref(),
        Some("rateLimitExceeded" | "userRateLimitExceeded")
    )
}

/// Test-only seam letting sibling API-façade test modules (which can't
/// reach `GmailClient`'s private fields directly, unlike this module's own
/// `tests` submodule) bootstrap a deterministic access token via wiremock.
#[cfg(test)]
pub(crate) mod test_support {
    use super::GmailClient;
    use crate::gmail::auth::{GmailCredentials, GmailSession};

    /// Replaces `client`'s session with one pointed at an explicit token
    /// endpoint.
    pub(crate) fn replace_session(
        client: &mut GmailClient,
        credentials: &GmailCredentials,
        token_endpoint: &str,
    ) {
        client.session = GmailSession::new_with_token_endpoint(
            client.client.clone(),
            credentials,
            token_endpoint,
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::gmail::auth::GmailScope;
    use crate::utils::secret::Secret;

    fn test_credentials() -> GmailCredentials {
        GmailCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: GmailScope::ReadOnly,
        }
    }

    #[test]
    fn new_client_strips_trailing_slash() {
        let client =
            GmailClient::new("https://gmail.googleapis.com/", &test_credentials()).unwrap();
        assert_eq!(client.base_url(), "https://gmail.googleapis.com");
    }

    #[test]
    fn new_client_preserves_clean_url() {
        let client = GmailClient::new("https://gmail.googleapis.com", &test_credentials()).unwrap();
        assert_eq!(client.base_url(), "https://gmail.googleapis.com");
    }

    #[test]
    fn from_credentials_uses_gmail_api_host() {
        // Via a fresh MapEnv, not from_credentials()'s real SystemEnv — a
        // stray GMAIL_API_URL in the actual process environment must not
        // make this test flaky (mirrors the Datadog precedent,
        // from_credentials_builds_base_url_from_site).
        let env = crate::test_support::env::MapEnv::new();
        let client = GmailClient::from_credentials_with(&env, &test_credentials()).unwrap();
        assert_eq!(client.base_url(), "https://gmail.googleapis.com");
    }

    #[test]
    fn from_credentials_honours_api_url_override() {
        let env = crate::test_support::env::MapEnv::new().with(
            crate::gmail::auth::GMAIL_API_URL,
            "http://proxy.example:8080",
        );
        let client = GmailClient::from_credentials_with(&env, &test_credentials()).unwrap();
        assert_eq!(client.base_url(), "http://proxy.example:8080");
    }

    #[test]
    fn from_credentials_ignores_empty_api_url_override() {
        let env =
            crate::test_support::env::MapEnv::new().with(crate::gmail::auth::GMAIL_API_URL, "");
        let client = GmailClient::from_credentials_with(&env, &test_credentials()).unwrap();
        assert_eq!(client.base_url(), "https://gmail.googleapis.com");
    }

    #[test]
    fn client_debug_never_mentions_session_field() {
        let client = GmailClient::new("https://gmail.googleapis.com", &test_credentials()).unwrap();
        let debug = format!("{client:?}");
        assert!(!debug.contains("secret-1"));
        assert!(!debug.contains("refresh-1"));
        assert!(!debug.contains("session"));
        assert!(debug.contains("GmailClient"));
    }

    /// Mounts a bootstrap token-endpoint mock at the same base URL as the
    /// Gmail API mock — `GmailSession` doesn't distinguish the two hosts in
    /// these tests, so pointing the token endpoint at the wiremock server
    /// too keeps the setup to one server per test.
    async fn client_with_bootstrapped_token(server: &wiremock::MockServer) -> GmailClient {
        // `up_to_n_times(1)` + `with_priority(1)` so a test's own follow-up
        // POST /token mock (registered at `with_priority(2)`, matched only
        // once this one is exhausted) can simulate a second, distinct
        // refresh without either mock racing the other for every request.
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/token"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "bootstrap-token",
                    "expires_in": 3600,
                })),
            )
            .up_to_n_times(1)
            .with_priority(1)
            .mount(server)
            .await;

        let mut client = GmailClient::new(&server.uri(), &test_credentials()).unwrap();
        client.session = GmailSession::new_with_token_endpoint(
            client.client.clone(),
            &test_credentials(),
            &format!("{}/token", server.uri()),
        );
        client
    }

    #[tokio::test]
    async fn get_json_sends_bearer_auth_header() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/test"))
            .and(wiremock::matchers::header(
                "Authorization",
                "Bearer bootstrap-token",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let resp = client
            .get_json(&format!("{}/test", server.uri()))
            .await
            .unwrap();
        assert!(resp.status().is_success());
    }

    #[tokio::test]
    async fn post_json_sends_body_and_bearer_auth() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/test"))
            .and(wiremock::matchers::header(
                "Authorization",
                "Bearer bootstrap-token",
            ))
            .and(wiremock::matchers::body_json(serde_json::json!({"k": "v"})))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let resp = client
            .post_json(
                &format!("{}/test", server.uri()),
                &serde_json::json!({"k": "v"}),
            )
            .await
            .unwrap();
        assert!(resp.status().is_success());
    }

    #[tokio::test]
    async fn get_json_retries_on_429() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/test"))
            .respond_with(wiremock::ResponseTemplate::new(429).append_header("Retry-After", "0"))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/test"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .with_priority(2)
            .mount(&server)
            .await;

        let resp = client
            .get_json(&format!("{}/test", server.uri()))
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
    }

    #[tokio::test]
    async fn get_json_retries_403_rate_limit_exceeded_then_succeeds() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/test"))
            .respond_with(
                wiremock::ResponseTemplate::new(403)
                    .append_header("Retry-After", "0")
                    .set_body_json(serde_json::json!({
                        "error": {"message": "Rate Limit Exceeded", "errors": [{"reason": "rateLimitExceeded"}]}
                    })),
            )
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/test"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .with_priority(2)
            .mount(&server)
            .await;

        let resp = client
            .get_json(&format!("{}/test", server.uri()))
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
    }

    #[tokio::test]
    async fn get_json_does_not_retry_insufficient_permissions_403() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/test"))
            .respond_with(
                wiremock::ResponseTemplate::new(403).set_body_json(serde_json::json!({
                    "error": {"message": "Insufficient Permission", "errors": [{"reason": "insufficientPermissions"}]}
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let resp = client
            .get_json(&format!("{}/test", server.uri()))
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 403);
    }

    #[tokio::test]
    async fn get_json_refreshes_and_retries_once_on_401() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        // The refresh endpoint issues a second, distinct token.
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/token"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "refreshed-token",
                    "expires_in": 3600,
                })),
            )
            .up_to_n_times(1)
            .with_priority(2)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/test"))
            .and(wiremock::matchers::header(
                "Authorization",
                "Bearer bootstrap-token",
            ))
            .respond_with(wiremock::ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/test"))
            .and(wiremock::matchers::header(
                "Authorization",
                "Bearer refreshed-token",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let resp = client
            .get_json(&format!("{}/test", server.uri()))
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
    }

    #[tokio::test]
    async fn get_json_does_not_retry_a_second_time_on_persistent_401() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/token"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "still-rejected-token",
                    "expires_in": 3600,
                })),
            )
            .up_to_n_times(1)
            .with_priority(2)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/test"))
            .respond_with(
                wiremock::ResponseTemplate::new(401).set_body_string("still unauthorized"),
            )
            .expect(2)
            .mount(&server)
            .await;

        let resp = client
            .get_json(&format!("{}/test", server.uri()))
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 401);
    }

    #[tokio::test]
    async fn response_to_error_extracts_gmail_message_and_reason() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        // `userRateLimitExceeded` is now retryable (`is_gmail_quota_exceeded`),
        // so without a zero-delay `Retry-After` this test would wait through
        // the real exponential backoff before giving up.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/test"))
            .respond_with(
                wiremock::ResponseTemplate::new(403)
                    .append_header("Retry-After", "0")
                    .set_body_json(serde_json::json!({
                        "error": {
                            "message": "User Rate Limit Exceeded",
                            "errors": [{"reason": "userRateLimitExceeded"}],
                        }
                    })),
            )
            .mount(&server)
            .await;

        let resp = client
            .get_json(&format!("{}/test", server.uri()))
            .await
            .unwrap();
        let err = GmailClient::response_to_error(resp).await;
        let msg = err.to_string();
        assert!(msg.contains("User Rate Limit Exceeded"));
        assert!(msg.contains("userRateLimitExceeded"));
    }

    #[tokio::test]
    async fn response_to_error_omits_reason_suffix_when_absent() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/test"))
            .respond_with(
                wiremock::ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": {
                        "message": "Invalid request",
                    }
                })),
            )
            .mount(&server)
            .await;

        let resp = client
            .get_json(&format!("{}/test", server.uri()))
            .await
            .unwrap();
        let err = GmailClient::response_to_error(resp).await;
        let msg = err.to_string();
        assert!(msg.contains("Invalid request"));
        assert!(!msg.contains("reason:"));
    }

    #[tokio::test]
    async fn response_to_error_falls_back_to_raw_body_when_not_gmail_shaped() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/test"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("internal error"))
            .mount(&server)
            .await;

        let resp = client
            .get_json(&format!("{}/test", server.uri()))
            .await
            .unwrap();
        let err = GmailClient::response_to_error(resp).await;
        assert!(err.to_string().contains("internal error"));
    }

    #[tokio::test]
    async fn get_json_propagates_network_errors() {
        let client = GmailClient::new("http://127.0.0.1:1", &test_credentials()).unwrap();
        let result = client.get_json("http://127.0.0.1:1/test").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn get_parsed_errors_on_malformed_json_response() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/test"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let result: Result<serde_json::Value> = client
            .get_parsed(&format!("{}/test", server.uri()), "test context")
            .await;
        assert!(result.is_err());
    }
}
