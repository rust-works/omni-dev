//! Claude API integration for commit message improvement

pub mod bedrock;
pub mod client;
pub mod context;
pub mod error;
pub mod prompts;
pub mod provider;

pub use bedrock::BedrockClient;
pub use client::ClaudeClient;
pub use context::*;
pub use error::ClaudeError;
pub use provider::{AiProvider, AiProviderFactory};
