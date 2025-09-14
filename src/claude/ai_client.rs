//! AI client trait and metadata definitions

use anyhow::Result;
use std::future::Future;
use std::pin::Pin;

/// Metadata about an AI client implementation
#[derive(Clone, Debug)]
pub struct AiClientMetadata {
    /// Service provider name
    pub provider: String,
    /// Model identifier
    pub model: String,
    /// Maximum context length supported
    pub max_context_length: usize,
    /// Maximum token response length supported
    pub max_response_length: usize,
}

/// Trait for AI service clients
pub trait AiClient: Send + Sync {
    /// Send a request to the AI service and return the raw response
    fn send_request<'a>(
        &'a self,
        system_prompt: &'a str,
        user_prompt: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;

    /// Get metadata about the AI client implementation
    fn get_metadata(&self) -> AiClientMetadata;
}
