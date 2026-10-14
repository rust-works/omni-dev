//! The `count_tokens` estimate.
//!
//! An exact count needs the upstream model's tokenizer, which no
//! OpenAI-protocol server exposes, so the gateway estimates: about four
//! characters per token over everything the upstream would see, plus a small
//! per-message and per-tool overhead and a flat cost per image. The response
//! is flagged with an `x-omni-dev-token-estimate: heuristic` header so a
//! client can tell. Clients use the count to decide when to compact, where an
//! estimate is good enough.

use serde_json::{Map, Value};

use super::anthropic::{ContentBlock, MessagesRequest, ToolResultContent};
use super::error::GatewayError;

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

/// Estimates native content without imposing the translation wire types.
/// Known blocks use the same costs as translated requests. Opaque native
/// blocks (for example base64 PDF documents) use their serialized size; this
/// is deliberately approximate and does not parse or fetch document content.
pub(crate) fn estimate_native(body: &Map<String, Value>) -> Result<u64, GatewayError> {
    if body.get("model").and_then(Value::as_str).is_none() {
        return Err(GatewayError::invalid("model: string required"));
    }
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .filter(|messages| !messages.is_empty())
        .ok_or_else(|| GatewayError::invalid("messages: nonempty array required"))?;
    let mut chars = 0;
    let mut extra = 0;
    if let Some(system) = body.get("system").filter(|v| !v.is_null()) {
        let (c, e) = native_content_cost(system)?;
        chars += c;
        extra += e + PER_MESSAGE;
    }
    for message in messages {
        if !matches!(
            message.get("role").and_then(Value::as_str),
            Some("user" | "assistant" | "system")
        ) {
            return Err(GatewayError::invalid("messages: invalid role"));
        }
        let content = message
            .get("content")
            .ok_or_else(|| GatewayError::invalid("messages: content required"))?;
        let (c, e) = native_content_cost(content)?;
        chars += c;
        extra += e + PER_MESSAGE;
    }
    if let Some(tools) = body.get("tools") {
        let tools = tools
            .as_array()
            .ok_or_else(|| GatewayError::invalid("tools: array required"))?;
        for tool in tools {
            chars += tool.to_string().len();
            extra += PER_TOOL;
        }
    }
    Ok(chars.div_ceil(CHARS_PER_TOKEN) as u64 + extra)
}

fn native_content_cost(content: &Value) -> Result<(usize, u64), GatewayError> {
    match content {
        Value::String(text) => Ok((text.len(), 0)),
        Value::Array(blocks) => {
            let mut chars = 0;
            let mut extra = 0;
            for block in blocks {
                if !block.is_object() {
                    return Err(GatewayError::invalid("content blocks must be objects"));
                }
                let (c, e) = native_block_cost(block);
                chars += c;
                extra += e;
            }
            Ok((chars, extra))
        }
        _ => Err(GatewayError::invalid("content: string or array required")),
    }
}

