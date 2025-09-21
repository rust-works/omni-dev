//! Claude API integration for commit message improvement

pub mod ai;
pub mod client;
pub mod context;
pub mod error;
pub mod model_config;
pub mod prompts;

pub use ai::bedrock::BedrockAiClient;
pub use ai::claude::ClaudeAiClient;
pub use ai::{AiClient, AiClientMetadata};
pub use client::{create_default_claude_client, ClaudeClient};
pub use context::*;
pub use error::ClaudeError;
