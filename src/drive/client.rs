//! Drive REST API client.
//!
//! A typed wrapper around [`GoogleApiClient`], the shared Google transport
//! (`crate::drive::api_client`), pinned to the Drive v3 host. The transport
//! attaches a Bearer access token refreshed by a shared
//! [`DriveSession`](crate::drive::auth::DriveSession),
//! retries HTTP 429 and Google's 403-shaped quota signal via the shared
//! [`retry_if`](crate::utils::http::retry_if) driver, and retries exactly once
//! on HTTP 401 by forcing a session refresh.
//!
//! Everything host-agnostic moved into `api_client.rs` in issue #1589 so the
//! Sheets API — a *different host*, which a `/drive/v3/...` base URL cannot
//! reach — could reuse it. What stays here is what is genuinely Drive's: the
//! default host, the `DRIVE_API_URL` override, and the type identity that
//! keeps `FilesApi` from being pointed at the wrong API.

use anyhow::Result;
use reqwest::Response;
use url::Url;

use crate::drive::api_client::GoogleApiClient;
use crate::drive::auth::DriveCredentials;
use crate::drive::error::DriveError;
use crate::utils::env::{EnvSource, SystemEnv};

/// The `service` tag Drive's HTTP records carry in the request log.
///
/// `SheetsClient` deliberately uses this **same** tag: the mutation records
/// are already hardcoded `service: "drive"`
/// (`crate::request_log::build_drive_mutation_record`), so splitting the HTTP
/// records off under a separate service would make `omni-dev log --service
/// drive` stop covering half of one feature's traffic. The host stays visible
/// in each record's URL.
pub(crate) const SERVICE_TAG: &str = "drive";

/// Human-readable API name for Drive error messages. Unlike
/// [`SERVICE_TAG`], which both clients share, this differs per API so a
/// Sheets failure doesn't announce itself as a Drive one.
pub(crate) const API_NAME: &str = "Drive";

/// HTTP client for the Drive v3 REST API.
pub struct DriveClient {
    inner: GoogleApiClient,
}

impl std::fmt::Debug for DriveClient {
    // Hand-written, not derived: omits `session` entirely rather than
    // relying on every nested `Secret` staying wrapped — the safest
    // possible redaction is "not mentioned at all." Deriving would also
    // print the inner client's field names, including `session`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriveClient")
            .field("base_url", &self.inner.base_url())
            .finish_non_exhaustive()
    }
}

impl DriveClient {
    /// The real Drive API host. Unlike Gmail, which lives on its own
    /// dedicated `gmail.googleapis.com` subdomain, Drive API v3 lives under
    /// the general `googleapis.com` host with a `/drive/v3/...` path prefix
    /// on each endpoint (see [ADR-0069](../../docs/adrs/adr-0069.md) §6).
    /// Overridable wholesale via `DRIVE_API_URL`
    /// (`crate::drive::auth::DRIVE_API_URL`; see
    /// [`Self::from_credentials_with`]). [`Self::new`]'s `base_url`
    /// parameter is the lower-level seam both the override and tests go
    /// through.
    const DEFAULT_BASE_URL: &'static str = "https://www.googleapis.com";

    /// Builds a client against `base_url` with already-loaded credentials.
    ///
    /// For production use, construct via [`Self::from_credentials`]; tests
    /// pass a wiremock URL directly.
    pub fn new(base_url: &str, credentials: &DriveCredentials) -> Result<Self> {
        Ok(Self {
            inner: GoogleApiClient::new(base_url, credentials, SERVICE_TAG, API_NAME)?,
        })
    }

    /// Creates a client from stored credentials against the real Drive API
    /// host.
    ///
    /// Respects `DRIVE_API_URL` as an optional override: when set (and
    /// non-empty) in the process environment it replaces
    /// [`Self::DEFAULT_BASE_URL`] wholesale — mirrors Gmail's `GMAIL_API_URL`
    /// (PR #1466), used to exercise CLI output shapes without a real Google
    /// Cloud project or to route through a forced egress proxy.
    pub fn from_credentials(credentials: &DriveCredentials) -> Result<Self> {
        Self::from_credentials_with(&SystemEnv, credentials)
    }

