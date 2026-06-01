//! Wire-protocol types for the browser bridge.
//!
//! Three layers share these structs. The **control plane** accepts a
//! [`ControlRequest`] body on `POST /__bridge/request` (and synthesises one for
//! the transparent proxy). The **WebSocket plane** serialises a [`Command`] to
//! the browser and deserialises a [`BrowserReply`] back. The control plane then
//! returns a [`ResponseEnvelope`] to the caller. Every frame is newline-free
//! JSON correlated by a monotonic integer `id` assigned by the server; the
//! browser echoes it back unchanged.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A request as supplied by a control-plane caller (the `POST /__bridge/request`
/// body, or synthesised from the path/method/headers of a transparent-proxy
/// request).
///
/// `url` is resolved against the page origin by the browser snippet; absolute
/// URLs are rejected by the server unless an `--allow-origin` permits them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControlRequest {
    /// Request URL. Relative (page-origin) by default.
    pub url: String,
    /// HTTP method. Defaults to `GET` when omitted.
    #[serde(default = "default_method")]
    pub method: String,
    /// Request headers. Forwarded verbatim to the browser `fetch()`.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Request body, or `null` for no body.
    #[serde(default)]
    pub body: Option<String>,
    /// When `true`, the response is streamed back as incremental chunk frames
    /// (`response.body.getReader()`) rather than buffered into one reply.
    #[serde(default)]
    pub stream: bool,
    /// Which connected tab to route this request to: a connection id (the
    /// canonical, always-unambiguous selector) or an `Origin` string that
    /// uniquely matches one tab. Omitted is allowed only when exactly one tab is
    /// connected. The `X-Omni-Bridge-Target` header takes precedence over this
    /// field. Server-side routing only — never sent to the browser.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Request-scoped outbound-origin override. When present it takes
    /// precedence over `serve --allow-origin` for *this request's*
    /// [`crate::browser::auth::validate_outbound_url`] check only, letting one
    /// request target a cross-origin URL without affecting the connection-time
    /// `ws_origin_allowed` gate. Server-side scope check only — never sent to
    /// the browser.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_origin: Option<String>,
    /// Fetch credentials mode (`include` | `omit` | `same-origin`). Absent means
    /// the browser snippet defaults to `include`, preserving v1 behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials: Option<String>,
}

fn default_method() -> String {
    "GET".to_string()
}

/// Serde predicate: skip a `bool` field when it is `false` (the default), so
/// buffered command frames stay byte-identical to the pre-streaming wire format.
// `skip_serializing_if` requires `fn(&T) -> bool`, so the `&bool` is mandatory.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
}

/// Server → browser command frame.
///
/// Identical shape to [`ControlRequest`] plus the server-assigned `id`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Command {
    /// Server-assigned correlation id; echoed back by the browser.
    pub id: u64,
    /// Request URL (already scope-validated by the server).
    pub url: String,
    /// HTTP method.
    pub method: String,
    /// Request headers.
    pub headers: BTreeMap<String, String>,
    /// Request body, or `null`.
    pub body: Option<String>,
    /// When `true`, the browser streams the response as chunk frames. Omitted
    /// from the wire when `false` for back-compat with buffered clients.
    #[serde(default, skip_serializing_if = "is_false")]
    pub stream: bool,
    /// Fetch credentials mode (`include` | `omit` | `same-origin`), or `null`
    /// to let the browser snippet default to `include`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials: Option<String>,
}

/// Server → browser cancellation frame.
///
/// Sent to stop an in-flight streamed response (the control-plane consumer
/// disconnected, or a limit tripped) so the browser cancels its reader.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CancelCommand {
    /// Correlation id of the stream to cancel.
    pub id: u64,
    /// Always `true`; distinguishes this frame from a [`Command`] in the browser.
    pub cancel: bool,
}

impl CancelCommand {
    /// Builds a cancellation frame for the given stream id.
    #[must_use]
    pub fn new(id: u64) -> Self {
        Self { id, cancel: true }
    }
}

