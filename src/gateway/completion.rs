//! A protocol-neutral upstream result, and its translation into an Anthropic
//! [`MessagesResponse`].
//!
//! The Chat Completions and Responses parsers both fill a [`Completion`]; the
//! stop-reason, usage, thinking-signature and tool-argument rules then live
//! here once, shared with the streaming path ([`stream`](super::stream)).

use serde_json::{json, Map, Value};

use super::anthropic::{
    new_message_id, new_tool_use_id, MessagesResponse, OutputBlock, OutputTokensDetails,
    StopDetails, StopReason, ThinkingDisplay, Usage,
};
use super::lower::ToolNames;
use super::signature::Signer;

/// Key under which unparseable tool arguments are passed to the client, so a
/// malformed call fails visibly in the client's tool validation instead of
/// being silently replaced by `{}`.
pub(crate) const INVALID_ARGUMENTS_KEY: &str = "__omni_dev_gateway_invalid_arguments";

/// Why the upstream stopped.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum Finish {
    /// A natural stop (or an unknown reason).
    #[default]
    Stop,
    /// The output cap was hit.
    Length,
    /// The model called tools.
    ToolCalls,
    /// The output was filtered.
    ContentFilter,
}

impl Finish {
    /// Maps a Chat Completions `finish_reason`.
    pub(crate) fn from_chat(reason: &str) -> Self {
        match reason {
            "length" => Self::Length,
            "tool_calls" | "function_call" => Self::ToolCalls,
            "content_filter" => Self::ContentFilter,
            _ => Self::Stop,
        }
    }
}

/// Upstream token usage, in OpenAI's terms.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct UpstreamUsage {
    /// All input tokens, cached ones included.
    pub prompt_tokens: u64,
    /// Output tokens, reasoning included.
    pub completion_tokens: u64,
    /// Input tokens served from the cache.
    pub cached_tokens: u64,
    /// Output tokens spent reasoning.
    pub reasoning_tokens: Option<u64>,
}

impl UpstreamUsage {
    /// Reads a Chat Completions `usage` object.
    pub(crate) fn from_chat(usage: &Value) -> Self {
        Self {
            prompt_tokens: number(usage, "/prompt_tokens"),
            completion_tokens: number(usage, "/completion_tokens"),
            cached_tokens: number(usage, "/prompt_tokens_details/cached_tokens"),
            reasoning_tokens: usage
                .pointer("/completion_tokens_details/reasoning_tokens")
                .and_then(Value::as_u64),
        }
    }

    /// Reads a Responses API `usage` object.
    pub(crate) fn from_responses(usage: &Value) -> Self {
        Self {
            prompt_tokens: number(usage, "/input_tokens"),
            completion_tokens: number(usage, "/output_tokens"),
            cached_tokens: number(usage, "/input_tokens_details/cached_tokens"),
            reasoning_tokens: usage
                .pointer("/output_tokens_details/reasoning_tokens")
                .and_then(Value::as_u64),
        }
    }

    /// Anthropic's usage: `input_tokens` excludes cache reads, which are
    /// reported separately; no OpenAI-protocol upstream reports cache writes.
    pub(crate) fn to_anthropic(self) -> Usage {
        Usage {
            input_tokens: self.prompt_tokens.saturating_sub(self.cached_tokens),
            output_tokens: self.completion_tokens,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: self.cached_tokens,
            output_tokens_details: self
                .reasoning_tokens
                .map(|thinking_tokens| OutputTokensDetails { thinking_tokens }),
        }
    }
}

fn number(value: &Value, pointer: &str) -> u64 {
    value.pointer(pointer).and_then(Value::as_u64).unwrap_or(0)
}

/// One reasoning segment from the upstream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Reasoning {
    /// The reasoning text.
    pub text: String,
    /// The signature payload (Responses: the encrypted reasoning item).
    pub payload: String,
}

/// A tool call from the upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UpstreamToolCall {
    /// The call id, if the upstream gave one.
    pub id: Option<String>,
    /// The upstream function name.
    pub name: String,
    /// The arguments as JSON text.
    pub arguments: String,
}

/// A complete upstream result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Completion {
    /// Reasoning segments.
    pub reasoning: Vec<Reasoning>,
    /// The answer text.
    pub text: String,
    /// Tool calls.
    pub tool_calls: Vec<UpstreamToolCall>,
    /// Why it stopped.
    pub finish: Finish,
    /// The upstream's refusal text, if it refused.
    pub refusal: Option<String>,
    /// Token usage.
    pub usage: UpstreamUsage,
}

/// What the response builder needs to know about the request.
#[derive(Debug)]
pub(crate) struct ResponseContext<'a> {
    /// The model name the client asked for (echoed back).
    pub client_model: &'a str,
    /// Whether the client asked for thinking blocks.
    pub wants_thinking: bool,
    /// How thinking is displayed.
    pub display: ThinkingDisplay,
    /// The thinking signer.
    pub signer: &'a Signer,
    /// The tool-name mapping.
    pub names: &'a ToolNames,
}

