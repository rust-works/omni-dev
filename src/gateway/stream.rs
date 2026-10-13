//! Re-frames upstream stream deltas as Anthropic's streaming event sequence.
//!
//! The upstream parsers ([`chat_stream`](super::chat_stream),
//! [`responses_stream`](super::responses_stream)) reduce their protocol to
//! [`UpstreamEvent`]s; [`AnthropicStream`] turns those into
//!
//! ```text
//! message_start, ping,
//! (content_block_start, content_block_delta*, content_block_stop)*,
//! message_delta, message_stop
//! ```
//!
//! Anthropic's blocks are strictly sequential, but an upstream may interleave
//! the deltas of parallel tool calls. The builder therefore streams the
//! earliest unfinished block live and buffers the deltas of any later block
//! until every block before it has closed — always a valid sequence, and live
//! whenever the upstream is sequential (the common case).

use std::sync::Arc;

use serde_json::{json, Value};

use super::anthropic::{new_tool_use_id, StopDetails, ThinkingDisplay};
use super::completion::{stop_reason, Finish, UpstreamUsage};
use super::error::GatewayError;
use super::lower::ToolNames;
use super::signature::Signer;

/// A protocol-neutral upstream stream event. `key`s identify upstream
/// segments; each parser chooses its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UpstreamEvent {
    /// Reasoning text.
    Reasoning {
        /// The segment.
        key: String,
        /// The text.
        text: String,
    },
    /// The signature payload of a reasoning segment (the Responses API's
    /// encrypted reasoning item).
    ReasoningPayload {
        /// The segment.
        key: String,
        /// The payload.
        payload: String,
    },
    /// Answer text.
    Text {
        /// The segment.
        key: String,
        /// The text.
        text: String,
    },
    /// A tool call begins.
    ToolStart {
        /// The segment.
        key: String,
        /// The call id, if the upstream gave one.
        id: Option<String>,
        /// The upstream function name.
        name: String,
    },
    /// Tool-call arguments (a JSON fragment).
    ToolArgs {
        /// The segment.
        key: String,
        /// The fragment.
        json: String,
    },
    /// A segment is complete.
    Close {
        /// The segment.
        key: String,
    },
    /// Refusal text.
    Refusal(String),
    /// Why the upstream stopped.
    Finish(Finish),
    /// Token usage.
    Usage(UpstreamUsage),
}

/// What the builder needs to know about the request.
#[derive(Debug, Clone)]
pub(crate) struct StreamSettings {
    /// The `msg_…` id.
    pub message_id: String,
    /// The model name the client asked for.
    pub client_model: String,
    /// Whether the client asked for thinking blocks.
    pub wants_thinking: bool,
    /// How thinking is displayed.
    pub display: ThinkingDisplay,
    /// The thinking signer.
    pub signer: Arc<Signer>,
    /// The tool-name mapping.
    pub names: ToolNames,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Thinking,
    Text,
    Tool,
}

#[derive(Debug)]
struct Block {
    key: String,
    kind: BlockKind,
    /// The `content_block` of its `content_block_start`.
    start: Value,
    /// Deltas buffered while an earlier block is still open.
    pending: Vec<Value>,
    closed: bool,
    /// Thinking text so far (signed at close).
    thinking: String,
    /// Signature payload.
    payload: String,
}

/// Builds the Anthropic event sequence.
#[derive(Debug)]
pub(crate) struct AnthropicStream {
    settings: StreamSettings,
    blocks: Vec<Block>,
    /// The first block not yet stopped; the only one streamed live.
    cursor: usize,
    finish: Finish,
    usage: UpstreamUsage,
    refusal: Option<String>,
    saw_tool: bool,
    done: bool,
}

const REFUSAL_KEY: &str = "\u{0}refusal";

impl AnthropicStream {
    /// A builder for one response.
    pub(crate) fn new(settings: StreamSettings) -> Self {
        Self {
            settings,
            blocks: Vec::new(),
            cursor: 0,
            finish: Finish::Stop,
            usage: UpstreamUsage::default(),
            refusal: None,
            saw_tool: false,
            done: false,
        }
    }

