//! OpenAI-compatible API client implementation (works with OpenAI, Ollama, etc.).

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use super::{AiClient, AiClientCapabilities, AiClientMetadata, RequestOptions};
use crate::claude::{error::ClaudeError, model_config::get_model_registry};

/// Per-request timeout used when probing a local server's loaded context
/// length at startup. Kept short so a stalled server can't delay the real
/// request behind a long handshake.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Identifier for the probe endpoint that supplied the cached context
/// length. Surfaces in the startup log line and in the metadata so users
/// can tell which server reported the value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeSource {
    /// LM Studio's `/api/v0/models` endpoint.
    LmStudio,
    /// Ollama's native `/api/show` endpoint.
    Ollama,
}

impl ProbeSource {
    /// Stable string used in logs (`source=lmstudio` etc.).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LmStudio => "lmstudio",
            Self::Ollama => "ollama",
        }
    }
}

/// OpenAI API request message.
#[derive(Serialize, Debug)]
struct Message {
    role: String,
    content: String,
}

/// OpenAI structured-output `response_format` field.
///
/// Top-level shape required by the chat-completions API for JSON Schema
/// mode (`{"type": "json_schema", "json_schema": {...}}`). Honoured by
/// OpenAI ≥2024-08-06, LM Studio, and Ollama ≥0.5; older servers either
/// 400 on the unknown field or silently ignore it. The
/// [`Option`] wrapper on the parent ([`OpenAiRequest::response_format`])
/// keeps the wire body byte-identical to today's when no schema is set.
#[derive(Serialize, Debug)]
struct ResponseFormatField {
    #[serde(rename = "type")]
    kind: &'static str,
    json_schema: JsonSchemaSpec,
}

/// Inner spec of an OpenAI `response_format: json_schema` envelope.
///
/// `name` is a label OpenAI requires but does not validate against; we
/// always emit the literal `"response"`. `strict: true` opts into the
/// hard-validated subset (every property required, no
/// `additionalProperties`, no `oneOf`/`anyOf`).
#[derive(Serialize, Debug)]
struct JsonSchemaSpec {
    name: &'static str,
    strict: bool,
    schema: serde_json::Value,
}

/// OpenAI API request body.
#[derive(Serialize, Debug)]
struct OpenAiRequest {
    model: String,
    messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormatField>,
}

/// OpenAI API response choice.
#[derive(Deserialize, Debug)]
struct Choice {
    message: ResponseMessage,
    #[allow(dead_code)] // Populated by serde, retained for debug output
    finish_reason: Option<String>,
}

/// OpenAI API response message.
#[derive(Deserialize, Debug)]
struct ResponseMessage {
    #[allow(dead_code)] // Populated by serde, retained for debug output
    role: String,
    content: String,
}

/// OpenAI API response.
#[derive(Deserialize, Debug)]
struct OpenAiResponse {
    choices: Vec<Choice>,
    model: Option<String>,
    usage: Option<Usage>,
}

/// OpenAI API usage statistics.
#[derive(Deserialize, Debug)]
#[allow(dead_code)] // Fields populated by serde deserialization, not accessed directly
struct Usage {
    prompt_tokens: Option<i32>,
    completion_tokens: Option<i32>,
    total_tokens: Option<i32>,
}

/// OpenAI-compatible API client (works with OpenAI, Ollama, etc.).
pub struct OpenAiAiClient {
    /// HTTP client for API requests.
    client: Client,
    /// API key for authentication (optional for Ollama).
    api_key: Option<String>,
    /// Model identifier.
    model: String,
    /// Base URL for the API (e.g., `https://api.openai.com` or `http://localhost:11434`).
    base_url: String,
    /// Maximum tokens for responses.
    max_tokens: Option<i32>,
    /// Temperature for response generation.
    temperature: Option<f32>,
    /// Active beta header (key, value) if enabled.
    active_beta: Option<(String, String)>,
    /// Cached context length discovered by probing a local server. When
    /// `Some`, this overrides the registry/default value in
    /// [`Self::get_metadata`]. When `None`, metadata falls back to the
    /// registry as before.
    loaded_context_length: Option<usize>,
}

/// LM Studio `/api/v0/models` response envelope.
#[derive(Deserialize, Debug)]
struct LmStudioModelsResponse {
    data: Vec<LmStudioModel>,
}

/// One entry from LM Studio's loaded-model list.
#[derive(Deserialize, Debug)]
struct LmStudioModel {
    id: String,
    state: Option<String>,
    loaded_context_length: Option<usize>,
}

impl OpenAiAiClient {
    /// Creates a new OpenAI-compatible API client.
    pub fn new(
        model: String,
        api_key: Option<String>,
        base_url: String,
        max_tokens: Option<i32>,
        temperature: Option<f32>,
        active_beta: Option<(String, String)>,
    ) -> Result<Self> {
        let client = super::build_http_client()?;

        Ok(Self {
            client,
            api_key,
            model,
            base_url,
            max_tokens,
            temperature,
            active_beta,
            loaded_context_length: None,
        })
    }

    /// Creates a new client for Ollama with sensible defaults.
    pub fn new_ollama(
        model: String,
        base_url: Option<String>,
        active_beta: Option<(String, String)>,
    ) -> Result<Self> {
        Self::new(
            model,
            None, // No API key needed for Ollama
            base_url.unwrap_or_else(|| "http://localhost:11434".to_string()),
            Some(4096), // Reasonable default
            Some(0.1),  // Low temperature for consistent output
            active_beta,
        )
    }

    /// Creates a new client for OpenAI with sensible defaults.
    pub fn new_openai(
        model: String,
        api_key: String,
        active_beta: Option<(String, String)>,
    ) -> Result<Self> {
        Self::new(
            model,
            Some(api_key),
            "https://api.openai.com".to_string(),
            None,      // Use model registry for max tokens
            Some(0.1), // Low temperature for consistent output
            active_beta,
        )
    }

