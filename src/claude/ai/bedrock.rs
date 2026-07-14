//! Bedrock API client implementation for Claude.

use std::future::Future;
use std::pin::Pin;

use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, info};
use url::Url;

use super::{AiClient, AiClientCapabilities, AiClientMetadata, RequestOptions};
use crate::claude::error::ClaudeError;
use crate::claude::model_config::get_model_registry;

/// Bedrock API request message.
#[derive(Serialize)]
struct Message {
    role: String,
    content: String,
}

/// Structured-output request envelope for the Bedrock Messages API.
///
/// Mirrors the direct-API shape (`{"output_config": {"format": {"type":
/// "json_schema", "schema": {...}}}}`); Bedrock exposes GA structured output
/// on the same recent models as the direct API. Only attached for models the
/// registry flags via
/// [`ModelRegistry::supports_structured_output`](crate::claude::model_config::ModelRegistry::supports_structured_output).
#[derive(Serialize)]
struct OutputConfig {
    format: OutputFormat,
}

/// Inner `format` block of an [`OutputConfig`]. `kind` is always
/// `"json_schema"`; `schema` is the JSON Schema the response must satisfy.
#[derive(Serialize)]
struct OutputFormat {
    #[serde(rename = "type")]
    kind: &'static str,
    schema: Value,
}

/// Bedrock API request body.
#[derive(Serialize)]
struct BedrockRequest {
    anthropic_version: String,
    max_tokens: i32,
    system: Option<String>,
    messages: Vec<Message>,
    /// Optional structured-output constraint. Omitted from the wire body when
    /// `None` so the default request stays byte-identical to the pre-#1119
    /// shape.
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<OutputConfig>,
}

/// Bedrock API content.
#[derive(Deserialize, Debug)]
struct Content {
    #[serde(rename = "type")]
    content_type: String,
    text: String,
}

/// Bedrock API response.
#[derive(Deserialize, Debug)]
#[allow(dead_code)] // Fields populated by serde deserialization, not accessed directly
struct BedrockResponse {
    id: String,
    #[serde(rename = "type")]
    response_type: String,
    role: String,
    model: String,
    content: Vec<Content>,
    stop_reason: Option<String>,
    stop_sequence: Option<String>,
    usage: Option<Usage>,
}

/// Bedrock API usage statistics.
#[derive(Deserialize, Debug)]
#[allow(dead_code)] // Fields populated by serde deserialization, not accessed directly
struct Usage {
    input_tokens: i32,
    output_tokens: i32,
    #[serde(default)]
    cache_creation_input_tokens: i32,
    #[serde(default)]
    cache_read_input_tokens: i32,
}

/// Bedrock API client implementation for Claude.
pub struct BedrockAiClient {
    /// HTTP client for API requests.
    client: Client,
    /// Authorization token.
    auth_token: String,
    /// Model identifier.
    model: String,
    /// Base URL for the Bedrock API.
    base_url: String,
    /// Active beta header (key, value) if enabled.
    active_beta: Option<(String, String)>,
}

impl BedrockAiClient {
    /// Creates a new Bedrock AI client.
    pub fn new(
        model: String,
        auth_token: String,
        base_url: String,
        active_beta: Option<(String, String)>,
    ) -> Result<Self> {
        let client = super::build_http_client()?;

        Ok(Self {
            client,
            auth_token,
            model,
            base_url,
            active_beta,
        })
    }

    /// Returns the max tokens from the model registry.
    fn get_max_tokens(&self) -> i32 {
        super::registry_max_output_tokens(&self.model, &self.active_beta)
    }

