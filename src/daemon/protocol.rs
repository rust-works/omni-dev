//! Wire types for the daemon's Unix-domain control socket.
//!
//! The control plane speaks newline-delimited JSON (NDJSON): one
//! [`DaemonEnvelope`] request per line, one [`DaemonReply`] response per line.
//! New optional fields use `#[serde(default, skip_serializing_if = ...)]` so
//! older peers stay byte-compatible on the wire.
//!
//! ## Subscriptions (streaming replies)
//!
//! Most ops are strictly request→one-reply. A **subscription** op is the one
//! exception (#1267): when a service recognises the op as streaming (via
//! [`DaemonService::subscribe`](super::service::DaemonService::subscribe)) the
//! server switches that connection to push mode and emits **many**
//! [`DaemonReply`] lines on the same connection — an initial snapshot, then a
//! fresh snapshot each time the service's state changes (coalesced, and diffed
//! so two identical frames are never sent in a row). Each pushed line is an
//! ordinary `DaemonReply::ok(payload)`; there is **no** new wire type, so a
//! reader distinguishes a subscription only by continuing to read lines instead
//! of stopping after one. The stream ends when the client sends any further line
//! (an explicit cancel), disconnects, or the daemon shuts down.
//!
//! The only subscription today is `worktrees` / `subscribe`, whose payload is
//! the `{ "repos": [...] }` tree snapshot. Back-compat is total: an older client
//! never sends a subscription op, so it only ever sees the classic one-reply
//! exchange.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::service::ServiceStatus;

/// The reserved service name for the daemon's own built-in operations
/// (`ping`, `status`, `shutdown`). A `None` `service` targets the same.
pub const DAEMON_SERVICE: &str = "daemon";

/// Maximum length, in bytes, of a single NDJSON line on the control socket.
///
/// Applies to both requests and replies, capping the per-connection read buffer
/// so a peer that never sends a newline can't exhaust memory. 1 MiB is far above
/// any real envelope.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

/// A request sent to the daemon over the control socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonEnvelope {
    /// Target service [`name`](super::service::DaemonService::name). `None`
    /// (or `"daemon"`) routes to the built-in daemon ops.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// Operation name, interpreted by the target service (or the daemon).
    pub op: String,
    /// Operation payload; `null` when the op takes no arguments.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub payload: Value,
    /// Originating client's request-log `invocation_id`, threaded across the
    /// socket so daemon-side HTTP records correlate to the CLI/MCP invocation
    /// that triggered them rather than the daemon's own (#1198). Non-secret.
    /// Absent from older clients; the daemon simply skips the correlation then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_invocation_id: Option<String>,
}

impl DaemonEnvelope {
    /// Builds an envelope targeting a named service.
    pub fn service(name: impl Into<String>, op: impl Into<String>, payload: Value) -> Self {
        Self {
            service: Some(name.into()),
            op: op.into(),
            payload,
            origin_invocation_id: None,
        }
    }

    /// Builds an envelope targeting the built-in daemon ops.
    pub fn builtin(op: impl Into<String>) -> Self {
        Self {
            service: None,
            op: op.into(),
            payload: Value::Null,
            origin_invocation_id: None,
        }
    }

    /// Stamps the originating client's request-log `invocation_id` on the
    /// envelope so the daemon can correlate the requests it serves back to the
    /// caller's invocation (#1198).
    #[must_use]
    pub fn with_origin(mut self, invocation_id: impl Into<String>) -> Self {
        self.origin_invocation_id = Some(invocation_id.into());
        self
    }
}

/// A response returned by the daemon over the control socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonReply {
    /// Whether the operation succeeded.
    pub ok: bool,
    /// Success payload; `null` for ops that return nothing.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub payload: Value,
    /// Error message when [`ok`](Self::ok) is `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl DaemonReply {
    /// Builds a successful reply carrying `payload`.
    pub fn ok(payload: Value) -> Self {
        Self {
            ok: true,
            payload,
            error: None,
        }
    }

    /// Builds a failure reply carrying an error message.
    pub fn err(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            payload: Value::Null,
            error: Some(message.into()),
        }
    }
}

/// The payload of a built-in `status` reply: per-service status snapshots.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusReport {
    /// One entry per registered service, in registration order.
    pub services: Vec<ServiceStatus>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn envelope_omits_origin_when_absent() {
        // Without an origin the field is skipped, keeping the wire byte-identical
        // to what older clients/servers exchange.
        let line = serde_json::to_string(&DaemonEnvelope::service(
            "snowflake",
            "query",
            serde_json::json!({ "sql": "SELECT 1" }),
        ))
        .unwrap();
        assert!(!line.contains("origin_invocation_id"), "{line}");
    }

    #[test]
    fn envelope_round_trips_origin() {
        let env = DaemonEnvelope::service("snowflake", "query", Value::Null).with_origin("cli-42");
        let line = serde_json::to_string(&env).unwrap();
        assert!(line.contains("origin_invocation_id"), "{line}");
        let back: DaemonEnvelope = serde_json::from_str(&line).unwrap();
        assert_eq!(back.origin_invocation_id.as_deref(), Some("cli-42"));
    }

    #[test]
    fn envelope_from_older_client_defaults_origin_to_none() {
        // A line written before #1198 has no origin field; it must decode fine.
        let back: DaemonEnvelope =
            serde_json::from_str(r#"{"service":"snowflake","op":"query"}"#).unwrap();
        assert!(back.origin_invocation_id.is_none());
    }
}
