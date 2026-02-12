//! Claude-specific error handling.

use thiserror::Error;

/// Claude API specific errors.
#[derive(Error, Debug)]
pub enum ClaudeError {
    /// API key not found in environment variables.
    #[error(
        "Claude API key not found. Set CLAUDE_API_KEY or ANTHROPIC_API_KEY environment variable"
    )]
    ApiKeyNotFound,

    /// Claude API request failed with error message.
    #[error("Claude API request failed: {0}")]
    ApiRequestFailed(String),

    /// Invalid response format from Claude API.
    #[error("Invalid response format from Claude API: {0}")]
    InvalidResponseFormat(String),

    /// Failed to parse amendments from Claude response.
    #[error("Failed to parse amendments from Claude response: {0}")]
    AmendmentParsingFailed(String),

    /// Prompt exceeds the model's available input token budget.
    #[error(
        "Prompt too large for model '{model}': estimated {estimated_tokens} tokens, \
         but only {max_tokens} input tokens available"
    )]
    PromptTooLarge {
        /// Estimated token count of the assembled prompt.
        estimated_tokens: usize,
        /// Maximum available input tokens (context minus reserved output).
        max_tokens: usize,
        /// Model identifier.
        model: String,
    },

    /// Rate limit exceeded for Claude API.
    #[error("Rate limit exceeded. Please try again later")]
    RateLimitExceeded,

    /// Network connectivity error.
    #[error("Network error: {0}")]
    NetworkError(String),
}

// Note: anyhow already has a blanket impl for thiserror::Error types
