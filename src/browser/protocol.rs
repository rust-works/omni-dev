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
}

fn default_method() -> String {
    "GET".to_string()
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

/// `GET /__bridge/status` response body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatusResponse {
    /// Whether an authenticated browser is currently connected.
    pub connected: bool,
    /// The connected browser's `Origin`, if it sent one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub browser_origin: Option<String>,
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
    }

    #[test]
    fn command_round_trips_and_is_newline_free() {
        let cmd = Command {
            id: 7,
            url: "/loki/api/v1/labels".to_string(),
            method: "GET".to_string(),
            headers: BTreeMap::new(),
            body: None,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(!json.contains('\n'));
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
            pending: 0,
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("browser_origin"));
    }
}