    /// `message_start` and the first `ping`.
    pub(crate) fn start(&self) -> Vec<Value> {
        vec![
            json!({
                "type": "message_start",
                "message": {
                    "id": self.settings.message_id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.settings.client_model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {
                        "input_tokens": 0,
                        "output_tokens": 0,
                        "cache_creation_input_tokens": 0,
                        "cache_read_input_tokens": 0
                    }
                }
            }),
            ping(),
        ]
    }

    /// Feeds one upstream event.
    pub(crate) fn push(&mut self, event: UpstreamEvent) -> Vec<Value> {
        let mut out = Vec::new();
        match event {
            UpstreamEvent::Reasoning { key, text } => {
                if self.settings.wants_thinking && !text.is_empty() {
                    let at = self.ensure(&key, BlockKind::Thinking, &mut out);
                    self.blocks[at].thinking.push_str(&text);
                    if self.settings.display == ThinkingDisplay::Summarized {
                        self.delta(
                            at,
                            json!({"type": "thinking_delta", "thinking": text}),
                            &mut out,
                        );
                    }
                }
            }
            UpstreamEvent::ReasoningPayload { key, payload } => {
                if self.settings.wants_thinking {
                    let at = self.ensure(&key, BlockKind::Thinking, &mut out);
                    self.blocks[at].payload = payload;
                }
            }
            UpstreamEvent::Text { key, text } => self.text(&key, text, &mut out),
            UpstreamEvent::Refusal(text) => {
                self.refusal.get_or_insert_with(String::new).push_str(&text);
                self.text(REFUSAL_KEY, text, &mut out);
            }
            UpstreamEvent::ToolStart { key, id, name } => {
                self.saw_tool = true;
                let start = json!({
                    "type": "tool_use",
                    "id": id.filter(|i| !i.is_empty()).unwrap_or_else(new_tool_use_id),
                    "name": self.settings.names.original(&name),
                    "input": {}
                });
                self.open(key, BlockKind::Tool, start, &mut out);
            }
            UpstreamEvent::ToolArgs { key, json } => {
                if let Some(at) = self.find_open(&key, BlockKind::Tool) {
                    if !json.is_empty() {
                        self.delta(
                            at,
                            json!({"type": "input_json_delta", "partial_json": json}),
                            &mut out,
                        );
                    }
                } else {
                    tracing::debug!(key, "tool arguments for an unknown call; dropped");
                }
            }
            UpstreamEvent::Close { key } => {
                if let Some(at) = self.blocks.iter().rposition(|b| b.key == key && !b.closed) {
                    self.blocks[at].closed = true;
                    self.advance(&mut out);
                }
            }
            UpstreamEvent::Finish(finish) => self.finish = finish,
            UpstreamEvent::Usage(usage) => self.usage = usage,
        }
        out
    }

    /// Closes every block, then `message_delta` and `message_stop`.
    pub(crate) fn finish(&mut self) -> Vec<Value> {
        if self.done {
            return Vec::new();
        }
        self.done = true;
        let mut out = Vec::new();
        for block in &mut self.blocks {
            block.closed = true;
        }
        self.advance(&mut out);
        let refused = self.refusal.is_some() || self.finish == Finish::ContentFilter;
        let mut delta = json!({
            "stop_reason": stop_reason(&self.finish, self.saw_tool, refused),
            "stop_sequence": null
        });
        if refused {
            delta["stop_details"] = json!(StopDetails::refusal(self.refusal.clone()));
        }
        out.push(json!({
            "type": "message_delta",
            "delta": delta,
            "usage": self.usage.to_anthropic()
        }));
        out.push(json!({"type": "message_stop"}));
        out
    }

    /// An `error` event ending a failed stream.
    pub(crate) fn fail(&mut self, error: &GatewayError) -> Vec<Value> {
        self.done = true;
        vec![error.stream_event()]
    }

    fn text(&mut self, key: &str, text: String, out: &mut Vec<Value>) {
        if text.is_empty() {
            return;
        }
        let at = self.ensure(key, BlockKind::Text, out);
        self.delta(at, json!({"type": "text_delta", "text": text}), out);
    }

    /// The open block for `key` (of `kind`), opening one if there is none.
    fn ensure(&mut self, key: &str, kind: BlockKind, out: &mut Vec<Value>) -> usize {
        if let Some(at) = self.find_open(key, kind) {
            return at;
        }
        let start = match kind {
            BlockKind::Thinking => json!({"type": "thinking", "thinking": "", "signature": ""}),
            BlockKind::Text | BlockKind::Tool => json!({"type": "text", "text": ""}),
        };
        self.open(key.to_string(), kind, start, out)
    }

