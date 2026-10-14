//! Upstream protocols and the per-provider quirks of the OpenAI protocol.
//!
//! "OpenAI-compatible" servers disagree on exactly the details a translator
//! trips over: which token-limit field they take, how reasoning is controlled
//! and returned, whether streamed usage must be requested, and how many stop
//! sequences fit. Each difference is one method on [`Flavor`], so a quirk is
//! named once instead of being an `if` scattered through the translators.

use std::fmt;

use anyhow::{bail, Result};

use super::anthropic::Effort;
use crate::claude::ai::openai::uses_max_completion_tokens;

/// The protocol the gateway speaks to its upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum UpstreamProtocol {
    /// OpenAI Chat Completions (`/v1/chat/completions`), the default.
    #[default]
    Chat,
    /// OpenAI Responses API (`/v1/responses`).
    Responses,
    /// Anthropic Messages (`/v1/messages`): passthrough with model mapping and
    /// an auth swap, no translation.
    Anthropic,
    /// Native Bedrock Claude InvokeModel, authenticated with a bearer token.
    Bedrock,
}

impl UpstreamProtocol {
    /// The configuration spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Responses => "responses",
            Self::Anthropic => "anthropic",
            Self::Bedrock => "bedrock",
        }
    }

    /// Parses the configuration spelling.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "chat" => Ok(Self::Chat),
            "responses" => Ok(Self::Responses),
            "anthropic" => Ok(Self::Anthropic),
            "bedrock" => Ok(Self::Bedrock),
            other => {
                bail!("unknown upstream protocol `{other}` (expected chat, responses, anthropic or bedrock)")
            }
        }
    }
}

impl fmt::Display for UpstreamProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The OpenAI-protocol provider family, for its quirks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Flavor {
    /// Ollama, LM Studio, vLLM, OpenRouter and other OpenAI-compatible
    /// servers: the conservative common subset.
    #[default]
    Generic,
    /// OpenAI itself.
    Openai,
    /// Fireworks AI.
    Fireworks,
}

/// How an upstream takes a reasoning request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReasoningStyle {
    /// `reasoning_effort` only.
    Effort,
    /// Fireworks: an Anthropic-style `thinking` object *or* `reasoning_effort`,
    /// never both (sending both is a validation error).
    ThinkingOrEffort,
}

impl Flavor {
    /// The configuration spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::Openai => "openai",
            Self::Fireworks => "fireworks",
        }
    }

    /// Parses the configuration spelling.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "generic" => Ok(Self::Generic),
            "openai" => Ok(Self::Openai),
            "fireworks" => Ok(Self::Fireworks),
            other => {
                bail!("unknown upstream flavor `{other}` (expected generic, openai or fireworks)")
            }
        }
    }

    /// The Chat Completions token-limit field for `model`. OpenAI deprecated
    /// `max_tokens` (GPT-5 and o-series reject it); Fireworks treats the two as
    /// aliases and other servers only know `max_tokens`. Exactly one is sent.
    pub(crate) fn max_tokens_field(self, model: &str) -> &'static str {
        match self {
            Self::Openai => "max_completion_tokens",
            Self::Generic if uses_max_completion_tokens(model) => "max_completion_tokens",
            Self::Generic | Self::Fireworks => "max_tokens",
        }
    }

    /// Whether `model` accepts `temperature`/`top_p` (GPT-5 and o-series
    /// accept only the defaults).
    pub(crate) fn accepts_sampling(self, model: &str) -> bool {
        !uses_max_completion_tokens(model)
    }

    /// Whether the upstream takes `top_k`.
    pub(crate) fn accepts_top_k(self) -> bool {
        !matches!(self, Self::Openai)
    }

    /// Whether streamed usage must be requested with
    /// `stream_options.include_usage` (Fireworks sends it by default).
    pub(crate) fn requests_stream_usage(self) -> bool {
        !matches!(self, Self::Fireworks)
    }

    /// The upstream's limit on stop sequences, if it has one.
    pub(crate) fn stop_limit(self) -> Option<usize> {
        match self {
            Self::Openai | Self::Fireworks => Some(4),
            Self::Generic => None,
        }
    }

    /// Whether the upstream takes `prompt_cache_key`.
    pub(crate) fn takes_prompt_cache_key(self) -> bool {
        matches!(self, Self::Openai | Self::Fireworks)
    }

    /// How reasoning is requested.
    pub(crate) fn reasoning_style(self) -> ReasoningStyle {
        match self {
            Self::Fireworks => ReasoningStyle::ThinkingOrEffort,
            Self::Generic | Self::Openai => ReasoningStyle::Effort,
        }
    }

    /// Whether prior reasoning is replayed as `reasoning_content` on assistant
    /// messages. Fireworks requires it for multi-turn tool use; OpenAI's chat
    /// API returns no reasoning text to replay.
    pub(crate) fn replays_reasoning(self) -> bool {
        !matches!(self, Self::Openai)
    }

    /// Whether to ask the upstream to error on an over-long prompt instead of
    /// silently truncating it (Fireworks' `context_length_exceeded_behavior`
    /// defaults to `truncate`; Anthropic errors).
    pub(crate) fn sets_context_overflow_error(self) -> bool {
        matches!(self, Self::Fireworks)
    }

    /// Whether `json_schema` response formats are sent `strict`.
    pub(crate) fn strict_json_schema(self) -> bool {
        matches!(self, Self::Openai)
    }

    /// The `reasoning_effort` values the upstream accepts, lowest first.
    pub(crate) fn effort_levels(self) -> &'static [Effort] {
        match self {
            Self::Openai => &[Effort::Low, Effort::Medium, Effort::High, Effort::Xhigh],
            Self::Fireworks => &Effort::ALL,
            Self::Generic => &[Effort::Low, Effort::Medium, Effort::High],
        }
    }
}

