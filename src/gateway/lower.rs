//! Lowers an Anthropic conversation into protocol-neutral turns.
//!
//! Chat Completions and the Responses API order a conversation the same way,
//! and differently from Anthropic: a tool's result must directly follow the
//! assistant turn that called it, and come before any user text in the same
//! Anthropic message. Lowering once into [`Turn`]s keeps that reordering, the
//! tool-name mapping and the reasoning-replay decision in one place; the
//! [`chat`](super::chat) and [`responses`](super::responses) builders only
//! choose the wire spelling.

use std::collections::HashMap;

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::anthropic::{
    join_text, ContentBlock, DocumentContent, DocumentSource, MessagesRequest, Role,
    ToolResultContent,
};
use super::error::GatewayError;
use super::policy::PolicyReport;
use super::signature::Signer;

/// OpenAI's limit on function names (Anthropic allows 128).
const MAX_TOOL_NAME: usize = 64;

/// A protocol-neutral turn.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Turn {
    /// A system instruction (top-level or mid-conversation).
    System(String),
    /// User content.
    User(Vec<UserPart>),
    /// An assistant turn.
    Assistant(AssistantTurn),
    /// A tool's result.
    ToolResult(ToolResultTurn),
}

/// A piece of user content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UserPart {
    /// Text.
    Text(String),
    /// An image, as a URL or `data:` URI.
    Image(String),
}

/// An assistant turn.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct AssistantTurn {
    /// Verified reasoning to replay.
    pub reasoning: Vec<ReplayedReasoning>,
    /// The text, blocks joined.
    pub text: String,
    /// Tool calls.
    pub tool_calls: Vec<ToolCall>,
}

/// Reasoning the gateway signed on an earlier turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReplayedReasoning {
    /// The reasoning text.
    pub text: String,
    /// The signature payload (the Responses API's encrypted reasoning).
    pub payload: String,
}

/// A tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolCall {
    /// The call id.
    pub id: String,
    /// The upstream (possibly shortened) function name.
    pub name: String,
    /// The arguments as JSON text.
    pub arguments: String,
}

/// A tool result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolResultTurn {
    /// The call it answers.
    pub call_id: String,
    /// The result as text.
    pub text: String,
    /// Images the tool returned, as URLs or `data:` URIs.
    pub images: Vec<String>,
}

/// A function definition in upstream spelling.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FunctionTool {
    /// The upstream (possibly shortened) name.
    pub name: String,
    /// The description, with any input examples appended.
    pub description: Option<String>,
    /// The JSON Schema of the arguments.
    pub parameters: Value,
    /// Strict schema adherence.
    pub strict: Option<bool>,
}

/// Maps tool names to upstream function names and back.
///
/// Names longer than OpenAI's 64-character limit (long MCP tool names reach
/// Anthropic's 128) are shortened to a prefix plus a hash, and mapped back on
/// the way out.
#[derive(Debug, Clone, Default)]
pub(crate) struct ToolNames {
    to_wire: HashMap<String, String>,
    to_original: HashMap<String, String>,
}

impl ToolNames {
    /// The upstream name for `original`, registering it.
    pub(crate) fn wire(&mut self, original: &str) -> String {
        if let Some(wire) = self.to_wire.get(original) {
            return wire.clone();
        }
        let wire = if original.chars().count() <= MAX_TOOL_NAME {
            original.to_string()
        } else {
            let digest = Sha256::digest(original.as_bytes());
            let hash = hex(&digest[..4]);
            let prefix: String = original.chars().take(MAX_TOOL_NAME - 9).collect();
            format!("{prefix}_{hash}")
        };
        self.to_wire.insert(original.to_string(), wire.clone());
        self.to_original.insert(wire.clone(), original.to_string());
        wire
    }

    /// The client's name for an upstream function name.
    pub(crate) fn original(&self, wire: &str) -> String {
        self.to_original
            .get(wire)
            .cloned()
            .unwrap_or_else(|| wire.to_string())
    }
}