/// Browser → server reply frame.
///
/// Either a success (`status`/`headers`/`body` present) or an error
/// (`error` present). Modelled as a flat struct of `Option`s so a single
/// `serde` deserialise accepts both shapes; [`BrowserReply::outcome`]
/// classifies it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BrowserReply {
    /// Correlation id echoed from the [`Command`].
    pub id: u64,
    /// HTTP status code on success.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub status: Option<u16>,
    /// Response headers on success.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub headers: Option<BTreeMap<String, String>>,
    /// Response body on success. Plain text unless [`Self::encoding`] tags it.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub body: Option<String>,
    /// Body transfer encoding. `Some("base64")` when the browser read a
    /// non-text body via `arrayBuffer()` and base64-encoded it; absent (the
    /// default) means `body` is plain text, for back-compat with v1.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub encoding: Option<String>,
    /// Error message when the browser `fetch()` failed.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<String>,
}

/// Classified outcome of a [`BrowserReply`].
pub enum ReplyOutcome {
    /// A successful response with status, headers, and body.
    Success {
        /// HTTP status code.
        status: u16,
        /// Response headers.
        headers: BTreeMap<String, String>,
        /// Response body (base64-encoded when `encoding` is `Some("base64")`).
        body: String,
        /// Body transfer encoding (`Some("base64")` for binary bodies).
        encoding: Option<String>,
    },
    /// The browser reported a `fetch()` failure.
    Error(String),
}

impl BrowserReply {
    /// Classifies this reply as success or error.
    ///
    /// A reply carrying an `error` is an error; otherwise the success fields
    /// are taken with sensible defaults for any that the browser omitted.
    pub fn outcome(self) -> ReplyOutcome {
        match self.error {
            Some(error) => ReplyOutcome::Error(error),
            None => ReplyOutcome::Success {
                status: self.status.unwrap_or(0),
                headers: self.headers.unwrap_or_default(),
                body: self.body.unwrap_or_default(),
                encoding: self.encoding,
            },
        }
    }
}

/// Browser → server frame: a superset of [`BrowserReply`] plus the streaming
/// fields (`stream`/`chunk`/`seq`/`done`).
///
/// The WebSocket reader deserialises every inbound frame into this struct, then
/// interprets it as a buffered reply or a [`StreamItem`] depending on which
/// waiter is registered for its `id`. Modelled as a flat struct of `Option`s so
/// one `serde` deserialise accepts every shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BrowserFrame {
    /// Correlation id echoed from the [`Command`].
    pub id: u64,
    /// HTTP status code (buffered success, or a stream's head frame).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub status: Option<u16>,
    /// Response headers (buffered success, or a stream's head frame).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub headers: Option<BTreeMap<String, String>>,
    /// Buffered response body.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub body: Option<String>,
    /// Buffered body transfer encoding (`Some("base64")` for binary bodies).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub encoding: Option<String>,
    /// Error message when the browser `fetch()` failed.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<String>,
    /// `Some(true)` on the first frame of a streamed response (head: status +
    /// headers, no body yet).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub stream: Option<bool>,
    /// Base64-encoded body chunk of a streamed response.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub chunk: Option<String>,
    /// Monotonic chunk sequence number within a stream.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub seq: Option<u64>,
    /// `Some(true)` on the terminating frame of a streamed response.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub done: Option<bool>,
}

/// One item of a streamed browser response, derived from a [`BrowserFrame`] when
/// the registered waiter is a stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamItem {
    /// Stream head: status and headers, before any body bytes.
    Head {
        /// HTTP status code.
        status: u16,
        /// Response headers.
        headers: BTreeMap<String, String>,
    },
    /// A base64-encoded body chunk.
    Chunk {
        /// Monotonic chunk sequence number.
        seq: u64,
        /// Base64-encoded chunk bytes.
        data: String,
    },
    /// The stream terminated normally.
    End,
    /// The browser reported an error (before or mid-stream).
    Error(String),
}

