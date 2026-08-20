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

    /// Returns `true` when the endpoint rejected the structured-output field
    /// `output_config` itself, rather than anything about the request's
    /// content.
    ///
    /// Anthropic's Messages API takes `output_config.format` on models the
    /// registry flags via `supports_structured_output`, but a gateway named by
    /// `ANTHROPIC_BEDROCK_BASE_URL` may not pass the field through, and answers
    /// a strict-schema rejection — `{"message": "output_config.format: Extra
    /// inputs are not permitted"}` — rather than a model or content error
    /// (issue #1561). That is a property of the *endpoint*, which the
    /// per-model registry gate cannot know, so callers use this to drop to the
    /// YAML response path instead of failing the run.
    ///
    /// The field-name match is the load-bearing half, keeping this narrow
    /// enough that no ordinary `400` (bad model, oversized prompt, malformed
    /// body) can trip it. `422` is accepted alongside `400` because
    /// pydantic-style gateways conventionally use it for exactly this
    /// unrecognised-field rejection.
    #[must_use]
    pub fn is_structured_output_rejection(&self) -> bool {
        match self {
            Self::ApiHttpError {
                status: 400 | 422,
                body,
            } => body.to_ascii_lowercase().contains("output_config"),
            _ => false,
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

/// Reports whether an AI error is an endpoint rejecting the `output_config`
/// structured-output field — see
/// [`ClaudeError::is_structured_output_rejection`].
///
/// Errors that are not a [`ClaudeError`] cannot be classified, so they are
/// reported as `false`: only a positively-identified rejection may make a
/// caller degrade to the YAML path.
#[must_use]
pub fn is_structured_output_rejection(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ClaudeError>()
        .is_some_and(ClaudeError::is_structured_output_rejection)
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

    fn http_body(status: u16, body: &str) -> ClaudeError {
        ClaudeError::ApiHttpError {
            status,
            body: String::from(body),
        }
    }

    /// The reported gateway rejection (#1561) — and its `422` variant — are
    /// recognised, in whatever case the endpoint spells the field.
    #[test]
    fn output_config_rejections_are_recognised() {
        for status in [400, 422] {
            assert!(
                http_body(
                    status,
                    r#"{"message":"output_config.format: Extra inputs are not permitted"}"#
                )
                .is_structured_output_rejection(),
                "HTTP {status} naming output_config should be recognised"
            );
        }
        assert!(http_body(400, "OUTPUT_CONFIG is not supported").is_structured_output_rejection());
    }

    /// The predicate must stay narrow: an ordinary `4xx`, and a `5xx` that
    /// happens to echo the field name, are not endpoint rejections of it.
    #[test]
    fn other_failures_are_not_output_config_rejections() {
        assert!(!http_body(400, "max_tokens: must be positive").is_structured_output_rejection());
        assert!(!http_body(404, "output_config").is_structured_output_rejection());
        assert!(!http_body(500, "output_config exploded").is_structured_output_rejection());
        assert!(
            !ClaudeError::ApiRequestFailed(String::from("output_config"))
                .is_structured_output_rejection()
        );
    }

    /// An error that is not a [`ClaudeError`] cannot be classified, so the
    /// `anyhow` helper reports `false` rather than degrading on a guess.
    #[test]
    fn anyhow_helper_classifies_only_claude_errors() {
        let rejection: anyhow::Error = http_body(400, "output_config.format: nope").into();
        assert!(is_structured_output_rejection(&rejection));

        let other: anyhow::Error = http_body(400, "bad request").into();
        assert!(!is_structured_output_rejection(&other));

        let foreign = anyhow::anyhow!("output_config");
        assert!(!is_structured_output_rejection(&foreign));
    }
}