/// A lowered request.
#[derive(Debug, Clone, Default)]
pub(crate) struct Lowered {
    /// The top-level system prompt.
    pub system: Option<String>,
    /// The conversation.
    pub turns: Vec<Turn>,
    /// The tool definitions.
    pub tools: Vec<FunctionTool>,
    /// The tool-name mapping.
    pub names: ToolNames,
}

/// Lowers `request`. `replay_reasoning` says whether verified thinking is sent
/// back upstream; unverified or unreplayable thinking is reported as ignored.
pub(crate) fn lower(
    request: &MessagesRequest,
    signer: &Signer,
    replay_reasoning: bool,
    report: &mut PolicyReport,
) -> Result<Lowered, GatewayError> {
    let mut names = ToolNames::default();
    let tools = request
        .tools
        .iter()
        .map(|tool| FunctionTool {
            name: names.wire(&tool.name),
            description: describe_tool(tool.description.as_deref(), &tool.input_examples),
            parameters: tool.input_schema.clone(),
            strict: tool.strict,
        })
        .collect();

    let mut turns = Vec::new();
    for (i, message) in request.messages.iter().enumerate() {
        let blocks = message.content.blocks();
        match message.role {
            Role::User => lower_user(&blocks, i, &mut turns)?,
            Role::System => turns.push(Turn::System(lower_system(&blocks, i)?)),
            Role::Assistant => {
                let turn =
                    lower_assistant(&blocks, i, signer, replay_reasoning, &mut names, report)?;
                turns.push(Turn::Assistant(turn));
            }
        }
    }
    if matches!(turns.last(), Some(Turn::Assistant(_))) {
        // Anthropic continues a trailing assistant turn (prefill); OpenAI-protocol
        // upstreams treat it as a finished turn and answer after it.
        report.ignore("assistant_prefill");
    }
    Ok(Lowered {
        system: request
            .system
            .as_ref()
            .map(super::anthropic::SystemPrompt::joined),
        turns,
        tools,
        names,
    })
}

fn describe_tool(description: Option<&str>, examples: &[Value]) -> Option<String> {
    if examples.is_empty() {
        return description.map(str::to_string);
    }
    let rendered: Vec<String> = examples.iter().map(Value::to_string).collect();
    let examples = format!("Example inputs:\n{}", rendered.join("\n"));
    Some(match description {
        Some(d) if !d.is_empty() => format!("{d}\n\n{examples}"),
        _ => examples,
    })
}

fn lower_user(
    blocks: &[ContentBlock],
    i: usize,
    turns: &mut Vec<Turn>,
) -> Result<(), GatewayError> {
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => turns.push(Turn::ToolResult(lower_tool_result(
                tool_use_id,
                content.as_ref(),
                *is_error,
            ))),
            ContentBlock::ToolUse { .. } => {
                return Err(GatewayError::invalid(format!(
                    "messages[{i}]: a user message cannot contain tool_use"
                )))
            }
            ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking {} => {
                return Err(GatewayError::invalid(format!(
                    "messages[{i}]: a user message cannot contain thinking"
                )))
            }
            other => parts.extend(content_part(other)),
        }
    }
    if !parts.is_empty() {
        turns.push(Turn::User(parts));
    }
    Ok(())
}

fn lower_system(blocks: &[ContentBlock], i: usize) -> Result<String, GatewayError> {
    let mut texts = Vec::new();
    for block in blocks {
        match content_part(block) {
            Some(UserPart::Text(text)) => texts.push(text),
            // An unknown block the operator chose to ignore (`warn`) is dropped.
            None if matches!(block, ContentBlock::Unknown) => {}
            _ => {
                return Err(GatewayError::invalid(format!(
                    "messages[{i}]: a system message may contain only text"
                )))
            }
        }
    }
    Ok(join_text(texts.iter().map(String::as_str)))
}

