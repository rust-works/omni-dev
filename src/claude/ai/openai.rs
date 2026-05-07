//! OpenAI-compatible API client implementation (works with OpenAI, Ollama, etc.).

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use super::{AiClient, AiClientMetadata};
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
    fn get_api_url(&self) -> Result<String> {
        let mut base = self.base_url.clone();

        // Ensure base URL doesn't end with a slash
        if base.ends_with('/') {
            base.pop();
        }

        // Add the chat completions endpoint
        let url = format!("{base}/v1/chat/completions");

        debug!(base_url = %self.base_url, full_url = %url, "Constructed OpenAI-compatible API URL");

        Ok(url)
    }

    /// Determines if this is likely an Ollama instance.
    fn is_ollama(&self) -> bool {
        self.base_url.contains("localhost")
            || self.base_url.contains("127.0.0.1")
            || self.api_key.is_none()
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

        if let Some(value) = probe_lm_studio(&self.client, &host, &self.model).await {
            self.loaded_context_length = Some(value);
            return Some(ProbeSource::LmStudio);
        }

        if let Some(value) = probe_ollama_native(&self.client, &host, &self.model).await {
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
async fn probe_lm_studio(client: &Client, host: &str, model: &str) -> Option<usize> {
    let url = format!("{host}/api/v0/models");
    debug!(url = %url, model = %model, "Probing LM Studio for loaded context length");

    let response = client.get(&url).timeout(PROBE_TIMEOUT).send().await.ok()?;
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
async fn probe_ollama_native(client: &Client, host: &str, model: &str) -> Option<usize> {
    let url = format!("{host}/api/show");
    debug!(url = %url, model = %model, "Probing Ollama for loaded context length");

    let response = client
        .post(&url)
        .timeout(PROBE_TIMEOUT)
        .json(&serde_json::json!({ "name": model }))
        .send()
        .await
        .ok()?;
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

            // Build messages array with system prompt first, then user prompt
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
            let request = if self.is_gpt5_series() {
                OpenAiRequest {
                    model: self.model.clone(),
                    messages,
                    max_tokens: None,
                    max_completion_tokens: Some(max_tokens),
                    temperature: None, // GPT-5 only supports default temperature (1.0)
                    stream: false,
                }
            } else {
                OpenAiRequest {
                    model: self.model.clone(),
                    messages,
                    max_tokens: Some(max_tokens),
                    max_completion_tokens: None,
                    temperature: self.temperature,
                    stream: false,
                }
            };

            debug!(
                max_tokens = max_tokens,
                configured_temperature = ?self.temperature,
                effective_temperature = ?request.temperature,
                message_count = request.messages.len(),
                is_gpt5_series = self.is_gpt5_series(),
                uses_max_completion_tokens = self.is_gpt5_series(),
                "Built OpenAI-compatible request payload"
            );

            let api_url = self.get_api_url()?;
            info!(url = %api_url, model = %self.model, "Sending request to OpenAI-compatible API");

            // Build the request
            let mut req_builder = self
                .client
                .post(&api_url)
                .header("Content-Type", "application/json")
                .json(&request);

            // Add authorization header if API key is provided
            if let Some(ref api_key) = self.api_key {
                req_builder = req_builder.header("Authorization", format!("Bearer {api_key}"));
            }

            let response = req_builder
                .send()
                .await
                .map_err(|e| ClaudeError::NetworkError(e.to_string()))?;

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

            // Extract text content from the first choice
            let result = openai_response
                .choices
                .first()
                .map(|choice| choice.message.content.clone())
                .ok_or_else(|| {
                    ClaudeError::InvalidResponseFormat("No choices in response".to_string()).into()
                });

            super::log_response_success("OpenAI-compatible", &result);

            result
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
        let url = client.get_api_url().unwrap();
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
        let url = client.get_api_url().unwrap();
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
        };

        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"max_tokens\""));
        assert!(!json.contains("max_completion_tokens"));
        assert!(json.contains("\"temperature\""));
    }

    /// OpenAI / Ollama backends don't expose JSON Schema enforcement
    /// here yet, so capabilities must report `false` for both.
    #[test]
    fn capabilities_default_to_no_schema_support_openai() {
        let client =
            OpenAiAiClient::new_openai("gpt-4o".to_string(), "key".to_string(), None).unwrap();
        assert!(!client.capabilities().supports_response_schema);
    }

    #[test]
    fn capabilities_default_to_no_schema_support_ollama() {
        let client = OpenAiAiClient::new_ollama("llama2".to_string(), None, None).unwrap();
        assert!(!client.capabilities().supports_response_schema);
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
}
