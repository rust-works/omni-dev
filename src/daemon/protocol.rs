//! Wire types for the daemon's Unix-domain control socket.
//!
//! The control plane speaks newline-delimited JSON (NDJSON): one
//! [`DaemonEnvelope`] request per line, one [`DaemonReply`] response per line.
//! New optional fields use `#[serde(default, skip_serializing_if = ...)]` so
//! older peers stay byte-compatible on the wire.

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
}

impl DaemonEnvelope {
    /// Builds an envelope targeting a named service.
    pub fn service(name: impl Into<String>, op: impl Into<String>, payload: Value) -> Self {
        Self {
            service: Some(name.into()),
            op: op.into(),
            payload,
        }
    }

    /// Builds an envelope targeting the built-in daemon ops.
    pub fn builtin(op: impl Into<String>) -> Self {
        Self {
            service: None,
            op: op.into(),
            payload: Value::Null,
        }
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
