//! Datadog REST API client.
//!
//! Thin `reqwest` wrapper that injects the `DD-API-KEY` and
//! `DD-APPLICATION-KEY` headers on every request and retries 429 responses via
//! the shared [`retry_429`](crate::utils::http::retry_429) driver (which honours
//! `Retry-After` / `X-RateLimit-Reset`).

use anyhow::{Context, Result};
use reqwest::Client;
use url::Url;

use crate::datadog::auth::{base_url_for_site, DatadogCredentials};
use crate::datadog::error::DatadogError;
use crate::request_log;
use crate::utils::http::{retry_429, REQUEST_TIMEOUT};
use crate::utils::secret::Secret;

/// HTTP client for Datadog REST APIs.
#[derive(Debug)]
pub struct DatadogClient {
    client: Client,
    base_url: String,
    api_key: Secret,
    app_key: Secret,
}

impl DatadogClient {
    /// Creates a new Datadog API client.
    ///
    /// `base_url` should be the full API host, e.g. `https://api.datadoghq.com`.
    /// For production use, construct via [`Self::from_credentials`]; tests
    /// pass a wiremock URL directly.
    pub fn new(base_url: &str, api_key: &str, app_key: &str) -> Result<Self> {
        let client = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("Failed to build HTTP client")?;

        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            app_key: app_key.into(),
        })
    }

    /// Creates a client from stored credentials.
    ///
    /// Respects `DATADOG_API_URL` as an optional override: when set in the
    /// process environment it replaces the site-derived base URL. Used for
    /// tests (wiremock) and on-prem Datadog installs.
    pub fn from_credentials(creds: &DatadogCredentials) -> Result<Self> {
        Self::from_credentials_with(&crate::utils::env::SystemEnv, creds)
    }

    /// [`from_credentials`](Self::from_credentials) over an injected
    /// [`EnvSource`](crate::utils::env::EnvSource).
    ///
    /// Tests pass a pure `MapEnv` to exercise the `DATADOG_API_URL` override
    /// without mutating the process environment (issue #1030).
    pub(crate) fn from_credentials_with(
        env: &impl crate::utils::env::EnvSource,
        creds: &DatadogCredentials,
    ) -> Result<Self> {
        let base_url = env
            .var(crate::datadog::auth::DATADOG_API_URL)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| base_url_for_site(&creds.site));
        Self::new(
            &base_url,
            creds.api_key.expose_secret(),
            creds.app_key.expose_secret(),
        )
    }

    /// Returns the API base URL (without trailing slash).
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Builds an absolute API URL by joining `path` onto `base_url`.
    ///
    /// `path` is the full path portion including the leading `/api/…` segment
    /// (the version varies: `/api/v1/…`, `/api/v2/…`). Centralises the
    /// `Url::parse(…).context("Invalid Datadog base URL")` spelling repeated
    /// across the `*_api.rs` modules. Takes `base_url` (rather than `&self`) so
    /// the free `build_*_url` functions — and their unit tests, which pass
    /// literal base URLs — can call it unchanged.
    pub(crate) fn api_url(base_url: &str, path: &str) -> Result<Url> {
        Url::parse(&format!("{base_url}{path}")).context("Invalid Datadog base URL")
    }

    /// Checks `response` for success and deserialises its JSON body into `T`.
    ///
    /// Non-success responses become a [`DatadogError`] via
    /// [`Self::response_to_error`] (preserving the 429 rate-limit summary); on
    /// success the body is parsed with `context` attached on failure. Used by
    /// the paginated and POST call sites that already hold a response;
    /// single-shot GETs use [`Self::get_parsed`].
    pub(crate) async fn parse_response<T: serde::de::DeserializeOwned>(
        &self,
        response: reqwest::Response,
        context: &'static str,
    ) -> Result<T> {
        if !response.status().is_success() {
            return Err(Self::response_to_error(response).await.into());
        }
        response.json().await.context(context)
    }

    /// Sends an authenticated GET and deserialises the JSON body into `T`.
    ///
    /// Convenience wrapper over [`Self::get_json`] + [`Self::parse_response`]
    /// for the common single-request GET-then-parse pattern.
    pub(crate) async fn get_parsed<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        context: &'static str,
    ) -> Result<T> {
        let response = self.get_json(url).await?;
        self.parse_response(response, context).await
    }

    /// Sends an authenticated GET request and returns the raw response.
    pub async fn get_json(&self, url: &str) -> Result<reqwest::Response> {
        retry_429(
            || {
                self.client
                    .get(url)
                    .header("DD-API-KEY", self.api_key.expose_secret())
                    .header("DD-APPLICATION-KEY", self.app_key.expose_secret())
                    .header("Accept", "application/json")
            },
            |started, result| {
                request_log::record_http_result("datadog", "GET", url, started, result);
            },
        )
        .await
        .context("Failed to send GET request to Datadog API")
    }

    /// Sends an authenticated POST request with a JSON body and returns the raw response.
    pub async fn post_json<T: serde::Serialize + Sync + ?Sized>(
        &self,
        url: &str,
        body: &T,
    ) -> Result<reqwest::Response> {
        retry_429(
            || {
                self.client
                    .post(url)
                    .header("DD-API-KEY", self.api_key.expose_secret())
                    .header("DD-APPLICATION-KEY", self.app_key.expose_secret())
                    .header("Content-Type", "application/json")
                    .header("Accept", "application/json")
                    .json(body)
            },
            |started, result| {
                request_log::record_http_result("datadog", "POST", url, started, result);
            },
        )
        .await
        .context("Failed to send POST request to Datadog API")
    }

    /// Consumes a non-success response and turns it into a [`DatadogError`].
    ///
    /// For 429 responses, appends a human-readable rate-limit summary
    /// (extracted from `X-RateLimit-*` headers) to the body, so the caller
    /// sees why the retry loop gave up.
    pub async fn response_to_error(response: reqwest::Response) -> DatadogError {
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let body = response.text().await.unwrap_or_default();
        let body = if status == 429 {
            match format_rate_limit(&headers) {
                Some(suffix) => format!("{body} {suffix}").trim().to_string(),
                None => body,
            }
        } else {
            body
        };
        DatadogError::ApiRequestFailed { status, body }
    }
}

