//! AI client trait and metadata definitions.

pub mod bedrock;
pub mod claude;
pub mod openai;

use std::future::Future;
use std::pin::Pin;

use anyhow::Result;

/// Metadata about an AI client implementation.
#[derive(Clone, Debug)]
pub struct AiClientMetadata {
    /// Service provider name.
    pub provider: String,
    /// Model identifier.
    pub model: String,
    /// Maximum context length supported.
    pub max_context_length: usize,
    /// Maximum token response length supported.
    pub max_response_length: usize,
    /// Active beta header, if any: (key, value).
    pub active_beta: Option<(String, String)>,
}

/// Prompt formatting families for AI providers.
///
/// Determines provider-specific prompt behaviour (e.g., how template
/// instructions are phrased). Parse once at the boundary via
/// [`AiClientMetadata::prompt_style`] and match on the enum downstream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptStyle {
    /// Claude models handle "literal template" instructions correctly.
    Claude,
    /// OpenAI-compatible models (OpenAI, Ollama) need different formatting.
    OpenAi,
}

impl AiClientMetadata {
    /// Derives the prompt style from the provider name.
    #[must_use]
    pub fn prompt_style(&self) -> PromptStyle {
        let p = self.provider.to_lowercase();
        if p.contains("openai") || p.contains("ollama") {
            PromptStyle::OpenAi
        } else {
            PromptStyle::Claude
        }
    }
}

/// Trait for AI service clients.
pub trait AiClient: Send + Sync {
    /// Sends a request to the AI service and returns the raw response.
    fn send_request<'a>(
        &'a self,
        system_prompt: &'a str,
        user_prompt: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;

    /// Returns metadata about the AI client implementation.
    fn get_metadata(&self) -> AiClientMetadata;
}