impl ResponseContext<'_> {
    /// A signed thinking block for `reasoning`, honouring the display mode.
    pub(crate) fn thinking_block(&self, reasoning: &Reasoning) -> OutputBlock {
        let thinking = if self.display == ThinkingDisplay::Omitted {
            String::new()
        } else {
            reasoning.text.clone()
        };
        let payload = super::responses::signing_payload(
            &reasoning.payload,
            &reasoning.text,
            self.display == ThinkingDisplay::Omitted,
        );
        let signature = self.signer.sign(&thinking, &payload);
        OutputBlock::Thinking {
            thinking,
            signature,
        }
    }

    /// The `tool_use` block for an upstream call.
    pub(crate) fn tool_block(&self, call: &UpstreamToolCall) -> OutputBlock {
        OutputBlock::ToolUse {
            id: call
                .id
                .clone()
                .filter(|i| !i.is_empty())
                .unwrap_or_else(new_tool_use_id),
            name: self.names.original(&call.name),
            input: parse_arguments(&call.arguments),
        }
    }
}

/// Parses tool arguments into the object Anthropic's `input` must be. Empty
/// arguments are `{}`; anything that is not a JSON object is passed under
/// [`INVALID_ARGUMENTS_KEY`].
pub(crate) fn parse_arguments(arguments: &str) -> Value {
    if arguments.trim().is_empty() {
        return Value::Object(Map::new());
    }
    match serde_json::from_str::<Value>(arguments) {
        Ok(value @ Value::Object(_)) => value,
        _ => json!({ INVALID_ARGUMENTS_KEY: arguments }),
    }
}

/// The Anthropic stop reason for an upstream finish.
pub(crate) fn stop_reason(finish: &Finish, has_tool_calls: bool, refused: bool) -> StopReason {
    if refused {
        return StopReason::Refusal;
    }
    match finish {
        Finish::ContentFilter => StopReason::Refusal,
        Finish::Length => StopReason::MaxTokens,
        Finish::ToolCalls => StopReason::ToolUse,
        Finish::Stop if has_tool_calls => StopReason::ToolUse,
        Finish::Stop => StopReason::EndTurn,
    }
}