fn lower_assistant(
    blocks: &[ContentBlock],
    i: usize,
    signer: &Signer,
    replay_reasoning: bool,
    names: &mut ToolNames,
    report: &mut PolicyReport,
) -> Result<AssistantTurn, GatewayError> {
    let mut turn = AssistantTurn::default();
    let mut texts = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text } => texts.push(text.as_str()),
            ContentBlock::Thinking {
                thinking,
                signature,
            } => match signer.verify(thinking, signature) {
                Some(payload) if replay_reasoning => turn.reasoning.push(ReplayedReasoning {
                    text: super::responses::replay_text(thinking, &payload),
                    payload,
                }),
                Some(_) => report.ignore("thinking (not replayable on this upstream)"),
                None => report.ignore("thinking (signature not from this gateway)"),
            },
            ContentBlock::RedactedThinking {} | ContentBlock::Unknown => {}
            ContentBlock::ToolUse { id, name, input } => turn.tool_calls.push(ToolCall {
                id: id.clone(),
                name: names.wire(name),
                arguments: input.to_string(),
            }),
            _ => {
                return Err(GatewayError::invalid(format!(
                    "messages[{i}]: an assistant message may contain only text, thinking and \
                     tool_use blocks"
                )))
            }
        }
    }
    turn.text = join_text(texts.into_iter());
    Ok(turn)
}

fn lower_tool_result(
    call_id: &str,
    content: Option<&ToolResultContent>,
    is_error: bool,
) -> ToolResultTurn {
    let mut texts = Vec::new();
    let mut images = Vec::new();
    match content {
        None => {}
        Some(ToolResultContent::Text(text)) => texts.push(text.clone()),
        Some(ToolResultContent::Blocks(blocks)) => {
            for block in blocks {
                match content_part(block) {
                    Some(UserPart::Text(text)) => texts.push(text),
                    Some(UserPart::Image(url)) => images.push(url),
                    None => {}
                }
            }
        }
    }
    let mut text = join_text(texts.iter().map(String::as_str));
    if is_error {
        text = format!("[tool error] {text}");
    }
    ToolResultTurn {
        call_id: call_id.to_string(),
        text,
        images,
    }
}

/// Renders a user-side block (text, image, document, search result, tool
/// reference) as a [`UserPart`]; `None` for blocks that have no user-side
/// rendering (and for an unknown block the operator chose to ignore).
fn content_part(block: &ContentBlock) -> Option<UserPart> {
    match block {
        ContentBlock::Text { text } => Some(UserPart::Text(text.clone())),
        ContentBlock::Image { source } => Some(UserPart::Image(source.to_url())),
        ContentBlock::Document {
            source,
            title,
            context,
        } => {
            let body = match source {
                DocumentSource::Text { data } => data.clone(),
                DocumentSource::Content { content } => match content {
                    DocumentContent::Text(text) => text.clone(),
                    DocumentContent::Blocks(blocks) => {
                        join_text(blocks.iter().map(|b| b.text.as_str()))
                    }
                },
            };
            let header = format!(
                "[Document{}]",
                title.as_ref().map(|t| format!(": {t}")).unwrap_or_default()
            );
            let parts = [Some(header), context.clone(), Some(body)];
            Some(UserPart::Text(
                parts.into_iter().flatten().collect::<Vec<_>>().join("\n"),
            ))
        }
        ContentBlock::SearchResult {
            source,
            title,
            content,
        } => Some(UserPart::Text(format!(
            "[Search result: {title} ({source})]\n{}",
            join_text(content.iter().map(|b| b.text.as_str()))
        ))),
        ContentBlock::ToolReference { tool_name } => {
            Some(UserPart::Text(format!("[Tool reference: {tool_name}]")))
        }
        ContentBlock::Thinking { .. }
        | ContentBlock::RedactedThinking {}
        | ContentBlock::ToolUse { .. }
        | ContentBlock::ToolResult { .. }
        | ContentBlock::Unknown => None,
    }
}

