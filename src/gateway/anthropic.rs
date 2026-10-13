//! Anthropic Messages API wire types: the inbound request, the response, and
//! the error envelope.
//!
//! The request is parsed in two steps. [`policy`](super::policy) first walks
//! the raw JSON and classes every field and nested value as translated,
//! ignored or rejected; only then is it deserialized into
//! [`MessagesRequest`], whose types name just the fields the gateway
//! translates. So a field missing from these types is never silently dropped:
//! the policy has already rejected it or recorded it as ignored.
//!
//! Field names follow the Anthropic SDK's generated types as of 2026-10-14
//! (`anthropic-sdk-python`, `src/anthropic/types/`), which the drift check
//! ([`drift`](super::drift)) re-reads weekly.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A `POST /v1/messages` (or `/v1/messages/count_tokens`) body, restricted to
/// the fields the gateway translates.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct MessagesRequest {
    /// The client's model name, mapped to an upstream model.
    pub model: String,
    /// Output cap. Required by `/v1/messages`, absent from `count_tokens`.
    #[serde(default)]
    pub max_tokens: Option<u64>,
    /// The conversation.
    pub messages: Vec<InputMessage>,
    /// The system prompt.
    #[serde(default)]
    pub system: Option<SystemPrompt>,
    /// Custom stop sequences.
    #[serde(default)]
    pub stop_sequences: Vec<String>,
    /// Whether to stream the response as server-sent events.
    #[serde(default)]
    pub stream: bool,
    /// Legacy sampling temperature (removed from the current API, still sent
    /// by older clients).
    #[serde(default)]
    pub temperature: Option<f64>,
    /// Legacy nucleus sampling.
    #[serde(default)]
    pub top_p: Option<f64>,
    /// Legacy top-k sampling.
    #[serde(default)]
    pub top_k: Option<u64>,
    /// Client tool definitions.
    #[serde(default)]
    pub tools: Vec<Tool>,
    /// How the model may use the tools.
    #[serde(default)]
    pub tool_choice: Option<ToolChoice>,
    /// Extended-thinking configuration.
    #[serde(default)]
    pub thinking: Option<ThinkingConfig>,
    /// Effort and structured-output configuration.
    #[serde(default)]
    pub output_config: Option<OutputConfig>,
}

/// The `system` field: a string or a list of text blocks.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub(crate) enum SystemPrompt {
    /// A plain string.
    Text(String),
    /// Text blocks (each may carry `cache_control`, read by the policy).
    Blocks(Vec<TextBlock>),
}

impl SystemPrompt {
    /// The prompt as one string, blocks joined by a blank line.
    pub(crate) fn joined(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Blocks(blocks) => join_text(blocks.iter().map(|b| b.text.as_str())),
        }
    }
}

/// A bare `{"type": "text", "text": ...}` block.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TextBlock {
    /// The text.
    pub text: String,
}

/// Joins text parts with a blank line, skipping empty ones.
pub(crate) fn join_text<'a>(parts: impl Iterator<Item = &'a str>) -> String {
    parts
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// A message's role. `system` is the 2026 mid-conversation system message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Role {
    /// The user turn.
    User,
    /// The assistant turn.
    Assistant,
    /// A mid-conversation system instruction.
    System,
}

/// One input message.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct InputMessage {
    /// Who said it.
    pub role: Role,
    /// What was said.
    pub content: MessageContent,
}

/// A message's content: a string or a list of blocks.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub(crate) enum MessageContent {
    /// Shorthand for one text block.
    Text(String),
    /// Content blocks.
    Blocks(Vec<ContentBlock>),
}

impl MessageContent {
    /// The content as a list of blocks.
    pub(crate) fn into_blocks(self) -> Vec<ContentBlock> {
        match self {
            Self::Text(text) => vec![ContentBlock::Text { text }],
            Self::Blocks(blocks) => blocks,
        }
    }
}

