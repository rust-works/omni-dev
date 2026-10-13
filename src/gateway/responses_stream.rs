//! Parses a streaming Responses API reply into [`UpstreamEvent`]s.
//!
//! The Responses API streams typed events keyed by `output_index` (and
//! `content_index` / `summary_index` within an item). Each output item maps to
//! one segment: `r<n>` reasoning, `t<n>.<m>` text, `f<n>` function call; the
//! item's `done` event closes it and, for reasoning, supplies the encrypted
//! content the thinking signature carries.

use std::collections::HashMap;

use serde_json::Value;

use super::completion::UpstreamUsage;
use super::error::{upstream_message, GatewayError};
use super::responses::{finish_for, ReasoningPayload};
use super::stream::UpstreamEvent;

/// The Responses API stream parser.
#[derive(Debug, Default)]
pub(crate) struct ResponsesStreamParser {
    /// The last summary index seen per reasoning item, to separate summaries.
    summary_index: HashMap<u64, u64>,
    saw_function_call: bool,
}

fn index(data: &Value, field: &str) -> u64 {
    data.get(field).and_then(Value::as_u64).unwrap_or(0)
}

fn text<'a>(data: &'a Value, field: &str) -> &'a str {
    data.get(field).and_then(Value::as_str).unwrap_or_default()
}

impl ResponsesStreamParser {
    /// Parses one event's `data`.
    pub(crate) fn push(&mut self, data: &str) -> Result<Vec<UpstreamEvent>, GatewayError> {
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            return Ok(Vec::new());
        }
        let event: Value = serde_json::from_str(data).map_err(|e| {
            GatewayError::bad_gateway(format!("unreadable upstream stream event: {e}"))
        })?;
        let oi = index(&event, "output_index");
        let mut out = Vec::new();
        match text(&event, "type") {
            "response.output_item.added" => {
                let item = event.get("item").unwrap_or(&Value::Null);
                if text(item, "type") == "function_call" {
                    self.saw_function_call = true;
                    out.push(UpstreamEvent::ToolStart {
                        key: format!("f{oi}"),
                        id: item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        name: text(item, "name").to_string(),
                    });
                }
            }
            "response.output_text.delta" => out.push(UpstreamEvent::Text {
                key: format!("t{oi}.{}", index(&event, "content_index")),
                text: text(&event, "delta").to_string(),
            }),
            "response.content_part.done" => out.push(UpstreamEvent::Close {
                key: format!("t{oi}.{}", index(&event, "content_index")),
            }),
            "response.refusal.delta" => {
                out.push(UpstreamEvent::Refusal(text(&event, "delta").to_string()));
            }
            "response.function_call_arguments.delta" => out.push(UpstreamEvent::ToolArgs {
                key: format!("f{oi}"),
                json: text(&event, "delta").to_string(),
            }),
            "response.reasoning_summary_text.delta" => {
                let summary = index(&event, "summary_index");
                let mut delta = text(&event, "delta").to_string();
                if let Some(previous) = self.summary_index.insert(oi, summary) {
                    if previous != summary {
                        delta.insert_str(0, "\n\n");
                    }
                }
                out.push(UpstreamEvent::Reasoning {
                    key: format!("r{oi}"),
                    text: delta,
                });
            }
            "response.reasoning_text.delta" => out.push(UpstreamEvent::Reasoning {
                key: format!("r{oi}"),
                text: text(&event, "delta").to_string(),
            }),
            "response.output_item.done" => {
                let item = event.get("item").unwrap_or(&Value::Null);
                match text(item, "type") {
                    "reasoning" => {
                        let payload = ReasoningPayload {
                            id: text(item, "id").to_string(),
                            encrypted_content: item
                                .get("encrypted_content")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                        };
                        out.push(UpstreamEvent::ReasoningPayload {
                            key: format!("r{oi}"),
                            payload: payload.encode(),
                        });
                        out.push(UpstreamEvent::Close {
                            key: format!("r{oi}"),
                        });
                    }
                    "function_call" => out.push(UpstreamEvent::Close {
                        key: format!("f{oi}"),
                    }),
                    _ => {}
                }
            }
            "response.completed" | "response.incomplete" => {
                let response = event.get("response").unwrap_or(&Value::Null);
                if let Some(usage) = response.get("usage").filter(|u| u.is_object()) {
                    out.push(UpstreamEvent::Usage(UpstreamUsage::from_responses(usage)));
                }
                out.push(UpstreamEvent::Finish(finish_for(
                    response,
                    self.saw_function_call,
                )));
            }
            "response.failed" => {
                let error = event
                    .pointer("/response/error")
                    .map(Value::to_string)
                    .unwrap_or_default();
                return Err(GatewayError::bad_gateway(format!(
                    "upstream response failed: {}",
                    upstream_message(&error)
                )));
            }
            "error" => {
                return Err(GatewayError::bad_gateway(format!(
                    "upstream stream error: {}",
                    upstream_message(data)
                )))
            }
            _ => {}
        }
        Ok(out)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::gateway::completion::Finish;