    /// [`from_credentials`](Self::from_credentials) over an injected
    /// [`EnvSource`], so tests can exercise the `DRIVE_API_URL` override via
    /// `MapEnv` without mutating the process environment.
    pub(crate) fn from_credentials_with(
        env: &impl EnvSource,
        credentials: &DriveCredentials,
    ) -> Result<Self> {
        let base_url = env
            .var(crate::drive::auth::DRIVE_API_URL)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| Self::DEFAULT_BASE_URL.to_string());
        Self::new(&base_url, credentials)
    }

    /// Returns the API base URL (without trailing slash).
    #[must_use]
    pub fn base_url(&self) -> &str {
        self.inner.base_url()
    }

    /// The shared transport, for deriving a client against another Google
    /// host that reuses this one's OAuth session and connection pool.
    ///
    /// `dead_code`-allowed until `SheetsClient` lands in the following commit
    /// (see [`GoogleApiClient::derived`]).
    #[allow(dead_code)]
    pub(in crate::drive) fn transport(&self) -> &GoogleApiClient {
        &self.inner
    }

    /// Builds an absolute API URL by joining `path` onto `base_url`.
    ///
    /// Takes `base_url` (rather than `&self`) so the free `build_*_url`
    /// functions in the API façade modules — and their unit tests, which
    /// pass literal base URLs — can call it without an instance.
    pub(crate) fn api_url(base_url: &str, path: &str) -> Result<Url> {
        GoogleApiClient::api_url(base_url, path)
    }

    /// Checks `response` for success and deserialises its JSON body into `T`.
    pub(crate) async fn parse_response<T: serde::de::DeserializeOwned>(
        &self,
        response: Response,
        context: &'static str,
    ) -> Result<T> {
        self.inner.parse_response(response, context).await
    }

    /// Sends an authenticated GET and deserialises the JSON body into `T`.
    pub(crate) async fn get_parsed<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        context: &'static str,
    ) -> Result<T> {
        self.inner.get_parsed(url, context).await
    }

    /// Sends an authenticated GET request and returns the raw response.
    ///
    /// Retries exactly once on HTTP 401 by forcing a session refresh.
    pub async fn get_json(&self, url: &str) -> Result<Response> {
        self.inner.get_json(url).await
    }

    /// Sends an authenticated GET request without forcing an `Accept:
    /// application/json` header, for endpoints that return raw bytes rather
    /// than JSON (`files.export`, `files.get?alt=media`) — see
    /// [`Self::get_json`] for the JSON counterpart. Retries exactly once on
    /// HTTP 401, identically to `get_json`.
    pub async fn get_bytes(&self, url: &str) -> Result<Response> {
        self.inner.get_bytes(url).await
    }

    /// Sends an authenticated POST request with a JSON body and returns the
    /// raw response.
    pub async fn post_json<T: serde::Serialize + Sync + ?Sized>(
        &self,
        url: &str,
        body: &T,
    ) -> Result<Response> {
        self.inner.post_json(url, body).await
    }

    /// Sends an authenticated PATCH request with a JSON body and returns the
    /// raw response. Drive's `files.update` (rename/move) is the only PATCH
    /// endpoint this client calls with a JSON body.
    pub async fn patch_json<T: serde::Serialize + Sync + ?Sized>(
        &self,
        url: &str,
        body: &T,
    ) -> Result<Response> {
        self.inner.patch_json(url, body).await
    }

    /// Sends an authenticated POST request with a raw byte body and returns
    /// the raw response — for Drive's multipart-upload endpoint, whose
    /// `multipart/related` body [`crate::drive::files_api::FilesApi::upload`]
    /// hand-assembles (Drive's upload endpoint rejects the
    /// `multipart/form-data` `reqwest::multipart::Form` would produce).
    pub async fn post_bytes(&self, url: &str, body: &[u8], content_type: &str) -> Result<Response> {
        self.inner.post_bytes(url, body, content_type).await
    }

    /// Sends an authenticated PATCH request with a raw byte body and
    /// returns the raw response —
    /// [`crate::drive::files_api::FilesApi::edit_content`]'s simple
    /// media-only content replacement (`uploadType=media`, no multipart
    /// envelope needed since there's no accompanying metadata change).
    pub async fn patch_bytes(
        &self,
        url: &str,
        body: &[u8],
        content_type: &str,
    ) -> Result<Response> {
        self.inner.patch_bytes(url, body, content_type).await
    }

    /// Consumes a non-success response into a [`DriveError`].
    ///
    /// Parses Google's `{"error":{"message":...}}` envelope — in either the
    /// legacy `errors[].reason` or the newer `status` spelling, see
    /// `api_client::error_reason` — into a human message when present
    /// (falls back to the raw body otherwise). Drive signals quota
    /// exhaustion as **403** `userRateLimitExceeded`, not `429`; that shape
    /// drives a retry, so this only sees the error once retries are
    /// exhausted (or the reason didn't match).
    pub async fn response_to_error(response: Response) -> DriveError {
        GoogleApiClient::response_to_error(API_NAME, response).await
    }
}