/// The input content blocks the gateway translates. Every other block type is
/// rejected (or, for `redacted_thinking`, ignored) by the policy before this
/// type is parsed.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ContentBlock {
    /// Text.
    Text {
        /// The text.
        text: String,
    },
    /// An image.
    Image {
        /// Where the image comes from.
        source: ImageSource,
    },
    /// A plain-text or inline-content document (PDF and file sources are
    /// rejected by the policy).
    Document {
        /// The document body.
        source: DocumentSource,
        /// Optional title.
        #[serde(default)]
        title: Option<String>,
        /// Optional context about the document.
        #[serde(default)]
        context: Option<String>,
    },
    /// A search result supplied by the client.
    SearchResult {
        /// Where the result came from.
        source: String,
        /// Its title.
        title: String,
        /// Its text.
        content: Vec<TextBlock>,
    },
    /// A thinking block from an earlier assistant turn.
    Thinking {
        /// The reasoning text.
        thinking: String,
        /// The signature that binds it to its producer.
        signature: String,
    },
    /// An encrypted thinking block (ignored: no upstream can read it).
    RedactedThinking {},
    /// A tool call from an earlier assistant turn.
    ToolUse {
        /// The call id.
        id: String,
        /// The tool name.
        name: String,
        /// The arguments.
        input: Value,
    },
    /// The result of a tool call.
    ToolResult {
        /// The call it answers.
        tool_use_id: String,
        /// The result content.
        #[serde(default)]
        content: Option<ToolResultContent>,
        /// Whether the tool failed.
        #[serde(default)]
        is_error: bool,
    },
    /// A reference expanding a deferred tool (tool search).
    ToolReference {
        /// The referenced tool.
        tool_name: String,
    },
}

/// A tool result's content: a string or blocks.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub(crate) enum ToolResultContent {
    /// Plain text.
    Text(String),
    /// Text, image, document, search-result or tool-reference blocks.
    Blocks(Vec<ContentBlock>),
}

/// An image source.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ImageSource {
    /// Inline base64 data.
    Base64 {
        /// `image/jpeg`, `image/png`, `image/gif` or `image/webp`.
        media_type: String,
        /// The encoded bytes.
        data: String,
    },
    /// A URL the upstream fetches.
    Url {
        /// The image URL.
        url: String,
    },
}

impl ImageSource {
    /// The OpenAI `image_url.url` value: the URL, or a `data:` URI.
    pub(crate) fn to_url(&self) -> String {
        match self {
            Self::Base64 { media_type, data } => format!("data:{media_type};base64,{data}"),
            Self::Url { url } => url.clone(),
        }
    }
}

/// A document source the gateway can render as text.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum DocumentSource {
    /// `text/plain` data.
    Text {
        /// The text.
        data: String,
    },
    /// Inline content: a string or text blocks.
    Content {
        /// The content.
        content: DocumentContent,
    },
}

/// Inline document content.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub(crate) enum DocumentContent {
    /// A string.
    Text(String),
    /// Text blocks (image blocks are rejected by the policy).
    Blocks(Vec<TextBlock>),
}

/// A client tool definition (`type` absent or `custom`).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Tool {
    /// The tool name.
    pub name: String,
    /// What the tool does.
    #[serde(default)]
    pub description: Option<String>,
    /// JSON Schema of the tool input.
    pub input_schema: Value,
    /// Strict schema adherence.
    #[serde(default)]
    pub strict: Option<bool>,
    /// Example inputs, appended to the description upstream.
    #[serde(default)]
    pub input_examples: Vec<Value>,
}

/// How the model may use tools.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ToolChoice {
    /// The model decides.
    Auto {
        /// At most one tool call.
        #[serde(default)]
        disable_parallel_tool_use: bool,
    },
    /// The model must call some tool.
    Any {
        /// At most one tool call.
        #[serde(default)]
        disable_parallel_tool_use: bool,
    },
    /// The model must call the named tool.
    Tool {
        /// The tool to call.
        name: String,
        /// At most one tool call.
        #[serde(default)]
        disable_parallel_tool_use: bool,
    },
    /// The model must not call tools.
    None {},
}

impl ToolChoice {
    /// Whether the client limited the model to one tool call.
    pub(crate) fn disables_parallel(&self) -> bool {
        match self {
            Self::Auto {
                disable_parallel_tool_use,
            }
            | Self::Any {
                disable_parallel_tool_use,
            }
            | Self::Tool {
                disable_parallel_tool_use,
                ..
            } => *disable_parallel_tool_use,
            Self::None {} => false,
        }
    }
}

/// Whether reasoning text is returned or only signed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ThinkingDisplay {
    /// Reasoning text is returned (the default).
    #[default]
    Summarized,
    /// The block is returned empty, with a signature.
    Omitted,
}

/// Extended-thinking configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ThinkingConfig {
    /// Thinking with a token budget.
    Enabled {
        /// The reasoning budget (Anthropic's minimum is 1024).
        budget_tokens: u64,
        /// Display mode.
        #[serde(default)]
        display: Option<ThinkingDisplay>,
    },
    /// No thinking.
    Disabled {},
    /// The model decides how much to think.
    Adaptive {
        /// Display mode.
        #[serde(default)]
        display: Option<ThinkingDisplay>,
    },
    /// Thinking only between tool calls.
    BetweenTools {},
}

