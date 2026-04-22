//! Datadog REST API client.
//!
//! Thin `reqwest` wrapper that injects the `DD-API-KEY` and
//! `DD-APPLICATION-KEY` headers on every request and retries 429 responses
//! with `Retry-After` / `X-RateLimit-Reset` awareness.

use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Client;

use crate::datadog::auth::{base_url_for_site, DatadogCredentials};
use crate::datadog::error::DatadogError;

/// HTTP request timeout for Datadog API calls.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum number of retries on HTTP 429 (Too Many Requests).
const MAX_RETRIES: u32 = 3;

/// Default retry delay when no `Retry-After` / `X-RateLimit-Reset` header
/// is present. Used as the base for exponential backoff.
const DEFAULT_RETRY_DELAY_SECS: u64 = 2;

/// HTTP client for Datadog REST APIs.
pub struct DatadogClient {
    client: Client,
    base_url: String,
    api_key: String,
    app_key: String,
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
            api_key: api_key.to_string(),
            app_key: app_key.to_string(),
        })
    }

    /// Creates a client from stored credentials.
    pub fn from_credentials(creds: &DatadogCredentials) -> Result<Self> {
        let base_url = base_url_for_site(&creds.site);
        Self::new(&base_url, &creds.api_key, &creds.app_key)
    }

    /// Returns the API base URL (without trailing slash).
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Sends an authenticated GET request and returns the raw response.
    pub async fn get_json(&self, url: &str) -> Result<reqwest::Response> {
        for attempt in 0..=MAX_RETRIES {
            let response = self
                .client
                .get(url)
                .header("DD-API-KEY", &self.api_key)
                .header("DD-APPLICATION-KEY", &self.app_key)
                .header("Accept", "application/json")
                .send()
                .await
                .context("Failed to send GET request to Datadog API")?;

            if response.status().as_u16() != 429 || attempt == MAX_RETRIES {
                return Ok(response);
            }
            Self::wait_for_retry(&response, attempt).await;
        }
        unreachable!()
    }

    /// Sends an authenticated POST request with a JSON body and returns the raw response.
    pub async fn post_json<T: serde::Serialize + Sync + ?Sized>(
        &self,
        url: &str,
        body: &T,
    ) -> Result<reqwest::Response> {
        for attempt in 0..=MAX_RETRIES {
            let response = self
                .client
                .post(url)
                .header("DD-API-KEY", &self.api_key)
                .header("DD-APPLICATION-KEY", &self.app_key)
                .header("Content-Type", "application/json")
                .header("Accept", "application/json")
                .json(body)
                .send()
                .await
                .context("Failed to send POST request to Datadog API")?;

            if response.status().as_u16() != 429 || attempt == MAX_RETRIES {
                return Ok(response);
            }
            Self::wait_for_retry(&response, attempt).await;
        }
        unreachable!()
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

    /// Waits before retrying a rate-limited request.
    ///
    /// Consults, in order: `Retry-After`, then Datadog's `X-RateLimit-Reset`,
    /// then exponential backoff (`DEFAULT_RETRY_DELAY_SECS ^ (attempt+1)`).
    async fn wait_for_retry(response: &reqwest::Response, attempt: u32) {
        let headers = response.headers();
        let delay = header_u64(headers, "Retry-After")
            .or_else(|| header_u64(headers, "X-RateLimit-Reset"))
            .unwrap_or_else(|| DEFAULT_RETRY_DELAY_SECS.pow(attempt + 1));

        eprintln!(
            "Rate limited (429). Retrying in {delay}s (attempt {})...",
            attempt + 1
        );
        tokio::time::sleep(Duration::from_secs(delay)).await;
    }
}

fn header_u64(headers: &reqwest::header::HeaderMap, name: &str) -> Option<u64> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
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
    fn from_credentials_builds_base_url_from_site() {
        let creds = DatadogCredentials {
            api_key: "api".to_string(),
            app_key: "app".to_string(),
            site: "us5.datadoghq.com".to_string(),
        };
        let client = DatadogClient::from_credentials(&creds).unwrap();
        assert_eq!(client.base_url(), "https://api.us5.datadoghq.com");
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