    fn feed(events: &[&str]) -> Vec<UpstreamEvent> {
        let mut parser = ResponsesStreamParser::default();
        events
            .iter()
            .flat_map(|e| parser.push(e).unwrap())
            .collect()
    }

    #[test]
    fn a_reasoning_text_and_tool_stream() {
        let out = feed(&[
            r#"{"type":"response.created","response":{}}"#,
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs"}}"#,
            r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":0,"delta":"a"}"#,
            r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":0,"delta":"b"}"#,
            r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":1,"delta":"c"}"#,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs","encrypted_content":"E"}}"#,
            r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"message"}}"#,
            r#"{"type":"response.output_text.delta","output_index":1,"content_index":0,"delta":"hi"}"#,
            r#"{"type":"response.content_part.done","output_index":1,"content_index":0}"#,
            r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"message"}}"#,
            r#"{"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","call_id":"c","name":"ls"}}"#,
            r#"{"type":"response.function_call_arguments.delta","output_index":2,"delta":"{}"}"#,
            r#"{"type":"response.output_item.done","output_index":2,"item":{"type":"function_call"}}"#,
            r#"{"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":3,"output_tokens":2}}}"#,
            "[DONE]",
        ]);
        let reasoning: String = out
            .iter()
            .filter_map(|e| match e {
                UpstreamEvent::Reasoning { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(reasoning, "ab\n\nc");
        assert!(out.contains(&UpstreamEvent::ReasoningPayload {
            key: "r0".into(),
            payload: r#"{"id":"rs","encrypted_content":"E"}"#.into()
        }));
        assert!(out.contains(&UpstreamEvent::Text {
            key: "t1.0".into(),
            text: "hi".into()
        }));
        assert!(out.contains(&UpstreamEvent::Close { key: "t1.0".into() }));
        assert!(out.contains(&UpstreamEvent::ToolStart {
            key: "f2".into(),
            id: Some("c".into()),
            name: "ls".into()
        }));
        assert!(out.contains(&UpstreamEvent::ToolArgs {
            key: "f2".into(),
            json: "{}".into()
        }));
        assert!(out.contains(&UpstreamEvent::Close { key: "f2".into() }));
        assert_eq!(out.last(), Some(&UpstreamEvent::Finish(Finish::ToolCalls)));
    }

    #[test]
    fn raw_reasoning_refusals_and_incomplete() {
        let out = feed(&[
            r#"{"type":"response.reasoning_text.delta","output_index":0,"delta":"raw"}"#,
            r#"{"type":"response.refusal.delta","delta":"no"}"#,
            r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}"#,
            "",
        ]);
        assert_eq!(
            out,
            vec![
                UpstreamEvent::Reasoning {
                    key: "r0".into(),
                    text: "raw".into()
                },
                UpstreamEvent::Refusal("no".into()),
                UpstreamEvent::Finish(Finish::Length),
            ]
        );
    }

    #[test]
    fn failures_are_errors() {
        let mut parser = ResponsesStreamParser::default();
        let err = parser
            .push(r#"{"type":"response.failed","response":{"error":{"message":"boom"}}}"#)
            .unwrap_err();
        assert!(err.message.contains("boom"));
        let err = parser
            .push(r#"{"type":"error","message":"rate"}"#)
            .unwrap_err();
        assert!(err.message.contains("rate"));
        assert!(parser.push("{bad").is_err());
    }
}
