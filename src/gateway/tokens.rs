//! The `count_tokens` estimate.
//!
//! An exact count needs the upstream model's tokenizer, which no
//! OpenAI-protocol server exposes, so the gateway estimates: about four
//! characters per token over everything the upstream would see, plus a small
//! per-message and per-tool overhead and a flat cost per image. The response
//! is flagged with an `x-omni-dev-token-estimate: heuristic` header so a
//! client can tell. Clients use the count to decide when to compact, where an
//! estimate is good enough.

use serde_json::Value;

use super::anthropic::{ContentBlock, MessagesRequest, ToolResultContent};

/// Characters per token for the estimate.
const CHARS_PER_TOKEN: usize = 4;
/// Overhead tokens per message (role and framing).
const PER_MESSAGE: u64 = 4;
/// Overhead tokens per tool definition.
const PER_TOOL: u64 = 8;
/// Tokens per image (Anthropic's cost for a ~1.15 megapixel image).
const PER_IMAGE: u64 = 1_600;

/// The response header that marks an estimate.
pub(crate) const ESTIMATE_HEADER: &str = "x-omni-dev-token-estimate";

/// Estimates the input tokens of `request`.
pub(crate) fn estimate(request: &MessagesRequest) -> u64 {
    let mut chars = 0usize;
    let mut extra = 0u64;
    if let Some(system) = &request.system {
        chars += system.joined().len();
        extra += PER_MESSAGE;
    }
    for message in &request.messages {
        extra += PER_MESSAGE;
        for block in message.content.blocks().iter() {
            let (c, e) = block_cost(block);
            chars += c;
            extra += e;
        }
    }
    for tool in &request.tools {
        chars += tool.name.len()
            + tool.description.as_deref().map_or(0, str::len)
            + tool.input_schema.to_string().len()
            + tool
                .input_examples
                .iter()
                .map(|e| e.to_string().len())
                .sum::<usize>();
        extra += PER_TOOL;
    }
    chars.div_ceil(CHARS_PER_TOKEN) as u64 + extra
}

fn block_cost(block: &ContentBlock) -> (usize, u64) {
    match block {
        ContentBlock::Text { text } => (text.len(), 0),
        ContentBlock::Image { .. } => (0, PER_IMAGE),
        ContentBlock::Thinking { thinking, .. } => (thinking.len(), 0),
        ContentBlock::ToolUse { name, input, .. } => (name.len() + input.to_string().len(), 0),
        ContentBlock::ToolResult { content, .. } => match content {
            None => (0, 0),
            Some(ToolResultContent::Text(text)) => (text.len(), 0),
            Some(ToolResultContent::Blocks(blocks)) => blocks
                .iter()
                .map(block_cost)
                .fold((0, 0), |(c, e), (bc, be)| (c + bc, e + be)),
        },
        other => (serialized_len(other), 0),
    }
}

/// Length of a block's text-bearing fields, for documents and the like.
fn serialized_len(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Document {
            source,
            title,
            context,
        } => {
            let body = match source {
                super::anthropic::DocumentSource::Text { data } => data.len(),
                super::anthropic::DocumentSource::Content { content } => match content {
                    super::anthropic::DocumentContent::Text(t) => t.len(),
                    super::anthropic::DocumentContent::Blocks(b) => {
                        b.iter().map(|t| t.text.len()).sum()
                    }
                },
            };
            body + title.as_deref().map_or(0, str::len) + context.as_deref().map_or(0, str::len)
        }
        ContentBlock::SearchResult {
            source,
            title,
            content,
        } => source.len() + title.len() + content.iter().map(|t| t.text.len()).sum::<usize>(),
        ContentBlock::ToolReference { tool_name } => tool_name.len(),
        _ => 0,
    }
}

/// The `count_tokens` response body.
pub(crate) fn response_body(input_tokens: u64) -> Value {
    serde_json::json!({ "input_tokens": input_tokens })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn est(body: Value) -> u64 {
        estimate(&serde_json::from_value(body).unwrap())
    }

    #[test]
    fn text_is_about_four_characters_per_token() {
        let n =
            est(json!({"model": "m", "messages": [{"role": "user", "content": "x".repeat(400)}]}));
        assert_eq!(n, 100 + PER_MESSAGE);
    }

    #[test]
    fn every_block_kind_counts() {
        let n = est(json!({"model": "m", "system": "abcd",
        "tools": [{"name": "t", "description": "d", "input_schema": {}, "input_examples": [{}]}],
        "messages": [
            {"role": "user", "content": [
                {"type": "image", "source": {"type": "url", "url": "u"}},
                {"type": "document", "title": "T", "context": "C", "source": {"type": "text", "media_type": "text/plain", "data": "dddd"}},
                {"type": "document", "source": {"type": "content", "content": "ee"}},
                {"type": "document", "source": {"type": "content", "content": [{"type": "text", "text": "ff"}]}},
                {"type": "search_result", "source": "s", "title": "t", "content": [{"type": "text", "text": "g"}]},
                {"type": "tool_reference", "tool_name": "tool"},
                {"type": "tool_result", "tool_use_id": "a"},
                {"type": "tool_result", "tool_use_id": "a", "content": "hh"},
                {"type": "tool_result", "tool_use_id": "a", "content": [{"type": "image", "source": {"type": "url", "url": "u"}}]}
            ]},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "ii", "signature": "s"},
                {"type": "redacted_thinking", "data": "x"},
                {"type": "tool_use", "id": "a", "name": "n", "input": {}}
            ]}
        ]}));
        assert!(n > 2 * PER_IMAGE + PER_TOOL + 3 * PER_MESSAGE, "{n}");
        assert_eq!(response_body(5), json!({"input_tokens": 5}));
    }
}