impl BrowserFrame {
    /// Interprets this frame as a buffered reply (the back-compat path taken
    /// when the registered waiter is a one-shot buffered request).
    #[must_use]
    pub fn into_reply(self) -> BrowserReply {
        BrowserReply {
            id: self.id,
            status: self.status,
            headers: self.headers,
            body: self.body,
            encoding: self.encoding,
            error: self.error,
        }
    }

    /// Interprets this frame as a [`StreamItem`] (taken when the registered
    /// waiter is a stream). An `error` wins; then `done`; then a `chunk`;
    /// otherwise the frame is the stream's head.
    #[must_use]
    pub fn stream_item(self) -> StreamItem {
        if let Some(error) = self.error {
            StreamItem::Error(error)
        } else if self.done == Some(true) {
            StreamItem::End
        } else if let Some(data) = self.chunk {
            StreamItem::Chunk {
                seq: self.seq.unwrap_or(0),
                data,
            }
        } else {
            StreamItem::Head {
                status: self.status.unwrap_or(0),
                headers: self.headers.unwrap_or_default(),
            }
        }
    }
}

/// One NDJSON line of a streamed `POST /__bridge/request` response.
///
/// The server serialises these (one per line); the thin client deserialises
/// them. Untagged so each line is the bare object the operator sees
/// (`{status,headers}` / `{seq,chunk}` / `{done}` / `{error}`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum StreamLine {
    /// Head line: status and headers.
    Head {
        /// HTTP status code.
        status: u16,
        /// Response headers.
        headers: BTreeMap<String, String>,
    },
    /// Chunk line: a base64-encoded body chunk.
    Chunk {
        /// Monotonic chunk sequence number.
        seq: u64,
        /// Base64-encoded chunk bytes.
        chunk: String,
    },
    /// Terminating line.
    Done {
        /// Always `true`.
        done: bool,
    },
    /// Error line.
    Error {
        /// Error message.
        error: String,
    },
}

/// Control-plane response envelope returned to the caller of
/// `POST /__bridge/request`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResponseEnvelope {
    /// Correlation id of the request this envelope answers.
    pub id: u64,
    /// HTTP status returned by the browser.
    pub status: u16,
    /// Response headers returned by the browser.
    pub headers: BTreeMap<String, String>,
    /// Response body returned by the browser. Base64-encoded when
    /// [`Self::encoding`] is `Some("base64")`; the caller decodes it.
    pub body: String,
    /// Body transfer encoding. `Some("base64")` for binary bodies; absent (the
    /// default) means `body` is plain text.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub encoding: Option<String>,
}

/// One connected tab, as reported by `GET /__bridge/status`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TabInfo {
    /// Server-assigned connection id; the canonical routing selector.
    pub id: u64,
    /// The connecting tab's `Origin`, if it sent one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

