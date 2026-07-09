//! Claude API client implementation.

use std::future::Future;
use std::pin::Pin;

use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use super::{AiClient, AiClientMetadata, AiResponse, InvocationMetrics, RequestOptions};
use crate::claude::error::ClaudeError;

/// Claude API request message.
#[derive(Serialize)]
struct Message {
    role: String,
    content: String,
}

/// Claude API request body.
#[derive(Serialize)]
struct ClaudeRequest {
    model: String,
    max_tokens: i32,
    system: String,
    messages: Vec<Message>,
}

/// Claude API response content.
#[derive(Deserialize)]
struct Content {
    #[serde(rename = "type")]
    content_type: String,
    text: String,
}

/// Token usage reported by the Claude API for one invocation.
///
/// Only the fields needed for cost computation are captured; the API may
/// include others (cache tokens, …) which serde ignores.
#[derive(Deserialize)]
struct Usage {
    input_tokens: u64,
    output_tokens: u64,
}

/// Claude API response.
#[derive(Deserialize)]
struct ClaudeResponse {
    content: Vec<Content>,
    /// Token usage for cost computation. Optional so responses without a
    /// `usage` object (e.g. test fixtures, older API shapes) still parse;
    /// cost is reported as unknown when absent.
    #[serde(default)]
    usage: Option<Usage>,
}

/// Anthropic Messages API endpoint. Held as a client field (defaulted here) so
/// tests can point the send path at a mock server.
const API_URL: &str = "https://api.anthropic.com/v1/messages";

/// Claude API client implementation.
pub struct ClaudeAiClient {
    /// HTTP client for API requests.
    client: Client,
    /// API key for authentication.
    api_key: String,
    /// Model identifier.
    model: String,
    /// Active beta header (key, value) if enabled.
    active_beta: Option<(String, String)>,
    /// Messages API endpoint (defaults to [`API_URL`]; overridable in tests).
    api_url: String,
}

impl ClaudeAiClient {
    /// Creates a new Claude AI client.
    pub fn new(
        model: String,
        api_key: String,
        active_beta: Option<(String, String)>,
    ) -> Result<Self> {
        let client = super::build_http_client()?;

        Ok(Self {
            client,
            api_key,
            model,
            active_beta,
            api_url: API_URL.to_string(),
        })
    }

    /// Returns the max tokens from the model registry.
    fn get_max_tokens(&self) -> i32 {
        super::registry_max_output_tokens(&self.model, &self.active_beta)
    }

    /// Sends the request and returns the response text plus reported token
    /// usage (`None` when the response carried no `usage` object).
    ///
    /// Shared by [`AiClient::send_request`] (which discards the usage) and
    /// [`AiClient::send_request_with_metrics`] (which turns it into a cost).
    async fn send_and_parse(
        &self,
        system_prompt: &str,
        user_prompt: &str,
    ) -> Result<(String, Option<Usage>)> {
        debug!(
            system_prompt_len = system_prompt.len(),
            user_prompt_len = user_prompt.len(),
            model = %self.model,
            "Preparing Claude API request"
        );

        debug!(
            system_prompt = %system_prompt,
            user_prompt = %user_prompt,
            "Claude API request content"
        );

        // Build request to Claude API
        let request = ClaudeRequest {
            model: self.model.clone(),
            max_tokens: self.get_max_tokens(),
            system: system_prompt.to_string(),
            messages: vec![Message {
                role: "user".to_string(),
                content: user_prompt.to_string(),
            }],
        };

        info!(
            url = %self.api_url,
            model = %self.model,
            max_tokens = self.get_max_tokens(),
            "Sending request to Claude API"
        );

        // Send request to Claude API
        let mut builder = self
            .client
            .post(&self.api_url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json");

        if let Some((ref key, ref value)) = self.active_beta {
            debug!(header_key = %key, header_value = %value, "Adding beta header to Claude API request");
            builder = builder.header(key, value);
        }

        let started = std::time::Instant::now();
        let send_result = builder.json(&request).send().await;
        super::record_ai_http("anthropic", "POST", &self.api_url, started, &send_result);
        let response = send_result.map_err(|e| ClaudeError::NetworkError(e.to_string()))?;

        let response = super::check_error_response(response).await?;

        let claude_response: ClaudeResponse = response
            .json()
            .await
            .map_err(|e| ClaudeError::InvalidResponseFormat(e.to_string()))?;

        debug!(
            content_count = claude_response.content.len(),
            "Received Claude API response"
        );

        // Extract text content from response
        let result: Result<String> = claude_response
            .content
            .first()
            .filter(|c| c.content_type == "text")
            .map(|c| c.text.clone())
            .ok_or_else(|| {
                ClaudeError::InvalidResponseFormat("No text content in response".to_string()).into()
            });

        super::log_response_success("Claude", &result);

        Ok((result?, claude_response.usage))
    }
}

impl AiClient for ClaudeAiClient {
    fn send_request<'a>(
        &'a self,
        system_prompt: &'a str,
        user_prompt: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
        // Use Box::pin to wrap the async block in a Pin<Box<...>>
        Box::pin(async move {
            let (text, _usage) = self.send_and_parse(system_prompt, user_prompt).await?;
            Ok(text)
        })
    }

