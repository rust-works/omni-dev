//! Gateway errors in Anthropic's error envelope.
//!
//! Every failure the gateway returns — its own validation, a policy
//! rejection, or an upstream error — goes back as
//! `{"type": "error", "error": {"type": ..., "message": ...}, "request_id": ...}`
//! with a status the Anthropic SDKs and Claude Code already understand, so a
//! client retries what is retryable and shows the message otherwise.

use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

/// Anthropic's error `type` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ErrorKind {
    /// 400: the request is malformed or uses something the gateway cannot
    /// serve.
    InvalidRequest,
    /// 401: missing or wrong gateway token.
    Authentication,
    /// 403: forbidden (a browser-originated request, or upstream credentials
    /// rejected).
    Permission,
    /// 404: unknown model or path.
    NotFound,
    /// 413: request body too large.
    RequestTooLarge,
    /// 429: rate limited (by the upstream, or the gateway's concurrency cap).
    RateLimit,
    /// 500/502: an internal or upstream failure.
    Api,
    /// 504: the upstream timed out.
    Timeout,
    /// 529: the upstream is overloaded.
    Overloaded,
}

impl ErrorKind {
    /// The wire spelling.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request_error",
            Self::Authentication => "authentication_error",
            Self::Permission => "permission_error",
            Self::NotFound => "not_found_error",
            Self::RequestTooLarge => "request_too_large",
            Self::RateLimit => "rate_limit_error",
            Self::Api => "api_error",
            Self::Timeout => "timeout_error",
            Self::Overloaded => "overloaded_error",
        }
    }
}

/// An error answered in Anthropic's envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GatewayError {
    /// The HTTP status.
    pub status: StatusCode,
    /// The error type.
    pub kind: ErrorKind,
    /// The human-readable message.
    pub message: String,
    /// A `Retry-After` value to forward from the upstream.
    pub retry_after: Option<String>,
    /// The upstream's own status, when this error relays one.
    pub upstream_status: Option<u16>,
}

/// The prefix Claude Code looks for to decide that a conversation needs
/// compacting; Anthropic's own wording for an over-long prompt.
pub(crate) const PROMPT_TOO_LONG: &str = "prompt is too long";

impl GatewayError {
    /// An error with an explicit status and kind.
    pub(crate) fn new(status: StatusCode, kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            status,
            kind,
            message: message.into(),
            retry_after: None,
            upstream_status: None,
        }
    }

    /// 400 `invalid_request_error`.
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, ErrorKind::InvalidRequest, message)
    }

    /// 404 `not_found_error`.
    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, ErrorKind::NotFound, message)
    }

    /// 500 `api_error` for a gateway-internal failure.
    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, ErrorKind::Api, message)
    }

    /// 502 `api_error`: the upstream could not be reached or answered with
    /// something unreadable.
    pub(crate) fn bad_gateway(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, ErrorKind::Api, message)
    }

    /// Maps an upstream's error response onto the status and type an
    /// Anthropic client expects.
    ///
    /// Upstream credentials failures become `403 permission_error` rather than
    /// `401`, because a `401` tells Claude Code its *own* key (the gateway
    /// token) is wrong. A context-length error is reworded to start with
    /// [`PROMPT_TOO_LONG`] so the client compacts and retries.
    pub(crate) fn from_upstream(status: u16, body: &str, retry_after: Option<String>) -> Self {
        let detail = upstream_message(body);
        let (status_code, kind, message) = match status {
            400 | 422 if is_context_length_error(body) => (
                StatusCode::BAD_REQUEST,
                ErrorKind::InvalidRequest,
                format!("{PROMPT_TOO_LONG} for the upstream model: {detail}"),
            ),
            400 | 422 => (
                StatusCode::BAD_REQUEST,
                ErrorKind::InvalidRequest,
                format!("upstream rejected the request (HTTP {status}): {detail}"),
            ),
            401 | 403 => (
                StatusCode::FORBIDDEN,
                ErrorKind::Permission,
                format!(
                    "upstream rejected the gateway's credentials (HTTP {status}); check \
                     OMNI_DEV_GATEWAY_API_KEY: {detail}"
                ),
            ),
            404 => (
                StatusCode::NOT_FOUND,
                ErrorKind::NotFound,
                format!("upstream returned 404: {detail}"),
            ),
            408 | 504 => (
                StatusCode::GATEWAY_TIMEOUT,
                ErrorKind::Timeout,
                format!("upstream timed out (HTTP {status}): {detail}"),
            ),
            413 => (
                StatusCode::PAYLOAD_TOO_LARGE,
                ErrorKind::RequestTooLarge,
                format!("upstream rejected the request size: {detail}"),
            ),
            429 => (
                StatusCode::TOO_MANY_REQUESTS,
                ErrorKind::RateLimit,
                format!("upstream rate limit: {detail}"),
            ),
            503 | 529 => (
                overloaded_status(),
                ErrorKind::Overloaded,
                format!("upstream overloaded (HTTP {status}): {detail}"),
            ),
            _ if status >= 500 => (
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorKind::Api,
                format!("upstream error (HTTP {status}): {detail}"),
            ),
            _ => (
                StatusCode::BAD_GATEWAY,
                ErrorKind::Api,
                format!("unexpected upstream status {status}: {detail}"),
            ),
        };
        Self {
            status: status_code,
            kind,
            message,
            retry_after,
            upstream_status: Some(status),
        }
    }

    /// The error envelope body.
    pub(crate) fn envelope(&self, request_id: &str) -> Value {
        json!({
            "type": "error",
            "error": {"type": self.kind.as_str(), "message": self.message},
            "request_id": request_id,
        })
    }

    /// The `data` of a mid-stream SSE `error` event.
    pub(crate) fn stream_event(&self) -> Value {
        json!({
            "type": "error",
            "error": {"type": self.kind.as_str(), "message": self.message},
        })
    }
}