impl fmt::Display for Flavor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Clamps `effort` to the highest supported level not above it (or the lowest
/// supported level when every level is above it).
pub(crate) fn clamp_effort(effort: Effort, levels: &[Effort]) -> Option<Effort> {
    levels
        .iter()
        .copied()
        .filter(|l| *l <= effort)
        .max()
        .or_else(|| levels.iter().copied().min())
}

/// Buckets an extended-thinking token budget into an effort level.
pub(crate) fn effort_for_budget(budget_tokens: u64) -> Effort {
    match budget_tokens {
        0..4_096 => Effort::Low,
        4_096..16_384 => Effort::Medium,
        16_384..32_768 => Effort::High,
        _ => Effort::Xhigh,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn spellings_round_trip() {
        for p in [
            UpstreamProtocol::Chat,
            UpstreamProtocol::Responses,
            UpstreamProtocol::Anthropic,
            UpstreamProtocol::Bedrock,
        ] {
            assert_eq!(UpstreamProtocol::parse(&p.to_string()).unwrap(), p);
        }
        for f in [Flavor::Generic, Flavor::Openai, Flavor::Fireworks] {
            assert_eq!(Flavor::parse(&f.to_string()).unwrap(), f);
        }
        assert!(UpstreamProtocol::parse("grpc").is_err());
        assert!(Flavor::parse("bedrock").is_err());
        assert_eq!(Flavor::parse(" OpenAI ").unwrap(), Flavor::Openai);
    }

    #[test]
    fn token_limit_field_is_per_flavor_and_model() {
        assert_eq!(
            Flavor::Openai.max_tokens_field("gpt-4o"),
            "max_completion_tokens"
        );
        assert_eq!(Flavor::Generic.max_tokens_field("llama3"), "max_tokens");
        assert_eq!(
            Flavor::Generic.max_tokens_field("gpt-5-mini"),
            "max_completion_tokens"
        );
        assert_eq!(Flavor::Fireworks.max_tokens_field("gpt-5"), "max_tokens");
    }

    #[test]
    fn quirk_table() {
        assert!(!Flavor::Openai.accepts_top_k());
        assert!(Flavor::Fireworks.accepts_top_k());
        assert!(!Flavor::Fireworks.requests_stream_usage());
        assert!(Flavor::Generic.requests_stream_usage());
        assert_eq!(Flavor::Generic.stop_limit(), None);
        assert_eq!(Flavor::Fireworks.stop_limit(), Some(4));
        assert!(Flavor::Fireworks.takes_prompt_cache_key());
        assert!(!Flavor::Generic.takes_prompt_cache_key());
        assert_eq!(
            Flavor::Fireworks.reasoning_style(),
            ReasoningStyle::ThinkingOrEffort
        );
        assert_eq!(Flavor::Openai.reasoning_style(), ReasoningStyle::Effort);
        assert!(!Flavor::Openai.replays_reasoning());
        assert!(Flavor::Fireworks.sets_context_overflow_error());
        assert!(Flavor::Openai.strict_json_schema());
        assert!(!Flavor::Generic.accepts_sampling("o1-mini"));
        assert!(Flavor::Generic.accepts_sampling("qwen3"));
        assert_eq!(Flavor::Fireworks.effort_levels().len(), 5);
    }

    #[test]
    fn effort_clamps_down_to_a_supported_level() {
        let openai = Flavor::Openai.effort_levels();
        assert_eq!(clamp_effort(Effort::Max, openai), Some(Effort::Xhigh));
        assert_eq!(clamp_effort(Effort::Medium, openai), Some(Effort::Medium));
        let generic = Flavor::Generic.effort_levels();
        assert_eq!(clamp_effort(Effort::Xhigh, generic), Some(Effort::High));
        assert_eq!(
            clamp_effort(Effort::Low, &[Effort::High]),
            Some(Effort::High)
        );
        assert_eq!(clamp_effort(Effort::Low, &[]), None);
    }

    #[test]
    fn budgets_bucket_into_efforts() {
        assert_eq!(effort_for_budget(1_024), Effort::Low);
        assert_eq!(effort_for_budget(8_000), Effort::Medium);
        assert_eq!(effort_for_budget(20_000), Effort::High);
        assert_eq!(effort_for_budget(64_000), Effort::Xhigh);
    }
}