    /// Returns the max tokens from the configured value or falls back to the model registry.
    fn get_max_tokens(&self) -> i32 {
        if let Some(configured_max) = self.max_tokens {
            return configured_max;
        }
        super::registry_max_output_tokens(&self.model, &self.active_beta)
    }

    /// Builds the full API URL.
    ///
    /// Infallible — only trims a trailing slash and concatenates the
    /// chat-completions path. Returning `String` directly removes a
    /// `?`-propagation site at the call site whose error branch could
    /// never fire.
    fn get_api_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        let url = format!("{base}/v1/chat/completions");
        debug!(base_url = %self.base_url, full_url = %url, "Constructed OpenAI-compatible API URL");
        url
    }

    /// Determines if this is likely an Ollama instance.
    fn is_ollama(&self) -> bool {
        self.base_url.contains("localhost")
            || self.base_url.contains("127.0.0.1")
            || self.api_key.is_none()
    }

    /// Request-log `service` tag for this backend: `ollama` for a local
    /// Ollama-shaped instance, else `openai`. Keeps the two OpenAI-compatible
    /// backends filterable apart in `omni-dev log`.
    fn service_tag(&self) -> &'static str {
        if self.is_ollama() {
            "ollama"
        } else {
            "openai"
        }
    }

    /// Determines if this model is GPT-5 series (uses max_completion_tokens instead of max_tokens).
    fn is_gpt5_series(&self) -> bool {
        self.model.starts_with("gpt-5") || self.model.starts_with("o1")
    }

    /// Returns the cached probed context length, if any.
    #[must_use]
    pub fn loaded_context_length(&self) -> Option<usize> {
        self.loaded_context_length
    }

    /// Stamps a probed context length onto the client. Used by callers
    /// that probe externally (or reconstruct a client whose probe value
    /// was discovered earlier).
    pub fn set_loaded_context_length(&mut self, value: usize) {
        self.loaded_context_length = Some(value);
    }

    /// Assembles an [`OpenAiRequest`] for the given prompts and optional
    /// `response_format`. Pure / synchronous so unit tests can assert
    /// the wire shape without spinning an HTTP mock.
    fn build_request(
        &self,
        system_prompt: &str,
        user_prompt: &str,
        response_format: Option<ResponseFormatField>,
    ) -> OpenAiRequest {
        let mut messages = Vec::new();

        if !system_prompt.is_empty() {
            messages.push(Message {
                role: "system".to_string(),
                content: system_prompt.to_string(),
            });
        }

        messages.push(Message {
            role: "user".to_string(),
            content: user_prompt.to_string(),
        });

        let max_tokens = self.get_max_tokens();
        if self.is_gpt5_series() {
            OpenAiRequest {
                model: self.model.clone(),
                messages,
                max_tokens: None,
                max_completion_tokens: Some(max_tokens),
                // GPT-5 / o-series only accept the default temperature (1.0).
                temperature: None,
                stream: false,
                response_format,
            }
        } else {
            OpenAiRequest {
                model: self.model.clone(),
                messages,
                max_tokens: Some(max_tokens),
                max_completion_tokens: None,
                temperature: self.temperature,
                stream: false,
                response_format,
            }
        }
    }

    /// Sends `request` to the configured chat-completions endpoint and
    /// extracts the assistant text from the first choice.
    ///
    /// Shared by [`send_request`](AiClient::send_request) and
    /// [`send_request_with_options`](AiClient::send_request_with_options) so
    /// the only difference between the two paths is whether `request` carries
    /// a `response_format` field.
    async fn send_inner(&self, request: OpenAiRequest) -> Result<String> {
        debug!(
            max_tokens = ?request.max_tokens,
            max_completion_tokens = ?request.max_completion_tokens,
            configured_temperature = ?self.temperature,
            effective_temperature = ?request.temperature,
            message_count = request.messages.len(),
            is_gpt5_series = self.is_gpt5_series(),
            response_format_set = request.response_format.is_some(),
            "Built OpenAI-compatible request payload"
        );

        let api_url = self.get_api_url();
        info!(url = %api_url, model = %self.model, "Sending request to OpenAI-compatible API");

        let mut req_builder = self
            .client
            .post(&api_url)
            .header("Content-Type", "application/json")
            .json(&request);

        if let Some(ref api_key) = self.api_key {
            req_builder = req_builder.header("Authorization", format!("Bearer {api_key}"));
        }

        let started = std::time::Instant::now();
        let send_result = req_builder.send().await;
        super::record_ai_http(self.service_tag(), "POST", &api_url, started, &send_result);
        let response = send_result.map_err(|e| ClaudeError::NetworkError(e.to_string()))?;

        let response = super::check_error_response(response).await?;

        let openai_response: OpenAiResponse = response
            .json()
            .await
            .map_err(|e| ClaudeError::InvalidResponseFormat(e.to_string()))?;

        debug!(
            choice_count = openai_response.choices.len(),
            model = ?openai_response.model,
            usage = ?openai_response.usage,
            "Received OpenAI-compatible API response"
        );

        let result = openai_response
            .choices
            .first()
            .map(|choice| choice.message.content.clone())
            .ok_or_else(|| {
                ClaudeError::InvalidResponseFormat("No choices in response".to_string()).into()
            });

        super::log_response_success("OpenAI-compatible", &result);

        result
    }

    /// Probes a local OpenAI-compatible server for the loaded model's
    /// actual context length and caches the result on the client.
    ///
    /// Tries LM Studio's `/api/v0/models` endpoint first, then falls
    /// back to Ollama's native `/api/show`. Returns the source on
    /// success so callers can log which endpoint supplied the value.
    /// Returns `None` if neither responds with a usable answer; in that
    /// case the client is left unchanged and [`Self::get_metadata`]
    /// will keep using the registry/default.
    ///
    /// Probe failures intentionally never bubble up as errors — the
    /// goal is to *refine* metadata when the server is reachable, not
    /// to abort startup.
    pub async fn probe_loaded_context_length(&mut self) -> Option<ProbeSource> {
        let host = host_root(&self.base_url);
        let service = self.service_tag();

        if let Some(value) = probe_lm_studio(&self.client, &host, &self.model, service).await {
            self.loaded_context_length = Some(value);
            return Some(ProbeSource::LmStudio);
        }

        if let Some(value) = probe_ollama_native(&self.client, &host, &self.model, service).await {
            self.loaded_context_length = Some(value);
            return Some(ProbeSource::Ollama);
        }

        None
    }
}

