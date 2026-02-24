//! Token estimation and budget validation for AI requests.
//!
//! Provides a lightweight heuristic to estimate token counts from text and
//! validates that assembled prompts fit within a model's input context window
//! before sending API requests.

use anyhow::Result;

use crate::claude::ai::AiClientMetadata;
use crate::claude::error::ClaudeError;

/// Approximate characters per token for heuristic estimation.
///
/// Claude tokenizers average roughly 3.5 characters per token for English
/// text with code mixed in.
const CHARS_PER_TOKEN: f64 = 3.5;

/// Safety margin multiplier applied to token estimates.
///
/// Adds 10% overhead to account for tokenizer variance (special tokens,
/// whitespace handling, non-ASCII characters).
const SAFETY_MARGIN: f64 = 1.10;

/// Estimates the token count for a text string using a character-based heuristic.
///
/// Uses the approximation of 1 token per 3.5 characters with a 10% safety
/// margin. Intentionally conservative — overestimating is safer than
/// underestimating.
#[must_use]
pub(crate) fn estimate_tokens(text: &str) -> usize {
    estimate_tokens_from_char_count(text.len())
}

/// Estimates token count from a byte count without requiring a string reference.
///
/// Same heuristic as [`estimate_tokens`] but accepts a pre-computed length.
/// Useful for batch planning where file sizes are known from `fs::metadata`
/// without reading file contents into memory.
#[must_use]
pub(crate) fn estimate_tokens_from_char_count(char_count: usize) -> usize {
    let raw_estimate = char_count as f64 / CHARS_PER_TOKEN;
    (raw_estimate * SAFETY_MARGIN).ceil() as usize
}

/// Result of a token budget validation.
#[derive(Debug, Clone)]
pub(crate) struct TokenEstimate {
    /// Estimated total prompt tokens (system + user).
    pub estimated_tokens: usize,
    /// Maximum available input tokens for this model.
    pub available_tokens: usize,
    /// Utilization percentage (0.0 to 100.0+).
    pub utilization_pct: f64,
}

/// Token budget derived from model metadata.
///
/// Holds the model's context window and reserved output tokens to validate
/// that assembled prompts fit within the available input budget.
#[derive(Debug, Clone)]
pub(crate) struct TokenBudget {
    /// Model identifier (for error messages).
    model: String,
    /// Total context window (input + output).
    max_context_length: usize,
    /// Tokens reserved for the model's response.
    reserved_output_tokens: usize,
}

impl TokenBudget {
    /// Creates a token budget from AI client metadata.
    #[must_use]
    pub fn from_metadata(metadata: &AiClientMetadata) -> Self {
        Self {
            model: metadata.model.clone(),
            max_context_length: metadata.max_context_length,
            reserved_output_tokens: metadata.max_response_length,
        }
    }

    /// Returns the maximum number of input tokens available after reserving
    /// output tokens.
    #[must_use]
    pub(crate) fn available_input_tokens(&self) -> usize {
        self.max_context_length
            .saturating_sub(self.reserved_output_tokens)
    }

    /// Validates that the combined prompt fits within the model's input token
    /// budget.
    ///
    /// Returns a [`TokenEstimate`] on success, or a
    /// [`ClaudeError::PromptTooLarge`] error if the estimated tokens exceed
    /// the available budget.
    pub fn validate_prompt(&self, system_prompt: &str, user_prompt: &str) -> Result<TokenEstimate> {
        let system_tokens = estimate_tokens(system_prompt);
        let user_tokens = estimate_tokens(user_prompt);
        let estimated_tokens = system_tokens + user_tokens;
        let available = self.available_input_tokens();
        let utilization_pct = if available > 0 {
            (estimated_tokens as f64 / available as f64) * 100.0
        } else {
            f64::INFINITY
        };

        if estimated_tokens > available {
            return Err(ClaudeError::PromptTooLarge {
                estimated_tokens,
                max_tokens: available,
                model: self.model.clone(),
            }
            .into());
        }

        Ok(TokenEstimate {
            estimated_tokens,
            available_tokens: available,
            utilization_pct,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_empty_string() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn estimate_tokens_short_text() {
        // "hello" = 5 bytes -> 5/3.5 * 1.10 = 1.571... -> ceil = 2
        let tokens = estimate_tokens("hello");
        assert_eq!(tokens, 2);
    }

    #[test]
    fn estimate_tokens_scales_linearly() {
        // 700 bytes -> 700/3.5 = 200.0, * 1.10 = 220.0 -> ceil = 220
        // (f64 precision: 200.0 * 1.1 may be 220.00...001, ceil still 221 — accept either)
        let text = "a".repeat(700);
        let tokens = estimate_tokens(&text);
        assert!(tokens == 220 || tokens == 221, "got {tokens}");
    }

    #[test]
    fn estimate_tokens_includes_safety_margin() {
        // 3500 bytes -> 3500/3.5 = 1000, * 1.10 = 1100
        let text = "x".repeat(3500);
        assert_eq!(estimate_tokens(&text), 1100);
    }

    fn make_metadata(context: usize, response: usize) -> AiClientMetadata {
        AiClientMetadata {
            provider: "test".to_string(),
            model: "test-model".to_string(),
            max_context_length: context,
            max_response_length: response,
            active_beta: None,
        }
    }

    #[test]
    fn budget_validation_within_limits() {
        let metadata = make_metadata(200_000, 64_000);
        let budget = TokenBudget::from_metadata(&metadata);
        // available = 200_000 - 64_000 = 136_000
        let estimate = budget.validate_prompt("system", "user").unwrap();
        assert!(estimate.utilization_pct < 1.0);
        assert_eq!(estimate.available_tokens, 136_000);
    }

    #[test]
    fn budget_validation_exceeds_limits() {
        let metadata = make_metadata(1000, 500);
        let budget = TokenBudget::from_metadata(&metadata);
        // available = 500 tokens
        // 2000 bytes -> 2000/3.5 * 1.10 ≈ 629 tokens -> exceeds 500
        let large_text = "x".repeat(2000);
        let result = budget.validate_prompt(&large_text, "user");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Prompt too large"));
        assert!(err_msg.contains("test-model"));
    }

    #[test]
    fn budget_saturates_when_output_exceeds_context() {
        let metadata = make_metadata(100, 200);
        let budget = TokenBudget::from_metadata(&metadata);
        // available = 0 (saturating_sub)
        let result = budget.validate_prompt("a", "b");
        assert!(result.is_err());
    }

    #[test]
    fn token_estimate_utilization_percentage() {
        let metadata = make_metadata(200_000, 0);
        let budget = TokenBudget::from_metadata(&metadata);
        let estimate = budget.validate_prompt("test prompt here", "").unwrap();
        assert!(estimate.utilization_pct > 0.0);
        assert!(estimate.utilization_pct < 100.0);
    }

    #[test]
    fn estimate_tokens_from_char_count_matches_estimate_tokens() {
        let text = "hello world, this is a test string for token estimation";
        assert_eq!(
            estimate_tokens(text),
            estimate_tokens_from_char_count(text.len())
        );
    }

    #[test]
    fn estimate_tokens_from_char_count_zero() {
        assert_eq!(estimate_tokens_from_char_count(0), 0);
    }
}
