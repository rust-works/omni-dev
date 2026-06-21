//! [`DaemonClient`]: a thin client for the daemon's Unix control socket, used
//! by `daemon status` / `stop` / `restart` and the single-instance `ping`
//! probe.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use futures::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio_util::codec::{Framed, LinesCodec};

use super::protocol::{DaemonEnvelope, DaemonReply, StatusReport, MAX_LINE_BYTES};

/// A one-shot client over the daemon's Unix-domain control socket. Each call
/// opens a fresh connection, sends one [`DaemonEnvelope`], and reads one
/// [`DaemonReply`].
#[derive(Debug, Clone)]
pub struct DaemonClient {
    socket_path: PathBuf,
}

impl DaemonClient {
    /// Creates a client targeting the socket at `socket_path`.
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    /// The socket path this client targets.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Sends one envelope and returns the daemon's reply.
    pub async fn request(&self, envelope: DaemonEnvelope) -> Result<DaemonReply> {
        let stream = UnixStream::connect(&self.socket_path)
            .await
            .with_context(|| {
                format!(
                    "failed to connect to daemon socket {} (is the daemon running?)",
                    self.socket_path.display()
                )
            })?;
        let mut framed = Framed::new(stream, LinesCodec::new_with_max_length(MAX_LINE_BYTES));
        let line = serde_json::to_string(&envelope).context("failed to encode daemon request")?;
        framed
            .send(line)
            .await
            .context("failed to send daemon request")?;
        let response = framed
            .next()
            .await
            .context("daemon closed the connection without replying")?
            .context("failed to read daemon reply")?;
        serde_json::from_str(&response).context("failed to decode daemon reply")
    }

    /// Sends an envelope and returns its payload, turning an `ok: false` reply
    /// into an `Err`.
    async fn request_ok(&self, envelope: DaemonEnvelope) -> Result<serde_json::Value> {
        let reply = self.request(envelope).await?;
        if reply.ok {
            Ok(reply.payload)
        } else {
            bail!(
                "daemon returned an error: {}",
                reply.error.as_deref().unwrap_or("unknown error")
            )
        }
    }

    /// Probes whether a live daemon is answering on the socket.
    pub async fn ping(&self) -> Result<()> {
        self.request_ok(DaemonEnvelope::builtin("ping"))
            .await
            .map(|_| ())
    }

    /// Requests aggregated per-service status.
    pub async fn status(&self) -> Result<StatusReport> {
        let payload = self.request_ok(DaemonEnvelope::builtin("status")).await?;
        serde_json::from_value(payload).context("failed to decode daemon status report")
    }

    /// Asks the daemon to shut down gracefully.
    pub async fn shutdown(&self) -> Result<()> {
        self.request_ok(DaemonEnvelope::builtin("shutdown"))
            .await
            .map(|_| ())
    }
}
