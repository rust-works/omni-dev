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
    ///
    /// Used where no HTTP status is available (subprocess failures, or an error
    /// the backend could not attribute to a status). Prefer
    /// [`ClaudeError::ApiHttpError`] whenever a status is known, so callers can
    /// tell a permanent failure from a retryable one.
    #[error("Claude API request failed: {0}")]
    ApiRequestFailed(String),

    /// AI API returned a non-success HTTP status.
    #[error("Claude API request failed (HTTP {status}): {body}")]
    ApiHttpError {
        /// HTTP status code returned by the API.
        status: u16,
        /// Response body, used as the error detail.
        body: String,
    },

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

impl ClaudeError {
    /// Returns `true` when retrying the request could plausibly succeed.
    ///
    /// Only a non-retryable 4xx is treated as permanent: the request is
    /// malformed, unauthorised, or names something that does not exist (a
    /// misspelled model, say), so no amount of retrying or falling back will
    /// help. Everything else — 5xx, network failures, timeouts, and any error
    /// this cannot positively classify — is reported as transient, which
    /// preserves the historical fall-back-and-degrade behaviour for errors
    /// whose permanence is unproven.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        match self {
            Self::ApiHttpError { status, .. } => match status {
                // Request timeout and rate limiting are explicitly retryable.
                408 | 429 => true,
                // Other client errors can never succeed as-issued.
                400..=499 => false,
                // 5xx, and anything unexpected, may be temporary.
                _ => true,
            },
            _ => true,
        }
    }
}

/// Reports whether an AI error could plausibly succeed on a retry.
///
/// Errors that are not a [`ClaudeError`] cannot be classified, so they are
/// reported as transient: only a positively-identified permanent failure should
/// abort a caller that would otherwise retry or degrade gracefully.
#[must_use]
pub fn is_transient_ai_error(error: &anyhow::Error) -> bool {
    // `is_none_or` would read better but is stable only since 1.82; the
    // project's MSRV is 1.80.
    error
        .downcast_ref::<ClaudeError>()
        .map_or(true, ClaudeError::is_transient)
}

// Note: anyhow already has a blanket impl for thiserror::Error types

#[cfg(test)]
mod tests {
    use super::*;

    fn http(status: u16) -> ClaudeError {
        ClaudeError::ApiHttpError {
            status,
            body: String::from("body"),
        }
    }

    #[test]
    fn non_retryable_client_errors_are_permanent() {
        for status in [400, 401, 403, 404, 422] {
            assert!(
                !http(status).is_transient(),
                "HTTP {status} should be permanent"
            );
        }
    }

    #[test]
    fn retryable_statuses_are_transient() {
        for status in [408, 429, 500, 502, 503, 529] {
            assert!(
                http(status).is_transient(),
                "HTTP {status} should be transient"
            );
        }
    }

    #[test]
    fn unclassified_errors_default_to_transient() {
        assert!(ClaudeError::RateLimitExceeded.is_transient());
        assert!(ClaudeError::NetworkError(String::from("reset")).is_transient());
        assert!(ClaudeError::SubprocessTimeout { secs: 300 }.is_transient());
        assert!(ClaudeError::InvalidResponseFormat(String::from("not yaml")).is_transient());
        assert!(ClaudeError::ApiRequestFailed(String::from("opaque")).is_transient());
    }

    #[test]
    fn api_http_error_displays_status_and_body() {
        let rendered = http(404).to_string();
        assert!(rendered.contains("404"), "{rendered}");
        assert!(rendered.contains("body"), "{rendered}");
    }
}