/// Lowercase hex of `bytes`.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// A stable `prompt_cache_key` for a request: a hash of the model, system
/// prompt and tools, so requests that share a cacheable prefix are routed to
/// the same cache.
pub(crate) fn prompt_cache_key(model: &str, lowered: &Lowered) -> String {
    let mut hasher = Sha256::new();
    hasher.update(model.as_bytes());
    hasher.update([0]);
    hasher.update(lowered.system.as_deref().unwrap_or("").as_bytes());
    for tool in &lowered.tools {
        hasher.update([0]);
        hasher.update(tool.name.as_bytes());
        hasher.update(tool.parameters.to_string().as_bytes());
    }
    let digest = hasher.finalize();
    format!("ogw-{}", hex(&digest[..16]))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(body: Value) -> MessagesRequest {
        serde_json::from_value(body).unwrap()
    }

    fn lower_ok(body: Value) -> (Lowered, PolicyReport) {
        let mut report = PolicyReport::default();
        let signer = Signer::from_token("t");
        let lowered = lower(&request(body), &signer, true, &mut report).unwrap();
        (lowered, report)
    }

    #[test]
    fn tool_results_come_before_user_text_in_the_same_message() {
        let (lowered, _) = lower_ok(json!({
            "model": "m",
            "system": "sys",
            "messages": [
                {"role": "user", "content": "go"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "calling"},
                    {"type": "tool_use", "id": "t1", "name": "ls", "input": {"p": 1}}
                ]},
                {"role": "user", "content": [
                    {"type": "text", "text": "and then"},
                    {"type": "tool_result", "tool_use_id": "t1", "content": [
                        {"type": "text", "text": "a"},
                        {"type": "image", "source": {"type": "url", "url": "https://i"}}
                    ], "is_error": true}
                ]}
            ]
        }));
        assert_eq!(lowered.system.as_deref(), Some("sys"));
        assert_eq!(lowered.turns.len(), 4);
        let Turn::Assistant(assistant) = &lowered.turns[1] else {
            panic!()
        };
        assert_eq!(assistant.text, "calling");
        assert_eq!(assistant.tool_calls[0].arguments, r#"{"p":1}"#);
        let Turn::ToolResult(result) = &lowered.turns[2] else {
            panic!()
        };
        assert_eq!(result.text, "[tool error] a");
        assert_eq!(result.images, vec!["https://i".to_string()]);
        assert_eq!(
            lowered.turns[3],
            Turn::User(vec![UserPart::Text("and then".into())])
        );
    }

    #[test]
    fn verified_thinking_replays_and_foreign_thinking_is_reported() {
        let signer = Signer::from_token("t");
        let good = signer.sign("mine", "");
        let (lowered, report) = lower_ok(json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "q"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "mine", "signature": good},
                    {"type": "thinking", "thinking": "theirs", "signature": "EqQB"},
                    {"type": "redacted_thinking", "data": "x"},
                    {"type": "text", "text": "a"}
                ]},
                {"role": "user", "content": "more"}
            ]
        }));
        let Turn::Assistant(turn) = &lowered.turns[1] else {
            panic!()
        };
        assert_eq!(turn.reasoning.len(), 1);
        assert_eq!(turn.reasoning[0].text, "mine");
        assert!(report
            .ignored
            .contains("thinking (signature not from this gateway)"));

        let mut report = PolicyReport::default();
        let req = request(json!({"model": "m", "messages": [
            {"role": "assistant", "content": [{"type": "thinking", "thinking": "mine", "signature": good}]}
        ]}));
        let lowered = lower(&req, &signer, false, &mut report).unwrap();
        let Turn::Assistant(turn) = &lowered.turns[0] else {
            panic!()
        };
        assert!(turn.reasoning.is_empty());
        assert!(report
            .ignored
            .contains("thinking (not replayable on this upstream)"));
        assert!(report.ignored.contains("assistant_prefill"));
    }

    #[test]
    fn documents_search_results_and_references_render_as_text() {
        let (lowered, _) = lower_ok(
            json!({"model": "m", "messages": [{"role": "user", "content": [
                {"type": "document", "title": "T", "context": "C", "source": {"type": "text", "media_type": "text/plain", "data": "body"}},
                {"type": "document", "source": {"type": "content", "content": [{"type": "text", "text": "x"}, {"type": "text", "text": "y"}]}},
                {"type": "document", "source": {"type": "content", "content": "z"}},
                {"type": "search_result", "source": "s", "title": "t", "content": [{"type": "text", "text": "r"}]},
                {"type": "tool_reference", "tool_name": "Grep"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AA"}}
            ]}]}),
        );
        let Turn::User(parts) = &lowered.turns[0] else {
            panic!()
        };
        assert_eq!(parts[0], UserPart::Text("[Document: T]\nC\nbody".into()));
        assert_eq!(parts[1], UserPart::Text("[Document]\nx\n\ny".into()));
        assert_eq!(parts[2], UserPart::Text("[Document]\nz".into()));
        assert_eq!(parts[3], UserPart::Text("[Search result: t (s)]\nr".into()));
        assert_eq!(parts[4], UserPart::Text("[Tool reference: Grep]".into()));
        assert_eq!(parts[5], UserPart::Image("data:image/png;base64,AA".into()));
    }

    #[test]
    fn mid_conversation_system_messages_are_turns() {
        let (lowered, _) = lower_ok(json!({"model": "m", "messages": [
            {"role": "user", "content": "q"},
            {"role": "system", "content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]}
        ]}));
        assert_eq!(lowered.turns[1], Turn::System("a\n\nb".into()));
    }

    #[test]
    fn misplaced_blocks_are_rejected() {
        let signer = Signer::from_token("t");
        for body in [
            json!({"model": "m", "messages": [{"role": "user", "content": [{"type": "tool_use", "id": "a", "name": "b", "input": {}}]}]}),
            json!({"model": "m", "messages": [{"role": "user", "content": [{"type": "thinking", "thinking": "a", "signature": "b"}]}]}),
            json!({"model": "m", "messages": [{"role": "assistant", "content": [{"type": "image", "source": {"type": "url", "url": "u"}}]}]}),
            json!({"model": "m", "messages": [{"role": "system", "content": [{"type": "image", "source": {"type": "url", "url": "u"}}]}]}),
        ] {
            let mut report = PolicyReport::default();
            assert!(lower(&request(body), &signer, true, &mut report).is_err());
        }
    }

    #[test]
    fn long_tool_names_are_shortened_and_mapped_back() {
        let long = format!("mcp__{}", "x".repeat(100));
        let (mut lowered, _) = lower_ok(
            json!({"model": "m", "messages": [{"role": "user", "content": "q"}],
            "tools": [
                {"name": long, "input_schema": {"type": "object"}, "input_examples": [{"a": 1}]},
                {"name": "short", "description": "d", "input_schema": {}, "input_examples": [{"b": 2}], "strict": true},
                {"name": "plain", "description": "p", "input_schema": {}}
            ]}),
        );
        let wire = lowered.tools[0].name.clone();
        assert_eq!(wire.chars().count(), MAX_TOOL_NAME);
        assert_eq!(lowered.names.original(&wire), long);
        assert_eq!(lowered.names.wire(&long), wire);
        assert_eq!(lowered.names.original("unknown"), "unknown");
        assert_eq!(
            lowered.tools[0].description.as_deref(),
            Some("Example inputs:\n{\"a\":1}")
        );
        assert_eq!(
            lowered.tools[1].description.as_deref(),
            Some("d\n\nExample inputs:\n{\"b\":2}")
        );
        assert_eq!(lowered.tools[1].strict, Some(true));
        assert_eq!(lowered.tools[2].description.as_deref(), Some("p"));
    }

    #[test]
    fn cache_keys_are_stable_and_input_sensitive() {
        let (a, _) = lower_ok(
            json!({"model": "m", "system": "s", "messages": [{"role": "user", "content": "q"}]}),
        );
        let (b, _) = lower_ok(
            json!({"model": "m", "system": "s2", "messages": [{"role": "user", "content": "q"}]}),
        );
        assert_eq!(prompt_cache_key("m", &a), prompt_cache_key("m", &a));
        assert_ne!(prompt_cache_key("m", &a), prompt_cache_key("m", &b));
        assert!(prompt_cache_key("m", &a).starts_with("ogw-"));
    }

    #[test]
    fn an_empty_tool_result_is_empty_text() {
        let turn = lower_tool_result("c", None, false);
        assert_eq!(turn.text, "");
    }
}
