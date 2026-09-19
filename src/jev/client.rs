//! Jev "System One" REST API client.
//!
//! Thin `reqwest` wrapper around the single `POST /v1/systemone` endpoint.
//! Retries `429`/`529` responses via the shared
//! [`retry_if`](crate::utils::http::retry_if) driver (honouring
//! `Retry-After` / `X-RateLimit-Reset`) — `529` is Jev's own "Overloaded"
//! status, so the literal-429 [`retry_429`](crate::utils::http::retry_429)
//! driver would not retry it.

use anyhow::{Context, Result};
use reqwest::Client;

use crate::jev::config::JevConfig;
use crate::jev::error::JevError;
use crate::jev::protocol::SystemOneRequest;
use crate::jev::protocol::SystemOneResponse;
use crate::request_log;
use crate::utils::http::{connect_timeout, read_timeout, retry_if};
use crate::utils::secret::Secret;

/// HTTP client for the Jev System One REST API.
#[derive(Debug)]
pub struct JevClient {
    client: Client,
    base_url: String,
    api_key: Secret,
}

impl JevClient {
    /// Creates a new Jev API client.
    ///
    /// `base_url` should be the full API host, e.g. `https://api.typesafe.ai`.
    /// For production use, construct via [`Self::from_config`]; tests pass a
    /// wiremock URL directly.
    ///
    /// Uses the shared REST-client connect/read timeout split
    /// ([`connect_timeout`], [`read_timeout`]) rather than the AI backends'
    /// much longer `OMNI_DEV_AI_TIMEOUT_SECS` budget — Jev is a low-latency
    /// model, not a chat completion.
    pub fn new(base_url: &str, api_key: &str) -> Result<Self> {
        let client = Client::builder()
            .connect_timeout(connect_timeout())
            .read_timeout(read_timeout())
            .build()
            .context("Failed to build HTTP client")?;

        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.into(),
        })
    }

    /// Creates a client from resolved [`JevConfig`].
    pub fn from_config(config: &JevConfig) -> Result<Self> {
        Self::new(&config.base_url, config.api_key.expose_secret())
    }

    /// Returns the API base URL (without trailing slash).
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Sends a `POST /v1/systemone` request and returns the parsed response.
    ///
    /// Retries on `429` (rate limited) and `529` (overloaded); any other
    /// non-2xx status becomes [`JevError::ApiRequestFailed`]. A `2xx` body
    /// that fails to parse (most likely an unrecognised `answers[*].type`
    /// from a newer Jev API version) surfaces a context message suggesting
    /// an omni-dev upgrade.
    pub async fn system_one(&self, req: &SystemOneRequest) -> Result<SystemOneResponse> {
        let url = format!("{}/v1/systemone", self.base_url);
        let response = retry_if(
            || {
                self.client
                    .post(&url)
                    .header(
                        "Authorization",
                        format!("Bearer {}", self.api_key.expose_secret()),
                    )
                    .header("Content-Type", "application/json")
                    .json(req)
            },
            |started, result| {
                request_log::record_http_result("jev", "POST", &url, started, result);
            },
            |status, _body| status == 429 || status == 529,
            None,
        )
        .await
        .context("Failed to send request to Jev API")?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(JevError::ApiRequestFailed { status, body }.into());
        }

        response.json().await.context(
            "Failed to parse Jev API response; this may mean the API returned an answer type \
             this version of omni-dev does not recognise yet — try upgrading omni-dev",
        )
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::jev::protocol::Question;
    use std::collections::BTreeMap;

    fn sample_request() -> SystemOneRequest {
        SystemOneRequest {
            state: serde_json::json!("payouts failing 3 days"),
            model: "jev-latest".to_string(),
            questions: BTreeMap::from([(
                "refund".to_string(),
                Question::Noul {
                    instructions: "Urgent?".to_string(),
                    criteria: None,
                },
            )]),
        }
    }

    #[test]
    fn new_client_strips_trailing_slash() {
        let client = JevClient::new("https://api.typesafe.ai/", "key").unwrap();
        assert_eq!(client.base_url(), "https://api.typesafe.ai");
    }

    #[test]
    fn client_debug_redacts_api_key() {
        let client = JevClient::new("https://api.typesafe.ai", "sekret-key").unwrap();
        let debug = format!("{client:?}");
        assert!(!debug.contains("sekret-key"));
        assert!(debug.contains("api_key: <redacted>"));
    }

    #[test]
    fn from_config_uses_resolved_values() {
        let config = JevConfig {
            api_key: "sekret-key".into(),
            base_url: "https://api.typesafe.ai".to_string(),
            model: "jev-latest".to_string(),
        };
        let client = JevClient::from_config(&config).unwrap();
        assert_eq!(client.base_url(), "https://api.typesafe.ai");
    }

    #[tokio::test]
    async fn system_one_sends_bearer_header_and_body() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/systemone"))
            .and(wiremock::matchers::header("Authorization", "Bearer my-key"))
            .and(wiremock::matchers::header(
                "Content-Type",
                "application/json",
            ))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "state": "payouts failing 3 days",
                "model": "jev-latest",
                "questions": {
                    "refund": {"type": "noul", "instructions": "Urgent?"}
                }
            })))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "jev-1.13.0",
                    "answers": {"refund": {"type": "noul", "noul": 0.5}},
                    "usage": {"input_tokens": 1, "output_tokens": 1}
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = JevClient::new(&server.uri(), "my-key").unwrap();
        let response = client.system_one(&sample_request()).await.unwrap();
        assert_eq!(response.model, "jev-1.13.0");
    }

    #[tokio::test]
    async fn system_one_200_parses() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/systemone"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "jev-1.13.0",
                    "answers": {},
                    "usage": {"input_tokens": 0, "output_tokens": 0}
                })),
            )
            .mount(&server)
            .await;

        let client = JevClient::new(&server.uri(), "my-key").unwrap();
        let response = client.system_one(&sample_request()).await.unwrap();
        assert_eq!(response.model, "jev-1.13.0");
        assert!(response.answers.is_empty());
    }

    #[tokio::test]
    async fn system_one_401_maps_to_api_request_failed() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/systemone"))
            .respond_with(wiremock::ResponseTemplate::new(401).set_body_string("Unauthorized"))
            .expect(1)
            .mount(&server)
            .await;

        let client = JevClient::new(&server.uri(), "my-key").unwrap();
        let err = client.system_one(&sample_request()).await.unwrap_err();
        match err.downcast_ref::<JevError>() {
            Some(JevError::ApiRequestFailed { status, body }) => {
                assert_eq!(*status, 401);
                assert_eq!(body, "Unauthorized");
            }
            other => panic!("expected ApiRequestFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn system_one_422_maps_to_api_request_failed_and_is_not_retried() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/systemone"))
            .respond_with(wiremock::ResponseTemplate::new(422).set_body_string("invalid criteria"))
            .expect(1)
            .mount(&server)
            .await;

        let client = JevClient::new(&server.uri(), "my-key").unwrap();
        let err = client.system_one(&sample_request()).await.unwrap_err();
        match err.downcast_ref::<JevError>() {
            Some(JevError::ApiRequestFailed { status, body }) => {
                assert_eq!(*status, 422);
                assert_eq!(body, "invalid criteria");
            }
            other => panic!("expected ApiRequestFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn system_one_retries_529_then_succeeds() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/systemone"))
            .respond_with(wiremock::ResponseTemplate::new(529).append_header("Retry-After", "0"))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/systemone"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "jev-1.13.0",
                    "answers": {},
                    "usage": {"input_tokens": 0, "output_tokens": 0}
                })),
            )
            .with_priority(2)
            .mount(&server)
            .await;

        let client = JevClient::new(&server.uri(), "my-key").unwrap();
        let response = client.system_one(&sample_request()).await.unwrap();
        assert_eq!(response.model, "jev-1.13.0");
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn system_one_retries_429_then_succeeds() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/systemone"))
            .respond_with(wiremock::ResponseTemplate::new(429).append_header("Retry-After", "0"))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/systemone"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "jev-1.13.0",
                    "answers": {},
                    "usage": {"input_tokens": 0, "output_tokens": 0}
                })),
            )
            .with_priority(2)
            .mount(&server)
            .await;

        let client = JevClient::new(&server.uri(), "my-key").unwrap();
        let response = client.system_one(&sample_request()).await.unwrap();
        assert_eq!(response.model, "jev-1.13.0");
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }
}