impl ThinkingConfig {
    /// Whether the client asked to receive thinking blocks.
    pub(crate) fn wants_thinking(&self) -> bool {
        !matches!(self, Self::Disabled {})
    }

    /// The display mode (default `summarized`).
    pub(crate) fn display(&self) -> ThinkingDisplay {
        match self {
            Self::Enabled { display, .. } | Self::Adaptive { display } => {
                display.unwrap_or_default()
            }
            Self::Disabled {} | Self::BetweenTools {} => ThinkingDisplay::Summarized,
        }
    }
}

/// `output_config.effort`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Effort {
    /// Least reasoning.
    Low,
    /// The middle setting.
    Medium,
    /// More reasoning.
    High,
    /// More than high.
    Xhigh,
    /// The most reasoning.
    Max,
}

impl Effort {
    /// Every level, lowest first.
    pub(crate) const ALL: [Self; 5] = [Self::Low, Self::Medium, Self::High, Self::Xhigh, Self::Max];

    /// The wire spelling.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// Parses the wire spelling.
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|e| e.as_str() == raw)
    }
}

/// `output_config`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OutputConfig {
    /// Reasoning effort.
    #[serde(default)]
    pub effort: Option<Effort>,
    /// Structured output format.
    #[serde(default)]
    pub format: Option<JsonOutputFormat>,
}

/// `output_config.format`: `{"type": "json_schema", "schema": {...}}`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct JsonOutputFormat {
    /// The JSON Schema the output must satisfy.
    pub schema: Value,
}

// ── Response ────────────────────────────────────────────────────────

/// Why generation stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StopReason {
    /// A natural stopping point.
    EndTurn,
    /// The output cap was reached.
    MaxTokens,
    /// A custom stop sequence matched (never reported: upstreams do not say
    /// which sequence matched).
    #[allow(dead_code)] // Part of the wire enum; unrepresentable upstream.
    StopSequence,
    /// The model called tools.
    ToolUse,
    /// The upstream refused or filtered the output.
    Refusal,
}

/// An output content block.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum OutputBlock {
    /// Text.
    Text {
        /// The text.
        text: String,
    },
    /// Reasoning, signed by the gateway.
    Thinking {
        /// The reasoning text (empty when display is `omitted`).
        thinking: String,
        /// The gateway's signature.
        signature: String,
    },
    /// A tool call.
    ToolUse {
        /// The call id.
        id: String,
        /// The tool name.
        name: String,
        /// The arguments.
        input: Value,
    },
}

/// `stop_details` for a refusal.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct StopDetails {
    /// Always `refusal`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// Anthropic's refusal category; upstreams report none.
    pub category: Option<String>,
    /// The upstream's refusal text, when it gave one.
    pub explanation: Option<String>,
}

impl StopDetails {
    /// A refusal with an optional explanation.
    pub(crate) fn refusal(explanation: Option<String>) -> Self {
        Self {
            kind: "refusal",
            category: None,
            explanation,
        }
    }
}

/// `usage.output_tokens_details`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct OutputTokensDetails {
    /// Output tokens spent on reasoning.
    pub thinking_tokens: u64,
}

/// Token usage in Anthropic's shape.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub(crate) struct Usage {
    /// Uncached input tokens.
    pub input_tokens: u64,
    /// Output tokens, reasoning included.
    pub output_tokens: u64,
    /// Tokens written to the cache (no upstream reports this).
    pub cache_creation_input_tokens: u64,
    /// Tokens read from the cache.
    pub cache_read_input_tokens: u64,
    /// Reasoning share of the output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens_details: Option<OutputTokensDetails>,
}

/// A non-streaming `/v1/messages` response.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct MessagesResponse {
    /// `msg_…` id.
    pub id: String,
    /// Always `message`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// Always `assistant`.
    pub role: &'static str,
    /// The model the client asked for.
    pub model: String,
    /// The content.
    pub content: Vec<OutputBlock>,
    /// Why generation stopped.
    pub stop_reason: StopReason,
    /// The matched stop sequence (always `null`, see [`StopReason::StopSequence`]).
    pub stop_sequence: Option<String>,
    /// Refusal details.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_details: Option<StopDetails>,
    /// Token usage.
    pub usage: Usage,
}