    fn find_open(&self, key: &str, kind: BlockKind) -> Option<usize> {
        self.blocks
            .iter()
            .rposition(|b| b.key == key && b.kind == kind && !b.closed)
    }

    fn open(&mut self, key: String, kind: BlockKind, start: Value, out: &mut Vec<Value>) -> usize {
        self.blocks.push(Block {
            key,
            kind,
            start,
            pending: Vec::new(),
            closed: false,
            thinking: String::new(),
            payload: String::new(),
        });
        let at = self.blocks.len() - 1;
        if at == self.cursor {
            out.push(self.start_event(at));
        }
        at
    }

    fn delta(&mut self, at: usize, delta: Value, out: &mut Vec<Value>) {
        if at == self.cursor {
            out.push(json!({"type": "content_block_delta", "index": at, "delta": delta}));
        } else {
            self.blocks[at].pending.push(delta);
        }
    }

    fn start_event(&self, at: usize) -> Value {
        json!({"type": "content_block_start", "index": at, "content_block": self.blocks[at].start})
    }

    /// Stops every closed block at the cursor, then starts the next one and
    /// flushes what it buffered.
    fn advance(&mut self, out: &mut Vec<Value>) {
        while self.cursor < self.blocks.len() && self.blocks[self.cursor].closed {
            let at = self.cursor;
            if self.blocks[at].kind == BlockKind::Thinking {
                let block = &self.blocks[at];
                let signed = match self.settings.display {
                    ThinkingDisplay::Summarized => block.thinking.as_str(),
                    ThinkingDisplay::Omitted => "",
                };
                let signature = self.settings.signer.sign(signed, &block.payload);
                out.push(json!({"type": "content_block_delta", "index": at,
                                "delta": {"type": "signature_delta", "signature": signature}}));
            }
            out.push(json!({"type": "content_block_stop", "index": at}));
            self.cursor += 1;
            if self.cursor < self.blocks.len() {
                let next = self.cursor;
                out.push(self.start_event(next));
                for delta in std::mem::take(&mut self.blocks[next].pending) {
                    out.push(json!({"type": "content_block_delta", "index": next, "delta": delta}));
                }
            }
        }
    }
}