/// Strips a trailing `/v1` segment (and any trailing slash) so a probe
/// URL can be built relative to the server root. LM Studio and Ollama
/// expose probe endpoints at the host root, but `OLLAMA_BASE_URL` is
/// commonly configured as the OpenAI-compatible path (e.g.
/// `http://localhost:1234/v1`).
fn host_root(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    trimmed
        .strip_suffix("/v1")
        .unwrap_or(trimmed)
        .trim_end_matches('/')
        .to_string()
}

/// Probes LM Studio's `/api/v0/models` endpoint for the loaded context
/// length of a specific model id. Returns `None` if the server doesn't
/// respond, doesn't return JSON in the expected shape, doesn't list the
/// requested model, or the model isn't currently loaded.
async fn probe_lm_studio(client: &Client, host: &str, model: &str, service: &str) -> Option<usize> {
    let url = format!("{host}/api/v0/models");
    debug!(url = %url, model = %model, "Probing LM Studio for loaded context length");

    let started = std::time::Instant::now();
    let result = client.get(&url).timeout(PROBE_TIMEOUT).send().await;
    super::record_ai_http(service, "GET", &url, started, &result);
    let response = result.ok()?;
    if !response.status().is_success() {
        debug!(status = %response.status(), "LM Studio probe returned non-success");
        return None;
    }
    let body: LmStudioModelsResponse = response.json().await.ok()?;
    body.data
        .into_iter()
        .find(|entry| entry.id == model && entry.state.as_deref() == Some("loaded"))
        .and_then(|entry| entry.loaded_context_length)
}

/// Probes Ollama's native `/api/show` endpoint for the loaded model's
/// declared context length. The architecture prefix on the
/// `model_info.<arch>.context_length` key varies (`llama`, `qwen2`,
/// `gemma`, …), so we scan for any key ending in `.context_length`.
async fn probe_ollama_native(
    client: &Client,
    host: &str,
    model: &str,
    service: &str,
) -> Option<usize> {
    let url = format!("{host}/api/show");
    debug!(url = %url, model = %model, "Probing Ollama for loaded context length");

    let started = std::time::Instant::now();
    let result = client
        .post(&url)
        .timeout(PROBE_TIMEOUT)
        .json(&serde_json::json!({ "name": model }))
        .send()
        .await;
    super::record_ai_http(service, "POST", &url, started, &result);
    let response = result.ok()?;
    if !response.status().is_success() {
        debug!(status = %response.status(), "Ollama probe returned non-success");
        return None;
    }
    let body: serde_json::Value = response.json().await.ok()?;
    let model_info = body.get("model_info")?.as_object()?;
    for (key, value) in model_info {
        if key.ends_with(".context_length") {
            if let Some(n) = value.as_u64() {
                return usize::try_from(n).ok();
            }
        }
    }
    None
}

