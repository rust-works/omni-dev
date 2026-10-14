//! Parses a streaming Chat Completions reply into [`UpstreamEvent`]s.
//!
//! Each `data:` chunk carries a `choices[0].delta` with any of `content`,
//! `reasoning_content` (or `reasoning`), `refusal` and `tool_calls`, a
//! `finish_reason` on the last content chunk, and — after it, before
//! `[DONE]` — a `usage` chunk with empty `choices` (OpenAI sends it when
//! `stream_options.include_usage` asks; Fireworks always does). Reasoning and
//! text alternate as segments; each switch closes the previous segment so the
//! Anthropic blocks come out in upstream order.

use std::collections::HashSet;

use serde_json::Value;

use super::chat::reasoning_text;
use super::completion::{Finish, UpstreamUsage};
use super::error::{upstream_message, GatewayError};
use super::stream::UpstreamEvent;
use super::think_tags::{Piece, ThinkTagSplitter};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Segment {
    None,
    Reasoning(usize),
    Text(usize),
}

impl Segment {
    fn key(self) -> Option<String> {
        match self {
            Self::None => None,
            Self::Reasoning(n) => Some(format!("reasoning{n}")),
            Self::Text(n) => Some(format!("text{n}")),
        }
    }
}

/// The Chat Completions stream parser.
#[derive(Debug)]
pub(crate) struct ChatStreamParser {
    splitter: Option<ThinkTagSplitter>,
    segment: Segment,
    next_segment: usize,
    tools: HashSet<String>,
    /// The call the last tool delta belonged to.
    last_tool: Option<String>,
}

impl ChatStreamParser {
    /// A parser; `think_tags` splits a leading inline `<think>` block out of
    /// `content`.
    pub(crate) fn new(think_tags: bool) -> Self {
        Self {
            splitter: think_tags.then(ThinkTagSplitter::default),
            segment: Segment::None,
            next_segment: 0,
            tools: HashSet::new(),
            last_tool: None,
        }
    }

