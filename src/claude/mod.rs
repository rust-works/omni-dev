//! Claude API integration for commit message improvement

pub mod client;
pub mod error;
pub mod prompts;

pub use client::ClaudeClient;
pub use error::ClaudeError;