impl AiClient for OpenAiAiClient {
    fn send_request<'a>(
        &'a self,
        system_prompt: &'a str,
        user_prompt: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
        Box::pin(async move {
            debug!(
                system_prompt_len = system_prompt.len(),
                user_prompt_len = user_prompt.len(),
                model = %self.model,
                base_url = %self.base_url,
                is_ollama = self.is_ollama(),
                "Preparing OpenAI-compatible API request"
            );

            let request = self.build_request(system_prompt, user_prompt, None);
            self.send_inner(request).await
        })
    }

    fn capabilities(&self) -> AiClientCapabilities {
        AiClientCapabilities {
            supports_response_schema: true,
        }
    }

    fn send_request_with_options<'a>(
        &'a self,
        system_prompt: &'a str,
        user_prompt: &'a str,
        options: RequestOptions,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
        Box::pin(async move {
            debug!(
                system_prompt_len = system_prompt.len(),
                user_prompt_len = user_prompt.len(),
                has_schema = options.response_schema.is_some(),
                model = %self.model,
                base_url = %self.base_url,
                is_ollama = self.is_ollama(),
                "Preparing OpenAI-compatible API request (with options)"
            );

            let response_format = options.response_schema.map(|schema| ResponseFormatField {
                kind: "json_schema",
                json_schema: JsonSchemaSpec {
                    name: "response",
                    strict: true,
                    schema,
                },
            });

            let request = self.build_request(system_prompt, user_prompt, response_format);
            self.send_inner(request).await
        })
    }

    fn get_metadata(&self) -> AiClientMetadata {
        let registry = get_model_registry();

        // A successful probe overrides the registry — the server's
        // loaded context is the authoritative limit, registry values
        // are only an estimate.
        let max_context_length = if let Some(probed) = self.loaded_context_length {
            probed
        } else if registry.get_input_context(&self.model) > 0 {
            registry.get_input_context(&self.model)
        } else {
            32768 // Reasonable default for modern models
        };

        let max_response_length = if registry.get_max_output_tokens(&self.model) > 0 {
            registry.get_max_output_tokens(&self.model)
        } else {
            4096 // Reasonable default
        };

        let provider = if self.is_ollama() {
            "Ollama".to_string()
        } else {
            "OpenAI".to_string()
        };

        AiClientMetadata {
            provider,
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
    fn new_ollama() {
        let client = OpenAiAiClient::new_ollama("llama2".to_string(), None, None).unwrap();
        assert_eq!(client.model, "llama2");
        assert_eq!(client.base_url, "http://localhost:11434");
        assert!(client.api_key.is_none());
        assert!(client.is_ollama());
    }

    #[test]
    fn new_ollama_custom_url() {
        let client = OpenAiAiClient::new_ollama(
            "codellama".to_string(),
            Some("http://192.168.1.100:11434".to_string()),
            None,
        )
        .unwrap();
        assert_eq!(client.base_url, "http://192.168.1.100:11434");
        assert!(client.is_ollama());
    }

    #[test]
    fn new_openai() {
        let client =
            OpenAiAiClient::new_openai("gpt-4".to_string(), "sk-test123".to_string(), None)
                .unwrap();
        assert_eq!(client.model, "gpt-4");
        assert_eq!(client.base_url, "https://api.openai.com");
        assert_eq!(client.api_key, Some("sk-test123".to_string()));
        assert!(!client.is_ollama());
    }

    #[test]
    fn get_api_url() {
        let client = OpenAiAiClient::new_ollama("llama2".to_string(), None, None).unwrap();
        let url = client.get_api_url();
        assert_eq!(url, "http://localhost:11434/v1/chat/completions");
    }

    #[test]
    fn get_api_url_trailing_slash() {
        let client = OpenAiAiClient::new(
            "test-model".to_string(),
            None,
            "http://localhost:11434/".to_string(),
            None,
            None,
            None,
        )
        .unwrap();
        let url = client.get_api_url();
        assert_eq!(url, "http://localhost:11434/v1/chat/completions");
    }

    #[test]
    fn is_ollama_detection() {
        // Test localhost detection
        let ollama_client = OpenAiAiClient::new(
            "llama2".to_string(),
            None,
            "http://localhost:11434".to_string(),
            None,
            None,
            None,
        )
        .unwrap();
        assert!(ollama_client.is_ollama());

        // Test 127.0.0.1 detection
        let local_client = OpenAiAiClient::new(
            "llama2".to_string(),
            Some("fake-key".to_string()),
            "http://127.0.0.1:11434".to_string(),
            None,
            None,
            None,
        )
        .unwrap();
        assert!(local_client.is_ollama());

        // Test no API key detection
        let no_key_client = OpenAiAiClient::new(
            "llama2".to_string(),
            None,
            "http://remote-server.com".to_string(),
            None,
            None,
            None,
        )
        .unwrap();
        assert!(no_key_client.is_ollama());

        // Test OpenAI detection
        let openai_client = OpenAiAiClient::new(
            "gpt-4".to_string(),
            Some("sk-real-key".to_string()),
            "https://api.openai.com".to_string(),
            None,
            None,
            None,
        )
        .unwrap();
        assert!(!openai_client.is_ollama());
    }

    #[test]
    fn service_tag_distinguishes_ollama_from_openai() {
        // Request-log service tag follows is_ollama so the two OpenAI-compatible
        // backends are filterable apart (#1122).
        let ollama = OpenAiAiClient::new(
            "llama2".to_string(),
            None,
            "http://localhost:11434".to_string(),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(ollama.service_tag(), "ollama");

        let openai = OpenAiAiClient::new(
            "gpt-4".to_string(),
            Some("sk-real-key".to_string()),
            "https://api.openai.com".to_string(),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(openai.service_tag(), "openai");
    }

    // ── is_gpt5_series ───────────────────────────────────────────────

    #[test]
    fn gpt5_series_gpt5_models() {
        let client = OpenAiAiClient::new(
            "gpt-5-preview".to_string(),
            Some("key".to_string()),
            "https://api.openai.com".to_string(),
            None,
            None,
            None,
        )
        .unwrap();
        assert!(client.is_gpt5_series());

        let client2 = OpenAiAiClient::new(
            "gpt-5".to_string(),
            Some("key".to_string()),
            "https://api.openai.com".to_string(),
            None,
            None,
            None,
        )
        .unwrap();
        assert!(client2.is_gpt5_series());
    }

    #[test]
    fn gpt5_series_o1_models() {
        let client = OpenAiAiClient::new(
            "o1-mini".to_string(),
            Some("key".to_string()),
            "https://api.openai.com".to_string(),
            None,
            None,
            None,
        )
        .unwrap();
        assert!(client.is_gpt5_series());

        let client2 = OpenAiAiClient::new(
            "o1-preview".to_string(),
            Some("key".to_string()),
            "https://api.openai.com".to_string(),
            None,
            None,
            None,
        )
        .unwrap();
        assert!(client2.is_gpt5_series());
    }

    #[test]
    fn gpt5_series_regular_models_not_matched() {
        let client = OpenAiAiClient::new(
            "gpt-4".to_string(),
            Some("key".to_string()),
            "https://api.openai.com".to_string(),
            None,
            None,
            None,
        )
        .unwrap();
        assert!(!client.is_gpt5_series());

        let client2 = OpenAiAiClient::new(
            "gpt-4o-mini".to_string(),
            Some("key".to_string()),
            "https://api.openai.com".to_string(),
            None,
            None,
            None,
        )
        .unwrap();
        assert!(!client2.is_gpt5_series());
    }

    // ── get_max_tokens ───────────────────────────────────────────────

    #[test]
    fn get_max_tokens_configured_value_wins() {
        let client = OpenAiAiClient::new(
            "gpt-4".to_string(),
            Some("key".to_string()),
            "https://api.openai.com".to_string(),
            Some(8192),
            None,
            None,
        )
        .unwrap();
        assert_eq!(client.get_max_tokens(), 8192);
    }

    #[test]
    fn get_max_tokens_from_registry() {
        // Ollama with no configured max → falls back to registry
        let client =
            OpenAiAiClient::new_openai("gpt-4o".to_string(), "key".to_string(), None).unwrap();
        let tokens = client.get_max_tokens();
        // Registry should return a positive value for a known model
        assert!(tokens > 0, "expected positive token limit, got {tokens}");
    }

    // ── get_metadata ─────────────────────────────────────────────────

    #[test]
    fn get_metadata_openai() {
        let client =
            OpenAiAiClient::new_openai("gpt-4o".to_string(), "key".to_string(), None).unwrap();
        let metadata = client.get_metadata();
        assert_eq!(metadata.provider, "OpenAI");
        assert_eq!(metadata.model, "gpt-4o");
        assert!(metadata.active_beta.is_none());
    }

    #[test]
    fn get_metadata_ollama() {
        let client = OpenAiAiClient::new_ollama("llama2".to_string(), None, None).unwrap();
        let metadata = client.get_metadata();
        assert_eq!(metadata.provider, "Ollama");
        assert_eq!(metadata.model, "llama2");
    }

    #[test]
    fn get_metadata_with_beta() {
        let beta = Some(("anthropic-beta".to_string(), "output-128k".to_string()));
        let client =
            OpenAiAiClient::new_openai("gpt-4o".to_string(), "key".to_string(), beta).unwrap();
        let metadata = client.get_metadata();
        assert!(metadata.active_beta.is_some());
        let (key, value) = metadata.active_beta.unwrap();
        assert_eq!(key, "anthropic-beta");
        assert_eq!(value, "output-128k");
    }

    // ── OpenAiRequest serialization ──────────────────────────────────

    #[test]
    fn request_gpt5_uses_max_completion_tokens() {
        let request = OpenAiRequest {
            model: "gpt-5".to_string(),
            messages: vec![Message {
                role: "user".to_string(),
                content: "hello".to_string(),
            }],
            max_tokens: None,
            max_completion_tokens: Some(4096),
            temperature: None,
            stream: false,
            response_format: None,
        };

        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("max_completion_tokens"));
        // max_tokens should be None and thus skipped
        assert!(!json.contains("\"max_tokens\""));
    }

    #[test]
    fn request_regular_model_uses_max_tokens() {
        let request = OpenAiRequest {
            model: "gpt-4".to_string(),
            messages: vec![Message {
                role: "user".to_string(),
                content: "hello".to_string(),
            }],
            max_tokens: Some(4096),
            max_completion_tokens: None,
            temperature: Some(0.1),
            stream: false,
            response_format: None,
        };

        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"max_tokens\""));
        assert!(!json.contains("max_completion_tokens"));
        assert!(json.contains("\"temperature\""));
    }

    /// OpenAI / Ollama backends now route schema-bearing options through
    /// `response_format: json_schema` on the chat-completions endpoint, so
    /// capabilities must advertise the support to drive the dispatch in
    /// `client.rs::send_with_optional_schema`.
    #[test]
    fn capabilities_advertise_response_schema_support_openai() {
        let client =
            OpenAiAiClient::new_openai("gpt-4o".to_string(), "key".to_string(), None).unwrap();
        assert!(client.capabilities().supports_response_schema);
    }

    #[test]
    fn capabilities_advertise_response_schema_support_ollama() {
        let client = OpenAiAiClient::new_ollama("llama2".to_string(), None, None).unwrap();
        assert!(client.capabilities().supports_response_schema);
    }

    // ── build_request: response_format wiring ────────────────────────

    /// Without a schema in `RequestOptions`, the serialized body must
    /// not contain the `response_format` key. This guards the
    /// `skip_serializing_if = "Option::is_none"` invariant — older OpenAI
    /// servers / pre-0.5 Ollama 400 on unknown fields, so the wire body
    /// must stay byte-identical to today's when no schema is set.
    #[test]
    fn build_request_omits_response_format_without_schema() {
        let client =
            OpenAiAiClient::new_openai("gpt-4o".to_string(), "key".to_string(), None).unwrap();
        let request = client.build_request("sys", "user", None);
        let body = serde_json::to_value(&request).unwrap();
        assert!(
            body.get("response_format").is_none(),
            "expected response_format to be omitted, got: {body}"
        );
    }

    /// With a schema attached, the serialized body must carry the exact
    /// OpenAI structured-output envelope shape.
    #[test]
    fn build_request_embeds_response_format_with_schema_regular_model() {
        let client =
            OpenAiAiClient::new_openai("gpt-4o".to_string(), "key".to_string(), None).unwrap();
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "answer": { "type": "string" } },
            "required": ["answer"],
            "additionalProperties": false,
        });
        let response_format = Some(ResponseFormatField {
            kind: "json_schema",
            json_schema: JsonSchemaSpec {
                name: "response",
                strict: true,
                schema: schema.clone(),
            },
        });
        let request = client.build_request("sys", "user", response_format);
        let body = serde_json::to_value(&request).unwrap();
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["name"], "response");
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
        assert_eq!(body["response_format"]["json_schema"]["schema"], schema);
        // Regular (non-GPT-5) path: max_tokens set, max_completion_tokens absent.
        assert!(body.get("max_tokens").is_some());
        assert!(body.get("max_completion_tokens").is_none());
    }

    /// `response_format` flows through identically on the GPT-5 / o1 path
    /// (which uses `max_completion_tokens` instead of `max_tokens`).
    #[test]
    fn build_request_embeds_response_format_with_schema_gpt5() {
        let client =
            OpenAiAiClient::new_openai("gpt-5".to_string(), "key".to_string(), None).unwrap();
        let schema = serde_json::json!({ "type": "object", "additionalProperties": false });
        let response_format = Some(ResponseFormatField {
            kind: "json_schema",
            json_schema: JsonSchemaSpec {
                name: "response",
                strict: true,
                schema: schema.clone(),
            },
        });
        let request = client.build_request("sys", "user", response_format);
        let body = serde_json::to_value(&request).unwrap();
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["schema"], schema);
        // GPT-5 path: max_completion_tokens set, max_tokens / temperature absent.
        assert!(body.get("max_completion_tokens").is_some());
        assert!(body.get("max_tokens").is_none());
        assert!(body.get("temperature").is_none());
    }

    // ── host_root ────────────────────────────────────────────────────

    #[test]
    fn host_root_strips_trailing_slash() {
        assert_eq!(host_root("http://localhost:1234/"), "http://localhost:1234");
    }

    #[test]
    fn host_root_strips_v1_suffix() {
        assert_eq!(
            host_root("http://localhost:1234/v1"),
            "http://localhost:1234"
        );
    }

    #[test]
    fn host_root_strips_v1_with_trailing_slash() {
        assert_eq!(
            host_root("http://localhost:1234/v1/"),
            "http://localhost:1234"
        );
    }

    #[test]
    fn host_root_passthrough_when_no_v1() {
        assert_eq!(
            host_root("http://localhost:11434"),
            "http://localhost:11434"
        );
    }

    // ── ProbeSource ──────────────────────────────────────────────────

    #[test]
    fn probe_source_as_str_stable() {
        assert_eq!(ProbeSource::LmStudio.as_str(), "lmstudio");
        assert_eq!(ProbeSource::Ollama.as_str(), "ollama");
    }

    // ── set_loaded_context_length / get_metadata interaction ─────────

    #[test]
    fn metadata_uses_probed_value_when_set() {
        let mut client = OpenAiAiClient::new_ollama("llama2".to_string(), None, None).unwrap();
        client.set_loaded_context_length(8192);
        let metadata = client.get_metadata();
        assert_eq!(metadata.max_context_length, 8192);
    }

    #[test]
    fn metadata_falls_back_to_registry_when_probe_value_absent() {
        // Probed value is intentionally unset → metadata mirrors the
        // pre-probe behaviour (registry estimate). For an unknown model
        // the registry resolves to its FALLBACK_INPUT_CONTEXT.
        let client =
            OpenAiAiClient::new_ollama("totally-unknown-model".to_string(), None, None).unwrap();
        let metadata = client.get_metadata();
        let expected = get_model_registry().get_input_context("totally-unknown-model");
        assert_eq!(metadata.max_context_length, expected);
    }

    #[test]
    fn loaded_context_length_starts_unset() {
        let client = OpenAiAiClient::new_ollama("llama2".to_string(), None, None).unwrap();
        assert!(client.loaded_context_length().is_none());
    }

    #[test]
    fn loaded_context_length_round_trips() {
        let mut client = OpenAiAiClient::new_ollama("llama2".to_string(), None, None).unwrap();
        client.set_loaded_context_length(4096);
        assert_eq!(client.loaded_context_length(), Some(4096));
    }

    // ── probe_loaded_context_length ──────────────────────────────────

    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn ollama_client_pointing_at(server_uri: &str, model: &str) -> OpenAiAiClient {
        OpenAiAiClient::new_ollama(model.to_string(), Some(server_uri.to_string()), None).unwrap()
    }

    #[tokio::test]
    async fn probe_returns_lm_studio_value_when_model_loaded() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v0/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    {
                        "id": "llama-3.2-3b-instruct",
                        "state": "loaded",
                        "loaded_context_length": 4096_u64,
                        "max_context_length": 131_072_u64,
                    }
                ]
            })))
            .mount(&server)
            .await;

        let mut client = ollama_client_pointing_at(&server.uri(), "llama-3.2-3b-instruct");
        let source = client.probe_loaded_context_length().await;
        assert_eq!(source, Some(ProbeSource::LmStudio));
        assert_eq!(client.loaded_context_length(), Some(4096));
        // get_metadata now reflects the probed value, not the registry.
        assert_eq!(client.get_metadata().max_context_length, 4096);
    }

    #[tokio::test]
    async fn probe_skips_lm_studio_entry_when_model_not_loaded() {
        let server = MockServer::start().await;
        // LM Studio knows the model but it isn't loaded → must fall
        // through to the Ollama probe (which we also stub here so it
        // succeeds, proving the fallthrough).
        Mock::given(method("GET"))
            .and(path("/api/v0/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    { "id": "model-a", "state": "not-loaded", "loaded_context_length": 4096_u64 }
                ]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/show"))
            .and(body_json(serde_json::json!({ "name": "model-a" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "model_info": { "llama.context_length": 8192_u64 }
            })))
            .mount(&server)
            .await;

        let mut client = ollama_client_pointing_at(&server.uri(), "model-a");
        let source = client.probe_loaded_context_length().await;
        assert_eq!(source, Some(ProbeSource::Ollama));
        assert_eq!(client.loaded_context_length(), Some(8192));
    }

    #[tokio::test]
    async fn probe_skips_lm_studio_when_model_id_does_not_match() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v0/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    { "id": "other-model", "state": "loaded", "loaded_context_length": 4096_u64 }
                ]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/show"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let mut client = ollama_client_pointing_at(&server.uri(), "wanted-model");
        let source = client.probe_loaded_context_length().await;
        assert!(source.is_none());
        assert!(client.loaded_context_length().is_none());
    }

    #[tokio::test]
    async fn probe_falls_back_to_ollama_native() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v0/models"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/show"))
            .and(body_json(serde_json::json!({ "name": "qwen2.5-coder" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "model_info": {
                    "general.architecture": "qwen2",
                    "qwen2.context_length": 32768_u64,
                    "qwen2.embedding_length": 3584_u64
                }
            })))
            .mount(&server)
            .await;

        let mut client = ollama_client_pointing_at(&server.uri(), "qwen2.5-coder");
        let source = client.probe_loaded_context_length().await;
        assert_eq!(source, Some(ProbeSource::Ollama));
        assert_eq!(client.loaded_context_length(), Some(32768));
    }

    #[tokio::test]
    async fn probe_returns_none_when_neither_endpoint_responds() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v0/models"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/show"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let mut client = ollama_client_pointing_at(&server.uri(), "anything");
        let source = client.probe_loaded_context_length().await;
        assert!(source.is_none());
        assert!(client.loaded_context_length().is_none());
        // Probe failure leaves metadata alone (registry/default still in effect).
        let registry_value = get_model_registry().get_input_context("anything");
        assert_eq!(client.get_metadata().max_context_length, registry_value);
    }

    #[tokio::test]
    async fn probe_returns_none_when_ollama_payload_lacks_context_length_key() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v0/models"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/show"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "model_info": {
                    "general.architecture": "phantom",
                    "phantom.embedding_length": 1024_u64
                }
            })))
            .mount(&server)
            .await;

        let mut client = ollama_client_pointing_at(&server.uri(), "ghost");
        let source = client.probe_loaded_context_length().await;
        assert!(source.is_none());
    }

    #[tokio::test]
    async fn probe_handles_v1_suffix_in_base_url() {
        // OLLAMA_BASE_URL is commonly the OpenAI-compatible path
        // (`.../v1`). The probe must hit `/api/v0/models` at the host
        // root, not `/v1/api/v0/models`.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v0/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    { "id": "lm", "state": "loaded", "loaded_context_length": 2048_u64 }
                ]
            })))
            .mount(&server)
            .await;

        let base_with_v1 = format!("{}/v1", server.uri());
        let mut client = ollama_client_pointing_at(&base_with_v1, "lm");
        let source = client.probe_loaded_context_length().await;
        assert_eq!(source, Some(ProbeSource::LmStudio));
        assert_eq!(client.loaded_context_length(), Some(2048));
    }

    #[tokio::test]
    async fn probe_ignores_lm_studio_entry_with_no_loaded_context_length() {
        let server = MockServer::start().await;
        // Loaded but the field is null — must not panic, must skip and
        // try the Ollama path.
        Mock::given(method("GET"))
            .and(path("/api/v0/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [ { "id": "x", "state": "loaded", "loaded_context_length": null } ]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/show"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let mut client = ollama_client_pointing_at(&server.uri(), "x");
        let source = client.probe_loaded_context_length().await;
        assert!(source.is_none());
    }

    #[tokio::test]
    async fn probe_returns_none_when_lm_studio_returns_invalid_json() {
        // Body is HTML (or anything non-JSON) — `.json::<…>().await.ok()?`
        // returns None and the probe must fall through to Ollama.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v0/models"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>not json</html>"))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/show"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let mut client = ollama_client_pointing_at(&server.uri(), "anything");
        let source = client.probe_loaded_context_length().await;
        assert!(source.is_none());
    }

    #[tokio::test]
    async fn probe_returns_none_when_ollama_returns_invalid_json() {
        // LM Studio is unavailable (404). Ollama returns malformed JSON
        // — `.json::<Value>()` errors, the probe must surface None.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v0/models"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/show"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{not json"))
            .mount(&server)
            .await;

        let mut client = ollama_client_pointing_at(&server.uri(), "anything");
        let source = client.probe_loaded_context_length().await;
        assert!(source.is_none());
    }

    #[tokio::test]
    async fn probe_returns_none_when_ollama_response_lacks_model_info() {
        // 200 OK with valid JSON, but no `model_info` key at all —
        // `body.get("model_info")?` short-circuits.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v0/models"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/show"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "details": { "family": "llama" }
            })))
            .mount(&server)
            .await;

        let mut client = ollama_client_pointing_at(&server.uri(), "anything");
        let source = client.probe_loaded_context_length().await;
        assert!(source.is_none());
    }

    #[tokio::test]
    async fn probe_returns_none_when_ollama_model_info_is_not_object() {
        // `model_info` exists but is a string (impossible in practice,
        // belt-and-braces against a malformed server). `.as_object()?`
        // short-circuits.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v0/models"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/show"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "model_info": "not an object"
            })))
            .mount(&server)
            .await;

        let mut client = ollama_client_pointing_at(&server.uri(), "anything");
        let source = client.probe_loaded_context_length().await;
        assert!(source.is_none());
    }

    #[tokio::test]
    async fn probe_returns_none_when_ollama_context_length_is_not_u64() {
        // The arch.context_length key exists but the value is a string
        // — `value.as_u64()` returns None, loop continues, no other
        // matching key, probe surfaces None.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v0/models"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/show"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "model_info": { "llama.context_length": "8192" }
            })))
            .mount(&server)
            .await;

        let mut client = ollama_client_pointing_at(&server.uri(), "anything");
        let source = client.probe_loaded_context_length().await;
        assert!(source.is_none());
    }

    #[tokio::test]
    async fn probe_returns_none_when_server_unreachable() {
        // Closed-port URL — both `send().await.ok()?` short-circuits
        // fire. Pick a port we don't bind to and is unlikely to host
        // anything; reqwest will fail fast (refused) well within the
        // 2-second probe timeout.
        let mut client = ollama_client_pointing_at("http://127.0.0.1:1", "anything");
        let source = client.probe_loaded_context_length().await;
        assert!(source.is_none());
    }

    // ── send_request_with_options round-trip (wiremock) ──────────────

    /// Stub `/v1/chat/completions` so a single request succeeds and the
    /// recorded request body can be introspected by the caller.
    async fn mock_chat_completion_ok(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [
                    {
                        "message": { "role": "assistant", "content": "ok" },
                        "finish_reason": "stop"
                    }
                ],
                "model": "test-model"
            })))
            .mount(server)
            .await;
    }

    fn openai_client_pointing_at(server_uri: &str, model: &str) -> OpenAiAiClient {
        OpenAiAiClient::new(
            model.to_string(),
            Some("test-key".to_string()),
            server_uri.to_string(),
            Some(1024),
            Some(0.1),
            None,
        )
        .unwrap()
    }

    /// Round-trips a request through the actual `reqwest` JSON serialization
    /// path (not just `serde_json::to_value`) and asserts the wire body
    /// carries the OpenAI structured-output envelope. Initialises a
    /// debug-level tracing subscriber so the `debug!` macro arguments in
    /// `send_request_with_options` actually evaluate (otherwise the
    /// tracing layer short-circuits at INFO level and llvm-cov sees the
    /// argument expressions as never executed).
    #[tokio::test]
    async fn send_request_with_options_serializes_response_format_on_the_wire() {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_test_writer()
            .try_init();

        let server = MockServer::start().await;
        mock_chat_completion_ok(&server).await;

        let client = openai_client_pointing_at(&server.uri(), "gpt-4o");
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "answer": { "type": "string" } },
            "required": ["answer"],
            "additionalProperties": false,
        });
        let options = RequestOptions::default().with_response_schema(schema.clone());

        let result = client
            .send_request_with_options("system", "user", options)
            .await
            .unwrap();
        assert_eq!(result, "ok");

        let received = server.received_requests().await.unwrap();
        assert_eq!(
            received.len(),
            1,
            "expected exactly one chat-completions request"
        );
        let body: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["name"], "response");
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
        assert_eq!(body["response_format"]["json_schema"]["schema"], schema);
    }

    /// Companion guard: the no-options path (or empty options) must not
    /// emit `response_format` on the wire — older Ollama / OpenAI servers
    /// 400 on unknown fields, so the byte-for-byte default body matters.
    #[tokio::test]
    async fn send_request_omits_response_format_on_the_wire() {
        let server = MockServer::start().await;
        mock_chat_completion_ok(&server).await;

        let client = openai_client_pointing_at(&server.uri(), "gpt-4o");
        let _ = client.send_request("system", "user").await.unwrap();

        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
        assert!(
            body.get("response_format").is_none(),
            "expected response_format to be absent from wire body, got: {body}"
        );
    }

    /// Empty system prompt skips the system message: the request body
    /// carries only the user message, not a stub `system` entry. Pins the
    /// `if !system_prompt.is_empty()` guard in `build_request`.
    #[test]
    fn build_request_skips_empty_system_prompt() {
        let client =
            OpenAiAiClient::new_openai("gpt-4o".to_string(), "key".to_string(), None).unwrap();
        let request = client.build_request("", "user prompt", None);
        assert_eq!(request.messages.len(), 1);
        assert_eq!(request.messages[0].role, "user");
        assert_eq!(request.messages[0].content, "user prompt");
    }

    /// Connection failure surfaces as `NetworkError` rather than panicking
    /// or hanging. Pins the `map_err(NetworkError)?` branch on the
    /// `req_builder.send()` call. Uses port 1 — the same closed-port
    /// trick the probe tests use — so reqwest fails fast with connection
    /// refused well before the request timeout.
    #[tokio::test]
    async fn send_request_propagates_network_error_on_unreachable_server() {
        let client = openai_client_pointing_at("http://127.0.0.1:1", "gpt-4o");
        let err = client
            .send_request("system", "user")
            .await
            .expect_err("expected network error against closed port");
        let chain = format!("{err:#}");
        assert!(
            chain.to_lowercase().contains("network"),
            "expected network-error wording in chain, got: {chain}"
        );
    }

    /// HTTP error responses propagate through `check_error_response` as a
    /// structured `ApiRequestFailed` rather than being misinterpreted as a
    /// successful body. Pins the `?` branch on the `check_error_response`
    /// call.
    #[tokio::test]
    async fn send_request_propagates_http_error_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500).set_body_string("upstream boom"))
            .mount(&server)
            .await;

        let client = openai_client_pointing_at(&server.uri(), "gpt-4o");
        let err = client
            .send_request("system", "user")
            .await
            .expect_err("expected error from 500 response");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("HTTP 500"),
            "expected 'HTTP 500' in error chain, got: {chain}"
        );
    }

    /// A malformed JSON body surfaces as `InvalidResponseFormat` rather
    /// than a panic. Pins the `?` branch on `response.json().await`.
    #[tokio::test]
    async fn send_request_propagates_json_parse_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{not valid json"))
            .mount(&server)
            .await;

        let client = openai_client_pointing_at(&server.uri(), "gpt-4o");
        let err = client
            .send_request("system", "user")
            .await
            .expect_err("expected error from malformed JSON body");
        // ClaudeError::InvalidResponseFormat renders as
        // "Invalid response format from Claude API: <inner>". The exact
        // serde message varies; the prefix is the contract we pin.
        let chain = format!("{err:#}");
        assert!(
            chain.contains("Invalid response format"),
            "expected 'Invalid response format' in error chain, got: {chain}"
        );
    }

    /// `send_inner` returns `InvalidResponseFormat` when the API replies
    /// with an empty `choices` array. Pins the `ok_or_else` defensive
    /// branch — without this, a malformed upstream response would surface
    /// as `unwrap_or_else` panicking later instead of a structured error.
    #[tokio::test]
    async fn send_request_errors_when_response_has_no_choices() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [],
                "model": "test-model"
            })))
            .mount(&server)
            .await;

        let client = openai_client_pointing_at(&server.uri(), "gpt-4o");
        let err = client
            .send_request("system", "user")
            .await
            .expect_err("expected error when choices array is empty");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("No choices in response"),
            "expected 'No choices in response' in error chain, got: {chain}"
        );
    }
}
