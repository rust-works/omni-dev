//! Claude API client implementation.

use std::future::Future;
use std::pin::Pin;

use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use serde_json::Value;

use super::{
    AiClient, AiClientCapabilities, AiClientMetadata, AiResponse, InvocationMetrics, RequestOptions,
};
use crate::claude::error::ClaudeError;
use crate::claude::model_config::get_model_registry;

/// Claude API request message.
#[derive(Serialize)]
struct Message {
    role: String,
    content: String,
}

/// Structured-output request envelope for the Anthropic Messages API.
///
/// Serialises to `{"output_config": {"format": {"type": "json_schema",
/// "schema": {...}}}}`. This is the GA structured-outputs surface (no beta
/// header required), so the API re-prompts the model until it emits a JSON
/// object validating against `schema`. Only attached for models the registry
/// flags via [`ModelRegistry::supports_structured_output`](crate::claude::model_config::ModelRegistry::supports_structured_output);
/// unsupported models `400` on the field, so they keep the YAML path.
#[derive(Serialize)]
struct OutputConfig {
    format: OutputFormat,
}

/// Inner `format` block of an [`OutputConfig`]. `kind` is always the literal
/// `"json_schema"`; `schema` is the JSON Schema the response must satisfy.
#[derive(Serialize)]
struct OutputFormat {
    #[serde(rename = "type")]
    kind: &'static str,
    schema: Value,
}

/// Claude API request body.
#[derive(Serialize)]
struct ClaudeRequest {
    model: String,
    max_tokens: i32,
    system: String,
    messages: Vec<Message>,
    /// Optional structured-output constraint. Omitted from the wire body when
    /// `None` (`skip_serializing_if`) so the default request stays
    /// byte-identical to the pre-#1119 shape.
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<OutputConfig>,
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
    /// Shared by [`AiClient::send_request`] (which discards the usage),
    /// [`AiClient::send_request_with_options`], and
    /// [`AiClient::send_request_with_metrics`] (which turns it into a cost).
    /// When `schema` is `Some`, it is attached as `output_config.format` so
    /// the API constrains the response to a JSON object matching the schema;
    /// the returned text is that JSON object (which downstream parses as YAML,
    /// a JSON superset).
    async fn send_and_parse(
        &self,
        system_prompt: &str,
        user_prompt: &str,
        schema: Option<&Value>,
    ) -> Result<(String, Option<Usage>)> {
        debug!(
            system_prompt_len = system_prompt.len(),
            user_prompt_len = user_prompt.len(),
            model = %self.model,
            has_schema = schema.is_some(),
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
            output_config: schema.map(|schema| OutputConfig {
                format: OutputFormat {
                    kind: "json_schema",
                    schema: schema.clone(),
                },
            }),
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
            let (text, _usage) = self
                .send_and_parse(system_prompt, user_prompt, None)
                .await?;
            Ok(text)
        })
    }

    fn capabilities(&self) -> AiClientCapabilities {
        // The Anthropic Messages API exposes GA structured output via
        // `output_config.format`, but only on recent models — the registry
        // gate keeps older models (which `400` on the field) on the YAML path.
        AiClientCapabilities {
            supports_response_schema: get_model_registry().supports_structured_output(&self.model),
        }
    }

    fn send_request_with_options<'a>(
        &'a self,
        system_prompt: &'a str,
        user_prompt: &'a str,
        options: RequestOptions,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
        Box::pin(async move {
            let (text, _usage) = self
                .send_and_parse(system_prompt, user_prompt, options.response_schema.as_ref())
                .await?;
            Ok(text)
        })
    }

    fn send_request_with_metrics<'a>(
        &'a self,
        system_prompt: &'a str,
        user_prompt: &'a str,
        options: RequestOptions,
    ) -> Pin<Box<dyn Future<Output = Result<AiResponse>> + Send + 'a>> {
        // Honours the request schema (when the caller attached one and the
        // model supports it) and derives cost from the response's token usage
        // and the model's registry prices.
        Box::pin(async move {
            let (text, usage) = self
                .send_and_parse(system_prompt, user_prompt, options.response_schema.as_ref())
                .await?;
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

    /// Structured-output capability is gated on the model registry (#1119):
    /// a flagged model advertises schema support; an older model that would
    /// `400` on `output_config` does not.
    #[test]
    fn capabilities_gate_on_model_support() {
        let supported =
            ClaudeAiClient::new("claude-sonnet-4-6".to_string(), "key".to_string(), None).unwrap();
        assert!(supported.capabilities().supports_response_schema);

        let unsupported = ClaudeAiClient::new(
            "claude-3-opus-20240229".to_string(),
            "key".to_string(),
            None,
        )
        .unwrap();
        assert!(!unsupported.capabilities().supports_response_schema);
    }

    /// A schema-bearing request serialises `output_config.format` on the wire
    /// and the returned JSON object is surfaced verbatim as the response text.
    #[tokio::test]
    async fn send_request_with_options_serializes_output_config() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(
                    r#"{"content":[{"type":"text","text":"{\"answer\":\"ok\"}"}]}"#,
                ),
            )
            .expect(1)
            .mount(&server)
            .await;

        let mut client =
            ClaudeAiClient::new("claude-sonnet-4-6".to_string(), "key".to_string(), None).unwrap();
        client.api_url = format!("{}/v1/messages", server.uri());

        let schema = serde_json::json!({
            "type": "object",
            "properties": { "answer": { "type": "string" } },
            "required": ["answer"],
            "additionalProperties": false,
        });
        let options = RequestOptions::default().with_response_schema(schema.clone());
        let out = client
            .send_request_with_options("system", "user", options)
            .await
            .unwrap();
        assert_eq!(out, r#"{"answer":"ok"}"#);

        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
        assert_eq!(body["output_config"]["format"]["schema"], schema);
    }

    /// The no-schema path must not emit `output_config` on the wire — the
    /// default request body stays byte-identical to the pre-#1119 shape.
    #[tokio::test]
    async fn send_request_omits_output_config_without_schema() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"content":[{"type":"text","text":"hi"}]}"#),
            )
            .expect(1)
            .mount(&server)
            .await;

        let mut client =
            ClaudeAiClient::new("claude-sonnet-4-6".to_string(), "key".to_string(), None).unwrap();
        client.api_url = format!("{}/v1/messages", server.uri());

        let _ = client.send_request("system", "user").await.unwrap();

        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
        assert!(
            body.get("output_config").is_none(),
            "expected output_config to be omitted, got: {body}"
        );
    }
}