fn format_rate_limit(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let remaining = headers
        .get("X-RateLimit-Remaining")
        .and_then(|v| v.to_str().ok());
    let reset = headers
        .get("X-RateLimit-Reset")
        .and_then(|v| v.to_str().ok());
    let limit = headers
        .get("X-RateLimit-Limit")
        .and_then(|v| v.to_str().ok());

    if remaining.is_none() && reset.is_none() && limit.is_none() {
        return None;
    }

    let mut parts = Vec::new();
    if let Some(v) = remaining {
        parts.push(format!("remaining={v}"));
    }
    if let Some(v) = limit {
        parts.push(format!("limit={v}"));
    }
    if let Some(v) = reset {
        parts.push(format!("reset_in={v}s"));
    }
    Some(format!("[rate-limit: {}]", parts.join(", ")))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn new_client_strips_trailing_slash() {
        let client = DatadogClient::new("https://api.datadoghq.com/", "api", "app").unwrap();
        assert_eq!(client.base_url(), "https://api.datadoghq.com");
    }

    #[test]
    fn new_client_preserves_clean_url() {
        let client = DatadogClient::new("https://api.datadoghq.com", "api", "app").unwrap();
        assert_eq!(client.base_url(), "https://api.datadoghq.com");
    }

    #[test]
    fn client_debug_redacts_keys() {
        let client = DatadogClient::new(
            "https://api.datadoghq.com",
            "sekret-api-key",
            "sekret-app-key",
        )
        .unwrap();
        // Debug must never print the key values (#1131).
        let debug = format!("{client:?}");
        assert!(!debug.contains("sekret-api-key"), "leaked api_key: {debug}");
        assert!(!debug.contains("sekret-app-key"), "leaked app_key: {debug}");
        assert!(debug.contains("api_key: <redacted>"));
        assert!(debug.contains("app_key: <redacted>"));
    }

    #[test]
    fn from_credentials_builds_base_url_from_site() {
        let env = crate::test_support::env::MapEnv::new();
        let creds = DatadogCredentials {
            api_key: "api".into(),
            app_key: "app".into(),
            site: "us5.datadoghq.com".to_string(),
        };
        let client = DatadogClient::from_credentials_with(&env, &creds).unwrap();
        assert_eq!(client.base_url(), "https://api.us5.datadoghq.com");
    }

    #[test]
    fn from_credentials_honours_api_url_override() {
        let env = crate::test_support::env::MapEnv::new().with(
            crate::datadog::auth::DATADOG_API_URL,
            "http://proxy.example:8080",
        );
        let creds = DatadogCredentials {
            api_key: "api".into(),
            app_key: "app".into(),
            site: "us5.datadoghq.com".to_string(),
        };
        let client = DatadogClient::from_credentials_with(&env, &creds).unwrap();
        assert_eq!(client.base_url(), "http://proxy.example:8080");
    }

    #[test]
    fn from_credentials_ignores_empty_api_url_override() {
        let env =
            crate::test_support::env::MapEnv::new().with(crate::datadog::auth::DATADOG_API_URL, "");
        let creds = DatadogCredentials {
            api_key: "api".into(),
            app_key: "app".into(),
            site: "datadoghq.com".to_string(),
        };
        let client = DatadogClient::from_credentials_with(&env, &creds).unwrap();
        assert_eq!(client.base_url(), "https://api.datadoghq.com");
    }

    #[tokio::test]
    async fn get_json_sends_auth_headers() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/test"))
            .and(wiremock::matchers::header("DD-API-KEY", "my-api"))
            .and(wiremock::matchers::header("DD-APPLICATION-KEY", "my-app"))
            .and(wiremock::matchers::header("Accept", "application/json"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "my-api", "my-app").unwrap();
        let resp = client
            .get_json(&format!("{}/test", server.uri()))
            .await
            .unwrap();
        assert!(resp.status().is_success());
    }

    #[tokio::test]
    async fn post_json_sends_body_and_auth() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/test"))
            .and(wiremock::matchers::header("DD-API-KEY", "my-api"))
            .and(wiremock::matchers::header("DD-APPLICATION-KEY", "my-app"))
            .and(wiremock::matchers::header(
                "Content-Type",
                "application/json",
            ))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "query": "hello"
            })))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "1"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "my-api", "my-app").unwrap();
        let body = serde_json::json!({"query": "hello"});
        let resp = client
            .post_json(&format!("{}/test", server.uri()), &body)
            .await
            .unwrap();
        assert!(resp.status().is_success());
    }

    #[tokio::test]
    async fn get_json_retries_on_429_via_retry_after() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/test"))
            .respond_with(wiremock::ResponseTemplate::new(429).append_header("Retry-After", "0"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/test"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let resp = client
            .get_json(&format!("{}/test", server.uri()))
            .await
            .unwrap();
        assert!(resp.status().is_success());
    }

    #[tokio::test]
    async fn get_json_retries_on_429_via_x_ratelimit_reset() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/test"))
            .respond_with(
                wiremock::ResponseTemplate::new(429).append_header("X-RateLimit-Reset", "0"),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/test"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let resp = client
            .get_json(&format!("{}/test", server.uri()))
            .await
            .unwrap();
        assert!(resp.status().is_success());
    }

    #[tokio::test]
    async fn post_json_retries_on_429() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/test"))
            .respond_with(wiremock::ResponseTemplate::new(429).append_header("Retry-After", "0"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/test"))
            .respond_with(wiremock::ResponseTemplate::new(201))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let resp = client
            .post_json(
                &format!("{}/test", server.uri()),
                &serde_json::json!({"k": "v"}),
            )
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 201);
    }

    #[tokio::test]
    async fn get_json_returns_429_after_max_retries() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/test"))
            .respond_with(wiremock::ResponseTemplate::new(429).append_header("Retry-After", "0"))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let resp = client
            .get_json(&format!("{}/test", server.uri()))
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 429);
    }

    #[tokio::test]
    async fn response_to_error_surfaces_rate_limit_headers_on_429() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/test"))
            .respond_with(
                wiremock::ResponseTemplate::new(429)
                    .append_header("Retry-After", "0")
                    .append_header("X-RateLimit-Remaining", "0")
                    .append_header("X-RateLimit-Reset", "42")
                    .append_header("X-RateLimit-Limit", "100")
                    .set_body_string("too many"),
            )
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let resp = client
            .get_json(&format!("{}/test", server.uri()))
            .await
            .unwrap();
        let err = DatadogClient::response_to_error(resp).await;
        let msg = err.to_string();
        assert!(msg.contains("429"));
        assert!(msg.contains("too many"));
        assert!(msg.contains("remaining=0"));
        assert!(msg.contains("limit=100"));
        assert!(msg.contains("reset_in=42s"));
    }

    #[tokio::test]
    async fn response_to_error_does_not_add_rate_limit_suffix_on_non_429() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/test"))
            .respond_with(wiremock::ResponseTemplate::new(401).set_body_string("Unauthorized"))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let resp = client
            .get_json(&format!("{}/test", server.uri()))
            .await
            .unwrap();
        let err = DatadogClient::response_to_error(resp).await;
        let msg = err.to_string();
        assert!(msg.contains("401"));
        assert!(msg.contains("Unauthorized"));
        assert!(!msg.contains("rate-limit"));
    }

    #[tokio::test]
    async fn response_to_error_omits_suffix_when_no_rate_limit_headers() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/test"))
            .respond_with(
                wiremock::ResponseTemplate::new(429)
                    .append_header("Retry-After", "0")
                    .set_body_string("slow down"),
            )
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let resp = client
            .get_json(&format!("{}/test", server.uri()))
            .await
            .unwrap();
        let err = DatadogClient::response_to_error(resp).await;
        let msg = err.to_string();
        assert!(msg.contains("slow down"));
        assert!(!msg.contains("rate-limit"));
    }
}