    /// Parses one `data:` payload. `Ok(None)` marks `[DONE]`.
    pub(crate) fn push(&mut self, data: &str) -> Result<Option<Vec<UpstreamEvent>>, GatewayError> {
        let data = data.trim();
        if data == "[DONE]" {
            return Ok(None);
        }
        if data.is_empty() {
            return Ok(Some(Vec::new()));
        }
        let chunk: Value = serde_json::from_str(data).map_err(|e| {
            GatewayError::bad_gateway(format!("unreadable upstream stream chunk: {e}"))
        })?;
        if chunk.get("error").is_some_and(|e| !e.is_null()) {
            return Err(GatewayError::bad_gateway(format!(
                "upstream stream error: {}",
                upstream_message(data)
            )));
        }
        let mut out = Vec::new();
        if let Some(choice) = chunk.pointer("/choices/0") {
            let delta = choice.get("delta").unwrap_or(&Value::Null);
            if let Some(text) = reasoning_text(delta).filter(|t| !t.is_empty()) {
                self.reasoning(text.to_string(), &mut out);
            }
            if let Some(content) = delta.get("content").and_then(Value::as_str) {
                self.content(content, &mut out);
            }
            if let Some(refusal) = delta
                .get("refusal")
                .and_then(Value::as_str)
                .filter(|r| !r.is_empty())
            {
                out.push(UpstreamEvent::Refusal(refusal.to_string()));
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                self.tool_calls(calls, &mut out);
            }
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                out.push(UpstreamEvent::Finish(Finish::from_chat(reason)));
            }
        }
        if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
            out.push(UpstreamEvent::Usage(UpstreamUsage::from_chat(usage)));
        }
        Ok(Some(out))
    }

    /// Flushes buffered content at the end of the stream.
    pub(crate) fn finish(&mut self) -> Vec<UpstreamEvent> {
        let mut out = Vec::new();
        if let Some(mut splitter) = self.splitter.take() {
            for piece in splitter.finish() {
                self.piece(piece, &mut out);
            }
        }
        self.close_segment(&mut out);
        out
    }

    fn content(&mut self, content: &str, out: &mut Vec<UpstreamEvent>) {
        if content.is_empty() {
            return;
        }
        let pieces = match self.splitter.as_mut() {
            Some(splitter) => splitter.push(content),
            None => vec![Piece::Text(content.to_string())],
        };
        for piece in pieces {
            self.piece(piece, out);
        }
    }

    fn piece(&mut self, piece: Piece, out: &mut Vec<UpstreamEvent>) {
        match piece {
            Piece::Reasoning(text) => self.reasoning(text, out),
            Piece::Text(text) => self.text(text, out),
        }
    }

    fn reasoning(&mut self, text: String, out: &mut Vec<UpstreamEvent>) {
        if !matches!(self.segment, Segment::Reasoning(_)) {
            self.close_segment(out);
            self.segment = Segment::Reasoning(self.bump());
        }
        if let Some(key) = self.segment.key() {
            out.push(UpstreamEvent::Reasoning { key, text });
        }
    }

    fn text(&mut self, text: String, out: &mut Vec<UpstreamEvent>) {
        if !matches!(self.segment, Segment::Text(_)) {
            self.close_segment(out);
            self.segment = Segment::Text(self.bump());
        }
        if let Some(key) = self.segment.key() {
            out.push(UpstreamEvent::Text { key, text });
        }
    }

    fn tool_calls(&mut self, calls: &[Value], out: &mut Vec<UpstreamEvent>) {
        self.close_segment(out);
        for (position, call) in calls.iter().enumerate() {
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .filter(|i| !i.is_empty());
            let key = tool_key(call, id, position, self.last_tool.as_deref());
            if self.tools.insert(key.clone()) {
                out.push(UpstreamEvent::ToolStart {
                    key: key.clone(),
                    id: id.map(str::to_string),
                    name: call
                        .pointer("/function/name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                });
            }
            self.last_tool = Some(key.clone());
            if let Some(arguments) = call.pointer("/function/arguments").and_then(Value::as_str) {
                out.push(UpstreamEvent::ToolArgs {
                    key,
                    json: arguments.to_string(),
                });
            }
        }
    }

    fn close_segment(&mut self, out: &mut Vec<UpstreamEvent>) {
        if let Some(key) = self.segment.key() {
            out.push(UpstreamEvent::Close { key });
        }
        self.segment = Segment::None;
    }

    fn bump(&mut self) -> usize {
        self.next_segment += 1;
        self.next_segment
    }
}

/// The segment key of a streamed tool-call delta.
///
/// OpenAI sends an `index` on every delta, which identifies the call. Some
/// servers omit it: they either send each call whole (each with its own `id`)
/// or continue the previous call with an id-less fragment. So without an
/// index, a new `id` starts a call and an id-less fragment continues the last
/// one; only the very first id-less delta falls back to its position.
fn tool_key(call: &Value, id: Option<&str>, position: usize, last: Option<&str>) -> String {
    if let Some(index) = call.get("index").and_then(Value::as_u64) {
        return format!("tool{index}");
    }
    match (id, last) {
        (Some(id), _) => format!("tool-id:{id}"),
        (None, Some(last)) => last.to_string(),
        (None, None) => format!("tool{position}"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn segments_alternate_and_close() {
        let mut parser = ChatStreamParser::new(false);
        let mut out = Vec::new();
        for chunk in [
            r#"{"choices":[{"delta":{"role":"assistant","reasoning_content":"a"}}]}"#,
            r#"{"choices":[{"delta":{"content":"b"}}]}"#,
            r#"{"choices":[{"delta":{"reasoning":"c"}}]}"#,
            r#"{"choices":[{"delta":{"content":""}}]}"#,
        ] {
            out.extend(parser.push(chunk).unwrap().unwrap());
        }
        out.extend(parser.finish());
        assert_eq!(
            out,
            vec![
                UpstreamEvent::Reasoning {
                    key: "reasoning1".into(),
                    text: "a".into()
                },
                UpstreamEvent::Close {
                    key: "reasoning1".into()
                },
                UpstreamEvent::Text {
                    key: "text2".into(),
                    text: "b".into()
                },
                UpstreamEvent::Close {
                    key: "text2".into()
                },
                UpstreamEvent::Reasoning {
                    key: "reasoning3".into(),
                    text: "c".into()
                },
                UpstreamEvent::Close {
                    key: "reasoning3".into()
                },
            ]
        );
    }

    #[test]
    fn inline_think_tags_become_reasoning() {
        let mut parser = ChatStreamParser::new(true);
        let mut out = Vec::new();
        for chunk in [
            r#"{"choices":[{"delta":{"content":"<thi"}}]}"#,
            r#"{"choices":[{"delta":{"content":"nk>x</think>y"}}]}"#,
        ] {
            out.extend(parser.push(chunk).unwrap().unwrap());
        }
        out.extend(parser.finish());
        assert!(out.contains(&UpstreamEvent::Reasoning {
            key: "reasoning1".into(),
            text: "x".into()
        }));
        assert!(out.contains(&UpstreamEvent::Text {
            key: "text2".into(),
            text: "y".into()
        }));

        let mut unterminated = ChatStreamParser::new(true);
        unterminated
            .push(r#"{"choices":[{"delta":{"content":"<th"}}]}"#)
            .unwrap();
        assert_eq!(
            unterminated.finish(),
            vec![
                UpstreamEvent::Text {
                    key: "text1".into(),
                    text: "<th".into()
                },
                UpstreamEvent::Close {
                    key: "text1".into()
                },
            ]
        );
    }

    #[test]
    fn tool_calls_finish_usage_and_done() {
        let mut parser = ChatStreamParser::new(false);
        let mut out = parser
            .push(r#"{"choices":[{"delta":{"content":"ok","tool_calls":[{"index":0,"id":"c","function":{"name":"ls","arguments":""}}]}}]}"#)
            .unwrap()
            .unwrap();
        out.extend(parser.push(r#"{"choices":[{"delta":{"tool_calls":[{"function":{"arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#).unwrap().unwrap());
        out.extend(
            parser
                .push(r#"{"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":1}}"#)
                .unwrap()
                .unwrap(),
        );
        assert_eq!(
            out,
            vec![
                UpstreamEvent::Text {
                    key: "text1".into(),
                    text: "ok".into()
                },
                UpstreamEvent::Close {
                    key: "text1".into()
                },
                UpstreamEvent::ToolStart {
                    key: "tool0".into(),
                    id: Some("c".into()),
                    name: "ls".into()
                },
                UpstreamEvent::ToolArgs {
                    key: "tool0".into(),
                    json: String::new()
                },
                UpstreamEvent::ToolArgs {
                    key: "tool0".into(),
                    json: "{}".into()
                },
                UpstreamEvent::Finish(Finish::ToolCalls),
                UpstreamEvent::Usage(UpstreamUsage {
                    prompt_tokens: 3,
                    completion_tokens: 1,
                    cached_tokens: 0,
                    reasoning_tokens: None
                }),
            ]
        );
        assert!(parser.push(" [DONE] ").unwrap().is_none());
        assert!(parser.push("").unwrap().unwrap().is_empty());
    }

    #[test]
    fn tool_calls_without_an_index_are_keyed_by_id() {
        let mut parser = ChatStreamParser::new(false);
        let mut out = Vec::new();
        for chunk in [
            r#"{"choices":[{"delta":{"tool_calls":[{"id":"a","function":{"name":"x","arguments":"{\"p\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"function":{"arguments":"1}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"id":"b","function":{"name":"y","arguments":"{}"}}]}}]}"#,
        ] {
            out.extend(parser.push(chunk).unwrap().unwrap());
        }
        let starts: Vec<_> = out
            .iter()
            .filter_map(|e| match e {
                UpstreamEvent::ToolStart { key, name, .. } => Some((key.as_str(), name.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(starts, [("tool-id:a", "x"), ("tool-id:b", "y")]);
        assert!(out.contains(&UpstreamEvent::ToolArgs {
            key: "tool-id:a".into(),
            json: "1}".into()
        }));

        let mut anonymous = ChatStreamParser::new(false);
        let out = anonymous
            .push(r#"{"choices":[{"delta":{"tool_calls":[{"function":{"name":"z","arguments":"{}"}}]}}]}"#)
            .unwrap()
            .unwrap();
        assert!(matches!(&out[0], UpstreamEvent::ToolStart { key, .. } if key == "tool0"));
    }

    #[test]
    fn refusals_errors_and_garbage() {
        let mut parser = ChatStreamParser::new(false);
        let out = parser
            .push(r#"{"choices":[{"delta":{"refusal":"no"}}]}"#)
            .unwrap()
            .unwrap();
        assert_eq!(out, vec![UpstreamEvent::Refusal("no".into())]);
        let err = parser
            .push(r#"{"error":{"message":"overloaded"}}"#)
            .unwrap_err();
        assert!(err.message.contains("overloaded"));
        assert!(parser.push("{not json").is_err());
    }
}
