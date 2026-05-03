//! Claude API integration for commit message improvement.

pub mod ai;
pub(crate) mod batch;
pub mod client;
pub mod context;
pub(crate) mod diff_pack;
pub mod error;
pub mod model_config;
pub mod prompts;
pub mod response_schema;
#[cfg(test)]
pub(crate) mod test_utils;
pub(crate) mod token_budget;

pub use ai::bedrock::BedrockAiClient;
pub use ai::claude::ClaudeAiClient;
pub use ai::{
    AiClient, AiClientCapabilities, AiClientMetadata, PromptStyle, RequestOptions, ResponseFormat,
};
pub use client::{create_default_claude_client, ClaudeClient};
pub use context::*;
pub use error::ClaudeError;