/// Test-only seam letting sibling API-façade test modules (which can't
/// reach `DriveClient`'s private fields directly, unlike this module's own
/// `tests` submodule) bootstrap a deterministic access token via wiremock.
#[cfg(test)]
#[allow(clippy::expect_used)]
pub(crate) mod test_support {
    use super::{DriveClient, GoogleApiClient, API_NAME, SERVICE_TAG};
    use crate::drive::auth::DriveCredentials;

    /// Replaces `client`'s transport with one whose session points at an
    /// explicit token endpoint.
    ///
    /// Rebuilds rather than mutating a session in place: the session lives
    /// behind an `Arc` that a derived client may share, and assigning a new
    /// one post-hoc would leave any already-derived client pointed at the
    /// real `oauth2.googleapis.com`. Callers that need a *pair* of clients
    /// should build the Drive client through this first and derive after.
    pub(crate) fn replace_session(
        client: &mut DriveClient,
        credentials: &DriveCredentials,
        token_endpoint: &str,
    ) {
        let base_url = client.base_url().to_string();
        client.inner = GoogleApiClient::new_with_token_endpoint(
            &base_url,
            credentials,
            SERVICE_TAG,
            API_NAME,
            token_endpoint,
        )
        .expect("failed to build a test transport");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::DriveGrantedScopes;
    use crate::utils::secret::Secret;

    fn test_credentials() -> DriveCredentials {
        DriveCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: DriveGrantedScopes::READONLY,
        }
    }

    #[test]
    fn new_client_strips_trailing_slash() {
        let client = DriveClient::new("https://www.googleapis.com/", &test_credentials()).unwrap();
        assert_eq!(client.base_url(), "https://www.googleapis.com");
    }

    #[test]
    fn new_client_preserves_clean_url() {
        let client = DriveClient::new("https://www.googleapis.com", &test_credentials()).unwrap();
        assert_eq!(client.base_url(), "https://www.googleapis.com");
    }

    #[test]
    fn from_credentials_uses_drive_api_host() {
        // Via a fresh MapEnv, not from_credentials()'s real SystemEnv — a
        // stray DRIVE_API_URL in the actual process environment must not
        // make this test flaky (mirrors the Datadog/Gmail precedent).
        let env = crate::test_support::env::MapEnv::new();
        let client = DriveClient::from_credentials_with(&env, &test_credentials()).unwrap();
        assert_eq!(client.base_url(), "https://www.googleapis.com");
    }

    #[test]
    fn from_credentials_honours_api_url_override() {
        let env = crate::test_support::env::MapEnv::new().with(
            crate::drive::auth::DRIVE_API_URL,
            "http://proxy.example:8080",
        );
        let client = DriveClient::from_credentials_with(&env, &test_credentials()).unwrap();
        assert_eq!(client.base_url(), "http://proxy.example:8080");
    }

    #[test]
    fn from_credentials_ignores_empty_api_url_override() {
        let env =
            crate::test_support::env::MapEnv::new().with(crate::drive::auth::DRIVE_API_URL, "");
        let client = DriveClient::from_credentials_with(&env, &test_credentials()).unwrap();
        assert_eq!(client.base_url(), "https://www.googleapis.com");
    }

    #[test]
    fn client_debug_never_mentions_session_field() {
        let client = DriveClient::new("https://www.googleapis.com", &test_credentials()).unwrap();
        let debug = format!("{client:?}");
        assert!(!debug.contains("secret-1"));
        assert!(!debug.contains("refresh-1"));
        assert!(!debug.contains("session"));
        assert!(debug.contains("DriveClient"));
    }

    /// Mounts a bootstrap token-endpoint mock at the same base URL as the
    /// Drive API mock — `DriveSession` doesn't distinguish the two hosts in
    /// these tests, so pointing the token endpoint at the wiremock server
    /// too keeps the setup to one server per test.
    async fn client_with_bootstrapped_token(server: &wiremock::MockServer) -> DriveClient {
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

        let mut client = DriveClient::new(&server.uri(), &test_credentials()).unwrap();
        test_support::replace_session(
            &mut client,
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
    async fn get_bytes_sends_bearer_auth_header_without_json_accept_header() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/test"))
            .and(wiremock::matchers::header(
                "Authorization",
                "Bearer bootstrap-token",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_bytes(b"raw bytes".to_vec()),
            )
            .expect(1)
            .mount(&server)
            .await;

        let resp = client
            .get_bytes(&format!("{}/test", server.uri()))
            .await
            .unwrap();
        assert!(resp.status().is_success());
        let bytes = resp.bytes().await.unwrap();
        assert_eq!(bytes.as_ref(), b"raw bytes");
    }

    #[tokio::test]
    async fn get_bytes_retries_on_429() {
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
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"ok".to_vec()))
            .with_priority(2)
            .mount(&server)
            .await;

        let resp = client
            .get_bytes(&format!("{}/test", server.uri()))
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
    }