/// `GET /__bridge/status` response body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatusResponse {
    /// Whether at least one authenticated browser tab is currently connected.
    pub connected: bool,
    /// The connected tab's `Origin` when exactly one tab is connected; `None`
    /// when zero or several are (ambiguous). Retained for v1 back-compat — use
    /// [`Self::tabs`] for the full picture.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub browser_origin: Option<String>,
    /// Every connected tab (id + origin), for routing with a `target`.
    pub tabs: Vec<TabInfo>,
    /// Number of in-flight requests awaiting a browser reply.
    pub pending: usize,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn control_request_defaults_method_and_body() {
        let req: ControlRequest = serde_json::from_str(r#"{"url":"/x"}"#).unwrap();
        assert_eq!(req.method, "GET");
        assert!(req.body.is_none());
        assert!(req.headers.is_empty());
        // The per-request outbound-origin override defaults to absent.
        assert!(req.allow_origin.is_none());
    }

    #[test]
    fn control_request_omits_allow_origin_when_absent() {
        let req = ControlRequest {
            url: "/x".to_string(),
            method: "GET".to_string(),
            headers: BTreeMap::new(),
            body: None,
            stream: false,
            target: None,
            allow_origin: None,
            credentials: None,
        };
        // Back-compat: an absent override is not serialised, so the wire body
        // stays byte-identical to a pre-feature client's.
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("allow_origin"));

        // A present override round-trips.
        let with = ControlRequest {
            allow_origin: Some("https://ok.test".to_string()),
            ..req
        };
        let json = serde_json::to_string(&with).unwrap();
        let back: ControlRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.allow_origin.as_deref(), Some("https://ok.test"));
    }

    #[test]
    fn command_round_trips_and_is_newline_free() {
        let cmd = Command {
            id: 7,
            url: "/loki/api/v1/labels".to_string(),
            method: "GET".to_string(),
            headers: BTreeMap::new(),
            body: None,
            stream: false,
            credentials: None,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(!json.contains('\n'));
        // A buffered command omits `stream` entirely (wire back-compat).
        assert!(!json.contains("stream"));
        let back: Command = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, back);
    }

    #[test]
    fn streaming_command_serialises_stream_flag() {
        let cmd = Command {
            id: 1,
            url: "/sse".to_string(),
            method: "GET".to_string(),
            headers: BTreeMap::new(),
            body: None,
            stream: true,
            credentials: None,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"stream\":true"));
    }

    #[test]
    fn cancel_command_serialises_with_cancel_true() {
        let json = serde_json::to_string(&CancelCommand::new(9)).unwrap();
        assert_eq!(json, r#"{"id":9,"cancel":true}"#);
    }

    #[test]
    fn frame_classifies_stream_head_chunk_and_end() {
        let head: BrowserFrame =
            serde_json::from_str(r#"{"id":1,"status":200,"headers":{"a":"b"},"stream":true}"#)
                .unwrap();
        assert!(matches!(
            head.stream_item(),
            StreamItem::Head { status: 200, headers } if headers.get("a").map(String::as_str) == Some("b")
        ));

        let chunk: BrowserFrame =
            serde_json::from_str(r#"{"id":1,"seq":3,"chunk":"aGk="}"#).unwrap();
        assert!(matches!(
            chunk.stream_item(),
            StreamItem::Chunk { seq: 3, data } if data == "aGk="
        ));

        let end: BrowserFrame = serde_json::from_str(r#"{"id":1,"done":true}"#).unwrap();
        assert_eq!(end.stream_item(), StreamItem::End);

        let err: BrowserFrame = serde_json::from_str(r#"{"id":1,"error":"boom"}"#).unwrap();
        assert_eq!(err.stream_item(), StreamItem::Error("boom".into()));
    }

    #[test]
    fn frame_into_reply_preserves_buffered_fields() {
        let frame: BrowserFrame = serde_json::from_str(
            r#"{"id":2,"status":200,"headers":{},"body":"hi","encoding":"base64"}"#,
        )
        .unwrap();
        let reply = frame.into_reply();
        assert_eq!(reply.id, 2);
        assert!(matches!(
            reply.outcome(),
            ReplyOutcome::Success { body, encoding, .. }
                if body == "hi" && encoding.as_deref() == Some("base64")
        ));
    }

    #[test]
    fn stream_lines_round_trip_untagged() {
        for (line, json) in [
            (
                StreamLine::Head {
                    status: 200,
                    headers: BTreeMap::new(),
                },
                r#"{"status":200,"headers":{}}"#,
            ),
            (
                StreamLine::Chunk {
                    seq: 0,
                    chunk: "aGk=".into(),
                },
                r#"{"seq":0,"chunk":"aGk="}"#,
            ),
            (StreamLine::Done { done: true }, r#"{"done":true}"#),
            (
                StreamLine::Error {
                    error: "boom".into(),
                },
                r#"{"error":"boom"}"#,
            ),
        ] {
            let serialised = serde_json::to_string(&line).unwrap();
            assert_eq!(serialised, json);
            assert!(!serialised.contains('\n'));
            let back: StreamLine = serde_json::from_str(json).unwrap();
            assert_eq!(back, line);
        }
    }

    #[test]
    fn control_request_defaults_credentials_to_none() {
        let req: ControlRequest = serde_json::from_str(r#"{"url":"/x"}"#).unwrap();
        assert!(req.credentials.is_none());
    }

    #[test]
    fn control_request_omits_credentials_when_absent() {
        let req = ControlRequest {
            url: "/x".to_string(),
            method: "GET".to_string(),
            headers: BTreeMap::new(),
            body: None,
            stream: false,
            target: None,
            allow_origin: None,
            credentials: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("credentials"));
    }

    #[test]
    fn command_serializes_credentials_when_present() {
        let cmd = Command {
            id: 1,
            url: "/x".to_string(),
            method: "GET".to_string(),
            headers: BTreeMap::new(),
            body: None,
            stream: false,
            credentials: Some("omit".to_string()),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains(r#""credentials":"omit""#));
        let back: Command = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, back);
    }

    #[test]
    fn success_reply_classifies_as_success() {
        let reply: BrowserReply =
            serde_json::from_str(r#"{"id":7,"status":200,"headers":{"a":"b"},"body":"hi"}"#)
                .unwrap();
        assert_eq!(reply.id, 7);
        // `matches!` keeps the assertion to one expression so there is no
        // never-taken `panic!` arm to register as an uncovered line.
        assert!(
            matches!(reply.outcome(),
                ReplyOutcome::Success { status, headers, body, encoding }
                    if status == 200
                        && headers.get("a").map(String::as_str) == Some("b")
                        && body == "hi"
                        && encoding.is_none()),
            "success reply must classify as Success with the expected fields"
        );
    }

    #[test]
    fn base64_reply_carries_encoding_through_outcome() {
        let reply: BrowserReply = serde_json::from_str(
            r#"{"id":7,"status":200,"headers":{},"body":"iVBOR=","encoding":"base64"}"#,
        )
        .unwrap();
        // `matches!` keeps the whole assertion on one expression so there is no
        // never-taken `panic!` arm to register as an uncovered line.
        assert!(
            matches!(reply.outcome(),
                ReplyOutcome::Success { body, encoding, .. }
                    if body == "iVBOR=" && encoding.as_deref() == Some("base64")),
            "base64 reply must classify as Success with its encoding preserved"
        );
    }

    #[test]
    fn text_reply_omits_encoding_on_serialise() {
        let reply = BrowserReply {
            id: 1,
            status: Some(200),
            headers: None,
            body: Some("hi".into()),
            encoding: None,
            error: None,
        };
        let json = serde_json::to_string(&reply).unwrap();
        assert!(!json.contains("encoding"));
    }

    #[test]
    fn envelope_omits_encoding_when_text() {
        let env = ResponseEnvelope {
            id: 1,
            status: 200,
            headers: BTreeMap::new(),
            body: "hi".into(),
            encoding: None,
        };
        let json = serde_json::to_string(&env).unwrap();
        assert!(!json.contains("encoding"));
    }

    #[test]
    fn error_reply_classifies_as_error() {
        let reply: BrowserReply =
            serde_json::from_str(r#"{"id":7,"error":"Failed to fetch"}"#).unwrap();
        match reply.outcome() {
            ReplyOutcome::Error(msg) => assert_eq!(msg, "Failed to fetch"),
            ReplyOutcome::Success { .. } => panic!("expected error"),
        }
    }

    #[test]
    fn status_response_omits_origin_when_absent() {
        let s = StatusResponse {
            connected: false,
            browser_origin: None,
            tabs: Vec::new(),
            pending: 0,
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("browser_origin"));
    }
}