impl std::fmt::Display for GatewayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind.as_str(), self.message)
    }
}

impl std::error::Error for GatewayError {}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        let request_id = super::anthropic::new_message_id().replacen("msg_", "req_", 1);
        let mut response = (self.status, axum::Json(self.envelope(&request_id))).into_response();
        let headers = response.headers_mut();
        if let Ok(value) = HeaderValue::from_str(&request_id) {
            headers.insert("request-id", value);
        }
        if let Some(retry) = self.retry_after.as_deref() {
            if let Ok(value) = HeaderValue::from_str(retry) {
                headers.insert("retry-after", value);
            }
        }
        response
    }
}

/// Anthropic's non-standard `529 Overloaded`.
fn overloaded_status() -> StatusCode {
    StatusCode::from_u16(529).unwrap_or(StatusCode::SERVICE_UNAVAILABLE)
}

/// Longest upstream detail quoted back to the client.
const MAX_DETAIL_CHARS: usize = 2000;

/// The most specific message an upstream error body offers: OpenAI's
/// `error.message`, Anthropic's `error.message`, a top-level `message` or
/// `detail`, else the raw body (truncated).
pub(crate) fn upstream_message(body: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let message = parsed.as_ref().and_then(|v| {
        v.pointer("/error/message")
            .or_else(|| v.get("message"))
            .or_else(|| v.get("detail"))
            .or_else(|| v.get("error").filter(|e| e.is_string()))
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    let text = message.unwrap_or_else(|| body.trim().to_string());
    if text.is_empty() {
        return "(no detail)".to_string();
    }
    if text.chars().count() > MAX_DETAIL_CHARS {
        let cut: String = text.chars().take(MAX_DETAIL_CHARS).collect();
        format!("{cut}…")
    } else {
        text
    }
}

/// Whether an upstream error body says the prompt exceeded the context
/// window. Matches OpenAI's `context_length_exceeded` code and the phrasings
/// OpenAI-protocol servers use (vLLM, Ollama, Fireworks, OpenRouter).
pub(crate) fn is_context_length_error(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    [
        "context_length_exceeded",
        "context length",
        "context window",
        "maximum context",
        "prompt is too long",
        "too many tokens",
        "exceeds the model's maximum",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn upstream_statuses_map_onto_anthropic_types() {
        let cases = [
            (400, 400, "invalid_request_error"),
            (401, 403, "permission_error"),
            (403, 403, "permission_error"),
            (404, 404, "not_found_error"),
            (408, 504, "timeout_error"),
            (413, 413, "request_too_large"),
            (422, 400, "invalid_request_error"),
            (429, 429, "rate_limit_error"),
            (500, 500, "api_error"),
            (502, 500, "api_error"),
            (503, 529, "overloaded_error"),
            (504, 504, "timeout_error"),
            (529, 529, "overloaded_error"),
            (302, 502, "api_error"),
        ];
        for (upstream, status, kind) in cases {
            let err = GatewayError::from_upstream(upstream, "{}", None);
            assert_eq!(err.status.as_u16(), status, "upstream {upstream}");
            assert_eq!(err.kind.as_str(), kind, "upstream {upstream}");
        }
    }

    #[test]
    fn a_context_length_error_is_reworded_for_compaction() {
        let body = r#"{"error":{"message":"This model's maximum context length is 8192 tokens","code":"context_length_exceeded"}}"#;
        let err = GatewayError::from_upstream(400, body, None);
        assert!(err.message.starts_with(PROMPT_TOO_LONG), "{}", err.message);
        assert!(err.message.contains("8192"));
    }

    #[test]
    fn upstream_message_prefers_the_structured_message() {
        assert_eq!(upstream_message(r#"{"error":{"message":"bad"}}"#), "bad");
        assert_eq!(upstream_message(r#"{"message":"m"}"#), "m");
        assert_eq!(upstream_message(r#"{"detail":"d"}"#), "d");
        assert_eq!(upstream_message(r#"{"error":"flat"}"#), "flat");
        assert_eq!(upstream_message("plain text "), "plain text");
        assert_eq!(upstream_message(""), "(no detail)");
        let long = "x".repeat(MAX_DETAIL_CHARS + 10);
        assert!(upstream_message(&long).ends_with('…'));
    }

    #[test]
    fn the_envelope_has_anthropic_shape() {
        let err = GatewayError::invalid("nope");
        let body = err.envelope("req_1");
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["message"], "nope");
        assert_eq!(body["request_id"], "req_1");
        assert_eq!(err.stream_event()["error"]["message"], "nope");
        assert_eq!(err.to_string(), "invalid_request_error: nope");
    }

    #[tokio::test]
    async fn into_response_sets_status_request_id_and_retry_after() {
        let err = GatewayError::from_upstream(429, "{}", Some("7".into()));
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()["retry-after"], "7");
        assert!(response.headers()["request-id"]
            .to_str()
            .unwrap()
            .starts_with("req_gw_"));
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["type"], "rate_limit_error");
    }

    #[test]
    fn constructors_pick_their_statuses() {
        assert_eq!(GatewayError::not_found("x").status, StatusCode::NOT_FOUND);
        assert_eq!(
            GatewayError::internal("x").status,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            GatewayError::bad_gateway("x").status,
            StatusCode::BAD_GATEWAY
        );
    }
}