fn native_block_cost(block: &Value) -> (usize, u64) {
    let opaque_cost = || (block.to_string().len(), 0);
    if block.get("type").and_then(Value::as_str) == Some("tool_result") {
        // Recurse over the raw content before translation deserialization can
        // discard unknown blocks or encrypted thinking, including nested results.
        return match block.get("content") {
            None | Some(Value::Null) => (0, 0),
            Some(content) => native_content_cost(content).unwrap_or_else(|_| opaque_cost()),
        };
    }
    match serde_json::from_value::<ContentBlock>(block.clone()) {
        Ok(ContentBlock::Unknown | ContentBlock::RedactedThinking {}) | Err(_) => opaque_cost(),
        Ok(known) => block_cost(&known),
    }
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
    fn native_text_and_image_use_the_existing_heuristic() {
        let body = json!({"model":"native", "system":"abcd", "messages":[
            {"role":"user","content":[
                {"type":"text","text":"x".repeat(400)},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"AA=="}}
            ]}
        ]});
        assert_eq!(
            estimate_native(body.as_object().unwrap()).unwrap(),
            est(body)
        );
    }

    #[test]
    fn native_opaque_documents_and_nested_tool_results_are_counted() {
        let document = json!({"type":"document","source":{
            "type":"base64","media_type":"application/pdf","data":"synthetic-base64".repeat(100)
        }});
        let mut body = json!({"model":"native","messages":[{"role":"user","content":[document]}]});
        let direct = estimate_native(body.as_object().unwrap()).unwrap();
        assert!(direct > PER_MESSAGE + 100);
        body["messages"][0]["content"] = json!([{
            "type":"tool_result","tool_use_id":"tool_1","content":[document]
        }]);
        assert!(estimate_native(body.as_object().unwrap()).unwrap() >= direct);
    }

    #[test]
    fn native_unknown_and_server_tool_use_blocks_use_serialized_size() {
        for block in [
            json!({"type":"synthetic_native_block","data":"synthetic-payload".repeat(100)}),
            json!({"type":"server_tool_use","id":"synthetic_id","name":"synthetic_tool",
                "input":{"query":"synthetic-query".repeat(100)}}),
        ] {
            let chars = block.to_string().len();
            let body =
                json!({"model":"native","messages":[{"role":"assistant","content":[block]}]});
            assert_eq!(
                estimate_native(body.as_object().unwrap()).unwrap(),
                chars.div_ceil(CHARS_PER_TOKEN) as u64 + PER_MESSAGE
            );
            // Translation still drops unsupported block types.
            assert_eq!(est(body), PER_MESSAGE);
        }
    }

    #[test]
    fn native_redacted_thinking_counts_raw_data_without_changing_translation() {
        let block =
            json!({"type":"redacted_thinking","data":"synthetic-encrypted-data".repeat(100)});
        let chars = block.to_string().len();
        let body = json!({"model":"native","messages":[{"role":"assistant","content":[block]}]});
        assert_eq!(
            estimate_native(body.as_object().unwrap()).unwrap(),
            chars.div_ceil(CHARS_PER_TOKEN) as u64 + PER_MESSAGE
        );
        assert_eq!(est(body), PER_MESSAGE);
    }

    #[test]
    fn native_nested_tool_results_preserve_opaque_payloads_and_known_costs() {
        let opaque = [
            json!({"type":"synthetic_native_block","data":"synthetic-payload".repeat(100)}),
            json!({"type":"server_tool_use","id":"synthetic_id","name":"synthetic_tool",
                "input":{"query":"synthetic-query".repeat(100)}}),
            json!({"type":"redacted_thinking","data":"synthetic-encrypted-data".repeat(100)}),
        ];
        let text = "abcd".repeat(100);
        let chars = opaque
            .iter()
            .map(|block| block.to_string().len())
            .sum::<usize>()
            + text.len();
        let mut content = json!([
            opaque[0], opaque[1], opaque[2],
            {"type":"text","text":text},
            {"type":"image","source":{"type":"base64","media_type":"image/png","data":"AA=="}}
        ]);
        for depth in 1..=3 {
            content =
                json!([{"type":"tool_result","tool_use_id":"synthetic_tool","content":content}]);
            let body = json!({"model":"native","messages":[{"role":"user","content":content}]});
            assert_eq!(
                estimate_native(body.as_object().unwrap()).unwrap(),
                chars.div_ceil(CHARS_PER_TOKEN) as u64 + PER_IMAGE + PER_MESSAGE,
                "nesting depth {depth}"
            );
            assert_eq!(
                est(body),
                text.len().div_ceil(CHARS_PER_TOKEN) as u64 + PER_IMAGE + PER_MESSAGE
            );
        }
    }

    #[test]
    fn native_estimates_reject_malformed_conversation_shapes() {
        for body in [
            json!({"model":"native"}),
            json!({"model":"native","messages":[]}),
            json!({"model":"native","messages":[{"role":"invalid","content":"x"}]}),
            json!({"model":"native","messages":[{"role":"user"}]}),
            json!({"model":"native","messages":[{"role":"user","content":42}]}),
            json!({"model":"native","messages":[{"role":"user","content":[42]}]}),
        ] {
            assert!(estimate_native(body.as_object().unwrap()).is_err());
        }
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