    /// Builds the full API URL.
    fn get_api_url(&self) -> Result<String> {
        let mut url = Url::parse(&self.base_url)
            .map_err(|e| ClaudeError::NetworkError(format!("Invalid base URL: {e}")))?;

        // Ensure the base URL ends with a trailing slash to preserve all path components
        if !url.as_str().ends_with('/') {
            url.set_path(&format!("{}/", url.path()));
        }

        // Create the base URL with path to the model endpoint
        let model_url = url
            .join("model/")
            .map_err(|e| ClaudeError::NetworkError(format!("Failed to build API URL: {e}")))?;

        // Now properly URL-encode the model ID and add it to the path
        let encoded_model =
            url::form_urlencoded::byte_serialize(self.model.as_bytes()).collect::<String>();
        debug!(original_model = %self.model, encoded_model = %encoded_model, "URL-encoded model ID");

        // Join the encoded model and invoke endpoint
        let full_url = model_url
            .join(&format!("{encoded_model}/invoke"))
            .map_err(|e| {
                ClaudeError::NetworkError(format!(
                    "Failed to build API URL with encoded model: {e}"
                ))
            })?;

        debug!(base_url = %self.base_url, full_url = %full_url, "Constructed Bedrock API URL");

        Ok(full_url.to_string())
    }

    /// Sends a request to the Bedrock invoke endpoint and extracts the text
    /// content. When `schema` is `Some`, it is attached as
    /// `output_config.format`, so the model is constrained to a JSON object
    /// matching the schema (the returned text is that JSON object, which
    /// downstream parses as YAML). Shared by
    /// [`send_request`](AiClient::send_request) and
    /// [`send_request_with_options`](AiClient::send_request_with_options).
    async fn send_with_schema(
        &self,
        system_prompt: &str,
        user_prompt: &str,
        schema: Option<&Value>,
    ) -> Result<String> {
        debug!(
            system_prompt_len = system_prompt.len(),
            user_prompt_len = user_prompt.len(),
            model = %self.model,
            has_schema = schema.is_some(),
            "Preparing Bedrock API request"
        );

        debug!(
            system_prompt = %system_prompt,
            user_prompt = %user_prompt,
            "Bedrock API request content"
        );

        // For Bedrock API, the system prompt is a separate parameter
        let messages = vec![Message {
            role: "user".to_string(),
            content: user_prompt.to_string(),
        }];

        // Build request to Bedrock API
        let request = BedrockRequest {
            anthropic_version: "bedrock-2023-05-31".to_string(),
            max_tokens: self.get_max_tokens(),
            // Add system prompt as a separate field if it exists
            system: if !system_prompt.is_empty() {
                Some(system_prompt.to_string())
            } else {
                None
            },
            messages,
            output_config: schema.map(|schema| OutputConfig {
                format: OutputFormat {
                    kind: "json_schema",
                    schema: schema.clone(),
                },
            }),
        };

        debug!(
            system_prompt_len = system_prompt.len(),
            max_tokens = self.get_max_tokens(),
            "Built Bedrock request payload"
        );

        // Get the API URL
        let api_url = self.get_api_url()?;

        // Log the URL for debugging purposes
        info!(url = %api_url, "Sending request to Bedrock API");
        debug!(model = %self.model, "Using model for Bedrock request");

        // Send request to Bedrock API
        let mut builder = self
            .client
            .post(&api_url)
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {}", self.auth_token));

        if let Some((ref key, ref value)) = self.active_beta {
            debug!(header_key = %key, header_value = %value, "Adding beta header to Bedrock API request");
            builder = builder.header(key, value);
        }

        let started = std::time::Instant::now();
        let send_result = builder.json(&request).send().await;
        super::record_ai_http("bedrock", "POST", &api_url, started, &send_result);
        let response = send_result.map_err(|e| ClaudeError::NetworkError(e.to_string()))?;

        let response = super::check_error_response(response).await?;

        let bedrock_response: BedrockResponse = response
            .json()
            .await
            .map_err(|e| ClaudeError::InvalidResponseFormat(e.to_string()))?;

        debug!(
            response_id = %bedrock_response.id,
            response_type = %bedrock_response.response_type,
            content_count = bedrock_response.content.len(),
            stop_reason = ?bedrock_response.stop_reason,
            usage = ?bedrock_response.usage,
            "Received Bedrock API response"
        );

        // Extract text content from response
        let result = bedrock_response
            .content
            .first()
            .filter(|c| c.content_type == "text")
            .map(|c| c.text.clone())
            .ok_or_else(|| {
                ClaudeError::InvalidResponseFormat("No text content in response".to_string()).into()
            });

        super::log_response_success("Bedrock", &result);

        result
    }
}

