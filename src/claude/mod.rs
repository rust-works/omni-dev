//! Claude API integration for commit message improvement

pub mod ai_client;
pub mod claude_ai_client;
pub mod client;
pub mod context;
pub mod error;
pub mod prompts;

pub use ai_client::{AiClient, AiClientMetadata};
pub use claude_ai_client::ClaudeAiClient;
pub use client::{create_default_claude_client, ClaudeClient};
pub use context::*;
pub use error::ClaudeError;
