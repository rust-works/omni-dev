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

    /// Required subprocess binary is missing from PATH.
    #[error("Subprocess binary not found: {0}")]
    SubprocessBinaryMissing(String),

    /// Failed to spawn a subprocess.
    #[error("Failed to spawn subprocess: {0}")]
    SubprocessSpawnFailed(String),

    /// Subprocess exceeded the configured timeout.
    #[error("Subprocess timed out after {secs} seconds")]
    SubprocessTimeout {
        /// Timeout that was exceeded, in seconds.
        secs: u64,
    },

    /// Subprocess produced more output than the configured cap.
    #[error("Subprocess output exceeded limit of {limit} bytes")]
    SubprocessOutputTooLarge {
        /// Configured stdout cap in bytes.
        limit: usize,
    },

    /// Subprocess stdout was not valid JSON.
    #[error("Subprocess produced invalid JSON output: {0}")]
    SubprocessJsonParseFailed(String),
}

// Note: anyhow already has a blanket impl for thiserror::Error types