impl AiClient for BedrockAiClient {
    fn send_request<'a>(
        &'a self,
        system_prompt: &'a str,
        user_prompt: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
        Box::pin(async move {
            self.send_with_schema(system_prompt, user_prompt, None)
                .await
        })
    }

    fn capabilities(&self) -> AiClientCapabilities {
        // Bedrock exposes GA structured output on the same recent models as
        // the direct API; the registry gate keeps older models on the YAML
        // path (they `400` on `output_config`).
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
            self.send_with_schema(system_prompt, user_prompt, options.response_schema.as_ref())
                .await
        })
    }

    fn get_metadata(&self) -> AiClientMetadata {
        let (max_context_length, max_response_length) =
            super::registry_model_limits(&self.model, &self.active_beta);

        AiClientMetadata {
            provider: "Anthropic Bedrock".to_string(),
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

    #[tokio::test]
    async fn send_request_posts_to_base_url_and_returns_text() {
        // Drives the whole send path (including the best-effort HTTP record)
        // against a mock server via the injectable base_url.
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"id":"msg_1","type":"message","role":"assistant","model":"m",
                    "content":[{"type":"text","text":"hi there"}],"stop_reason":"end_turn"}"#,
            ))
            .expect(1)
            .mount(&server)
            .await;

        let client = BedrockAiClient::new(
            "claude-3-opus-20240229".to_string(),
            "test_token".to_string(),
            server.uri(),
            None,
        )
        .unwrap();

        let out = client.send_request("system", "user").await.unwrap();
        assert_eq!(out, "hi there");
    }

    #[test]
    fn get_api_url() {
        let client = BedrockAiClient::new(
            "us.anthropic.claude-3-7-sonnet-20250219-v1:0".to_string(),
            "test_token".to_string(),
            "https://bedrock-api.com/bedrock".to_string(),
            None,
        )
        .unwrap();

        let url = client.get_api_url().unwrap();
        assert_eq!(
            url,
            "https://bedrock-api.com/bedrock/model/us.anthropic.claude-3-7-sonnet-20250219-v1%3A0/invoke"
        );
    }

    #[test]
    fn get_max_tokens() {
        // Test legacy Claude 3 Opus
        let client = BedrockAiClient::new(
            "claude-3-opus-20240229".to_string(),
            "test_token".to_string(),
            "https://example.com".to_string(),
            None,
        )
        .unwrap();
        assert_eq!(client.get_max_tokens(), 4096); // Correct legacy limit

        // Test Claude Sonnet 4
        let client = BedrockAiClient::new(
            "claude-sonnet-4-20250514".to_string(),
            "test_token".to_string(),
            "https://example.com".to_string(),
            None,
        )
        .unwrap();
        assert_eq!(client.get_max_tokens(), 64000); // New high limit

        // Test unknown model falls back to provider defaults
        let client = BedrockAiClient::new(
            "claude-unknown-model".to_string(),
            "test_token".to_string(),
            "https://example.com".to_string(),
            None,
        )
        .unwrap();
        assert_eq!(client.get_max_tokens(), 4096); // Claude provider default
    }

    #[test]
    fn get_api_url_with_trailing_slash() {
        let client = BedrockAiClient::new(
            "us.anthropic.claude-3-7-sonnet-20250219-v1:0".to_string(),
            "test_token".to_string(),
            "https://bedrock-api.com/bedrock/".to_string(),
            None,
        )
        .unwrap();

        let url = client.get_api_url().unwrap();
        assert_eq!(
            url,
            "https://bedrock-api.com/bedrock/model/us.anthropic.claude-3-7-sonnet-20250219-v1%3A0/invoke"
        );
    }

    #[test]
    fn get_api_url_simple_model() {
        let client = BedrockAiClient::new(
            "claude-sonnet-4-20250514".to_string(),
            "test_token".to_string(),
            "https://bedrock-api.com/api".to_string(),
            None,
        )
        .unwrap();

        let url = client.get_api_url().unwrap();
        assert!(url.contains("model/claude-sonnet-4-20250514/invoke"));
    }

    #[test]
    fn get_metadata_without_beta() {
        let client = BedrockAiClient::new(
            "claude-sonnet-4-20250514".to_string(),
            "token".to_string(),
            "https://example.com".to_string(),
            None,
        )
        .unwrap();
        let metadata = client.get_metadata();
        assert_eq!(metadata.provider, "Anthropic Bedrock");
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
        let client = BedrockAiClient::new(
            "claude-sonnet-4-20250514".to_string(),
            "token".to_string(),
            "https://example.com".to_string(),
            beta,
        )
        .unwrap();
        let metadata = client.get_metadata();
        assert!(metadata.active_beta.is_some());
    }

    #[test]
    fn bedrock_client_new() {
        let client = BedrockAiClient::new(
            "claude-sonnet-4-20250514".to_string(),
            "my-token".to_string(),
            "https://bedrock.us-east-1.amazonaws.com".to_string(),
            None,
        )
        .unwrap();
        assert_eq!(client.model, "claude-sonnet-4-20250514");
        assert_eq!(client.auth_token, "my-token");
        assert_eq!(client.base_url, "https://bedrock.us-east-1.amazonaws.com");
        assert!(client.active_beta.is_none());
    }

    /// Structured-output capability is gated on the model registry (#1119):
    /// a flagged model advertises schema support; an older model that would
    /// `400` on `output_config` does not.
    #[test]
    fn capabilities_gate_on_model_support() {
        let unsupported = BedrockAiClient::new(
            "claude-sonnet-4-20250514".to_string(),
            "token".to_string(),
            "https://example.com".to_string(),
            None,
        )
        .unwrap();
        assert!(!unsupported.capabilities().supports_response_schema);

        let supported = BedrockAiClient::new(
            "claude-sonnet-4-6".to_string(),
            "token".to_string(),
            "https://example.com".to_string(),
            None,
        )
        .unwrap();
        assert!(supported.capabilities().supports_response_schema);
    }

    /// A schema-bearing request serialises `output_config.format` on the
    /// Bedrock invoke body; the no-schema path omits it entirely.
    #[tokio::test]
    async fn send_request_with_options_serializes_output_config() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"id":"m","type":"message","role":"assistant","model":"m",
                    "content":[{"type":"text","text":"{\"answer\":\"ok\"}"}],"stop_reason":"end_turn"}"#,
            ))
            .expect(1)
            .mount(&server)
            .await;

        let client = BedrockAiClient::new(
            "claude-sonnet-4-6".to_string(),
            "token".to_string(),
            server.uri(),
            None,
        )
        .unwrap();

        let schema = serde_json::json!({
            "type": "object",
            "properties": { "answer": { "type": "string" } },
            "required": ["answer"],
            "additionalProperties": false,
        });
        let options = RequestOptions::default().with_response_schema(schema.clone());
        let out = client
            .send_request_with_options("sys", "user", options)
            .await
            .unwrap();
        assert_eq!(out, r#"{"answer":"ok"}"#);

        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
        assert_eq!(body["output_config"]["format"]["schema"], schema);
    }

    #[tokio::test]
    async fn send_request_omits_output_config_without_schema() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"id":"m","type":"message","role":"assistant","model":"m",
                    "content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn"}"#,
            ))
            .expect(1)
            .mount(&server)
            .await;

        let client = BedrockAiClient::new(
            "claude-sonnet-4-6".to_string(),
            "token".to_string(),
            server.uri(),
            None,
        )
        .unwrap();
        let _ = client.send_request("sys", "user").await.unwrap();

        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
        assert!(
            body.get("output_config").is_none(),
            "expected output_config to be omitted, got: {body}"
        );
    }
}