/// A `ping` event.
pub(crate) fn ping() -> Value {
    json!({"type": "ping"})
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn settings(display: ThinkingDisplay) -> StreamSettings {
        StreamSettings {
            message_id: "msg_test".into(),
            client_model: "claude-test".into(),
            wants_thinking: true,
            display,
            signer: Arc::new(Signer::from_token("t")),
            names: ToolNames::default(),
        }
    }

    fn types(events: &[Value]) -> Vec<String> {
        events
            .iter()
            .map(|e| {
                let kind = e["type"].as_str().unwrap().to_string();
                match e["index"].as_u64() {
                    Some(i) => format!("{kind}#{i}"),
                    None => kind,
                }
            })
            .collect()
    }

    fn text(key: &str, t: &str) -> UpstreamEvent {
        UpstreamEvent::Text {
            key: key.into(),
            text: t.into(),
        }
    }

    #[test]
    fn interleaved_tool_calls_are_serialized() {
        let mut stream = AnthropicStream::new(settings(ThinkingDisplay::Summarized));
        let mut out = stream.start();
        for event in [
            UpstreamEvent::ToolStart {
                key: "t0".into(),
                id: Some("a".into()),
                name: "x".into(),
            },
            UpstreamEvent::ToolStart {
                key: "t1".into(),
                id: Some("b".into()),
                name: "y".into(),
            },
            UpstreamEvent::ToolArgs {
                key: "t1".into(),
                json: "{\"q\"".into(),
            },
            UpstreamEvent::ToolArgs {
                key: "t0".into(),
                json: "{}".into(),
            },
            UpstreamEvent::ToolArgs {
                key: "t1".into(),
                json: ":1}".into(),
            },
            UpstreamEvent::ToolArgs {
                key: "t9".into(),
                json: "lost".into(),
            },
            UpstreamEvent::Finish(Finish::ToolCalls),
        ] {
            out.extend(stream.push(event));
        }
        out.extend(stream.finish());
        assert_eq!(
            types(&out),
            [
                "message_start",
                "ping",
                "content_block_start#0",
                "content_block_delta#0",
                "content_block_stop#0",
                "content_block_start#1",
                "content_block_delta#1",
                "content_block_delta#1",
                "content_block_stop#1",
                "message_delta",
                "message_stop"
            ]
        );
        assert_eq!(out[6]["delta"]["partial_json"], "{\"q\"");
        assert_eq!(out[9]["delta"]["stop_reason"], "tool_use");
        assert!(stream.finish().is_empty());
    }

    #[test]
    fn thinking_is_signed_at_close_and_omitted_display_hides_it() {
        for display in [ThinkingDisplay::Summarized, ThinkingDisplay::Omitted] {
            let mut stream = AnthropicStream::new(settings(display));
            let mut out = Vec::new();
            out.extend(stream.push(UpstreamEvent::Reasoning {
                key: "r".into(),
                text: "hm".into(),
            }));
            out.extend(stream.push(UpstreamEvent::ReasoningPayload {
                key: "r".into(),
                payload: "p".into(),
            }));
            out.extend(stream.push(UpstreamEvent::Close { key: "r".into() }));
            out.extend(stream.push(text("t", "hi")));
            out.extend(stream.finish());
            let signature = out
                .iter()
                .find(|e| e["delta"]["type"] == "signature_delta")
                .unwrap()["delta"]["signature"]
                .as_str()
                .unwrap()
                .to_string();
            let signer = Signer::from_token("t");
            let thinking_deltas = out
                .iter()
                .filter(|e| e["delta"]["type"] == "thinking_delta")
                .count();
            match display {
                ThinkingDisplay::Summarized => {
                    assert_eq!(thinking_deltas, 1);
                    assert_eq!(signer.verify("hm", &signature).as_deref(), Some("p"));
                }
                ThinkingDisplay::Omitted => {
                    assert_eq!(thinking_deltas, 0);
                    assert_eq!(signer.verify("", &signature).as_deref(), Some("p"));
                }
            }
        }
    }

    #[test]
    fn unwanted_thinking_is_dropped() {
        let mut s = settings(ThinkingDisplay::Summarized);
        s.wants_thinking = false;
        let mut stream = AnthropicStream::new(s);
        assert!(stream
            .push(UpstreamEvent::Reasoning {
                key: "r".into(),
                text: "x".into()
            })
            .is_empty());
        assert!(stream
            .push(UpstreamEvent::ReasoningPayload {
                key: "r".into(),
                payload: "p".into()
            })
            .is_empty());
        assert!(stream.push(text("t", "")).is_empty());
        let out = stream.finish();
        assert_eq!(types(&out), ["message_delta", "message_stop"]);
        assert_eq!(out[0]["delta"]["stop_reason"], "end_turn");
    }

    #[test]
    fn refusals_and_usage_reach_message_delta() {
        let mut stream = AnthropicStream::new(settings(ThinkingDisplay::Summarized));
        let mut out = stream.push(UpstreamEvent::Refusal("no".into()));
        out.extend(stream.push(UpstreamEvent::Usage(UpstreamUsage {
            prompt_tokens: 10,
            completion_tokens: 2,
            cached_tokens: 4,
            reasoning_tokens: None,
        })));
        out.extend(stream.finish());
        let delta = out.iter().find(|e| e["type"] == "message_delta").unwrap();
        assert_eq!(delta["delta"]["stop_reason"], "refusal");
        assert_eq!(delta["delta"]["stop_details"]["explanation"], "no");
        assert_eq!(delta["usage"]["input_tokens"], 6);
        assert_eq!(delta["usage"]["cache_read_input_tokens"], 4);
        assert_eq!(out[1]["delta"]["text"], "no");
    }

    #[test]
    fn closing_an_unknown_key_is_harmless_and_fail_ends_the_stream() {
        let mut stream = AnthropicStream::new(settings(ThinkingDisplay::Summarized));
        assert!(stream
            .push(UpstreamEvent::Close { key: "nope".into() })
            .is_empty());
        let out = stream.fail(&GatewayError::bad_gateway("gone"));
        assert_eq!(out[0]["type"], "error");
        assert!(stream.finish().is_empty());
    }
}
