//! Server-sent events: an incremental parser for upstream streams and the
//! encoder for the Anthropic events the gateway sends.

use axum::body::Bytes;
use serde_json::Value;

/// One parsed SSE event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SseEvent {
    /// The `event:` field, if present.
    pub event: Option<String>,
    /// The `data:` lines, joined by `\n`.
    pub data: String,
}

/// An incremental SSE parser: feed it bytes as they arrive, get back the
/// events completed so far. Handles `\n`, `\r\n` and `\r` line endings,
/// comments, multi-line `data`, and events split across chunks.
#[derive(Debug, Default)]
pub(crate) struct SseParser {
    pending: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
}

impl SseParser {
    /// Feeds a chunk.
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.pending.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n' || *b == b'\r') {
            // A `\r\n` pair is one line ending; wait for the `\n` if the `\r`
            // ends the buffer.
            let crlf = self.pending[end] == b'\r' && self.pending.get(end + 1) == Some(&b'\n');
            if self.pending[end] == b'\r' && end + 1 == self.pending.len() {
                break;
            }
            let line = String::from_utf8_lossy(&self.pending[..end]).into_owned();
            self.pending.drain(..end + if crlf { 2 } else { 1 });
            if let Some(event) = self.line(&line) {
                out.push(event);
            }
        }
        out
    }

    /// Flushes an event left unterminated when the stream ends.
    pub(crate) fn finish(&mut self) -> Option<SseEvent> {
        if !self.pending.is_empty() {
            let line = String::from_utf8_lossy(&std::mem::take(&mut self.pending)).into_owned();
            if let Some(event) = self.line(&line) {
                return Some(event);
            }
        }
        self.dispatch()
    }

    fn line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            return self.dispatch();
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            _ => {}
        }
        None
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        if self.data.is_empty() {
            self.event = None;
            return None;
        }
        Some(SseEvent {
            event: self.event.take(),
            data: std::mem::take(&mut self.data).join("\n"),
        })
    }
}

/// Encodes one Anthropic event: `event: <type>\ndata: <json>\n\n`, the type
/// taken from the payload's `type` field.
pub(crate) fn encode(payload: &Value) -> Bytes {
    let kind = payload
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message");
    Bytes::from(format!("event: {kind}\ndata: {payload}\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse_all(chunks: &[&[u8]]) -> Vec<SseEvent> {
        let mut parser = SseParser::default();
        let mut out: Vec<SseEvent> = chunks.iter().flat_map(|c| parser.push(c)).collect();
        out.extend(parser.finish());
        out
    }

    #[test]
    fn events_split_anywhere_reassemble() {
        let raw = b"event: a\ndata: {\"x\":1}\n\n: comment\ndata: one\ndata: two\n\n";
        let expected = vec![
            SseEvent {
                event: Some("a".into()),
                data: "{\"x\":1}".into(),
            },
            SseEvent {
                event: None,
                data: "one\ntwo".into(),
            },
        ];
        assert_eq!(parse_all(&[raw]), expected);
        let bytes: Vec<&[u8]> = raw.chunks(1).collect();
        assert_eq!(parse_all(&bytes), expected);
    }

    #[test]
    fn crlf_and_cr_line_endings() {
        assert_eq!(parse_all(&[b"data: a\r\n\r\n"])[0].data, "a");
        assert_eq!(parse_all(&[b"data: a\r", b"\n\r\n"])[0].data, "a");
        assert_eq!(parse_all(&[b"data: b\r\r"])[0].data, "b");
    }

    #[test]
    fn an_unterminated_final_event_is_flushed() {
        assert_eq!(parse_all(&[b"data: [DONE]"])[0].data, "[DONE]");
        assert_eq!(parse_all(&[b"data: x\n"])[0].data, "x");
        assert!(parse_all(&[b"event: only\n\n", b"retry: 5\n\n"]).is_empty());
        assert_eq!(parse_all(&[b"data"])[0].data, "");
        assert!(parse_all(&[b""]).is_empty());
    }

    #[test]
    fn encode_uses_the_payload_type() {
        let bytes = encode(&json!({"type": "ping"}));
        assert_eq!(&bytes[..], b"event: ping\ndata: {\"type\":\"ping\"}\n\n");
        assert!(encode(&json!({})).starts_with(b"event: message\n"));
    }
}