/// A fresh `msg_…` id.
pub(crate) fn new_message_id() -> String {
    format!("msg_gw_{}", random_suffix())
}

/// A fresh `toolu_…` id, for an upstream tool call that carried none.
pub(crate) fn new_tool_use_id() -> String {
    format!("toolu_gw_{}", random_suffix())
}

/// 24 random alphanumeric characters.
fn random_suffix() -> String {
    use rand::RngExt;
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    (0..24)
        .map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char)
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_full_request_parses() {
        let request: MessagesRequest = serde_json::from_value(json!({
            "model": "claude-opus-4-7",
            "max_tokens": 1024,
            "system": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}],
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "hmm", "signature": "s"},
                    {"type": "tool_use", "id": "t1", "name": "ls", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "x"}
                ]},
                {"role": "system", "content": "be brief"}
            ],
            "tools": [{"name": "ls", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "any", "disable_parallel_tool_use": true},
            "thinking": {"type": "enabled", "budget_tokens": 2048, "display": "omitted"},
            "output_config": {"effort": "xhigh"}
        }))
        .unwrap();
        assert_eq!(request.system.unwrap().joined(), "a\n\nb");
        assert_eq!(request.messages.len(), 4);
        assert_eq!(request.messages[3].role, Role::System);
        assert!(request.tool_choice.unwrap().disables_parallel());
        let thinking = request.thinking.unwrap();
        assert_eq!(thinking.display(), ThinkingDisplay::Omitted);
        assert!(thinking.wants_thinking());
        assert_eq!(request.output_config.unwrap().effort, Some(Effort::Xhigh));
    }

    #[test]
    fn image_sources_render_as_urls() {
        let b64 = ImageSource::Base64 {
            media_type: "image/png".into(),
            data: "AAAA".into(),
        };
        assert_eq!(b64.to_url(), "data:image/png;base64,AAAA");
        let url = ImageSource::Url {
            url: "https://x/y.png".into(),
        };
        assert_eq!(url.to_url(), "https://x/y.png");
    }

    #[test]
    fn effort_round_trips_its_wire_spelling() {
        for effort in Effort::ALL {
            assert_eq!(Effort::parse(effort.as_str()), Some(effort));
        }
        assert_eq!(Effort::parse("extreme"), None);
    }

    #[test]
    fn thinking_display_defaults_and_disabled_wants_none() {
        let adaptive: ThinkingConfig = serde_json::from_value(json!({"type": "adaptive"})).unwrap();
        assert_eq!(adaptive.display(), ThinkingDisplay::Summarized);
        let disabled: ThinkingConfig = serde_json::from_value(json!({"type": "disabled"})).unwrap();
        assert!(!disabled.wants_thinking());
        assert_eq!(disabled.display(), ThinkingDisplay::Summarized);
        let between: ThinkingConfig =
            serde_json::from_value(json!({"type": "between_tools"})).unwrap();
        assert!(between.wants_thinking());
    }

    #[test]
    fn the_response_serializes_in_anthropic_shape() {
        let response = MessagesResponse {
            id: "msg_1".into(),
            kind: "message",
            role: "assistant",
            model: "claude-x".into(),
            content: vec![OutputBlock::Text { text: "hi".into() }],
            stop_reason: StopReason::Refusal,
            stop_sequence: None,
            stop_details: Some(StopDetails::refusal(None)),
            usage: Usage {
                input_tokens: 1,
                output_tokens: 2,
                ..Usage::default()
            },
        };
        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value["type"], "message");
        assert_eq!(value["stop_reason"], "refusal");
        assert_eq!(value["stop_details"]["type"], "refusal");
        assert_eq!(value["stop_sequence"], Value::Null);
        assert_eq!(value["usage"]["cache_read_input_tokens"], 0);
        assert!(value["usage"].get("output_tokens_details").is_none());
        assert_eq!(value["content"][0]["type"], "text");
    }

    #[test]
    fn ids_are_prefixed_and_unique() {
        let (a, b) = (new_message_id(), new_message_id());
        assert!(a.starts_with("msg_gw_"));
        assert_ne!(a, b);
        assert!(new_tool_use_id().starts_with("toolu_gw_"));
    }

    #[test]
    fn content_shorthand_becomes_one_text_block() {
        let blocks = MessageContent::Text("hi".into()).into_blocks();
        assert!(matches!(&blocks[..], [ContentBlock::Text { text }] if text == "hi"));
    }
}