    fn send_request_with_metrics<'a>(
        &'a self,
        system_prompt: &'a str,
        user_prompt: &'a str,
        _options: RequestOptions,
    ) -> Pin<Box<dyn Future<Output = Result<AiResponse>> + Send + 'a>> {
        // The direct Anthropic backend ignores request options (it advertises
        // no schema support), so `_options` is dropped just as `send_request`
        // drops it. Cost is derived from the response's token usage and the
        // model's registry prices.
        Box::pin(async move {
            let (text, usage) = self.send_and_parse(system_prompt, user_prompt).await?;
            let cost_usd = usage.and_then(|u| {
                super::compute_cost_usd(&self.model, u.input_tokens, u.output_tokens)
            });
            Ok(AiResponse {
                text,
                metrics: InvocationMetrics { cost_usd },
            })
        })
    }

    fn get_metadata(&self) -> AiClientMetadata {
        let (max_context_length, max_response_length) =
            super::registry_model_limits(&self.model, &self.active_beta);

        AiClientMetadata {
            provider: "Anthropic".to_string(),
            model: self.model.clone(),
            max_context_length,
            max_response_length,
            active_beta: self.active_beta.clone(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn claude_client_new() {
        let client = ClaudeAiClient::new(
            "claude-sonnet-4-20250514".to_string(),
            "sk-ant-test".to_string(),
            None,
        )
        .unwrap();
        assert_eq!(client.model, "claude-sonnet-4-20250514");
        assert_eq!(client.api_key, "sk-ant-test");
        assert!(client.active_beta.is_none());
    }

    #[tokio::test]
    async fn send_request_posts_to_configured_url_and_returns_text() {
        // Drives the whole send path (including the best-effort HTTP record)
        // against a mock server by overriding the endpoint field.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"content":[{"type":"text","text":"hi there"}]}"#),
            )
            .expect(1)
            .mount(&server)
            .await;

        let mut client = ClaudeAiClient::new(
            "claude-sonnet-4-20250514".to_string(),
            "sk-ant-test".to_string(),
            None,
        )
        .unwrap();
        client.api_url = format!("{}/v1/messages", server.uri());

        let out = client.send_request("system", "user").await.unwrap();
        assert_eq!(out, "hi there");
    }

    #[tokio::test]
    async fn send_request_with_metrics_computes_cost_from_usage() {
        // A response carrying `usage` yields a cost computed from the model's
        // registry price (claude-sonnet-4-20250514 = $3 / $15 per MTok).
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"content":[{"type":"text","text":"hi there"}],"usage":{"input_tokens":200000,"output_tokens":100000}}"#,
            ))
            .expect(1)
            .mount(&server)
            .await;

        let mut client = ClaudeAiClient::new(
            "claude-sonnet-4-20250514".to_string(),
            "sk-ant-test".to_string(),
            None,
        )
        .unwrap();
        client.api_url = format!("{}/v1/messages", server.uri());

        let response = client
            .send_request_with_metrics("system", "user", RequestOptions::default())
            .await
            .unwrap();
        assert_eq!(response.text, "hi there");
        // 200k input * $3/MTok + 100k output * $15/MTok = 0.6 + 1.5 = 2.1.
        let cost = response.metrics.cost_usd.expect("cost should be present");
        assert!((cost - 2.1).abs() < 1e-9, "expected ~2.1, got {cost}");
    }

    #[tokio::test]
    async fn send_request_with_metrics_reports_none_cost_without_usage() {
        // A response without a `usage` object still parses; cost is unknown.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"content":[{"type":"text","text":"hi there"}]}"#),
            )
            .expect(1)
            .mount(&server)
            .await;

        let mut client = ClaudeAiClient::new(
            "claude-sonnet-4-20250514".to_string(),
            "sk-ant-test".to_string(),
            None,
        )
        .unwrap();
        client.api_url = format!("{}/v1/messages", server.uri());

        let response = client
            .send_request_with_metrics("system", "user", RequestOptions::default())
            .await
            .unwrap();
        assert_eq!(response.text, "hi there");
        assert_eq!(response.metrics.cost_usd, None);
    }

    #[tokio::test]
    async fn send_request_sends_configured_beta_header() {
        // When an active beta is configured, its header is attached to the
        // outbound request (exercises the beta-header branch of the send path).
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("anthropic-beta", "context-1m-2025-08-07"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"content":[{"type":"text","text":"hi"}]}"#),
            )
            .expect(1)
            .mount(&server)
            .await;

        let beta = Some((
            "anthropic-beta".to_string(),
            "context-1m-2025-08-07".to_string(),
        ));
        let mut client =
            ClaudeAiClient::new("claude-sonnet-4-6".to_string(), "key".to_string(), beta).unwrap();
        client.api_url = format!("{}/v1/messages", server.uri());

        let out = client.send_request("system", "user").await.unwrap();
        assert_eq!(out, "hi");
    }

    #[tokio::test]
    async fn send_request_errors_when_no_text_content() {
        // A response with no text content block yields a typed error rather
        // than an empty string.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"content":[]}"#))
            .expect(1)
            .mount(&server)
            .await;

        let mut client =
            ClaudeAiClient::new("claude-sonnet-4-6".to_string(), "key".to_string(), None).unwrap();
        client.api_url = format!("{}/v1/messages", server.uri());

        let err = client.send_request("system", "user").await.unwrap_err();
        assert!(
            format!("{err:#}").contains("No text content"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn claude_client_with_beta() {
        let beta = Some((
            "anthropic-beta".to_string(),
            "output-128k-2025-02-19".to_string(),
        ));
        let client = ClaudeAiClient::new(
            "claude-sonnet-4-20250514".to_string(),
            "key".to_string(),
            beta,
        )
        .unwrap();
        assert!(client.active_beta.is_some());
        let (key, value) = client.active_beta.unwrap();
        assert_eq!(key, "anthropic-beta");
        assert_eq!(value, "output-128k-2025-02-19");
    }

    #[test]
    fn get_max_tokens_known_model() {
        let client = ClaudeAiClient::new(
            "claude-sonnet-4-20250514".to_string(),
            "key".to_string(),
            None,
        )
        .unwrap();
        let tokens = client.get_max_tokens();
        assert!(tokens > 0, "expected positive token limit, got {tokens}");
    }

    #[test]
    fn get_max_tokens_legacy_model() {
        let client = ClaudeAiClient::new(
            "claude-3-opus-20240229".to_string(),
            "key".to_string(),
            None,
        )
        .unwrap();
        assert_eq!(client.get_max_tokens(), 4096);
    }

    #[test]
    fn get_metadata_without_beta() {
        let client = ClaudeAiClient::new(
            "claude-sonnet-4-20250514".to_string(),
            "key".to_string(),
            None,
        )
        .unwrap();
        let metadata = client.get_metadata();
        assert_eq!(metadata.provider, "Anthropic");
        assert_eq!(metadata.model, "claude-sonnet-4-20250514");
        assert!(metadata.active_beta.is_none());
        assert!(metadata.max_context_length > 0);
        assert!(metadata.max_response_length > 0);
    }

    #[test]
    fn get_metadata_with_beta() {
        let beta = Some((
            "anthropic-beta".to_string(),
            "output-128k-2025-02-19".to_string(),
        ));
        let client = ClaudeAiClient::new(
            "claude-sonnet-4-20250514".to_string(),
            "key".to_string(),
            beta,
        )
        .unwrap();
        let metadata = client.get_metadata();
        assert!(metadata.active_beta.is_some());
    }

    /// Anthropic backend doesn't expose JSON Schema enforcement (only
    /// claude-cli does today), so capabilities must report `false`.
    #[test]
    fn capabilities_default_to_no_schema_support() {
        let client =
            ClaudeAiClient::new("claude-sonnet-4-6".to_string(), "key".to_string(), None).unwrap();
        assert!(!client.capabilities().supports_response_schema);
    }
}