/// Translates a complete upstream result.
pub(crate) fn to_response(completion: Completion, ctx: &ResponseContext<'_>) -> MessagesResponse {
    let mut content = Vec::new();
    if ctx.wants_thinking {
        content.extend(completion.reasoning.iter().map(|r| ctx.thinking_block(r)));
    }
    let refused = completion.refusal.is_some() || completion.finish == Finish::ContentFilter;
    let text = if completion.text.is_empty() {
        completion.refusal.clone().unwrap_or_default()
    } else {
        completion.text
    };
    if !text.is_empty() {
        content.push(OutputBlock::Text { text });
    }
    content.extend(completion.tool_calls.iter().map(|c| ctx.tool_block(c)));
    MessagesResponse {
        id: new_message_id(),
        kind: "message",
        role: "assistant",
        model: ctx.client_model.to_string(),
        content,
        stop_reason: stop_reason(
            &completion.finish,
            !completion.tool_calls.is_empty(),
            refused,
        ),
        stop_sequence: None,
        stop_details: refused.then(|| StopDetails::refusal(completion.refusal)),
        usage: completion.usage.to_anthropic(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn ctx<'a>(
        signer: &'a Signer,
        names: &'a ToolNames,
        display: ThinkingDisplay,
    ) -> ResponseContext<'a> {
        ResponseContext {
            client_model: "claude-x",
            wants_thinking: true,
            display,
            signer,
            names,
        }
    }

    #[test]
    fn usage_splits_cached_input_and_reports_reasoning() {
        let usage = UpstreamUsage::from_chat(&json!({
            "prompt_tokens": 100, "completion_tokens": 40,
            "prompt_tokens_details": {"cached_tokens": 30},
            "completion_tokens_details": {"reasoning_tokens": 25}
        }))
        .to_anthropic();
        assert_eq!(usage.input_tokens, 70);
        assert_eq!(usage.cache_read_input_tokens, 30);
        assert_eq!(usage.output_tokens, 40);
        assert_eq!(usage.output_tokens_details.unwrap().thinking_tokens, 25);

        let responses = UpstreamUsage::from_responses(&json!({
            "input_tokens": 10, "output_tokens": 5,
            "input_tokens_details": {"cached_tokens": 4},
            "output_tokens_details": {"reasoning_tokens": 2}
        }));
        assert_eq!(responses.to_anthropic().input_tokens, 6);
        assert_eq!(
            UpstreamUsage::from_chat(&json!({})).to_anthropic(),
            Usage::default()
        );
    }

    #[test]
    fn finish_reasons_map_to_stop_reasons() {
        assert_eq!(Finish::from_chat("stop"), Finish::Stop);
        assert_eq!(Finish::from_chat("length"), Finish::Length);
        assert_eq!(Finish::from_chat("tool_calls"), Finish::ToolCalls);
        assert_eq!(Finish::from_chat("function_call"), Finish::ToolCalls);
        assert_eq!(Finish::from_chat("content_filter"), Finish::ContentFilter);
        assert_eq!(Finish::from_chat("eos"), Finish::Stop);
        assert_eq!(
            stop_reason(&Finish::Stop, false, false),
            StopReason::EndTurn
        );
        assert_eq!(stop_reason(&Finish::Stop, true, false), StopReason::ToolUse);
        assert_eq!(
            stop_reason(&Finish::Length, true, false),
            StopReason::MaxTokens
        );
        assert_eq!(
            stop_reason(&Finish::ToolCalls, false, false),
            StopReason::ToolUse
        );
        assert_eq!(
            stop_reason(&Finish::ContentFilter, false, false),
            StopReason::Refusal
        );
        assert_eq!(stop_reason(&Finish::Stop, false, true), StopReason::Refusal);
    }

    #[test]
    fn arguments_parse_or_are_passed_visibly() {
        assert_eq!(parse_arguments(""), json!({}));
        assert_eq!(parse_arguments(r#"{"a":1}"#), json!({"a": 1}));
        assert_eq!(
            parse_arguments("[1]"),
            json!({INVALID_ARGUMENTS_KEY: "[1]"})
        );
        assert_eq!(
            parse_arguments("{bad"),
            json!({INVALID_ARGUMENTS_KEY: "{bad"})
        );
    }

    #[test]
    fn a_full_completion_becomes_thinking_text_and_tools() {
        let signer = Signer::from_token("t");
        let mut names = ToolNames::default();
        let wire = names.wire("Read");
        let completion = Completion {
            reasoning: vec![Reasoning {
                text: "think".into(),
                payload: String::new(),
            }],
            text: "answer".into(),
            tool_calls: vec![
                UpstreamToolCall {
                    id: Some("call_1".into()),
                    name: wire,
                    arguments: r#"{"f":"a"}"#.into(),
                },
                UpstreamToolCall {
                    id: None,
                    name: "other".into(),
                    arguments: String::new(),
                },
            ],
            finish: Finish::ToolCalls,
            refusal: None,
            usage: UpstreamUsage::default(),
        };
        let response = to_response(
            completion,
            &ctx(&signer, &names, ThinkingDisplay::Summarized),
        );
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert_eq!(response.model, "claude-x");
        let OutputBlock::Thinking {
            thinking,
            signature,
        } = &response.content[0]
        else {
            panic!()
        };
        assert_eq!(thinking, "think");
        assert!(signer.verify("think", signature).is_some());
        assert_eq!(
            response.content[1],
            OutputBlock::Text {
                text: "answer".into()
            }
        );
        let OutputBlock::ToolUse { id, name, input } = &response.content[2] else {
            panic!()
        };
        assert_eq!((id.as_str(), name.as_str()), ("call_1", "Read"));
        assert_eq!(input, &json!({"f": "a"}));
        let OutputBlock::ToolUse { id, input, .. } = &response.content[3] else {
            panic!()
        };
        assert!(id.starts_with("toolu_gw_"));
        assert_eq!(input, &json!({}));
    }

    #[test]
    fn omitted_display_signs_an_empty_block_and_unwanted_thinking_is_dropped() {
        let signer = Signer::from_token("t");
        let names = ToolNames::default();
        let completion = Completion {
            reasoning: vec![Reasoning {
                text: "secret".into(),
                payload: "p".into(),
            }],
            ..Completion::default()
        };
        let response = to_response(
            completion.clone(),
            &ctx(&signer, &names, ThinkingDisplay::Omitted),
        );
        let OutputBlock::Thinking {
            thinking,
            signature,
        } = &response.content[0]
        else {
            panic!()
        };
        assert!(thinking.is_empty());
        assert_eq!(signer.verify("", signature).as_deref(), Some("p"));

        let mut no_thinking = ctx(&signer, &names, ThinkingDisplay::Summarized);
        no_thinking.wants_thinking = false;
        assert!(to_response(completion, &no_thinking).content.is_empty());
    }

    #[test]
    fn a_refusal_carries_its_text_and_details() {
        let signer = Signer::from_token("t");
        let names = ToolNames::default();
        let completion = Completion {
            refusal: Some("I can't help with that".into()),
            ..Completion::default()
        };
        let response = to_response(
            completion,
            &ctx(&signer, &names, ThinkingDisplay::Summarized),
        );
        assert_eq!(response.stop_reason, StopReason::Refusal);
        assert_eq!(
            response.content,
            vec![OutputBlock::Text {
                text: "I can't help with that".into()
            }]
        );
        assert_eq!(
            response.stop_details.unwrap().explanation.as_deref(),
            Some("I can't help with that")
        );

        let filtered = Completion {
            finish: Finish::ContentFilter,
            ..Completion::default()
        };
        let response = to_response(filtered, &ctx(&signer, &names, ThinkingDisplay::Summarized));
        assert_eq!(response.stop_reason, StopReason::Refusal);
        assert!(response.content.is_empty());
    }
}
