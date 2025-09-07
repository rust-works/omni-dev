//! Claude API integration for commit message improvement

pub mod client;
pub mod context;
pub mod error;
pub mod prompts;

pub use client::ClaudeClient;
pub use context::*;
pub use error::ClaudeError;