    #[tokio::test]
    async fn get_bytes_refreshes_and_retries_once_on_401() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
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
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"ok".to_vec()))
            .expect(1)
            .mount(&server)
            .await;

        let resp = client
            .get_bytes(&format!("{}/test", server.uri()))
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
    }

    #[tokio::test]
    async fn get_bytes_propagates_network_errors() {
        let client = DriveClient::new("http://127.0.0.1:1", &test_credentials()).unwrap();
        let result = client.get_bytes("http://127.0.0.1:1/test").await;
        assert!(result.is_err());
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
    async fn patch_json_sends_body_and_bearer_auth() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path("/test"))
            .and(wiremock::matchers::header(
                "Authorization",
                "Bearer bootstrap-token",
            ))
            .and(wiremock::matchers::body_json(
                serde_json::json!({"name": "new-name"}),
            ))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let resp = client
            .patch_json(
                &format!("{}/test", server.uri()),
                &serde_json::json!({"name": "new-name"}),
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
    async fn get_json_retries_403_user_rate_limit_exceeded_then_succeeds() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/test"))
            .respond_with(
                wiremock::ResponseTemplate::new(403)
                    .append_header("Retry-After", "0")
                    .set_body_json(serde_json::json!({
                        "error": {"message": "User Rate Limit Exceeded", "errors": [{"reason": "userRateLimitExceeded"}]}
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
    async fn response_to_error_extracts_drive_message_and_reason() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        // `userRateLimitExceeded` is now retryable (`is_drive_quota_exceeded`),
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
        let err = DriveClient::response_to_error(resp).await;
        let msg = err.to_string();
        assert!(msg.contains("User Rate Limit Exceeded"));
        assert!(msg.contains("userRateLimitExceeded"));
        assert_eq!(err.reason(), Some("userRateLimitExceeded"));
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
        let err = DriveClient::response_to_error(resp).await;
        let msg = err.to_string();
        assert!(msg.contains("Invalid request"));
        assert!(!msg.contains("reason:"));
        assert_eq!(err.reason(), None);
    }

    #[tokio::test]
    async fn response_to_error_falls_back_to_raw_body_when_not_drive_shaped() {
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
        let err = DriveClient::response_to_error(resp).await;
        assert!(err.to_string().contains("internal error"));
    }

    #[tokio::test]
    async fn get_json_propagates_network_errors() {
        let client = DriveClient::new("http://127.0.0.1:1", &test_credentials()).unwrap();
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

    #[tokio::test]
    async fn get_parsed_errors_on_non_success_status_without_parsing_the_body() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/test"))
            .respond_with(
                wiremock::ResponseTemplate::new(404).set_body_json(serde_json::json!({
                    "error": {"message": "File not found"}
                })),
            )
            .mount(&server)
            .await;

        let result: Result<serde_json::Value> = client
            .get_parsed(&format!("{}/test", server.uri()), "test context")
            .await;
        let err = result.unwrap_err();
        assert!(err.to_string().contains("File not found"));
    }

    #[tokio::test]
    async fn put_json_sends_body_and_bearer_auth() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/test"))
            .and(wiremock::matchers::header(
                "Authorization",
                "Bearer bootstrap-token",
            ))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "values": [["a", "b"]],
            })))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("{}"))
            .expect(1)
            .mount(&server)
            .await;

        let response = client
            .transport()
            .put_json(
                &format!("{}/test", server.uri()),
                &serde_json::json!({"values": [["a", "b"]]}),
            )
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
    }

    #[tokio::test]
    async fn a_drive_failure_still_names_drive() {
        // The other half of the pair: threading an API name must not have
        // changed what Drive's own failures say.
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/test"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let response = client
            .get_json(&format!("{}/test", server.uri()))
            .await
            .unwrap();
        let text = DriveClient::response_to_error(response).await.to_string();
        assert!(text.starts_with("Drive API request failed"), "{text}");
    }

    #[tokio::test]
    async fn response_to_error_reads_the_google_rpc_envelope_sheets_returns() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/test"))
            .respond_with(
                wiremock::ResponseTemplate::new(403).set_body_json(serde_json::json!({
                    "error": {
                        "code": 403,
                        "message": "The caller does not have permission",
                        "status": "PERMISSION_DENIED",
                    },
                })),
            )
            .mount(&server)
            .await;

        let response = client
            .get_json(&format!("{}/test", server.uri()))
            .await
            .unwrap();
        let error = DriveClient::response_to_error(response).await;
        let DriveError::ApiRequestFailed { reason, .. } = error else {
            panic!("expected ApiRequestFailed");
        };
        assert_eq!(reason.as_deref(), Some("PERMISSION_DENIED"));
    }
}
